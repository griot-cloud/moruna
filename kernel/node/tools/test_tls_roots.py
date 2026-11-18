#!/usr/bin/env python3
"""tls-roots.py (system design 3D): a release checked and converted, the committed files checked,
and every refusal. Offline: the fetch is given its answers.

Run: python3 -I kernel/node/tools/test_tls_roots.py
"""

import base64
import hashlib
import importlib.util
import io
import struct
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("tls_roots", TOOLS / "tls-roots.py")
roots = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(roots)

RELEASE = "2026-11-04"
URL = f"https://curl.se/ca/cacert-{RELEASE}.pem"
# Two stand-ins for DER certificates: a SEQUENCE each. The tool checks the framing; the network
# server parses the certificates themselves.
FIRST = b"\x30\x03\x02\x01\x01"
SECOND = b"\x30\x81\x80" + bytes(range(128))


def block(der: bytes) -> str:
    text = base64.b64encode(der).decode()
    lines = [text[i:i + 64] for i in range(0, len(text), 64)]
    return "\n".join([roots.BEGIN, *lines, roots.END]) + "\n"


PEM = ("##\n## Bundle of CA Root Certificates\n##\n\nFirst Root\n==========\n" + block(FIRST)
       + "\nSecond Root\n===========\n" + block(SECOND)).encode()


def published(data: bytes) -> bytes:
    return f"{hashlib.sha256(data).hexdigest()}  cacert-{RELEASE}.pem\n".encode()


class Curl:
    """curl's site, as far as the tool asks it."""

    def __init__(self, pem=PEM, digest=None):
        self.files = {URL: pem, f"{URL}.sha256": digest if digest is not None else published(pem)}
        self.asked = []

    def __call__(self, url):
        self.asked.append(url)
        return self.files[url]


class Conversion(unittest.TestCase):
    def test_the_list_is_the_server_s_length_and_wire_encoding(self):
        self.assertEqual(roots.certificates(PEM.decode()), [FIRST, SECOND])
        encoded = roots.encode([FIRST, SECOND])
        payload = struct.pack("<I", 2) + struct.pack("<I", len(FIRST)) + FIRST \
            + struct.pack("<I", len(SECOND)) + SECOND
        self.assertEqual(encoded, struct.pack("<I", len(payload)) + payload)

    def test_what_is_not_a_bundle_of_certificates_is_refused(self):
        cases = [
            ("", "no certificate"),
            ("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n", "not a certificate"),
            (f"{roots.BEGIN}\nAAAA\n", "no end"),
            (f"{roots.BEGIN}\nnot base64!\n{roots.END}\n", "not base64"),
            (block(b"\x04\x01\x00"), "not DER"),
        ]
        for text, says in cases:
            with self.assertRaises(roots.Refused) as refused:
                roots.certificates(text)
            self.assertIn(says, str(refused.exception), text)

    def test_the_server_s_bounds_hold(self):
        with self.assertRaisesRegex(roots.Refused, "more than the network server's 256"):
            roots.encode([FIRST] * (roots.ROOTS_MAX + 1))
        with self.assertRaisesRegex(roots.Refused, "more than a Wire value's"):
            roots.encode([b"\x30" + bytes(roots.VALUE_MAX)])
        with self.assertRaisesRegex(roots.Refused, "bytes of roots"):
            roots.encode([b"\x30" + bytes(roots.VALUE_MAX - 1)] * 17)
        self.assertEqual(len(roots.encode([FIRST] * roots.ROOTS_MAX)), 8 + roots.ROOTS_MAX * 9)


class Fetching(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.roots = Path(self.temporary.name) / "roots"
        self.dir = Path(self.temporary.name) / "cloud"
        self.roots.mkdir()
        self.dir.mkdir()

    def tearDown(self):
        self.temporary.cleanup()

    def test_a_release_curl_published_is_written_with_its_record(self):
        curl = Curl()
        written = roots.fetch(RELEASE, self.roots, self.dir, curl)
        self.assertEqual(curl.asked, [URL, f"{URL}.sha256"])
        # The list where it ships from, and nothing else there; the release and its record apart.
        self.assertEqual(sorted(p.name for p in self.dir.iterdir()), ["tls.roots"])
        self.assertEqual(sorted(p.name for p in self.roots.iterdir()), ["cacert.pem", "tls-roots.txt"])
        self.assertEqual((self.roots / "cacert.pem").read_bytes(), PEM)
        list_bytes = (self.dir / "tls.roots").read_bytes()
        self.assertEqual(list_bytes, roots.encode([FIRST, SECOND]))
        self.assertEqual(written, (self.roots / "tls-roots.txt").read_text())
        self.assertEqual(written.splitlines(), [
            f"release {RELEASE}",
            f"source {URL}",
            f"cacert.pem sha256 {hashlib.sha256(PEM).hexdigest()}",
            f"tls.roots sha256 {hashlib.sha256(list_bytes).hexdigest()}",
            "roots 2",
        ])
        # And the committed files check, offline.
        self.assertEqual(roots.check(self.roots, self.dir), written)

    def test_a_release_that_is_not_what_curl_published_writes_nothing(self):
        cases = [
            (Curl(digest=published(b"something else")), "not what curl published"),
            (Curl(digest=b"<html>not found</html>"), "not one"),
            (Curl(pem=PEM + b"\xff"), "not UTF-8"),
            (Curl(pem=b"## no certificates\n"), "no certificate"),
        ]
        for curl, says in cases:
            with self.assertRaises(roots.Refused) as refused:
                roots.fetch(RELEASE, self.roots, self.dir, curl)
            self.assertIn(says, str(refused.exception))
            self.assertEqual(list(self.dir.iterdir()), [])
            self.assertEqual(list(self.roots.iterdir()), [])
        with self.assertRaisesRegex(roots.Refused, "a release is a date"):
            roots.fetch("latest", self.roots, self.dir, Curl())

    def test_committed_files_that_disagree_are_refused(self):
        roots.fetch(RELEASE, self.roots, self.dir, Curl())
        record = (self.roots / "tls-roots.txt").read_text()
        (self.dir / "tls.roots").write_bytes(roots.encode([FIRST]))
        with self.assertRaisesRegex(roots.Refused, "not cacert.pem's certificates"):
            roots.check(self.roots, self.dir)
        (self.dir / "tls.roots").write_bytes(roots.encode([FIRST, SECOND]))
        (self.roots / "tls-roots.txt").write_text(record.replace("roots 2", "roots 3"))
        with self.assertRaisesRegex(roots.Refused, "does not match"):
            roots.check(self.roots, self.dir)
        (self.roots / "tls-roots.txt").write_text("roots 2\n")
        with self.assertRaisesRegex(roots.Refused, "names no release"):
            roots.check(self.roots, self.dir)
        (self.dir / "tls.roots").unlink()
        with self.assertRaisesRegex(roots.Refused, "tls.roots"):
            roots.check(self.roots, self.dir)

    def test_the_command_line(self):
        roots.fetch(RELEASE, self.roots, self.dir, Curl())
        places = ["--roots", str(self.roots), "--dir", str(self.dir)]
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(roots.main(["check", *places]), 0)
        self.assertIn(f"release {RELEASE}", out.getvalue())
        (self.roots / "cacert.pem").write_bytes(PEM.replace(b"First", b"Other"))
        err = io.StringIO()
        with redirect_stderr(err):
            self.assertEqual(roots.main(["check", *places]), 1)
        self.assertTrue(err.getvalue().startswith("tls-roots.py: "))
        with redirect_stderr(io.StringIO()):
            self.assertEqual(roots.main(["fetch", "--release", "soon", *places]), 1)


class Committed(unittest.TestCase):
    def test_the_cloud_profile_s_roots_agree_with_their_release(self):
        written = roots.check(roots.DEFAULT_ROOTS, roots.DEFAULT_DIR)
        count = int(written.splitlines()[-1].split()[1])
        self.assertGreater(count, 100)
        self.assertLessEqual(count, roots.ROOTS_MAX)


if __name__ == "__main__":
    unittest.main()
