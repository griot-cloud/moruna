"""Driving the Moruna node through its API (system design 2C, 3B) for the benchmark.

- **Tokens:** an EdDSA JWT (`{"alg":"EdDSA","typ":"JWT"}`, claims `aud`, `exp`, `scope`), signed
  with the run's own Ed25519 key, whose public half is node config's one `token_keys` entry.
  OpenSSL 3 signs it (`openssl pkeyutl -sign -rawin`); macOS's LibreSSL cannot, so the harness
  names Homebrew's (`OPENSSL`).
- **Pinning:** the node's certificate is accepted when its DER's SHA-256 is the fingerprint the
  console printed (`moruna: tls sha256=<hex>`), as `moruna.api.Client` does.
- **A job:** `POST /v1/jobs` with `{"v1": <job document>}`; its events, its report and its exit
  over Server-Sent Events (`GET /v1/jobs/{id}/events`, `Https.stream`); proof.py makes the
  document and follows the stream, for itself and for run.py.

The first rows at the sink (`first_rows_s`: the sink's task starting, its event's `t_us`, else
the first heartbeat with rows out, as bench/vm/report.py's `first_rows` reads a 0.7 run) are kept
among a run's marks; operational time is the report's `wall_ms` (run.py).
"""
import base64
import hashlib
import http.client
import json
import os
import ssl
import subprocess
import tempfile
import time
import urllib.parse

OPENSSL = os.environ.get("MORUNA_BENCH_OPENSSL", "/opt/homebrew/bin/openssl")
HEADER = {"alg": "EdDSA", "typ": "JWT"}
ENDED = ("completed", "failed", "cancelled")


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def keygen(path: str, openssl: str = OPENSSL) -> str:
    """A new Ed25519 key at `path`; its public half as node config's `token_keys` holds it."""
    subprocess.run([openssl, "genpkey", "-algorithm", "ed25519", "-out", path], check=True, capture_output=True)
    os.chmod(path, 0o600)
    der = subprocess.run([openssl, "pkey", "-in", path, "-pubout", "-outform", "DER"], check=True,
                         capture_output=True).stdout
    return base64.b64encode(der[-32:]).decode()


def token(key_path: str, aud: str, scope, ttl_s: int = 3000, now=None, openssl: str = OPENSSL) -> str:
    """A token for node `aud`: `exp` at most one hour ahead (2C), here `ttl_s`."""
    claims = {"aud": aud, "exp": int((now or time.time()) + ttl_s), "scope": list(scope)}
    signed = f"{b64url(json.dumps(HEADER, separators=(',', ':')).encode())}." \
             f"{b64url(json.dumps(claims, separators=(',', ':')).encode())}"
    with tempfile.NamedTemporaryFile() as f:
        f.write(signed.encode())
        f.flush()
        sig = subprocess.run([openssl, "pkeyutl", "-sign", "-inkey", key_path, "-rawin", "-in", f.name],
                             check=True, capture_output=True).stdout
    return f"{signed}.{b64url(sig)}"


class Tokens:
    """A run's tokens by scope, made again before they expire: a token lives at most an hour
    (2C) and a run lasts hours."""

    def __init__(self, key_path, aud, ttl_s=3000, refresh_s=2400, mint=None, clock=time.time):
        self.key, self.aud, self.ttl, self.refresh, self.clock = key_path, aud, ttl_s, refresh_s, clock
        self.mint = mint or (lambda scope: token(key_path, aud, [scope], ttl_s))
        self.made = {}

    def get(self, scope):
        now = self.clock()
        tok, at = self.made.get(scope, (None, None))
        if tok is None or now - at > self.refresh:
            tok = self.mint(scope)
            self.made[scope] = (tok, now)
        return tok


def sign_digest(key_path: str, sha256_hex: str, openssl: str = OPENSSL) -> str:
    """A function image's signature (3E): Ed25519 over the 32 bytes of its SHA-256, in base64."""
    with tempfile.NamedTemporaryFile() as f:
        f.write(bytes.fromhex(sha256_hex))
        f.flush()
        sig = subprocess.run([openssl, "pkeyutl", "-sign", "-inkey", key_path, "-rawin", "-in", f.name],
                             check=True, capture_output=True).stdout
    return base64.b64encode(sig).decode()


def node_config(token_key: str, function_keys=()) -> dict:
    """Node config version 1 (1D): the run's token key; no `tls`, so the node makes its own."""
    cfg = {"moruna": 1, "api": {"token_keys": [token_key]}}
    if function_keys:
        cfg["functions"] = {"keys": list(function_keys)}
    return cfg


def console(text: str) -> dict:
    """What the harness needs from the node's console (1B): its boot lines' values."""
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("moruna: "):
            continue
        words = line[len("moruna: "):].split()
        if not words:
            continue
        if words[0] == "tls" and len(words) > 1 and words[1].startswith("sha256="):
            out["pin"] = words[1][len("sha256="):]
        elif words[0] == "ready":
            for w in words[1:]:
                k, _, v = w.partition("=")
                if k == "api":
                    out["api"] = v
                elif k == "boot_ms":
                    out["boot_ms"] = int(v)
        elif words[0] == "cloud" and len(words) >= 4:
            out["cloud"], out["instance"] = words[1], words[3]
        elif words[0] == "fatal":
            out["fatal"] = " ".join(words[1:])
    return out


class Https:
    """The real transport: HTTPS to the node, its certificate pinned."""

    def __init__(self, url: str, pin: str, timeout: float = 60.0):
        parts = urllib.parse.urlsplit(url)
        self.host, self.port = parts.hostname, parts.port or 443
        self.pin, self.timeout = pin.lower(), timeout

    def _connect(self):
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        conn = http.client.HTTPSConnection(self.host, self.port, context=ctx, timeout=self.timeout)
        try:
            conn.connect()
            der = conn.sock.getpeercert(binary_form=True)
            if hashlib.sha256(der).hexdigest() != self.pin:
                raise ConnectionError(f"the node's certificate is not the pinned one ({self.pin})")
        except BaseException:
            conn.close()
            raise
        return conn

    @staticmethod
    def _request(conn, method, path, body, token):
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        conn.request(method, path, body=data, headers=headers)
        return conn.getresponse()

    def __call__(self, method, path, body=None, token=None):
        conn = self._connect()
        try:
            r = self._request(conn, method, path, body, token)
            return r.status, r.read()
        finally:
            conn.close()

    def stream(self, method, path, token=None):
        """A streamed answer (Server-Sent Events) line by line, as it comes. A status other than
        200 is an `ApiError`; a connection that ends early raises what ended it (`OSError`,
        `http.client.HTTPException`, a read that timed out), for the caller to resume from."""
        conn = self._connect()
        try:
            r = self._request(conn, method, path, None, token)
            if r.status != 200:
                raise ApiError(f"{method} {path}: {r.status} {r.read()[:300]!r}")
            while True:
                line = r.readline()
                if not line:
                    return
                yield line.decode(errors="replace").rstrip("\r\n")
        finally:
            conn.close()


class ApiError(RuntimeError):
    pass


class Node:
    """The calls the benchmark makes, over a transport `(method, path, body, token) -> (status, bytes)`."""

    def __init__(self, transport, tokens):
        self.t, self.tokens = transport, tokens   # tokens: scope -> token

    def call(self, method, path, body=None, scope="observe"):
        status, data = self.t(method, path, body, self.tokens.get(scope) if scope else None)
        if not 200 <= status < 300:
            raise ApiError(f"{method} {path}: {status} {data[:300]!r}")
        return data

    def json(self, method, path, body=None, scope="observe"):
        return json.loads(self.call(method, path, body, scope))

    def node(self):
        return self.json("GET", "/v1/node")

    def register(self, name, address, sha256, signature):
        return self.json("POST", "/v1/functions", {"name": name, "address": address, "sha256": sha256,
                                                   "signature": signature}, "admin")

    def submit(self, document):
        return self.json("POST", "/v1/jobs", {"v1": document}, "submit")["id"]

    def job(self, jid):
        return self.json("GET", f"/v1/jobs/{jid}")

    def wait(self, jid, timeout_s, poll_s=2.0, sleep=time.sleep, clock=time.time):
        end = clock() + timeout_s
        while True:
            j = self.job(jid)
            if j.get("state") in ENDED:
                return j
            if clock() > end:
                self.call("POST", f"/v1/jobs/{jid}/cancel", None, "submit")
                return dict(j, state="timed out")
            sleep(poll_s)

    def events(self, jid):
        return sse(self.call("GET", f"/v1/jobs/{jid}/events?from=0").decode(errors="replace"))

    def report(self, jid):
        return self.json("GET", f"/v1/jobs/{jid}/report")


def sse(text: str):
    """The records of a Server-Sent Events stream: each event's `data` lines joined, as JSON."""
    out, data = [], []
    for line in text.splitlines() + [""]:
        if line.startswith("data:"):
            data.append(line[5:].lstrip())
        elif line == "" and data:
            try:
                out.append(json.loads("\n".join(data)))
            except ValueError:
                pass
            data = []
    return out


def first_rows_s(events):
    """When the first rows reached the sink, in seconds from the engine's start (report.py's
    rule: the sink's task starting, else the first heartbeat with rows out)."""
    beat = None
    for e in events:
        if e.get("type") == "event" and e.get("kind") == "task_started" and e.get("task") == "sink":
            return e["t_us"] / 1e6
        if beat is None and e.get("type") == "heartbeat" and (e.get("rows_out") or 0) > 0:
            beat = e["t_ms"] / 1e3
    return beat
