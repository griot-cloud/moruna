#!/usr/bin/env python3
"""The cloud profile's TLS roots (system design 3D): Mozilla's root set, as curl publishes it.

    tls-roots.py fetch --release YYYY-MM-DD [--from DIR] [--roots DIR] [--dir DIR]
    tls-roots.py check [--roots DIR] [--dir DIR]

`fetch` takes curl's dated release `cacert-<date>.pem` from https://curl.se/ca/ (MPL-2.0), checks
its SHA-256 against the one curl publishes beside it (`cacert-<date>.pem.sha256`), and writes:

- into `--dir` (kernel/node/cloud, the cloud profile's assets, which hold only what ships):
  `tls.roots`, its certificates as the network server reads its roots (SDD 31 d): four
  little-endian bytes of length, then the SDK Wire encoding of the list of DER certificates
  (a four-byte count, then each certificate's four-byte length and bytes);
- into `--roots` (kernel/node/tools/roots, the source material): `cacert.pem`, the release byte
  for byte as curl published it, and `tls-roots.txt`, the release, where it came from, and both
  files' SHA-256.

With `--from DIR` the two files are read from DIR, as downloaded by another client (a host
whose Python has no certificate store), and checked the same way.

`check` converts the committed `cacert.pem` again, offline, and refuses unless the list and the
record agree with it. Nothing else is fetched, and nothing from the release is run.
"""

import argparse
import base64
import binascii
import hashlib
import re
import struct
import sys
import urllib.request
from pathlib import Path

SOURCE = "https://curl.se/ca"
# The network server's bounds (SDD 31 d): roots, payload bytes, and one Wire value's bytes.
ROOTS_MAX = 256
PAYLOAD_MAX = 1 << 20
VALUE_MAX = 64 * 1024
# The most bytes a release may be: curl's are about 220 KiB.
RELEASE_MAX = 2 << 20
BEGIN = "-----BEGIN CERTIFICATE-----"
END = "-----END CERTIFICATE-----"
RELEASE = re.compile(r"\d{4}-\d{2}-\d{2}")
DIGEST = re.compile(r"[0-9a-f]{64}")
# Where the list ships from, and where its source and record are kept.
DEFAULT_DIR = Path(__file__).resolve().parents[1] / "cloud"
DEFAULT_ROOTS = Path(__file__).resolve().parent / "roots"


class Refused(Exception):
    """Why the roots were not written, or do not agree."""


def certificates(pem: str) -> list[bytes]:
    """The DER certificates of a PEM bundle, in order. Anything else in it must be outside a block."""
    found = []
    lines = iter(pem.splitlines())
    for line in lines:
        if line.strip() != BEGIN:
            if line.startswith("-----"):
                raise Refused(f"a block that is not a certificate: {line.strip()}")
            continue
        body = []
        for inner in lines:
            if inner.strip() == END:
                break
            body.append(inner.strip())
        else:
            raise Refused("a certificate block with no end")
        try:
            der = base64.b64decode("".join(body), validate=True)
        except binascii.Error as error:
            raise Refused(f"certificate {len(found) + 1} is not base64: {error}") from None
        if not der or der[0] != 0x30:
            raise Refused(f"certificate {len(found) + 1} is not DER")
        found.append(der)
    if not found:
        raise Refused("no certificate in the release")
    return found


def utf8(pem: bytes) -> str:
    """A release's text: UTF-8, as its comments name some certificates in it."""
    try:
        return pem.decode("utf-8")
    except UnicodeDecodeError:
        raise Refused("the release is not UTF-8") from None


def encode(roots: list[bytes]) -> bytes:
    """The roots as the network server reads them, within its bounds."""
    if len(roots) > ROOTS_MAX:
        raise Refused(f"{len(roots)} roots, more than the network server's {ROOTS_MAX}")
    payload = bytearray(struct.pack("<I", len(roots)))
    for der in roots:
        if len(der) > VALUE_MAX:
            raise Refused(f"a certificate of {len(der)} bytes, more than a Wire value's {VALUE_MAX}")
        payload += struct.pack("<I", len(der)) + der
    if len(payload) > PAYLOAD_MAX:
        raise Refused(f"{len(payload)} bytes of roots, more than the network server's {PAYLOAD_MAX}")
    return struct.pack("<I", len(payload)) + bytes(payload)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def record(release: str, pem: bytes, roots: bytes, count: int) -> str:
    """The record written beside the files."""
    return (f"release {release}\n"
            f"source {SOURCE}/cacert-{release}.pem\n"
            f"cacert.pem sha256 {sha256(pem)}\n"
            f"tls.roots sha256 {sha256(roots)}\n"
            f"roots {count}\n")


def published(text: str) -> str:
    """The SHA-256 curl publishes beside a release: its first word."""
    words = text.split()
    if not words or not DIGEST.fullmatch(words[0]):
        raise Refused("curl's published SHA-256 is not one")
    return words[0]


def download(url: str) -> bytes:
    """`url`'s bytes, at most RELEASE_MAX of them."""
    request = urllib.request.Request(url, headers={"User-Agent": "tls-roots.py"})
    try:
        with urllib.request.urlopen(request, timeout=60) as answer:
            data = answer.read(RELEASE_MAX + 1)
    except OSError as error:
        raise Refused(f"{url}: {error} (download it and its .sha256, and give --from)") from None
    if len(data) > RELEASE_MAX:
        raise Refused(f"{url} is larger than {RELEASE_MAX} bytes")
    return data


def downloaded(directory: Path):
    """What `download` gives, from files already in `directory` under their names in the URL."""
    def get(url: str) -> bytes:
        path = directory / url.rsplit("/", 1)[1]
        try:
            data = path.read_bytes()
        except OSError as error:
            raise Refused(f"{path}: {error.strerror}") from None
        if len(data) > RELEASE_MAX:
            raise Refused(f"{path} is larger than {RELEASE_MAX} bytes")
        return data
    return get


def fetch(release: str, sources: Path, directory: Path, get=download) -> str:
    """Fetches `release` and checks it; writes the list into `directory` and the release and its
    record into `sources`; returns the record."""
    if not RELEASE.fullmatch(release):
        raise Refused(f"a release is a date, YYYY-MM-DD: {release!r}")
    url = f"{SOURCE}/cacert-{release}.pem"
    pem = get(url)
    expected = published(get(f"{url}.sha256").decode("ascii", "replace"))
    if sha256(pem) != expected:
        raise Refused(f"{url} is not what curl published: SHA-256 {sha256(pem)}, not {expected}")
    roots = certificates(utf8(pem))
    encoded = encode(roots)
    written = record(release, pem, encoded, len(roots))
    (sources / "cacert.pem").write_bytes(pem)
    (directory / "tls.roots").write_bytes(encoded)
    (sources / "tls-roots.txt").write_text(written)
    return written


def check(sources: Path, directory: Path) -> str:
    """Refuses unless `directory`'s `tls.roots` and `sources`' `tls-roots.txt` agree with
    `sources`' `cacert.pem`; returns the record."""
    try:
        pem = (sources / "cacert.pem").read_bytes()
        roots = (directory / "tls.roots").read_bytes()
        written = (sources / "tls-roots.txt").read_text()
    except OSError as error:
        raise Refused(f"{error.filename}: {error.strerror}") from None
    fields = dict(line.split(" ", 1) for line in written.splitlines() if " " in line)
    release = fields.get("release", "")
    if not RELEASE.fullmatch(release):
        raise Refused("tls-roots.txt names no release")
    converted = certificates(utf8(pem))
    if encode(converted) != roots:
        raise Refused("tls.roots is not cacert.pem's certificates")
    if record(release, pem, roots, len(converted)) != written:
        raise Refused("tls-roots.txt does not match cacert.pem and tls.roots")
    return written


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    fetching = commands.add_parser("fetch", help="fetch and convert a dated release")
    fetching.add_argument("--release", required=True, help="the release's date, YYYY-MM-DD")
    fetching.add_argument("--from", dest="source", type=Path,
                          help="read the release and its .sha256 from this directory")
    for command in (fetching, commands.add_parser("check", help="check the committed files agree")):
        command.add_argument("--roots", type=Path, default=DEFAULT_ROOTS,
                             help="where cacert.pem and tls-roots.txt are kept")
        command.add_argument("--dir", type=Path, default=DEFAULT_DIR, help="where tls.roots ships from")
    args = parser.parse_args(argv)
    try:
        if args.command == "fetch":
            get = downloaded(args.source) if args.source else download
            print(fetch(args.release, args.roots, args.dir, get), end="")
        else:
            print(check(args.roots, args.dir), end="")
    except Refused as refused:
        print(f"tls-roots.py: {refused}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
