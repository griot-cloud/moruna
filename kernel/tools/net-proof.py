#!/usr/bin/env python3
"""Controlled KE2 proof inputs and loopback TLS peer; never starts a VM or a build.

After an authorized kernel/tools/build.sh to a fresh owned OUT:
  python3 kernel/tools/net-proof.py prepare OUT
  python3 kernel/tools/net-proof.py peer OUT --port 0 --connections 1
The peer prints its loopback port and writes OUT/ke2/peer-ready.json.
Then, in a separate bounded proof step, run prepare again with --port PORT,
regenerate OUT's initramfs from OUT/ke2/boot.image, and boot/check it normally.
TLS keys, CA and the UTC anchor are proof inputs, never production defaults.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import socket
import ssl
import struct
import subprocess
import time


def output_root(value):
    root = Path(value).resolve()
    if not (root / "moruna-microkernel.elf").is_file():
        raise ValueError("OUT must be your already-built, owned kernel proof output")
    return root


def certificate(directory):
    """OpenSSL is an existing proof-image dependency; never install it here."""
    paths = [directory / name for name in ("ca.key", "ca.pem", "peer.key", "peer.pem", "root.der")]
    if any(path.exists() for path in paths):
        if not all(path.is_file() for path in paths):
            raise ValueError("partial proof certificate fixture; inspect it, no keys replaced")
        return
    commands = [
        ["req", "-new", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes",
         "-sha256", "-days", "2", "-keyout", "ca.key", "-out", "ca.pem", "-subj", "/CN=Moruna KE2 proof CA",
         "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign"],
        ["req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes",
         "-keyout", "peer.key", "-out", "peer.csr", "-subj", "/CN=peer.ke2.test"],
        ["x509", "-req", "-in", "peer.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
         "-out", "peer.pem", "-days", "2", "-sha256", "-extfile", "peer.ext"],
        ["x509", "-in", "ca.pem", "-outform", "DER", "-out", "root.der"],
    ]
    (directory / "peer.ext").write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:peer.ke2.test\n")
    for command in commands:
        result = subprocess.run(["openssl", *command], cwd=directory, timeout=30, capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(f"OpenSSL {command[0]} failed ({result.returncode}): {result.stderr.strip()}")


def prepare(args):
    root = output_root(args.out)
    directory = root / "ke2"
    directory.mkdir(exist_ok=True)
    certificate(directory)
    der = (directory / "root.der").read_bytes()
    payload = struct.pack("<II", 1, len(der)) + der
    (directory / "roots").write_bytes(struct.pack("<I", len(payload)) + payload)
    utc = int(time.time())
    config = f"""moruna-netstack 1
setting tls-unix-seconds {utc}
setting tls-time-valid-ms 3600000
setting tcp-receive-buffer 131072
setting tcp-send-buffer 131072
interface wan card={args.card} dhcp
dns 127.0.0.1
grant listen-test rights=connect,listen,udp,ping,resolve,grant remote=127.0.0.0/8,::1/128,10.0.2.100/32@47012 local=127.0.0.0/8,::1/128,10.0.2.15/32 names=peer.ke2.test@47012 sockets=64
"""
    (directory / "config").write_text(config)
    source = Path(__file__).resolve().parent.parent / "initramfs/boot.image"
    description = source.read_text() + f"""
# SDD 31: an isolated user-network NIC. No public network is needed; guest TCP
# 10.0.2.100:47012 is forwarded only to this proof's loopback TLS peer.
machine -netdev user,id=ke2-user,restrict=on,dhcpstart=10.0.2.15,guestfwd=tcp:10.0.2.100:47012-tcp:127.0.0.1:{args.port}
machine -device virtio-net-pci,netdev=ke2-user,mac=52:54:00:4b:45:02,romfile=,addr=0x14,disable-legacy=on
process netstack
  handle console console
  handle data:ke2/config config
  handle data:ke2/roots tls-roots
  expect KE2 netstack: up: 2 interfaces, 1 grants,
  exits 0
link netstack.directory devices.directory-ke2
process netstack-test
  handle console console
  expect KE2 user DHCP: ok:
  expect KE2 user DNS: ok:
  expect KE2 user TLS: ok:
  expect KE2 conformance moruna.net: ok:
  exits 0
link netstack-test.server netstack.listen-test
"""
    (directory / "boot.image").write_text(description)
    (directory / "inputs.json").write_text(json.dumps({"utc_anchor": utc, "utc_anchor_iso": datetime.datetime.fromtimestamp(utc, datetime.timezone.utc).isoformat(), "valid_seconds": 3600, "host_peer": f"127.0.0.1:{args.port}", "guest_endpoint": "10.0.2.100:47012", "public_network_required": False}, indent=2) + "\n")
    print("Prepared controlled KE2 inputs; no image regenerated or VM started.")


def peer(args):
    root = output_root(args.out)
    directory = root / "ke2"
    if not (directory / "peer.pem").is_file():
        raise ValueError("prepare the controlled proof certificates first")
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.maximum_version = ssl.TLSVersion.TLSv1_3
    context.load_cert_chain(directory / "peer.pem", directory / "peer.key")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", args.port))
        listener.listen(1)
        listener.settimeout(args.timeout)
        port = listener.getsockname()[1]
        (directory / "peer-ready.json").write_text(json.dumps({"host": "127.0.0.1", "port": port, "connections": args.connections}) + "\n")
        print(f"KE2 controlled peer listening on 127.0.0.1:{port}", flush=True)
        for index in range(args.connections):
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(60)
                with context.wrap_socket(connection, server_side=True) as secure:
                    if secure.version() != "TLSv1.3":
                        raise ValueError("peer negotiated an unexpected protocol")
                    request = bytearray()
                    while not request.endswith(b"\n") and len(request) < 128:
                        part = secure.recv(128 - len(request))
                        if not part:
                            raise ValueError("peer ended before the fixture request")
                        request.extend(part)
                    if request != b"moruna-ke2-private-request\n":
                        raise ValueError("peer received an unexpected fixture request")
                    secure.sendall(b"moruna-ke2-verified-reply\n")
                    # Initiate and require the reciprocal authenticated close.
                    raw = secure.unwrap()
                    raw.close()
                print(f"KE2 controlled peer: verified exchange {index + 1}/{args.connections}", flush=True)


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("prepare", "peer"):
        command = commands.add_parser(name)
        command.add_argument("out")
        command.add_argument("--port", type=int, default=47012 if name == "prepare" else 0)
    commands.choices["prepare"].add_argument("--card", choices=("net0", "net2"), default="net2")
    commands.choices["peer"].add_argument("--connections", type=int, choices=range(1, 4), default=1)
    commands.choices["peer"].add_argument("--timeout", type=int, choices=range(1, 1801), default=1800)
    args = parser.parse_args()
    if not 0 <= args.port <= 65535 or (args.command == "prepare" and args.port == 0):
        parser.error("prepare requires the peer's actual nonzero port")
    {"prepare": prepare, "peer": peer}[args.command](args)


if __name__ == "__main__":
    main()
