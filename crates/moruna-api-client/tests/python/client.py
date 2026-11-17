"""The Python client, ``moruna.api``, against the test node (item 2.5, "Success").

Run by ``tests/clients.rs``, which starts the node and passes it in the environment:

- ``MORUNA_TEST_PYTHON_ROOT``: the repository's ``python`` directory;
- ``MORUNA_TEST_URL``, ``MORUNA_TEST_PIN``, ``MORUNA_TEST_WRONG_PIN``: the node, its
  certificate's fingerprint, and one that is not;
- ``MORUNA_TEST_OBSERVE``, ``MORUNA_TEST_ADMIN``: a token of each scope;
- ``MORUNA_TEST_DOCUMENT``: the node document it serves, as JSON;
- ``MORUNA_TEST_KEY_ID``: the ID its audit gives the token key.

``moruna.api`` is the standard library only, so it is imported from the source tree without the
package's ``__init__``, which needs the compiled engine.
"""

from __future__ import annotations

import json
import os
import sys
import types
import unittest
from pathlib import Path

_package = types.ModuleType("moruna")
_package.__path__ = [str(Path(os.environ["MORUNA_TEST_PYTHON_ROOT"]) / "moruna")]
sys.modules["moruna"] = _package

from moruna import api  # noqa: E402, the package above first
from moruna.api import _answer, _NoRedirect  # noqa: E402

URL = os.environ["MORUNA_TEST_URL"]
PIN = os.environ["MORUNA_TEST_PIN"]
WRONG_PIN = os.environ["MORUNA_TEST_WRONG_PIN"]
OBSERVE = os.environ["MORUNA_TEST_OBSERVE"]
ADMIN = os.environ["MORUNA_TEST_ADMIN"]
DOCUMENT = json.loads(os.environ["MORUNA_TEST_DOCUMENT"])
KEY_ID = os.environ["MORUNA_TEST_KEY_ID"]


class AgainstTheNode(unittest.TestCase):
    """Calls over TLS to the test node."""

    def test_reads_node_with_an_observe_token(self) -> None:
        node = api.node(api.Client(URL, OBSERVE, pin=PIN))
        self.assertEqual(node, api.Node.from_json(DOCUMENT))
        self.assertEqual(node.to_json(), DOCUMENT)
        self.assertIs(node.cloud, api.Cloud.OPENSTACK)
        self.assertEqual(node.executors, [api.Executor(api.ExecutorKind.CPU, 4)])

    def test_reads_audit_with_an_admin_token(self) -> None:
        api.node(api.Client(URL, OBSERVE, pin=PIN))
        audit = api.node_audit(api.Client(URL, ADMIN, pin=PIN.upper()))
        newest = audit.records[0]
        self.assertEqual(newest.operation, "node")
        self.assertEqual(newest.key, KEY_ID)
        self.assertEqual(newest.scope, [api.Scope.OBSERVE])
        self.assertEqual(newest.status, 200)
        self.assertEqual(api.Audit.from_json(audit.to_json()), audit)

    def test_is_refused_with_the_wrong_pin_and_sends_nothing(self) -> None:
        admin = api.Client(URL, ADMIN, pin=PIN)
        before = len(api.node_audit(admin).records)
        with self.assertRaises(api.PinMismatch) as caught:
            api.node(api.Client(URL, OBSERVE, pin=WRONG_PIN))
        self.assertEqual(caught.exception.found, PIN)
        self.assertEqual(caught.exception.pin, WRONG_PIN)
        self.assertIn(f"has SHA-256 {PIN}, not the pinned {WRONG_PIN}", str(caught.exception))
        after = api.node_audit(admin).records
        # The one new record is the first audit read: the refused call never reached the node.
        self.assertEqual(len(after), before + 1)
        self.assertEqual(after[0].operation, "node_audit")

    def test_health_needs_no_token(self) -> None:
        health = api.health(api.Client(URL, "not a token", pin=PIN))
        self.assertEqual(health, api.Health(api.HealthStatus.OK))

    def test_an_observe_token_is_forbidden_the_audit(self) -> None:
        with self.assertRaises(api.ApiError) as caught:
            api.node_audit(api.Client(URL, OBSERVE, pin=PIN))
        error = caught.exception
        self.assertEqual((error.status, error.code), (403, api.ErrorCode.FORBIDDEN))
        self.assertEqual(error.operation, "node_audit")
        self.assertTrue(str(error).startswith("the node refused node_audit: forbidden (403): "))

    def test_a_bad_token_is_unauthorized(self) -> None:
        with self.assertRaises(api.ApiError) as caught:
            api.node(api.Client(URL, "not a token", pin=PIN))
        self.assertEqual((caught.exception.status, caught.exception.code), (401, "unauthorized"))

    def test_an_unknown_path_is_not_found(self) -> None:
        with self.assertRaises(api.ApiError) as caught:
            api.Client(URL, OBSERVE, pin=PIN).call("missing", "GET", "/v1/missing", token=True)
        self.assertEqual(caught.exception.code, api.ErrorCode.NOT_FOUND)

    def test_without_a_pin_the_certificate_is_verified(self) -> None:
        with self.assertRaises(api.Error) as caught:
            api.node(api.Client(URL, OBSERVE))
        self.assertNotIsInstance(caught.exception, api.ApiError)
        self.assertIn("CERTIFICATE_VERIFY_FAILED", str(caught.exception))

    def test_an_unreachable_node_is_an_error(self) -> None:
        with self.assertRaises(api.Error) as caught:
            api.health(api.Client("https://127.0.0.1:1", OBSERVE, pin=PIN))
        self.assertTrue(str(caught.exception).startswith("could not call health at "))


class WithoutTheNode(unittest.TestCase):
    """The client's arguments, its answers and the generated types, read without a call."""

    def test_the_url_must_be_https_with_a_host(self) -> None:
        for url in ("http://127.0.0.1", "https://", "127.0.0.1:443", "https://node:port"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                api.Client(url, OBSERVE)

    def test_the_url_keeps_its_host_and_port_only(self) -> None:
        cases = {
            "https://user:secret@node.example:8443/x?q#f": "https://node.example:8443",
            "https://[::1]/": "https://[::1]",
        }
        for url, kept in cases.items():
            with self.subTest(url=url):
                self.assertEqual(api.Client(url, OBSERVE).url, kept)

    def test_the_pin_must_be_a_sha256_in_hex(self) -> None:
        for pin in ("", PIN[:-1], PIN[:-1] + "g", PIN + "0"):
            with self.subTest(pin=pin), self.assertRaises(ValueError):
                api.Client(URL, OBSERVE, pin=pin)

    def test_the_token_is_never_shown(self) -> None:
        client = api.Client(URL + "/ignored?query", OBSERVE, pin=PIN)
        self.assertEqual(client.url, URL)
        self.assertNotIn(OBSERVE, repr(client))

    def test_a_success_that_is_not_json_is_an_error(self) -> None:
        with self.assertRaises(api.Error) as caught:
            _answer("node", 200, b"<html>")
        self.assertIn("answer to node is not JSON", str(caught.exception))

    def test_an_error_without_an_error_body(self) -> None:
        with self.assertRaises(api.ApiError) as caught:
            _answer("node", 502, b"Bad Gateway")
        self.assertIsNone(caught.exception.code)
        self.assertEqual(
            str(caught.exception),
            "the node refused node: status 502: the answer carries no error body",
        )

    def test_no_redirect_is_followed(self) -> None:
        self.assertIsNone(_NoRedirect().redirect_request())

    def test_an_answer_is_checked_field_by_field(self) -> None:
        cases = {
            "Node: expected an object, found list": [],
            "Node: missing field 'cores'": {k: v for k, v in DOCUMENT.items() if k != "cores"},
            "Node.cores: expected an integer, found str": {**DOCUMENT, "cores": "4"},
            "Node.cores: expected an integer, found bool": {**DOCUMENT, "cores": True},
            "Node.cores: -1 is out of range": {**DOCUMENT, "cores": -1},
            "Node.version: expected a string, found int": {**DOCUMENT, "version": 7},
            "Node.cloud: 'gcp' is not a Cloud": {**DOCUMENT, "cloud": "gcp"},
            "Node.executors: expected a list, found dict": {**DOCUMENT, "executors": {}},
            "Node.executors[0]: missing field 'slots'": {
                **DOCUMENT,
                "executors": [{"kind": "cpu"}],
            },
        }
        for message, value in cases.items():
            with self.subTest(message=message), self.assertRaises(ValueError) as caught:
                api.Node.from_json(value)
            self.assertEqual(str(caught.exception), message)
        with self.assertRaises(ValueError) as caught:
            api.AuditRecord.from_json(
                {"time": 1, "key": "", "scope": [], "operation": "x", "status": 70000}
            )
        self.assertEqual(str(caught.exception), "AuditRecord.status: 70000 is out of range")

    def test_an_unknown_field_of_an_answer_is_ignored(self) -> None:
        node = api.Node.from_json({**DOCUMENT, "later": True})
        self.assertEqual(node.to_json(), DOCUMENT)

    def test_an_error_body_reads_both_ways(self) -> None:
        body = {"error": {"code": "busy", "message": "later"}}
        self.assertEqual(api.ErrorBody.from_json(body).to_json(), body)


if __name__ == "__main__":
    unittest.main(verbosity=2)
