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
        client = api.Client(URL, OBSERVE, pin=PIN)
        node = client.node()
        self.assertEqual(node, api.Node.from_json(DOCUMENT))
        self.assertEqual(node.to_json(), DOCUMENT)
        self.assertIs(node.cloud, api.Cloud.OPENSTACK)
        self.assertEqual(node.executors, [api.Executor(api.ExecutorKind.CPU, 4)])
        # The method is the function.
        self.assertEqual(api.node(client), node)

    def test_reads_audit_with_an_admin_token(self) -> None:
        api.Client(URL, OBSERVE, pin=PIN).node()
        admin = api.Client(URL, ADMIN, pin=PIN.upper())
        audit = admin.node_audit()
        newest = audit.records[0]
        self.assertEqual(newest.operation, "node")
        self.assertEqual(newest.key, KEY_ID)
        self.assertEqual(newest.scope, [api.Scope.OBSERVE])
        self.assertEqual(newest.status, 200)
        self.assertEqual(api.Audit.from_json(audit.to_json()), audit)
        # The method is the function: the function's read shows the method's call, newest.
        self.assertEqual(api.node_audit(admin).records[0].operation, "node_audit")

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

    def test_runs_a_job_through_to_its_report(self) -> None:
        admin = api.Client(URL, ADMIN, pin=PIN)
        observe = api.Client(URL, OBSERVE, pin=PIN)
        job = api.Job(v1={"moruna_spec": 1, "source": {"kind": "rows"}})
        self.assertEqual(admin.jobs_validate(job), api.Validation(ok=True))
        refused = admin.jobs_validate(api.Job(v1={"refuse": "source.kind: no such kind"}))
        self.assertFalse(refused.ok)
        self.assertEqual(refused.refusals, [api.Refusal("spec", "source.kind: no such kind")])
        with self.assertRaises(api.ApiError) as caught:
            admin.jobs_submit(api.Job(v1={"refuse": "source.kind: no such kind"}))
        self.assertEqual(caught.exception.code, api.ErrorCode.INVALID)
        self.assertEqual(caught.exception.message, "source.kind: no such kind")
        with self.assertRaises(api.ApiError) as caught:
            observe.jobs_submit(job)
        self.assertEqual(caught.exception.code, api.ErrorCode.FORBIDDEN)
        run = admin.jobs_submit(job).id
        with self.assertRaises(api.ApiError) as caught:
            api.jobs_submit(admin, api.Job(v2="TUpCMg=="))
        self.assertEqual((caught.exception.status, caught.exception.code), (503, "busy"))
        # Reading needs only observe.
        status = observe.jobs_get(run)
        self.assertIs(status.state, api.JobState.RUNNING)
        self.assertIsNotNone(status.heartbeat)
        assert status.heartbeat is not None
        self.assertEqual((status.heartbeat.rows_out, status.heartbeat.bottleneck), (3, "sink"))
        self.assertIsNone(status.exit_code)
        # The times to the millisecond beside their seconds (item 2.4).
        assert status.started_ms is not None
        self.assertLessEqual(status.submitted_ms, status.started_ms)
        self.assertEqual((status.submitted, status.started),
                         (status.submitted_ms // 1000, status.started_ms // 1000))
        self.assertIsNone(status.ended_ms)
        self.assertEqual(api.JobStatus.from_json(status.to_json()), status)
        self.assertEqual(api.jobs_list(observe).jobs[0].id, run)
        with self.assertRaises(api.ApiError) as caught:
            observe.jobs_report(run)
        self.assertEqual(caught.exception.code, api.ErrorCode.CONFLICT)
        running = list(observe.jobs_events(run))
        self.assertEqual([e.event for e in running], ["event", "event", "heartbeat"])
        self.assertEqual((running[0].id, running[2].id), (1, None))
        self.assertEqual(running[1].data["kind"], "task_started")
        # Cancel answers the job as it stood; then it has ended, with exit code 130.
        self.assertIs(admin.jobs_cancel(run).state, api.JobState.RUNNING)
        ended = observe.jobs_get(run)
        self.assertEqual((ended.state, ended.exit_code), (api.JobState.CANCELLED, 130))
        assert ended.ended_ms is not None
        self.assertEqual(ended.ended, ended.ended_ms // 1000)
        with self.assertRaises(api.ApiError) as caught:
            api.jobs_cancel(admin, run)
        self.assertEqual(caught.exception.code, api.ErrorCode.CONFLICT)
        replay = list(api.jobs_events(observe, run, from_=1))
        self.assertEqual([e.event for e in replay], ["event", "heartbeat", "report", "exit"])
        self.assertEqual((replay[0].id, replay[3].data["code"]), (2, 130))
        self.assertEqual(observe.jobs_report(run), {"rows_out": 3})
        for call in (observe.jobs_get, observe.jobs_report, observe.jobs_events):
            with self.assertRaises(api.ApiError) as caught:
                call("no such run")
            self.assertEqual(caught.exception.code, api.ErrorCode.NOT_FOUND)

    def test_health_needs_no_token(self) -> None:
        client = api.Client(URL, "not a token", pin=PIN)
        self.assertEqual(client.health(), api.Health(api.HealthStatus.OK))
        self.assertEqual(api.health(client), api.Health(api.HealthStatus.OK))

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

    def test_the_client_has_a_method_per_operation(self) -> None:
        self.assertTrue(issubclass(api.Client, api.Operations))
        for name in ("health", "node", "node_audit"):
            self.assertIs(getattr(api.Client, name), getattr(api.Operations, name))

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
