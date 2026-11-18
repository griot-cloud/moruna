#!/usr/bin/env python3
"""api-checks.py (work item 2.6), run as azure-up.sh and openstack-up.sh run it, against a fake
node over real TLS on the loopback.

The fake answers as the node's API does (2A to 2D): health without a token; `/v1/node` and
`/mcp` only with a token whose Ed25519 signature verifies under the run's key (OpenSSL checks
it), whose `aud` is the node's instance and whose `exp` is at most an hour ahead, else `401
unauthorized`; MCP's `initialize`, `notifications/initialized`, `tools/list` and `tools/call`.
Each way the fake can be wrong (a token from another key taken, a wrong `aud` taken, another
cloud, a tool missing, a tool error) fails its check and the run. A certificate that is not the
pinned one fails every check, with no request sent at all.

Run: python3 -I kernel/node/tools/test_api_checks.py
"""
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOL = Path(__file__).resolve().parent / "api-checks.py"
INSTANCE = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"
NAMES = ["moruna_health", "moruna_node", "moruna_node_audit", "moruna_jobs_submit"]


def find_openssl():
    """An OpenSSL that makes Ed25519 keys (OpenSSL 3), or None."""
    for candidate in (os.environ.get("MORUNA_OPENSSL"), "/opt/homebrew/bin/openssl", shutil.which("openssl")):
        if not candidate or not os.access(candidate, os.X_OK):
            continue
        with tempfile.TemporaryDirectory() as d:
            done = subprocess.run([candidate, "genpkey", "-algorithm", "ed25519", "-out", f"{d}/k.pem"],
                                  capture_output=True, check=False)
        if done.returncode == 0:
            return candidate
    return None


OPENSSL = find_openssl()


def load_tool():
    spec = importlib.util.spec_from_file_location("api_checks", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def unb64(part):
    return base64.urlsafe_b64decode(part + "=" * (-len(part) % 4))


class Quiet(ThreadingHTTPServer):
    """A server that says nothing of a client gone mid-handshake (the pin's refusal)."""

    def handle_error(self, request, client_address):
        pass


class FakeNode:
    """The node's API on the loopback, over TLS with a certificate of its own."""

    def __init__(self, root: Path, public_key: Path, **wrong):
        self.root, self.public, self.wrong = root, public_key, wrong
        self.requests = []
        key, cert = root / "tls-key.pem", root / "tls-cert.pem"
        subprocess.run([OPENSSL, "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
                        "-keyout", str(key), "-out", str(cert), "-days", "1", "-subj", f"/CN={INSTANCE}"],
                       check=True, capture_output=True)
        self.pin = hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.read_text())).hexdigest()
        fake = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def answer(self, status, body):
                data = b"" if body is None else json.dumps(body).encode()
                self.send_response(status)
                if data:
                    self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(data)

            def refuse(self, why):
                self.answer(401, {"error": {"code": "unauthorized", "message": why}})

            def body(self):
                return json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"null")

            def do_GET(self):
                fake.requests.append(("GET", self.path, self.headers.get("Authorization")))
                if self.path == "/v1/health":
                    return self.answer(200, {"status": "ok"})
                if self.path != "/v1/node":
                    return self.answer(404, {"error": {"code": "not_found", "message": "no such path"}})
                why = fake.verify(self.headers.get("Authorization"))
                if why:
                    return self.refuse(why)
                self.answer(200, fake.document())

            def do_POST(self):
                fake.requests.append(("POST", self.path, self.headers.get("Authorization")))
                if self.path != "/mcp":
                    return self.answer(404, {"error": {"code": "not_found", "message": "no such path"}})
                why = fake.verify(self.headers.get("Authorization"))
                if why:
                    return self.refuse(why)
                message = self.body()
                if "id" not in message:
                    return self.answer(fake.wrong.get("initialized", 202), None)
                method, n = message["method"], message["id"]
                if method == "initialize":
                    result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                              "serverInfo": {"name": "moruna", "version": "0.7.0"}}
                elif method == "tools/list":
                    result = {"tools": [{"name": name, "inputSchema": {"type": "object"}}
                                        for name in fake.wrong.get("tools", NAMES)]}
                elif method == "tools/call" and message["params"]["name"] == "moruna_node":
                    if fake.wrong.get("tool_error"):
                        error = {"error": {"code": "forbidden", "message": "no"}}
                        result = {"isError": True, "structuredContent": error,
                                  "content": [{"type": "text", "text": json.dumps(error)}]}
                    else:
                        doc = fake.document()
                        text = dict(doc, cores=1) if fake.wrong.get("text_differs") else doc
                        result = {"structuredContent": doc, "content": [{"type": "text", "text": json.dumps(text)}]}
                else:
                    return self.answer(200, {"jsonrpc": "2.0", "id": n,
                                             "error": {"code": -32601, "message": "Method not found"}})
                self.answer(200, {"jsonrpc": "2.0", "id": n, "result": result})

            def log_message(self, *args):
                pass

        self.server = Quiet(("127.0.0.1", 0), Handler)
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(str(cert), str(key))
        self.server.socket = ctx.wrap_socket(self.server.socket, server_side=True)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.server.shutdown()
        self.server.server_close()

    def document(self):
        return {"version": "0.7.0", "instance": INSTANCE, "cloud": self.wrong.get("cloud", "openstack"),
                "cores": 4, "memory_bytes": 17179869184, "executors": [{"kind": "cpu", "slots": 4}],
                "boot_ms": 1982, "time": int(time.time()), "health": "ok"}

    def verify(self, authorization):
        """Why the token is refused (2C), or None."""
        if not authorization or not authorization.startswith("Bearer "):
            return "no token"
        try:
            header, claims, signature = authorization[len("Bearer "):].split(".")
            claims_json = json.loads(unb64(claims))
            assert json.loads(unb64(header)) == {"alg": "EdDSA", "typ": "JWT"}
        except (ValueError, AssertionError):
            return "not a token"
        if not self.wrong.get("any_key"):
            with tempfile.TemporaryDirectory() as d:
                Path(d, "signed").write_text(f"{header}.{claims}")
                Path(d, "signature").write_bytes(unb64(signature))
                done = subprocess.run([OPENSSL, "pkeyutl", "-verify", "-pubin", "-inkey", str(self.public), "-rawin",
                                       "-in", f"{d}/signed", "-sigfile", f"{d}/signature"], capture_output=True,
                                      check=False)
            if done.returncode != 0:
                return "the signature does not verify under any token key"
        if not self.wrong.get("any_aud") and claims_json.get("aud") != INSTANCE:
            return "the token is for another node"
        if not time.time() < claims_json.get("exp", 0) <= time.time() + 3600:
            return "the token has expired or lives too long"
        if "observe" not in claims_json.get("scope", []):
            return "the scope does not cover observe"
        return None


@unittest.skipIf(OPENSSL is None, "needs OpenSSL 3, which makes Ed25519 keys")
class ApiChecks(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.key = self.root / "token-key.pem"
        subprocess.run([OPENSSL, "genpkey", "-algorithm", "ed25519", "-out", str(self.key)], check=True,
                       capture_output=True)
        self.public = self.root / "token-key.pub.pem"
        subprocess.run([OPENSSL, "pkey", "-in", str(self.key), "-pubout", "-out", str(self.public)], check=True,
                       capture_output=True)
        self.results = self.root / "api-checks.json"

    def run_tool(self, node, *more, pin=None, cloud="openstack", url=None):
        argv = [sys.executable, "-I", str(TOOL), "--url", url or f"https://127.0.0.1:{node.port}",
                "--pin", pin or node.pin, "--token-key", str(self.key), "--instance", INSTANCE,
                "--cloud", cloud, "--openssl", OPENSSL, "--results", str(self.results), *more]
        return subprocess.run(argv, capture_output=True, text=True, timeout=120, check=False)

    def checks(self):
        return {c["check"]: c["ok"] for c in json.loads(self.results.read_text())["checks"]}

    def test_a_node_that_answers_as_it_should_passes_every_check(self):
        with FakeNode(self.root, self.public) as node:
            done = self.run_tool(node)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(done.stdout.splitlines()[-1], "api-checks: all 9 checks pass")
        record = json.loads(self.results.read_text())
        self.assertTrue(record["ok"])
        self.assertEqual((record["instance"], record["cloud"], record["pin"]), (INSTANCE, "openstack", node.pin))
        self.assertEqual([c["check"] for c in record["checks"]], [
            "GET /v1/health, no token", "GET /v1/node over HTTPS, an observe token", "MCP initialize",
            "MCP tools/list", "MCP tools/call moruna_node",
            "GET /v1/node refuses a token signed by another key", "POST /mcp refuses a token signed by another key",
            "GET /v1/node refuses a token whose aud is another instance",
            "POST /mcp refuses a token whose aud is another instance"])
        self.assertTrue(all(c["ok"] for c in record["checks"]))
        self.assertIn("api-checks: pass: GET /v1/node over HTTPS, an observe token: 200, cloud openstack, "
                      f"instance {INSTANCE}, version 0.7.0, health ok", done.stdout)
        self.assertIn("api-checks: pass: MCP initialize: server moruna 0.7.0, protocol 2025-06-18; "
                      "notifications/initialized 202", done.stdout)
        self.assertIn("api-checks: pass: POST /mcp refuses a token signed by another key: 401 unauthorized: "
                      "the signature does not verify under any token key", done.stdout)
        self.assertIn("api-checks: pass: GET /v1/node refuses a token whose aud is another instance: 401 "
                      "unauthorized: the token is for another node", done.stdout)
        # The requests: health without a token, then every other with one; three distinct tokens.
        self.assertEqual([(m, p) for m, p, _ in node.requests], [
            ("GET", "/v1/health"), ("GET", "/v1/node"), ("POST", "/mcp"), ("POST", "/mcp"), ("POST", "/mcp"),
            ("POST", "/mcp"), ("GET", "/v1/node"), ("POST", "/mcp"), ("GET", "/v1/node"), ("POST", "/mcp")])
        self.assertIsNone(node.requests[0][2])
        tokens = [auth for _, _, auth in node.requests[1:]]
        self.assertEqual(len(set(tokens)), 3)
        # A token is an observe token of 10 minutes; neither it nor the others are shown or kept.
        claims = json.loads(unb64(tokens[0].split(".")[1]))
        self.assertEqual((claims["aud"], claims["scope"]), (INSTANCE, ["observe"]))
        self.assertTrue(time.time() + 500 < claims["exp"] <= time.time() + 600)
        self.assertNotEqual(json.loads(unb64(tokens[-1].split(".")[1]))["aud"], INSTANCE)
        for token in set(tokens):
            for said in (done.stdout, done.stderr, self.results.read_text()):
                self.assertNotIn(token.split(" ")[1].split(".")[2], said)

    def test_a_node_that_takes_a_token_from_another_key_fails(self):
        with FakeNode(self.root, self.public, any_key=True) as node:
            done = self.run_tool(node)
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertEqual(done.stdout.splitlines()[-1], "api-checks: 2 of 9 checks FAIL")
        failed = [name for name, ok in self.checks().items() if not ok]
        self.assertEqual(failed, ["GET /v1/node refuses a token signed by another key",
                                  "POST /mcp refuses a token signed by another key"])
        self.assertIn("api-checks: FAIL: GET /v1/node refuses a token signed by another key: 200 ", done.stdout)

    def test_a_node_that_takes_a_token_for_another_instance_fails(self):
        with FakeNode(self.root, self.public, any_aud=True) as node:
            done = self.run_tool(node)
        self.assertEqual(done.returncode, 1, done.stdout)
        failed = [name for name, ok in self.checks().items() if not ok]
        self.assertEqual(failed, ["GET /v1/node refuses a token whose aud is another instance",
                                  "POST /mcp refuses a token whose aud is another instance"])

    def test_a_node_on_another_cloud_fails_its_node_checks(self):
        with FakeNode(self.root, self.public, cloud="azure") as node:
            done = self.run_tool(node)
        self.assertEqual(done.returncode, 1, done.stdout)
        failed = [name for name, ok in self.checks().items() if not ok]
        self.assertEqual(failed, ["GET /v1/node over HTTPS, an observe token", "MCP tools/call moruna_node"])
        with FakeNode(self.root, self.public, cloud="azure") as node:
            done = self.run_tool(node, cloud="azure")
        self.assertEqual(done.returncode, 0, done.stdout)

    def test_mcp_that_lacks_a_tool_errs_or_differs_fails(self):
        for wrong, check, said in (
                ({"tools": NAMES[:2]}, "MCP tools/list", "2 tools, missing moruna_node_audit"),
                ({"tool_error": True}, "MCP tools/call moruna_node", "a tool error: "),
                ({"text_differs": True}, "MCP tools/call moruna_node", "its text is not the same JSON"),
                ({"initialized": 200}, "MCP initialize", "notifications/initialized 200")):
            with FakeNode(self.root, self.public, **wrong) as node:
                done = self.run_tool(node)
            self.assertEqual(done.returncode, 1, (wrong, done.stdout))
            self.assertEqual([name for name, ok in self.checks().items() if not ok], [check], wrong)
            self.assertIn(said, done.stdout)

    def test_a_certificate_that_is_not_the_pinned_one_fails_everything_and_nothing_is_sent(self):
        with FakeNode(self.root, self.public) as node:
            done = self.run_tool(node, pin="0" * 64)
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertEqual(done.stdout.splitlines()[-1], "api-checks: 9 of 9 checks FAIL")
        self.assertIn(f"the certificate served is {node.pin}, not the pinned {'0' * 64}: nothing sent", done.stdout)
        self.assertEqual(node.requests, [], "no request, and so no token, reaches a server not pinned")

    def test_a_node_that_does_not_answer_fails_every_check_within_its_bound(self):
        with FakeNode(self.root, self.public) as node:
            port = node.port
        started = time.monotonic()
        done = self.run_tool(node, "--timeout", "5", url=f"https://127.0.0.1:{port}")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertEqual(done.stdout.splitlines()[-1], "api-checks: 9 of 9 checks FAIL")
        self.assertIn("ConnectionRefusedError", done.stdout)
        self.assertLess(time.monotonic() - started, 60)

    def test_usage_errors_are_refused_before_anything_is_sent(self):
        with FakeNode(self.root, self.public) as node:
            for args, said in (
                    (["--url", "http://127.0.0.1:1"], "is not https://ADDRESS"),
                    (["--url", "https://127.0.0.1/v1"], "is not https://ADDRESS"),
                    (["--pin", "abc"], "is not a SHA-256"),
                    (["--instance", "a b"], "is not an instance ID"),
                    (["--token-key", "/nonexistent/key.pem"], "no token key at"),
                    (["--timeout", "0"], "--timeout is more than 0"),
                    (["--cloud", "gcp"], "invalid choice")):
                argv = [sys.executable, "-I", str(TOOL), "--url", f"https://127.0.0.1:{node.port}", "--pin", node.pin,
                        "--token-key", str(self.key), "--instance", INSTANCE, "--cloud", "openstack",
                        "--openssl", OPENSSL]
                for flag, value in zip(args[::2], args[1::2]):
                    if flag in argv:
                        argv[argv.index(flag) + 1] = value
                    else:
                        argv += [flag, value]
                done = subprocess.run(argv, capture_output=True, text=True, timeout=60, check=False)
                self.assertEqual(done.returncode, 2, (args, done.stderr))
                self.assertIn(said, done.stderr)
            self.assertEqual(node.requests, [])

    def test_tokens_that_cannot_be_made_fail_the_run(self):
        with FakeNode(self.root, self.public) as node:
            done = self.run_tool(node, "--openssl", "/usr/bin/false")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("api-checks: FAIL: the tokens could not be made with /usr/bin/false", done.stdout)
        self.assertEqual(node.requests, [])


class Tokens(unittest.TestCase):
    def test_a_token_is_an_eddsa_jwt_with_its_claims_and_signature(self):
        tool = load_tool()
        signed = []

        def sign(key, data):
            signed.append((key, data))
            return b"\x01\x02signature"
        made = tool.token(sign, "/k.pem", "node-1", "observe", 1000.9)
        header, claims, signature = made.split(".")
        self.assertEqual(json.loads(unb64(header)), {"alg": "EdDSA", "typ": "JWT"})
        self.assertEqual(json.loads(unb64(claims)), {"aud": "node-1", "exp": 1600, "scope": ["observe"]})
        self.assertEqual(unb64(signature), b"\x01\x02signature")
        self.assertEqual(signed, [("/k.pem", f"{header}.{claims}".encode())])
        self.assertNotIn("=", made)

    def test_the_default_openssl(self):
        tool = load_tool()
        saved = os.environ.get("MORUNA_OPENSSL")
        try:
            os.environ["MORUNA_OPENSSL"] = "/some/openssl"
            self.assertEqual(tool.default_openssl(), "/some/openssl")
            del os.environ["MORUNA_OPENSSL"]
            self.assertIn(tool.default_openssl(), ("/opt/homebrew/bin/openssl", "openssl"))
        finally:
            if saved is not None:
                os.environ["MORUNA_OPENSSL"] = saved


if __name__ == "__main__":
    unittest.main()
