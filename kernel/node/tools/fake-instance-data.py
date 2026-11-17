#!/usr/bin/env python3
"""A cloud's instance data, for booted tests of the node's `platform` (item 1.4).

QEMU's user network, given `-netdev user,id=net0,net=169.254.169.0/24,host=169.254.169.254`,
forwards the node's connections to 169.254.169.254 port 80 to this machine's own 127.0.0.1
port 80. This server answers there as OpenStack's metadata service or Azure's Instance Metadata
Service does, from the recorded answers in `kernel/node/platform/fixtures`, with the instance's
user-data the node config it is given, and the `Date` of each answer this machine's time:

    fake-instance-data.py --cloud openstack|azure --config NAME|PATH|none
        [--bind 127.0.0.1] [--port 80] [--wireserver HOST:PORT [--report take|refuse]]
        [--chunked]
    fake-instance-data.py --list

`--config` is a built-in config by name (`--list` names them, each with the line a boot given it
ends on), a file whose bytes are the user-data, or `none` for an instance created without any.

- OpenStack: `/openstack/latest/meta_data.json` is the recorded document; `/openstack/latest/
  user_data` is the config, or a 404 for `none`; any other path is OpenStack's 404.
- Azure: a request without `Metadata: true` is Azure's 400; `/metadata/instance?api-version=
  2021-02-01` with it is the recorded document, its `compute.userData` the config in base64 (empty
  for `none`); any other path is Azure's 404.

With `--wireserver HOST:PORT` (Azure only, item 1.12) it is Azure's WireServer too, on that
second address, where a boot forwards the node's connections to 168.63.129.16 port 80. It is as
strict as the WireServer the platform library's host tests play:

- `GET /machine?comp=goalstate` with `x-ms-version: 2012-11-30` is the recorded goal state;
- `POST /machine?comp=health` with that header, `Content-Type: text/xml;charset=utf-8`,
  `x-ms-agent-name: moruna` and the `Health` document that reports the recorded goal state ready
  is taken, a `200`, and logged as `report ready taken`. The document
  it wants is made as Microsoft's "Create Linux images without a provisioning agent" makes it,
  with `ElementTree` from the goal state's `Incarnation`, `Container/ContainerId` and
  `Container/RoleInstanceList/RoleInstance/InstanceId`;
- anything else is a `400`, or a `404` for a path it does not have. These two are this fake's
  answers, not documented ones.

`--report refuse` answers every request to the WireServer `503`, so a boot shows the report's
fatal line after its 2 minutes.

`--chunked` sends every answer, on either address, in chunks (`Transfer-Encoding: chunked`) of
at most 512 bytes in place of its `Content-Length`, each chunk with an extension and the last
with a trailer, which a reader must step over.

Each answer closes its connection. Each request is logged on standard error, after a first line
that says where the server listens (`--port 0` takes a free port), and a second for the
WireServer when there is one. Run it inside `moruna-arm`, as root, so it may bind port 80 (PM,
2026-11-18): nothing then listens on the Mac. Standard library only.
"""

import argparse
import base64
import email.utils
import json
import socketserver
import sys
import threading
from pathlib import Path
from xml.etree import ElementTree

FIXTURES = Path(__file__).resolve().parents[1] / "platform" / "fixtures"
AZURE_PATH = "/metadata/instance?api-version=2021-02-01"
OPENSTACK_META = "/openstack/latest/meta_data.json"
OPENSTACK_USER = "/openstack/latest/user_data"
GOAL_STATE = "/machine?comp=goalstate"
HEALTH = "/machine?comp=health"
VERSION = "2012-11-30"
HEAD_MAX = 16 * 1024
BODY_MAX = 64 * 1024
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


def plain(status, text):
    """An answer of this fake's own: `status` and a line of text."""
    body = f"{text}\n".encode()
    return (
        f"HTTP/1.1 {status}\r\nDate: {email.utils.formatdate(usegmt=True)}\r\n"
        f"Content-Type: text/plain\r\nContent-Length: {len(body)}\r\n\r\n"
    ).encode() + body


def in_chunks(reply, size=512):
    """`reply`, an answer with a `Content-Length`, as the same answer in chunks of at most
    `size` bytes: `Transfer-Encoding: chunked` in place of the length, an extension on every
    chunk, and a trailer after the last."""
    head, body = reply.split(b"\r\n\r\n", 1)
    lines = [
        line for line in head.split(b"\r\n") if not line.lower().startswith(b"content-length:")
    ]
    lines.append(b"Transfer-Encoding: chunked")
    out = b"\r\n".join(lines) + b"\r\n\r\n"
    for at in range(0, len(body), size):
        piece = body[at : at + size]
        out += f"{len(piece):x};fake=chunked\r\n".encode() + piece + b"\r\n"
    return out + b"0;fake=last\r\nX-Fake: chunked\r\n\r\n"


def health_for(goal_state):
    """The `Health` document that reports `goal_state` ready, as Microsoft's example makes it."""
    root = ElementTree.fromstring(goal_state)
    container_id = root.findtext("Container/ContainerId")
    instance_id = root.findtext("Container/RoleInstanceList/RoleInstance/InstanceId")
    incarnation = root.findtext("Incarnation")
    health = ElementTree.Element("Health")
    ElementTree.SubElement(health, "GoalStateIncarnation").text = incarnation
    container = ElementTree.SubElement(health, "Container")
    ElementTree.SubElement(container, "ContainerId").text = container_id
    role_instance_list = ElementTree.SubElement(container, "RoleInstanceList")
    role = ElementTree.SubElement(role_instance_list, "Role")
    ElementTree.SubElement(role, "InstanceId").text = instance_id
    health_second = ElementTree.SubElement(role, "Health")
    ElementTree.SubElement(health_second, "State").text = "Ready"
    document = ElementTree.tostring(health, encoding="unicode", method="xml")
    return document, (incarnation, container_id, instance_id)


class WireServer:
    """Azure's WireServer: the recorded goal state, and the report that it is ready."""

    log = "wireserver "

    def __init__(self, refuse):
        self.refuse = refuse
        self.goal_state = fixture("azure/goalstate.http")
        self.health, self.names = health_for(self.goal_state[2])

    def respond(self, method, path, headers, body):
        refusal = self.refusal(method, path, headers, body)
        if refusal is not None:
            return refusal
        if method == "GET":
            return answer(self.goal_state)
        incarnation, container, instance = self.names
        print(
            f"fake-instance-data: report ready taken: incarnation {incarnation} "
            f"container {container} instance {instance}",
            file=sys.stderr,
            flush=True,
        )
        return answer(fixture("azure/health.http"))

    def refusal(self, method, path, headers, body):
        """This fake's answer to a request it does not take, or None for one it takes."""
        if self.refuse:
            return plain("503 Service Unavailable", "the report is refused (--report refuse)")
        if path not in (GOAL_STATE, HEALTH):
            return plain("404 Not Found", f"{path} is not served here")
        why = self.wrong(method, path, headers, body)
        return None if why is None else plain("400 Bad Request", why)

    def wrong(self, method, path, headers, body):
        """What is wrong with a request to a path served here, or None."""
        if headers.get("x-ms-version") != VERSION:
            return f"x-ms-version is not {VERSION}"
        if (method, path) not in (("GET", GOAL_STATE), ("POST", HEALTH)):
            return f"{method} {path} is not served here"
        if method == "GET":
            return None
        if (
            headers.get("content-type") != "text/xml;charset=utf-8"
            or headers.get("x-ms-agent-name") != "moruna"
        ):
            return "not posted as text/xml by the agent moruna"
        if body != self.health.encode():
            return "not the Health document of the goal state"
        return None


class Cloud:
    """What one cloud answers, given the instance's user-data (None for none)."""

    log = ""

    def __init__(self, cloud, user_data):
        self.cloud = cloud
        self.user_data = user_data

    def respond(self, method, path, headers, _body):
        if method != "GET":
            return not_allowed(method)
        return self.answer(path, headers)

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
        received = b""
        while b"\r\n\r\n" not in received:
            chunk = self.request.recv(4096)
            if not chunk or len(received) + len(chunk) > HEAD_MAX:
                return
            received += chunk
        head, body = received.split(b"\r\n\r\n", 1)
        lines = head.decode("latin-1").split("\r\n")
        method, path, _ = (lines[0].split(" ") + ["", "", ""])[:3]
        headers = {}
        for line in lines[1:]:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
        # A body, by its Content-Length.
        length = headers.get("content-length", "0")
        if not length.isdigit() or int(length) > BODY_MAX:
            return
        while len(body) < int(length):
            chunk = self.request.recv(4096)
            if not chunk:
                return
            body += chunk
        service = self.server.service
        reply = service.respond(method, path, headers, body[: int(length)])
        if self.server.chunked:
            reply = in_chunks(reply)
        self.wfile.write(reply)
        status = reply.split(b"\r\n", 1)[0].decode()
        print(
            f"fake-instance-data: {service.log}{method} {path}: {status}",
            file=sys.stderr,
            flush=True,
        )


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, address, service, chunked):
        super().__init__(address, Handler)
        self.service = service
        self.chunked = chunked


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
    parser.add_argument("--wireserver", metavar="HOST:PORT", help="be Azure's WireServer there too")
    parser.add_argument("--report", choices=["take", "refuse"], default="take")
    parser.add_argument("--chunked", action="store_true", help="send every answer in chunks")
    parser.add_argument("--list", action="store_true", help="name the built-in configs")
    args = parser.parse_args(argv)
    if args.list:
        for name, (_, line) in CONFIGS.items():
            print(f"{name}\t{line}")
        return 0
    if args.cloud is None or args.config is None:
        parser.error("--cloud and --config are needed")
    wireserver = None
    if args.wireserver is not None:
        if args.cloud != "azure":
            parser.error("--wireserver is Azure's: it needs --cloud azure")
        host, _, port = args.wireserver.rpartition(":")
        if not host or not port.isdigit():
            parser.error(f"--wireserver is HOST:PORT: {args.wireserver}")
        refuse = args.report == "refuse"
        wireserver = Server((host, int(port)), WireServer(refuse), args.chunked)
    elif args.report != "take":
        parser.error("--report needs --wireserver")
    cloud = Cloud(args.cloud, user_data(args.config))
    with Server((args.bind, args.port), cloud, args.chunked) as server:
        host, port = server.server_address[:2]
        print(f"fake-instance-data: {args.cloud} on {host}:{port}", file=sys.stderr, flush=True)
        if wireserver is not None:
            host, port = wireserver.server_address[:2]
            print(f"fake-instance-data: wireserver on {host}:{port}", file=sys.stderr, flush=True)
            threading.Thread(target=wireserver.serve_forever, daemon=True).start()
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
        finally:
            if wireserver is not None:
                wireserver.shutdown()
                wireserver.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
