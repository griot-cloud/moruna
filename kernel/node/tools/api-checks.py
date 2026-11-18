#!/usr/bin/env python3
"""The API's proof from outside the cloud (work item 2.6; system design 2B, 2C and 2D): run last
by azure-up.sh and openstack-up.sh on every run, against the node they made.

    api-checks.py --url https://ADDRESS:443 --pin SHA256 --token-key PEM --instance ID
                  --cloud azure|openstack [--openssl BIN] [--results FILE] [--timeout S]

Every connection is pinned: the node's certificate is taken only when the SHA-256 of its DER is
`--pin`, the fingerprint its console printed (`moruna: tls sha256=<hex>`), and nothing is sent
before that, so no token reaches a server that is not the node.

The checks, in order:

1. `GET /v1/health`, with no token: `200` and `{"status":"ok"}`.
2. `GET /v1/node` with an `observe` token made from `--token-key` for `--instance` (2C: an EdDSA
   JWT, 10 minutes): `200`, its `cloud` is `--cloud` and its `instance` is `--instance`.
3. Over MCP, `POST /mcp` with the same token (2D: Streamable HTTP, stateless): `initialize`
   answers as server `moruna`, and `notifications/initialized` is taken (`202`); `tools/list`
   names `moruna_health`, `moruna_node` and `moruna_node_audit`; `tools/call` of `moruna_node` is
   no tool error, and its `structuredContent` (the same JSON as its text) is the node document,
   with the same cloud and instance.
4. Refused with `401` and the error `unauthorized`, on `GET /v1/node` and on `POST /mcp` alike:
   a token signed by another Ed25519 key, made here for this run and thrown away, and a token of
   `--token-key` whose `aud` is another instance.

It prints a line for each check and the verdict, and writes them as JSON to `--results`. The
tokens are neither printed nor written. Each request is bounded by `--timeout` (30 s). It exits 0
when every check passes, 1 when any fails, and 2 on a usage error.

The rest of 2.6, `api` killed and the node still answering, cannot be done on a cloud node: a node
has no way in but its API. Item 2.2c's booted test covers it.

OpenSSL 3 signs the tokens (`openssl pkeyutl -sign -rawin`); macOS's own `openssl` is LibreSSL,
which has no Ed25519, so `--openssl` names another (by default `MORUNA_OPENSSL`, else Homebrew's
when it is there, else `openssl`). Standard library only.
"""
import argparse
import base64
import hashlib
import http.client
import json
import os
import re
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.parse
import uuid

HEADER = {"alg": "EdDSA", "typ": "JWT"}
TOOLS = ("moruna_health", "moruna_node", "moruna_node_audit")
TOKEN_TTL_S = 600
PROTOCOL = "2025-06-18"


def default_openssl():
    if os.environ.get("MORUNA_OPENSSL"):
        return os.environ["MORUNA_OPENSSL"]
    return "/opt/homebrew/bin/openssl" if os.access("/opt/homebrew/bin/openssl", os.X_OK) else "openssl"


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def compact(value) -> bytes:
    return json.dumps(value, separators=(",", ":")).encode()


def token(sign, key: str, aud: str, scope: str, now: float) -> str:
    """An EdDSA JWT for node `aud` (2C), signed by `sign(key, bytes)`, expiring in 10 minutes."""
    claims = {"aud": aud, "exp": int(now) + TOKEN_TTL_S, "scope": [scope]}
    signed = f"{b64url(compact(HEADER))}.{b64url(compact(claims))}"
    return f"{signed}.{b64url(sign(key, signed.encode()))}"


class OpenSSL:
    """Ed25519 keys and signatures by OpenSSL 3's command line."""

    def __init__(self, program: str):
        self.program = program

    def keygen(self, path: str) -> None:
        subprocess.run([self.program, "genpkey", "-algorithm", "ed25519", "-out", path], check=True,
                       capture_output=True, timeout=30)
        os.chmod(path, 0o600)

    def sign(self, key: str, data: bytes) -> bytes:
        with tempfile.NamedTemporaryFile() as f:
            f.write(data)
            f.flush()
            done = subprocess.run([self.program, "pkeyutl", "-sign", "-inkey", key, "-rawin", "-in", f.name],
                                  check=True, capture_output=True, timeout=30)
        return done.stdout


class PinMismatch(ConnectionError):
    pass


class Pinned:
    """HTTPS to the node, one connection a request, its certificate pinned before anything is
    sent: `(method, path, body, token) -> (status, bytes)`."""

    def __init__(self, url: str, pin: str, timeout: float):
        parts = urllib.parse.urlsplit(url)
        self.host, self.port, self.pin, self.timeout = parts.hostname, parts.port or 443, pin.lower(), timeout

    def __call__(self, method, path, body=None, token=None):
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        conn = http.client.HTTPSConnection(self.host, self.port, context=ctx, timeout=self.timeout)
        try:
            conn.connect()
            served = hashlib.sha256(conn.sock.getpeercert(binary_form=True)).hexdigest()
            if served != self.pin:
                raise PinMismatch(f"the certificate served is {served}, not the pinned {self.pin}: nothing sent")
            headers = {}
            if token is not None:
                headers["Authorization"] = f"Bearer {token}"
            data = None
            if body is not None:
                data = compact(body)
                headers["Content-Type"] = "application/json"
                headers["Accept"] = "application/json, text/event-stream"
            conn.request(method, path, body=data, headers=headers)
            answer = conn.getresponse()
            return answer.status, answer.read()
        finally:
            conn.close()


def parsed(data: bytes):
    """The answer's JSON, or None when it is none."""
    try:
        return json.loads(data)
    except ValueError:
        return None


def shown(data: bytes, limit: int = 200) -> str:
    return data[:limit].decode(errors="replace")


def rpc(n: int, method: str, params=None):
    message = {"jsonrpc": "2.0", "id": n, "method": method}
    if params is not None:
        message["params"] = params
    return message


class Checks:
    """The checks against one node, over `transport`, with the three tokens."""

    def __init__(self, transport, cloud: str, instance: str, good: str, other_key: str, other_aud: str, say=print):
        self.t, self.cloud, self.instance, self.say = transport, cloud, instance, say
        self.good, self.other_key, self.other_aud = good, other_key, other_aud
        self.results = []

    def check(self, name, fn):
        try:
            ok, detail = fn()
        except (OSError, http.client.HTTPException, ValueError, KeyError, TypeError, AttributeError) as e:
            ok, detail = False, f"{type(e).__name__}: {e}"
        self.results.append({"check": name, "ok": bool(ok), "detail": detail})
        self.say(f"api-checks: {'pass' if ok else 'FAIL'}: {name}: {detail}")

    def the_node(self, doc) -> bool:
        return isinstance(doc, dict) and doc.get("cloud") == self.cloud and doc.get("instance") == self.instance

    def health(self):
        status, data = self.t("GET", "/v1/health")
        return status == 200 and parsed(data) == {"status": "ok"}, f"{status} {shown(data)}"

    def node(self):
        status, data = self.t("GET", "/v1/node", token=self.good)
        doc = parsed(data)
        if not isinstance(doc, dict):
            return False, f"{status} {shown(data)}"
        return status == 200 and self.the_node(doc), (
            f"{status}, cloud {doc.get('cloud')}, instance {doc.get('instance')}, version {doc.get('version')}, "
            f"health {doc.get('health')}")

    def mcp(self, message, token=None):
        status, data = self.t("POST", "/mcp", message, self.good if token is None else token)
        return status, data, parsed(data)

    def initialize(self):
        status, data, answer = self.mcp(rpc(1, "initialize", {
            "protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "api-checks", "version": "1"}}))
        result = (answer or {}).get("result") or {}
        server = result.get("serverInfo") or {}
        if status != 200 or server.get("name") != "moruna":
            return False, f"{status} {shown(data)}"
        initialized, data = self.t("POST", "/mcp", {"jsonrpc": "2.0", "method": "notifications/initialized"},
                                   self.good)
        return initialized == 202, (f"server {server.get('name')} {server.get('version')}, protocol "
                                    f"{result.get('protocolVersion')}; notifications/initialized {initialized}")

    def tools_list(self):
        status, data, answer = self.mcp(rpc(2, "tools/list"))
        tools = ((answer or {}).get("result") or {}).get("tools")
        if status != 200 or not isinstance(tools, list):
            return False, f"{status} {shown(data)}"
        names = [tool.get("name") for tool in tools]
        missing = [name for name in TOOLS if name not in names]
        detail = f"{len(names)} tools" + (f", missing {', '.join(missing)}" if missing else
                                          f", {', '.join(TOOLS)} among them")
        return not missing, detail

    def tools_call(self):
        status, data, answer = self.mcp(rpc(3, "tools/call", {"name": "moruna_node", "arguments": {}}))
        result = (answer or {}).get("result")
        if status != 200 or not isinstance(result, dict):
            return False, f"{status} {shown(data)}"
        if result.get("isError"):
            return False, f"a tool error: {json.dumps(result.get('structuredContent'))[:200]}"
        doc = result.get("structuredContent")
        text = [c.get("text") for c in result.get("content") or [] if c.get("type") == "text"]
        same = len(text) == 1 and parsed(text[0].encode()) == doc
        detail = (f"cloud {doc.get('cloud')}, instance {doc.get('instance')}" if isinstance(doc, dict) else
                  f"no structuredContent: {shown(data)}")
        if not same:
            detail += "; its text is not the same JSON"
        return self.the_node(doc) and same, detail

    def refused(self, path, bad):
        if path == "/mcp":
            status, data = self.t("POST", "/mcp", rpc(4, "tools/list"), bad)
        else:
            status, data = self.t("GET", path, token=bad)
        answer = parsed(data)
        error = answer.get("error") if isinstance(answer, dict) else None
        if not isinstance(error, dict):
            return False, f"{status} {shown(data)}"
        ok = status == 401 and error.get("code") == "unauthorized"
        return ok, f"{status} {error.get('code')}: {error.get('message')}"

    def all(self):
        self.check("GET /v1/health, no token", self.health)
        self.check("GET /v1/node over HTTPS, an observe token", self.node)
        self.check("MCP initialize", self.initialize)
        self.check("MCP tools/list", self.tools_list)
        self.check("MCP tools/call moruna_node", self.tools_call)
        for what, bad in (("a token signed by another key", self.other_key),
                          ("a token whose aud is another instance", self.other_aud)):
            self.check(f"GET /v1/node refuses {what}", lambda bad=bad: self.refused("/v1/node", bad))
            self.check(f"POST /mcp refuses {what}", lambda bad=bad: self.refused("/mcp", bad))
        return self.results


def usage(why):
    print(f"api-checks.py: {why}", file=sys.stderr)
    sys.exit(2)


def parse(argv):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--url", required=True, help="the node's API, https://ADDRESS:443")
    p.add_argument("--pin", required=True, help="the SHA-256 the console's `moruna: tls` line printed")
    p.add_argument("--token-key", required=True, help="the run's token key, whose public half is in node config")
    p.add_argument("--instance", required=True, help="the node's instance ID, its tokens' `aud`")
    p.add_argument("--cloud", required=True, choices=("azure", "openstack"))
    p.add_argument("--openssl", default=default_openssl())
    p.add_argument("--results", default=None, help="where the checks are written, as JSON")
    p.add_argument("--timeout", type=float, default=30.0, help="each request's bound, in seconds")
    a = p.parse_args(argv)
    parts = urllib.parse.urlsplit(a.url)
    if parts.scheme != "https" or not parts.hostname or parts.path not in ("", "/") or parts.query:
        usage(f"--url {a.url!r} is not https://ADDRESS[:PORT]")
    if not re.fullmatch(r"[0-9a-fA-F]{64}", a.pin):
        usage(f"--pin {a.pin!r} is not a SHA-256 in hex")
    if not a.instance or any(c.isspace() for c in a.instance):
        usage(f"--instance {a.instance!r} is not an instance ID")
    if not os.path.isfile(a.token_key):
        usage(f"no token key at {a.token_key}")
    if not 0 < a.timeout <= 300:
        usage("--timeout is more than 0 and at most 300 seconds")
    return a


def main(argv=None, transport=None, keys=None, say=print, clock=time.time):
    a = parse(argv)
    keys = keys or OpenSSL(a.openssl)
    now = clock()
    with tempfile.TemporaryDirectory() as scratch:
        other = os.path.join(scratch, "other-key.pem")
        try:
            keys.keygen(other)
            good = token(keys.sign, a.token_key, a.instance, "observe", now)
            other_key = token(keys.sign, other, a.instance, "observe", now)
            other_aud = token(keys.sign, a.token_key, str(uuid.uuid4()), "observe", now)
        except (OSError, subprocess.SubprocessError) as e:
            detail = getattr(e, "stderr", b"") or b""
            say(f"api-checks: FAIL: the tokens could not be made with {a.openssl}: {e} {shown(detail)}".rstrip())
            return 1
    checks = Checks(transport or Pinned(a.url, a.pin, a.timeout), a.cloud, a.instance, good, other_key, other_aud,
                    say)
    results = checks.all()
    failed = sum(not r["ok"] for r in results)
    if a.results:
        record = {"url": a.url, "pin": a.pin.lower(), "instance": a.instance, "cloud": a.cloud,
                  "time": int(now), "ok": failed == 0, "checks": results}
        with open(a.results, "w") as f:
            json.dump(record, f, indent=1)
            f.write("\n")
    say(f"api-checks: all {len(results)} checks pass" if not failed else
        f"api-checks: {failed} of {len(results)} checks FAIL")
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
