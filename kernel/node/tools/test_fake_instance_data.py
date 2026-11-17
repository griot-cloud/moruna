#!/usr/bin/env python3
"""fake-instance-data.py (items 1.4 and 1.12): what it answers as each cloud, and as Azure's
WireServer, run as the boots run it.

Run: python3 -I kernel/node/tools/test_fake_instance_data.py
"""

import base64
import email.utils
import http.client
import json
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from xml.etree import ElementTree

TOOL = Path(__file__).resolve().parent / "fake-instance-data.py"
KEY = base64.b64encode(bytes(range(32))).decode()
VALID = json.dumps({"moruna": 1, "api": {"token_keys": [KEY]}}, separators=(",", ":")).encode()
AZURE = "/metadata/instance?api-version=2021-02-01"
META = "/openstack/latest/meta_data.json"
USER = "/openstack/latest/user_data"


class Serving:
    """The tool, serving on a free port of the loopback, until the block ends."""

    def __init__(self, *args):
        self.args = [sys.executable, "-I", str(TOOL), *args, "--port", "0"]

    def __enter__(self):
        self.process = subprocess.Popen(self.args, stderr=subprocess.PIPE, text=True)
        first = self.process.stderr.readline()
        if " on 127.0.0.1:" not in first:
            self.process.kill()
            raise AssertionError(f"the tool did not start: {first!r}")
        self.port = int(first.rsplit(":", 1)[1])
        if "--wireserver" in self.args:
            second = self.process.stderr.readline()
            if not second.startswith("fake-instance-data: wireserver on 127.0.0.1:"):
                self.process.kill()
                raise AssertionError(f"the WireServer did not start: {second!r}")
            self.wireserver = int(second.rsplit(":", 1)[1])
        return self

    def __exit__(self, *_):
        if self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=10)
        self.process.stderr.close()

    def get(self, path, headers=None, port=None):
        return self.request("GET", path, headers, None, port)

    def request(self, method, path, headers=None, body=None, port=None):
        connection = http.client.HTTPConnection("127.0.0.1", port or self.port, timeout=10)
        connection.request(method, path, body=body, headers=headers or {})
        response = connection.getresponse()
        body = response.read()
        connection.close()
        return response, body

    def log(self):
        """What the tool logged after its first lines, once it has stopped."""
        self.process.terminate()
        self.process.wait(timeout=10)
        return self.process.stderr.read()


def run(*args):
    return subprocess.run(
        [sys.executable, "-I", str(TOOL), *args],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )


class Answers(unittest.TestCase):
    def check_head(self, response, body):
        # The node's client reads only Content-Length bodies, and the Date its clock is set from.
        self.assertEqual(int(response.getheader("Content-Length")), len(body))
        self.assertIsNone(response.getheader("Transfer-Encoding"))
        date = response.getheader("Date")
        self.assertRegex(date, r"^[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT$")
        sent = email.utils.parsedate_to_datetime(date).timestamp()
        self.assertLess(abs(sent - time.time()), 60)

    def test_openstack(self):
        with Serving("--cloud", "openstack", "--config", "valid") as cloud:
            response, body = cloud.get(META)
            self.assertEqual(response.status, 200)
            self.check_head(response, body)
            self.assertEqual(json.loads(body)["uuid"], "83679162-1378-4288-a2d4-70e13ec132aa")
            response, body = cloud.get(USER)
            self.assertEqual(response.status, 200)
            self.check_head(response, body)
            self.assertEqual(body, VALID)
            # Azure's request, with its header, finds OpenStack's 404.
            response, body = cloud.get(AZURE, {"Metadata": "true"})
            self.assertEqual(response.status, 404)
            self.check_head(response, body)
            self.assertIn(b"404 Not Found", body)

    def test_openstack_without_user_data(self):
        with Serving("--cloud", "openstack", "--config", "none") as cloud:
            self.assertEqual(cloud.get(META)[0].status, 200)
            response, body = cloud.get(USER)
            self.assertEqual(response.status, 404)
            self.check_head(response, body)

    def test_azure(self):
        with Serving("--cloud", "azure", "--config", "valid") as cloud:
            response, body = cloud.get(AZURE, {"Metadata": "true"})
            self.assertEqual(response.status, 200)
            self.check_head(response, body)
            compute = json.loads(body)["compute"]
            self.assertEqual(compute["vmId"], "5c08b38e-4d57-4c23-ac45-aca61037f084")
            self.assertEqual(base64.b64decode(compute["userData"], validate=True), VALID)
            # OpenStack's requests carry no Metadata header: Azure's 400, whatever the path.
            for path in (META, USER, AZURE):
                response, body = cloud.get(path)
                self.assertEqual(response.status, 400, path)
                self.check_head(response, body)
                self.assertIn(b"Required metadata header not specified", body)
            # With the header, a path Azure does not have is its 404.
            response, body = cloud.get(META, {"Metadata": "true"})
            self.assertEqual(response.status, 404)
            self.assertEqual(json.loads(body), {"error": "Not found"})

    def test_azure_without_user_data(self):
        with Serving("--cloud", "azure", "--config", "none") as cloud:
            response, body = cloud.get(AZURE, {"Metadata": "true"})
            self.assertEqual(response.status, 200)
            self.assertEqual(json.loads(body)["compute"]["userData"], "")

    def test_each_built_in_config_and_a_file(self):
        listed = run("--list")
        self.assertEqual(listed.returncode, 0)
        names = [line.split("\t")[0] for line in listed.stdout.splitlines()]
        self.assertEqual(
            names,
            ["valid", "no-token-keys", "nine-keys", "bad-key", "unknown-field", "wrong-version"],
        )
        expected = {
            "no-token-keys": {"moruna": 1, "api": {}},
            "nine-keys": {"moruna": 1, "api": {"token_keys": [KEY] * 9}},
            "bad-key": {"moruna": 1, "api": {"token_keys": ["not-a-key"]}},
            "unknown-field": {"moruna": 1, "api": {"token_keys": [KEY]}, "extra": True},
            "wrong-version": {"moruna": 2, "api": {"token_keys": [KEY]}},
        }
        for name, config in expected.items():
            with Serving("--cloud", "openstack", "--config", name) as cloud:
                self.assertEqual(json.loads(cloud.get(USER)[1]), config, name)
            line = next(line for line in listed.stdout.splitlines() if line.startswith(name))
            self.assertTrue(line.split("\t")[1].startswith("moruna: fatal config: "), line)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            path.write_bytes(b"#cloud-config\n")
            with Serving("--cloud", "azure", "--config", str(path)) as cloud:
                body = cloud.get(AZURE, {"Metadata": "true"})[1]
                user_data = json.loads(body)["compute"]["userData"]
                self.assertEqual(base64.b64decode(user_data), b"#cloud-config\n")

    def test_what_it_does_not_serve(self):
        with Serving("--cloud", "openstack", "--config", "valid") as cloud:
            connection = http.client.HTTPConnection("127.0.0.1", cloud.port, timeout=10)
            connection.request("POST", USER, body=b"x")
            response = connection.getresponse()
            self.assertEqual(response.status, 405)
            response.read()
            connection.close()
            # A client that goes before its request is whole gets nothing, and the server
            # serves the next one.
            with socket.create_connection(("127.0.0.1", cloud.port), timeout=10) as early:
                early.sendall(b"GET / HTTP/1.1\r\n")
            self.assertEqual(cloud.get(META)[0].status, 200)
        refused = run("--cloud", "openstack", "--config", "no-such-config")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("--config is valid, no-token-keys", refused.stderr)
        self.assertNotEqual(run("--cloud", "gcp", "--config", "valid").returncode, 0)
        self.assertNotEqual(run("--cloud", "azure").returncode, 0)


# What the node's platform library posts for the recorded goal state (item 1.12): the document
# of Microsoft's "Create Linux images without a provisioning agent".
HEALTH = (
    "<Health><GoalStateIncarnation>1</GoalStateIncarnation><Container><ContainerId>"
    "0d3f6c58-1b2e-4c8a-9f61-2a7e5b9c4d10</ContainerId><RoleInstanceList><Role><InstanceId>"
    "a41f2c7e-93b5-4d0a-8e6c-1f7b2d9e3c58._moruna-node-1</InstanceId><Health><State>Ready"
    "</State></Health></Role></RoleInstanceList></Container></Health>"
)
VERSION = {"x-ms-version": "2012-11-30"}
# The headers the node posts the Health document with (PM, item 1.12).
POSTED = {**VERSION, "Content-Type": "text/xml;charset=utf-8", "x-ms-agent-name": "moruna"}


class WireServer(unittest.TestCase):
    def serving(self, *extra):
        return Serving(
            "--cloud", "azure", "--config", "valid", "--wireserver", "127.0.0.1:0", *extra
        )

    def test_the_report_as_the_node_makes_it(self):
        with self.serving() as cloud:
            port = cloud.wireserver
            self.assertNotEqual(port, cloud.port)
            # The goal state, read at the paths Microsoft's example reads.
            response, body = cloud.get("/machine?comp=goalstate", VERSION, port)
            self.assertEqual(response.status, 200)
            Answers.check_head(self, response, body)
            root = ElementTree.fromstring(body)
            self.assertEqual(root.findtext("Incarnation"), "1")
            self.assertEqual(
                root.findtext("Container/ContainerId"), "0d3f6c58-1b2e-4c8a-9f61-2a7e5b9c4d10"
            )
            self.assertEqual(
                root.findtext("Container/RoleInstanceList/RoleInstance/InstanceId"),
                "a41f2c7e-93b5-4d0a-8e6c-1f7b2d9e3c58._moruna-node-1",
            )
            # The Health document that reports it ready is taken.
            response, body = cloud.request("POST", "/machine?comp=health", POSTED, HEALTH, port)
            self.assertEqual(response.status, 200)
            Answers.check_head(self, response, body)
            self.assertEqual(body, b"")
            # Instance data is still served on the first address.
            self.assertEqual(cloud.get(AZURE, {"Metadata": "true"})[0].status, 200)
            log = cloud.log()
        self.assertIn(
            "fake-instance-data: report ready taken: incarnation 1 container "
            "0d3f6c58-1b2e-4c8a-9f61-2a7e5b9c4d10 instance "
            "a41f2c7e-93b5-4d0a-8e6c-1f7b2d9e3c58._moruna-node-1",
            log,
        )
        self.assertIn("fake-instance-data: wireserver POST /machine?comp=health: HTTP/1.1 200", log)

    def test_it_is_strict(self):
        with self.serving() as cloud:
            port = cloud.wireserver
            goal, health = "/machine?comp=goalstate", "/machine?comp=health"
            # Without the version header, or with another version.
            self.assertEqual(cloud.get(goal, None, port)[0].status, 400)
            self.assertEqual(cloud.get(goal, {"x-ms-version": "2015-04-05"}, port)[0].status, 400)
            self.assertEqual(cloud.request("POST", health, None, HEALTH, port)[0].status, 400)
            # Without the document's type or this agent's name, or with others.
            for name in ("Content-Type", "x-ms-agent-name"):
                for value in (None, "other"):
                    headers = {k: v for k, v in POSTED.items() if k != name}
                    if value is not None:
                        headers[name] = value
                    response, _ = cloud.request("POST", health, headers, HEALTH, port)
                    self.assertEqual(response.status, 400, (name, value))
            # A document that is not the goal state's, or that says something else.
            for document in [
                "",
                HEALTH.replace(">1<", ">2<"),
                HEALTH.replace("Ready", "NotReady"),
                HEALTH + "\n",
            ]:
                response, _ = cloud.request("POST", health, POSTED, document, port)
                self.assertEqual(response.status, 400, document)
            # The methods swapped, and a path it does not have.
            self.assertEqual(cloud.request("POST", goal, POSTED, HEALTH, port)[0].status, 400)
            self.assertEqual(cloud.get(health, VERSION, port)[0].status, 400)
            self.assertEqual(cloud.get("/machine?comp=config", VERSION, port)[0].status, 404)
            log = cloud.log()
        self.assertNotIn("report ready taken", log)

    def test_refused_reports(self):
        with self.serving("--report", "refuse") as cloud:
            port = cloud.wireserver
            response, body = cloud.get("/machine?comp=goalstate", VERSION, port)
            self.assertEqual(response.status, 503)
            Answers.check_head(self, response, body)
            response, _ = cloud.request("POST", "/machine?comp=health", POSTED, HEALTH, port)
            self.assertEqual(response.status, 503)

    def test_chunked(self):
        with self.serving("--chunked") as cloud:
            # Each answer in chunks, on either address, read whole by a client that reads them.
            response, body = cloud.get(AZURE, {"Metadata": "true"})
            self.assertEqual(response.status, 200)
            self.assertEqual(response.getheader("Transfer-Encoding"), "chunked")
            self.assertIsNone(response.getheader("Content-Length"))
            self.assertEqual(json.loads(body)["compute"]["vmId"], AZURE_ID)
            port = cloud.wireserver
            response, body = cloud.get("/machine?comp=goalstate", VERSION, port)
            self.assertEqual(response.getheader("Transfer-Encoding"), "chunked")
            self.assertEqual(ElementTree.fromstring(body).findtext("Incarnation"), "1")
            response, body = cloud.request("POST", "/machine?comp=health", POSTED, HEALTH, port)
            self.assertEqual((response.status, body), (200, b""))
            # On the wire: chunks of at most 512 bytes, each with an extension, and a trailer.
            raw = raw_get(cloud.port, AZURE, "Metadata: true")
            head, framed = raw.split(b"\r\n\r\n", 1)
            self.assertIn(b"\r\nTransfer-Encoding: chunked", head)
            self.assertNotIn(b"content-length", head.lower())
            sizes = []
            rest = framed
            while True:
                line, rest = rest.split(b"\r\n", 1)
                size, extension = line.split(b";", 1)
                self.assertTrue(extension.startswith(b"fake="), line)
                sizes.append(int(size, 16))
                if sizes[-1] == 0:
                    break
                self.assertEqual(rest[sizes[-1] : sizes[-1] + 2], b"\r\n")
                rest = rest[sizes[-1] + 2 :]
            self.assertEqual(rest, b"X-Fake: chunked\r\n\r\n")
            self.assertGreater(len(sizes), 2)
            self.assertTrue(all(0 < size <= CHUNK_MAX for size in sizes[:-1]), sizes)

    def test_what_it_refuses_to_start_with(self):
        # The WireServer is Azure's; its address is HOST:PORT; --report needs it.
        on_openstack = run(
            "--cloud", "openstack", "--config", "valid", "--wireserver", "127.0.0.1:0"
        )
        self.assertNotEqual(on_openstack.returncode, 0)
        self.assertIn("--wireserver is Azure's", on_openstack.stderr)
        for address in ["127.0.0.1", ":80", "127.0.0.1:http"]:
            refused = run("--cloud", "azure", "--config", "valid", "--wireserver", address)
            self.assertNotEqual(refused.returncode, 0, address)
            self.assertIn("--wireserver is HOST:PORT", refused.stderr)
        alone = run("--cloud", "azure", "--config", "valid", "--report", "refuse")
        self.assertNotEqual(alone.returncode, 0)
        self.assertIn("--report needs --wireserver", alone.stderr)


AZURE_ID = "5c08b38e-4d57-4c23-ac45-aca61037f084"
# The most bytes of a chunk `--chunked` sends.
CHUNK_MAX = 512


def raw_get(port, path, header):
    """The bytes a GET of `path` is answered with, as they come off the wire."""
    with socket.create_connection(("127.0.0.1", port), timeout=10) as connection:
        connection.sendall(f"GET {path} HTTP/1.1\r\nHost: x\r\n{header}\r\n\r\n".encode())
        answer = b""
        while chunk := connection.recv(65536):
            answer += chunk
    return answer


if __name__ == "__main__":
    unittest.main()
