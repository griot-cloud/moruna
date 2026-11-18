"""A fake object store over HTTPS on 127.0.0.1, for ``moruna.register``'s tests (work item 5.4).

``PUT`` keeps an object by its path and answers 201; ``GET`` gives it back. Every request is
recorded, its path, query and headers (their names in lower case), and :attr:`Store.status`
makes the next uploads fail
with that status. Its certificate is made at start with the ``cryptography`` package, for
``127.0.0.1``; :meth:`Store.context` trusts it, and nothing else does.
"""

from __future__ import annotations

import dataclasses
import datetime
import http.server
import ipaddress
import ssl
import threading
import urllib.parse
from pathlib import Path


@dataclasses.dataclass(frozen=True)
class Request:
    method: str
    path: str
    query: str
    headers: dict[str, str]


def certificate(directory: Path) -> tuple[Path, Path]:
    """A self-signed certificate for ``127.0.0.1`` and its key, as PEM files in ``directory``."""
    from cryptography import x509  # noqa: PLC0415, the tests' own dependency
    from cryptography.hazmat.primitives import hashes, serialization  # noqa: PLC0415
    from cryptography.hazmat.primitives.asymmetric import ec  # noqa: PLC0415
    from cryptography.x509.oid import NameOID  # noqa: PLC0415

    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "moruna-test-store")])
    now = datetime.datetime.now(datetime.UTC)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(hours=1))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .add_extension(
            x509.SubjectAlternativeName([x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]),
            critical=False,
        )
        .sign(key, hashes.SHA256())
    )
    directory.mkdir(parents=True, exist_ok=True)
    cert_path, key_path = directory / "store.pem", directory / "store.key"
    cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    key_path.write_bytes(
        key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
    )
    return cert_path, key_path


class Store:
    """The store: :attr:`url` is ``https://127.0.0.1:<port>``."""

    def __init__(self, directory: Path) -> None:
        self.cert, key = certificate(directory)
        self.objects: dict[str, bytes] = {}
        self.requests: list[Request] = []
        #: The status the next uploads are answered with; 201 keeps them.
        self.status = 201
        #: Where uploads are redirected to, when set.
        self.redirect: str | None = None
        store = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args: object) -> None:
                pass

            def _record(self) -> urllib.parse.SplitResult:
                parts = urllib.parse.urlsplit(self.path)
                headers = {name.lower(): value for name, value in self.headers.items()}
                store.requests.append(Request(self.command, parts.path, parts.query, headers))
                return parts

            def do_PUT(self) -> None:  # noqa: N802, http.server's name
                parts = self._record()
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                if store.redirect is not None:
                    self.send_response(307)
                    self.send_header("Location", store.redirect)
                elif store.status == 201:
                    store.objects[parts.path] = body
                    self.send_response(201)
                else:
                    self.send_response(store.status)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def do_GET(self) -> None:  # noqa: N802, http.server's name
                parts = self._record()
                body = store.objects.get(parts.path)
                self.send_response(200 if body is not None else 404)
                self.send_header("Content-Length", str(len(body or b"")))
                self.end_headers()
                self.wfile.write(body or b"")

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(self.cert, key)
        self.server.socket = context.wrap_socket(self.server.socket, server_side=True)
        self.url = f"https://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def context(self) -> ssl.SSLContext:
        """A client context that trusts this store's certificate, and only it."""
        return ssl.create_default_context(cafile=str(self.cert))

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
