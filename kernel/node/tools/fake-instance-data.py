#!/usr/bin/env python3
"""A cloud's instance data, for booted tests of the node's `platform` (item 1.4).

QEMU's user network, given `-netdev user,id=net0,net=169.254.169.0/24,host=169.254.169.254`,
forwards the node's connections to 169.254.169.254 port 80 to this machine's own 127.0.0.1
port 80. This server answers there as OpenStack's metadata service or Azure's Instance Metadata
Service does, from the recorded answers in `kernel/node/platform/fixtures`, with the instance's
user-data the node config it is given, and the `Date` of each answer this machine's time:

    fake-instance-data.py --cloud openstack|azure --config NAME|PATH|none
        [--bind 127.0.0.1] [--port 80]
    fake-instance-data.py --list

`--config` is a built-in config by name (`--list` names them, each with the line a boot given it
ends on), a file whose bytes are the user-data, or `none` for an instance created without any.

- OpenStack: `/openstack/latest/meta_data.json` is the recorded document; `/openstack/latest/
  user_data` is the config, or a 404 for `none`; any other path is OpenStack's 404.
- Azure: a request without `Metadata: true` is Azure's 400; `/metadata/instance?api-version=
  2021-02-01` with it is the recorded document, its `compute.userData` the config in base64 (empty
  for `none`); any other path is Azure's 404.

Each answer closes its connection. Each request is logged on standard error, after a first line
that says where the server listens (`--port 0` takes a free port). Run it inside `moruna-arm`,
as root, so it may bind port 80 (PM, 2026-11-18): nothing then listens on the Mac. Standard
library only.
"""

import argparse
import base64
import email.utils
import json
import socketserver
import sys
from pathlib import Path

FIXTURES = Path(__file__).resolve().parents[1] / "platform" / "fixtures"
AZURE_PATH = "/metadata/instance?api-version=2021-02-01"
OPENSTACK_META = "/openstack/latest/meta_data.json"
OPENSTACK_USER = "/openstack/latest/user_data"
HEAD_MAX = 16 * 1024
# The recorded config's one token key: the standard base64 of the bytes 0 to 31.
KEY = base64.b64encode(bytes(range(32))).decode()


def compact(value):
    return json.dumps(value, separators=(",", ":")).encode()


def config_with(**fields):
    config = {"moruna": 1, "api": {"token_keys": [KEY]}}
    config.update(fields)
    return compact(config)


# Each built-in config, and the line a boot given it ends on (`ready` for the valid one).
CONFIGS = {
    "valid": (config_with(), "moruna: ready api=<address>:443 boot_ms=<n>"),
    "no-token-keys": (
        config_with(api={}),
        "moruna: fatal config: `api.token_keys` is missing",
    ),
    "nine-keys": (
        config_with(api={"token_keys": [KEY] * 9}),
        "moruna: fatal config: `api.token_keys` holds 9 keys: 1 to 8 are allowed",
    ),
    "bad-key": (
        config_with(api={"token_keys": ["not-a-key"]}),
        "moruna: fatal config: `api.token_keys[0]` is not the standard base64 of a 32-byte "
        "Ed25519 public key",
    ),
    "unknown-field": (
        config_with(extra=True),
        "moruna: fatal config: unknown field `extra` in node config",
    ),
    "wrong-version": (
        config_with(moruna=2),
        "moruna: fatal config: `moruna` is 2: this node reads version 1",
    ),
}


def fixture(path):
    """A recorded answer: its status line, its headers and its body."""
    text = (FIXTURES / path).read_text()
    head, body = text.split("\n\n", 1)
    lines = head.split("\n")
    headers = [tuple(line.split(": ", 1)) for line in lines[1:]]
    return lines[0], headers, body.removesuffix("\n").encode()


def answer(recorded, body=None):
    """The bytes of a recorded answer, with `body` in place of its own when given, its length,
    and this machine's time as its `Date`."""
    status, headers, recorded_body = recorded
    body = recorded_body if body is None else body
    fresh = {"date": email.utils.formatdate(usegmt=True), "content-length": str(len(body))}
    out = [status] + [f"{name}: {fresh.get(name.lower(), value)}" for name, value in headers]
    return ("\r\n".join(out) + "\r\n\r\n").encode() + body


def not_allowed(method):
    body = f"{method} is not served here\n".encode()
    return (
        b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\nContent-Type: text/plain\r\n"
        + f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode()
        + body
    )


class Cloud:
    """What one cloud answers, given the instance's user-data (None for none)."""

    def __init__(self, cloud, user_data):
        self.cloud = cloud
        self.user_data = user_data

    def answer(self, path, headers):
        if self.cloud == "openstack":
            return self.openstack(path)
        return self.azure(path, headers)

    def openstack(self, path):
        if path == OPENSTACK_META:
            return answer(fixture("openstack/meta_data.json.http"))
        if path == OPENSTACK_USER:
            if self.user_data is None:
                return answer(fixture("openstack/user_data-404.http"))
            return answer(fixture("openstack/user_data.http"), self.user_data)
        return answer(fixture("openstack/azure-path-404.http"))

    def azure(self, path, headers):
        if headers.get("metadata") != "true":
            return answer(fixture("azure/openstack-path-400.http"))
        if path != AZURE_PATH:
            return answer(fixture("azure/openstack-path-404.http"))
        recorded = fixture("azure/instance.http")
        document = json.loads(recorded[2])
        document["compute"]["userData"] = base64.b64encode(self.user_data or b"").decode()
        return answer(recorded, compact(document))


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.request.recv(4096)
            if not chunk or len(head) + len(chunk) > HEAD_MAX:
                return
            head += chunk
        lines = head.split(b"\r\n\r\n", 1)[0].decode("latin-1").split("\r\n")
        method, path, _ = (lines[0].split(" ") + ["", "", ""])[:3]
        headers = {}
        for line in lines[1:]:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
        if method == "GET":
            reply = self.server.cloud.answer(path, headers)
        else:
            reply = not_allowed(method)
        self.wfile.write(reply)
        status = reply.split(b"\r\n", 1)[0].decode()
        print(f"fake-instance-data: {method} {path}: {status}", file=sys.stderr, flush=True)


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, address, cloud):
        super().__init__(address, Handler)
        self.cloud = cloud


def user_data(config):
    """The user-data `--config` names: a built-in config, a file's bytes, or None."""
    if config == "none":
        return None
    if config in CONFIGS:
        return CONFIGS[config][0]
    path = Path(config)
    if not path.is_file():
        names = ", ".join(CONFIGS)
        raise SystemExit(f"fake-instance-data: --config is {names}, none, or a file: {config}")
    return path.read_bytes()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--cloud", choices=["openstack", "azure"])
    parser.add_argument("--config", help="a built-in config's name, a file, or none")
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=80)
    parser.add_argument("--list", action="store_true", help="name the built-in configs")
    args = parser.parse_args(argv)
    if args.list:
        for name, (_, line) in CONFIGS.items():
            print(f"{name}\t{line}")
        return 0
    if args.cloud is None or args.config is None:
        parser.error("--cloud and --config are needed")
    cloud = Cloud(args.cloud, user_data(args.config))
    with Server((args.bind, args.port), cloud) as server:
        host, port = server.server_address[:2]
        print(f"fake-instance-data: {args.cloud} on {host}:{port}", file=sys.stderr, flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
