"""A client of a Moruna node's API (system design 2E), on the standard library only.

The types and operations are generated from the API's OpenAPI document into ``_generated``
(``tools/api/gen_python.py``); this module is the client they are called through. Each operation
is both a function and a method of the client, generated alike::

    from moruna import api

    client = api.Client("https://203.0.113.7", token, pin="9f86d0...0a08")
    node = client.node()             # GET /v1/node, an observe token; api.node(client) too
    audit = client.node_audit()      # GET /v1/node/audit, an admin token

A job (system design 3B) is its document, as version 1 JSON or version 2's FlatBuffers in
base64; the node runs one at a time::

    job = api.Job(v1={"moruna_spec": 1, ...})
    if client.jobs_validate(job).ok:                 # a submit token
        run = client.jobs_submit(job).id
        for event in client.jobs_events(run):        # Server-Sent Events, as they arrive
            print(event.event, event.data)
        report = client.jobs_report(run)

A function (system design 3E) is registered by name, with its image's address, the image's
SHA-256 and an Ed25519 signature over that SHA-256 by one of node config's ``functions.keys``::

    client.functions_register(api.FunctionRegistration(
        name="gate-score", address="https://...", sha256="<64 hex digits>",
        signature="<base64>"))                   # an admin token
    client.functions_list()                       # an observe token

**Pinning.** With ``pin``, the SHA-256 of the server's certificate in hex (what the node prints
on its console as ``moruna: tls sha256=<hex>``), the client connects without verifying the
certificate, then refuses the connection, before anything is sent, unless the certificate's
SHA-256 equals the pin. Without it, the certificate is verified as usual, against the system's
trusted roots, and must name the host.
"""

from __future__ import annotations

import hashlib
import http.client
import json
import ssl
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Iterator
from typing import Any

from moruna.api._generated import (
    Audit,
    AuditRecord,
    Caller,
    Cloud,
    ErrorBody,
    ErrorCode,
    ErrorDetail,
    Executor,
    ExecutorKind,
    Function,
    FunctionRegistration,
    Functions,
    Health,
    HealthStatus,
    Heartbeat,
    Job,
    Jobs,
    JobState,
    JobStatus,
    JobSummary,
    Node,
    NodeHealth,
    Operations,
    Refusal,
    Scope,
    ServerSentEvent,
    Submitted,
    Validation,
    functions_list,
    functions_register,
    health,
    jobs_cancel,
    jobs_events,
    jobs_get,
    jobs_list,
    jobs_report,
    jobs_submit,
    jobs_validate,
    node,
    node_audit,
)

__all__ = [
    "ApiError",
    "Audit",
    "AuditRecord",
    "Caller",
    "Client",
    "Cloud",
    "Error",
    "ErrorBody",
    "ErrorCode",
    "ErrorDetail",
    "Executor",
    "ExecutorKind",
    "Function",
    "FunctionRegistration",
    "Functions",
    "Health",
    "HealthStatus",
    "Heartbeat",
    "Job",
    "JobState",
    "JobStatus",
    "JobSummary",
    "Jobs",
    "Node",
    "NodeHealth",
    "Operations",
    "PinMismatch",
    "Refusal",
    "Scope",
    "ServerSentEvent",
    "Submitted",
    "Validation",
    "functions_list",
    "functions_register",
    "health",
    "jobs_cancel",
    "jobs_events",
    "jobs_get",
    "jobs_list",
    "jobs_report",
    "jobs_submit",
    "jobs_validate",
    "node",
    "node_audit",
]

#: Seconds a call may take, connecting included: what the Rust client allows.
TIMEOUT = 30.0
#: The statuses of a successful answer.
_SUCCESS = range(200, 300)
#: The hexadecimal digits of a pin, a SHA-256.
_PIN_DIGITS = 64


class Error(Exception):
    """A call to the node failed."""


class ApiError(Error):
    """The node answered with an error: its HTTP status, its code and its message."""

    def __init__(self, operation: str, status: int, code: ErrorCode | None, message: str) -> None:
        self.operation = operation
        self.status = status
        #: ``None`` when the answer carried no error body.
        self.code = code
        self.message = message
        said = f"{code.value} ({status}): {message}" if code else f"status {status}: {message}"
        super().__init__(f"the node refused {operation}: {said}")


class PinMismatch(Error):
    """The server's certificate is not the pinned one. Nothing was sent to it."""

    def __init__(self, pin: str, found: str) -> None:
        self.pin = pin
        self.found = found
        super().__init__(f"the server's certificate has SHA-256 {found}, not the pinned {pin}")


class Client(Operations):
    """A client of one node's API: ``Client(url, token, pin=None)``.

    ``url`` is ``https://<host>[:<port>]``. ``token`` is sent, as ``Authorization: Bearer``, to
    every operation that needs one, and to no other. ``pin`` is the SHA-256 of the server's
    certificate in hex; see the module's documentation. The operations' methods
    (``client.node()`` and the others) are generated, in ``Operations``.
    """

    def __init__(self, url: str, token: str, pin: str | None = None) -> None:
        parts = urllib.parse.urlsplit(url)
        if parts.scheme != "https" or not parts.hostname:
            raise ValueError(f"{url!r}: a node's API is served over HTTPS only (https://<host>)")
        # The host and port only: a path, a query, a user or a password has no place here.
        host = f"[{parts.hostname}]" if ":" in parts.hostname else parts.hostname
        self.url = f"https://{host}" + (f":{parts.port}" if parts.port is not None else "")
        self._token = token
        if pin is None:
            handler: urllib.request.HTTPSHandler = urllib.request.HTTPSHandler(
                context=ssl.create_default_context()
            )
        else:
            handler = _PinnedHandler(_fingerprint(pin))
        # Never a redirect: the API has none, and one would carry the token elsewhere.
        self._opener = urllib.request.build_opener(handler, _NoRedirect())

    def __repr__(self) -> str:
        return f"Client({self.url!r})"

    def call(self, operation: str, method: str, path: str, *, token: bool, body: Any = None) -> Any:
        """Call ``method path``, with the token when ``token`` and ``body`` as its JSON body when
        it has one, and return its JSON answer.

        The generated operation functions (``node(client)`` and the others) call this.
        """
        request = self._request(method, path, token=token, body=body)
        try:
            with self._opener.open(request, timeout=TIMEOUT) as response:
                return _answer(operation, response.status, response.read())
        except urllib.error.HTTPError as error:
            with error:
                return _answer(operation, error.code, error.read())
        except urllib.error.URLError as error:
            raise Error(f"could not call {operation} at {self.url}: {error.reason}") from error
        except (OSError, http.client.HTTPException) as error:
            raise Error(f"could not call {operation} at {self.url}: {error}") from error

    def stream(self, operation: str, path: str, *, token: bool) -> Iterator[ServerSentEvent]:
        """Call ``GET path`` and return its answer's Server-Sent Events as they arrive, each
        one's data read as JSON; the stream ends when the node closes it.

        The call is made, and a refusal raised, here; the events are read as the iterator is.
        The generated ``jobs_events(client, id)`` calls this.
        """
        request = self._request("GET", path, token=token, body=None)
        request.add_header("Accept", "text/event-stream")
        try:
            response = self._opener.open(request, timeout=TIMEOUT)
        except urllib.error.HTTPError as error:
            with error:
                _answer(operation, error.code, error.read())
            raise Error(f"the node's answer to {operation} is not a stream") from error
        except urllib.error.URLError as error:
            raise Error(f"could not call {operation} at {self.url}: {error.reason}") from error
        except (OSError, http.client.HTTPException) as error:
            raise Error(f"could not call {operation} at {self.url}: {error}") from error
        return _events(operation, response)

    def _request(self, method: str, path: str, *, token: bool, body: Any) -> urllib.request.Request:
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.url + path, data=data, method=method)
        if data is not None:
            request.add_header("Content-Type", "application/json")
        if token:
            request.add_header("Authorization", f"Bearer {self._token}")
        return request


def _events(operation: str, response: http.client.HTTPResponse) -> Iterator[ServerSentEvent]:
    """The Server-Sent Events of ``response``, one at a time, until the stream ends."""
    with response:
        identity: int | None = None
        name = ""
        data: Any = None
        try:
            for raw in response:
                line = raw.decode().rstrip("\r\n")
                if not line:
                    if data is not None:
                        yield ServerSentEvent(identity, name, data)
                    identity, name, data = None, "", None
                elif line.startswith("id: "):
                    identity = int(line[4:])
                elif line.startswith("event: "):
                    name = line[7:]
                elif line.startswith("data: "):
                    data = json.loads(line[6:])
        except ValueError as error:
            raise Error(f"the node's stream of {operation} is not its events: {error}") from None
        except (OSError, http.client.HTTPException) as error:
            raise Error(f"the stream of {operation} failed: {error}") from error


def _answer(operation: str, status: int, body: bytes) -> Any:
    """An answer's JSON value on success; otherwise the error it carries, raised."""
    if status in _SUCCESS:
        try:
            return json.loads(body)
        except ValueError as error:
            raise Error(f"the node's answer to {operation} is not JSON: {error}") from None
    try:
        detail = ErrorBody.from_json(json.loads(body)).error
    except ValueError:
        raise ApiError(operation, status, None, "the answer carries no error body") from None
    raise ApiError(operation, status, detail.code, detail.message)


def _fingerprint(pin: str) -> bytes:
    """A pin's 32 bytes, from 64 hexadecimal digits in either case."""
    if len(pin) != _PIN_DIGITS or any(c not in "0123456789abcdefABCDEF" for c in pin):
        raise ValueError(f"the pin {pin!r} is not a SHA-256 fingerprint: 64 hexadecimal digits")
    return bytes.fromhex(pin)


class _PinnedConnection(http.client.HTTPSConnection):
    """An HTTPS connection that refuses, before it sends anything, a server whose certificate
    is not the pinned one."""

    def __init__(self, host: str, *, fingerprint: bytes, **kwargs: Any) -> None:
        super().__init__(host, **kwargs)
        self._fingerprint = fingerprint

    def connect(self) -> None:
        super().connect()
        sock = self.sock
        der = sock.getpeercert(binary_form=True) if isinstance(sock, ssl.SSLSocket) else None
        found = hashlib.sha256(der).hexdigest() if der else "(no certificate)"
        if found != self._fingerprint.hex():
            self.close()
            raise PinMismatch(self._fingerprint.hex(), found)


class _PinnedHandler(urllib.request.HTTPSHandler):
    """Opens HTTPS URLs over a :class:`_PinnedConnection`: TLS without verification, then the
    pin."""

    def __init__(self, fingerprint: bytes) -> None:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        super().__init__(context=context)
        self._pinned_context = context
        self._fingerprint = fingerprint

    def https_open(self, req: urllib.request.Request) -> http.client.HTTPResponse:
        fingerprint = self._fingerprint

        def connection(host: str, **kwargs: Any) -> _PinnedConnection:
            return _PinnedConnection(host, fingerprint=fingerprint, **kwargs)

        return self.do_open(connection, req, context=self._pinned_context)


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Follows no redirect: a 3xx answer is the answer."""

    def redirect_request(self, *args: Any, **kwargs: Any) -> None:
        return None
