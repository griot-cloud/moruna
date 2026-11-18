"""``moruna.register``: a Python function registered on a node (system design 3E and 5C, work item
5.4).

Registration is four steps, each here:

1. **The image as one file** (:func:`pack`): the function's image, as ``moruna build`` makes it
   (``_build.build_images``), converted here, on the client, once, into the layer files Moruna's
   image filesystem serves, and written as a **Moruna image archive**: a small index (the
   image's manifest and config, each layer's digest, seal, offset and length), then the layer
   files at page-aligned offsets (``moruna_image::archive``, through the engine's
   ``_core.image_archive``; PM, item 3.6). A node fetches it and serves its layers in place,
   converting nothing. One image is one archive, bit for bit, and one SHA-256.
2. **The signature** (:func:`signature`): Ed25519 over the 32 bytes of that SHA-256 by the
   operator's key, one of node config's ``functions.keys``, in standard base64. The key is a
   PKCS#8 private key, PEM or DER, as ``openssl genpkey -algorithm ed25519`` writes it, and the
   signing is the ``cryptography`` package's (PM, item 5.4): the extra ``moruna[register]``.
3. **The upload** (:func:`upload`) to ``store``, object storage the node can read over HTTPS:
   either an ``https://`` prefix, which may carry a query (an Azure SAS), where the file goes to
   ``<prefix>/<name>-<sha256>.mia`` and that same URL is what the node fetches; or a callable that
   is given that object's name and returns its two URLs, the one to ``PUT`` it to and the one the
   node fetches it from (an S3 bucket's pair of pre-signed URLs).
4. **The registration** through ``moruna.api``: ``POST /v1/functions`` with the name, the
   address, the SHA-256 and the signature (``client.functions_register``).

A URL's query is never put in a message: a SAS or a pre-signed URL's query is its credential.
Only the first step needs the engine; the others are the standard library and ``cryptography``.
"""

from __future__ import annotations

import base64
import dataclasses
import os
import re
import ssl
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any

from . import _build
from .api import Function, FunctionRegistration

#: A store: an ``https://`` prefix, or a callable from an object's name to its ``PUT`` URL and
#: the URL the node fetches it from.
Store = str | Callable[[str], tuple[str, str]]

#: Seconds an upload may take, connecting included.
UPLOAD_TIMEOUT = 300.0
#: The name of a registration (``FunctionRegistration.name``): what a job names the function by.
_NAME = re.compile(r"^[a-z][a-z0-9_.-]{0,63}$")
_SUCCESS = range(200, 300)


class Refused(Exception):
    """A registration that did not reach the node, with the sentence that says why."""


@dataclasses.dataclass(frozen=True)
class Archive:
    """An image as one file: where it is, its SHA-256 in hex, and its size."""

    path: Path
    sha256: str
    size: int


#: What writes an archive: ``(layout, reference, out) -> (sha256 hex, length)``.
Writer = Callable[[str, str, str], tuple[str, int]]


def pack(built: _build.Built, work: Path, *, writer: Writer | None = None) -> Archive:
    """``built``'s image as one file under ``work``: a Moruna image archive named by the image's
    digest, written by ``writer`` (the engine's ``_core.image_archive`` when it is not given).
    Its SHA-256 is what is signed and registered."""
    if writer is None:
        from . import _core  # noqa: PLC0415, the engine converts the layers

        writer = _core.image_archive
    work.mkdir(parents=True, exist_ok=True)
    out = work / f"{built.digest.split(':', 1)[1]}.mia"
    out.unlink(missing_ok=True)
    try:
        sha256, size = writer(str(built.layout), built.digest, str(out))
    except ValueError as exc:
        raise Refused(f"{built.task}: its image cannot be made an archive: {exc}") from exc
    return Archive(out, sha256, size)


#: What the signing needs, and how it is installed.
EXTRA = "the `cryptography` package: pip install 'moruna[register]'"


def _cryptography() -> tuple[Any, Any, Any]:
    """``cryptography``'s key loading, its Ed25519 and its exceptions, or :class:`Refused`
    naming the extra that installs it."""
    try:
        from cryptography import exceptions  # noqa: PLC0415, an optional extra
        from cryptography.hazmat.primitives import serialization  # noqa: PLC0415
        from cryptography.hazmat.primitives.asymmetric import ed25519  # noqa: PLC0415
    except ImportError as exc:
        raise Refused(f"signing a function's image needs {EXTRA}") from exc
    return serialization, ed25519, exceptions


def signing_key(key: bytes | str | os.PathLike[str]) -> Any:
    """The operator's Ed25519 private key, from its PKCS#8 bytes (PEM or DER), its PEM text, or
    the path of the file that holds it, unencrypted: a ``cryptography`` ``Ed25519PrivateKey``."""
    if isinstance(key, bytes):
        data = key
    elif isinstance(key, str) and key.lstrip().startswith("-----BEGIN"):
        data = key.encode()
    else:
        try:
            data = Path(key).read_bytes()
        except OSError as exc:
            raise Refused(f"the key {os.fspath(key)!r} cannot be read: {exc.strerror}") from exc
    serialization, ed25519, exceptions = _cryptography()
    pem = data.lstrip().startswith(b"-----BEGIN")
    load = serialization.load_pem_private_key if pem else serialization.load_der_private_key
    try:
        loaded = load(data, password=None)
    except TypeError as exc:
        raise Refused("the key is encrypted; register takes an unencrypted PKCS#8 key") from exc
    except (ValueError, exceptions.UnsupportedAlgorithm) as exc:
        raise Refused(
            "the key is not a private key in PKCS#8, PEM or DER (openssl genpkey -algorithm "
            "ed25519 writes one)"
        ) from exc
    if not isinstance(loaded, ed25519.Ed25519PrivateKey):
        raise Refused("the key is not an Ed25519 key; node config's functions.keys are Ed25519")
    return loaded


def signature(sha256: str, key: Any) -> str:
    """3E's signature of an image whose SHA-256 is ``sha256`` (hex): Ed25519 over its 32 bytes by
    ``key`` (:func:`signing_key`), in standard base64."""
    return base64.b64encode(key.sign(bytes.fromhex(sha256))).decode()


def shown(url: str) -> str:
    """``url`` as a message may show it: without its query, which may be its credential."""
    parts = urllib.parse.urlsplit(url)
    return urllib.parse.urlunsplit((parts.scheme, parts.netloc, parts.path, "", ""))


def _https(url: str, what: str) -> str:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme != "https" or not parts.hostname:
        raise Refused(
            f"{what} {shown(url)!r} is not an https:// URL; the node fetches over HTTPS only"
        )
    if parts.username is not None or parts.password is not None:
        raise Refused(f"{what} {shown(url)!r} carries a user or a password; a node takes none")
    return url


def object_name(name: str, sha256: str) -> str:
    """The object an image is uploaded as: the registration's name and its SHA-256, so that one
    image of one function is one object, and a changed one is another."""
    return f"{name}-{sha256}.mia"


def destination(store: Store, name: str, sha256: str) -> tuple[str, str]:
    """Where the image goes: the URL to ``PUT`` it to and the address the node fetches it from."""
    obj = object_name(name, sha256)
    if callable(store):
        try:
            put, address = store(obj)
        except (TypeError, ValueError) as exc:
            raise Refused(f"the store gave no pair of URLs for {obj}: {exc}") from exc
        return _https(put, "the store's upload URL"), _https(address, "the store's address")
    if not isinstance(store, str):
        raise Refused("the store is an https:// prefix or a callable that gives an object's URLs")
    parts = urllib.parse.urlsplit(_https(store, "the store"))
    if parts.fragment:
        raise Refused(f"the store {shown(store)!r} has a fragment; a prefix has none")
    path = parts.path.rstrip("/") + "/" + urllib.parse.quote(obj)
    url = urllib.parse.urlunsplit((parts.scheme, parts.netloc, path, parts.query, ""))
    return url, url


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """An upload is never redirected: the redirect would carry the image, and the URL's
    credential, elsewhere."""

    def redirect_request(self, *args: Any, **kwargs: Any) -> None:
        return None


def upload(archive: Archive, url: str, *, context: ssl.SSLContext | None = None) -> None:
    """``PUT`` ``archive`` to ``url``: with ``x-ms-blob-type: BlockBlob``, which Azure Blob needs
    and S3 and others ignore. Anything but a 2xx answer is :class:`Refused`."""
    handler = urllib.request.HTTPSHandler(context=context or ssl.create_default_context())
    opener = urllib.request.build_opener(handler, _NoRedirect())
    with archive.path.open("rb") as body:
        request = urllib.request.Request(url, data=body, method="PUT")
        request.add_header("Content-Type", "application/octet-stream")
        request.add_header("Content-Length", str(archive.size))
        request.add_header("x-ms-blob-type", "BlockBlob")
        try:
            with opener.open(request, timeout=UPLOAD_TIMEOUT) as answer:
                status = answer.status
        except urllib.error.HTTPError as error:
            with error:
                status = error.code
        except (urllib.error.URLError, OSError) as error:
            reason = getattr(error, "reason", error)
            raise Refused(f"the upload to {shown(url)} failed: {reason}") from error
    if status not in _SUCCESS:
        raise Refused(f"the upload to {shown(url)} was refused: HTTP {status}")


def publish(  # noqa: PLR0913, keyword-only past the archive and the client
    archive: Archive,
    client: Any,
    *,
    name: str,
    store: Store,
    key: bytes | str | os.PathLike[str],
    context: ssl.SSLContext | None = None,
) -> Function:
    """``archive`` signed with ``key``, uploaded to ``store`` and registered as ``name`` through
    ``client`` (a ``moruna.api.Client`` with an admin token): the registration the node answers.
    Nothing is uploaded when the name or the key is refused, and nothing registered when the
    upload fails. The node's own refusal is ``moruna.api.ApiError``."""
    if not _NAME.match(name):
        raise Refused(
            f"{name!r} is not a function's name: 1 to 64 characters, a lowercase letter first, "
            "then lowercase letters, digits, `_`, `-` and `.`"
        )
    signed = signature(archive.sha256, signing_key(key))
    put, address = destination(store, name, archive.sha256)
    upload(archive, put, context=context)
    return client.functions_register(
        FunctionRegistration(name=name, address=address, sha256=archive.sha256, signature=signed)
    )


def register_built(  # noqa: PLR0913, publish's and where to pack
    built: _build.Built,
    client: Any,
    *,
    name: str,
    store: Store,
    key: bytes | str | os.PathLike[str],
    work: Path,
    context: ssl.SSLContext | None = None,
    writer: Writer | None = None,
) -> Function:
    """``built``'s image packed into ``work`` (by ``writer``, as :func:`pack` takes it) and
    published as ``name``."""
    archive = pack(built, work, writer=writer)
    return publish(archive, client, name=name, store=store, key=key, context=context)
