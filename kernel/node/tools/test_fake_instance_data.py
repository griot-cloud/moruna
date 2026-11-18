#!/usr/bin/env python3
"""fake-instance-data.py (item 1.4): what it answers as each cloud, run as the boots run it.

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
        return self

    def __exit__(self, *_):
        self.process.terminate()
        self.process.wait(timeout=10)
        self.process.stderr.close()

    def get(self, path, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        connection.request("GET", path, headers=headers or {})
        response = connection.getresponse()
        body = response.read()
        connection.close()
        return response, body


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


if __name__ == "__main__":
    unittest.main()
