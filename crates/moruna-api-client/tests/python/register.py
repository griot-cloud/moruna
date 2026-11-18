"""``moruna.register``'s last steps against the test node (work item 5.4): an image file signed
with the operator's key, uploaded to a fake object store over HTTPS, and registered through
``moruna.api``, the node checking the signature as 3E says.

Run by ``tests/clients.rs``, which starts the node and passes it in the environment:

- ``MORUNA_TEST_PYTHON_ROOT``: the repository's ``python`` directory;
- ``MORUNA_TEST_URL``, ``MORUNA_TEST_PIN``: the node and its certificate's fingerprint;
- ``MORUNA_TEST_OBSERVE``, ``MORUNA_TEST_ADMIN``: a token of each scope;
- ``MORUNA_TEST_FUNCTION_KEY``: the operator's key, node config's one ``functions.keys``
  entry's private half, PKCS#8 DER in hex, as ``openssl genpkey -algorithm ed25519`` writes it;
- ``MORUNA_TEST_FUNCTION_KEY_ID``: the ID the node gives that key.

The registration's own modules (``_register``, ``_build``, ``moruna.api``) are the standard
library and ``cryptography``, so they are imported from the source tree without the package's
``__init__``, which needs the compiled engine. The store is ``python/tests/store_support.py``.
"""

from __future__ import annotations

import base64
import hashlib
import importlib.util
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path

ROOT = Path(os.environ["MORUNA_TEST_PYTHON_ROOT"])
_package = types.ModuleType("moruna")
_package.__path__ = [str(ROOT / "moruna")]
sys.modules["moruna"] = _package

from moruna import _register, api  # noqa: E402, the package above first

_spec = importlib.util.spec_from_file_location("store_support", ROOT / "tests" / "store_support.py")
assert _spec is not None and _spec.loader is not None
store_support = importlib.util.module_from_spec(_spec)
sys.modules["store_support"] = store_support
_spec.loader.exec_module(store_support)

URL = os.environ["MORUNA_TEST_URL"]
PIN = os.environ["MORUNA_TEST_PIN"]
OBSERVE = os.environ["MORUNA_TEST_OBSERVE"]
ADMIN = os.environ["MORUNA_TEST_ADMIN"]
KEY_DER = bytes.fromhex(os.environ["MORUNA_TEST_FUNCTION_KEY"])
KEY_ID = os.environ["MORUNA_TEST_FUNCTION_KEY_ID"]
#: An Azure SAS, as a store's prefix carries it: the credential no message may show.
SAS = "sv=2026-02-01&sr=c&sp=rcw&sig=c2VjcmV0LXNhcy1zaWduYXR1cmU%3D"


def pem(der: bytes) -> bytes:
    lines = base64.encodebytes(der).replace(b"\n", b"")
    body = b"\n".join(lines[i : i + 64] for i in range(0, len(lines), 64))
    return b"-----BEGIN PRIVATE KEY-----\n" + body + b"\n-----END PRIVATE KEY-----\n"


class AgainstTheNode(unittest.TestCase):
    """Registrations over TLS to the test node, images to the fake store."""

    def setUp(self) -> None:
        self.scratch = Path(tempfile.mkdtemp(prefix="moruna-register-"))
        self.store = store_support.Store(self.scratch / "store")
        self.admin = api.Client(URL, ADMIN, pin=PIN)
        self.observe = api.Client(URL, OBSERVE, pin=PIN)

    def tearDown(self) -> None:
        self.store.close()

    def archive(self, data: bytes) -> _register.Archive:
        path = self.scratch / hashlib.sha256(data).hexdigest()
        path.write_bytes(data)
        return _register.Archive(path, hashlib.sha256(data).hexdigest(), len(data))

    def publish(self, archive: _register.Archive, name: str, key: object = None, **kw: object):
        return _register.publish(
            archive, self.admin, name=name,
            store=kw.pop("store", f"{self.store.url}/functions?{SAS}"),
            key=pem(KEY_DER) if key is None else key, context=self.store.context(), **kw,
        )  # fmt: skip

    def listed(self) -> dict[str, api.Function]:
        return {f.name: f for f in self.observe.functions_list().functions}

    def test_registers_an_image_the_node_verifies(self) -> None:
        image = self.archive(b"an image of add-one")
        function = self.publish(image, "add-one")
        put = f"/functions/add-one-{image.sha256}.tar"
        self.assertEqual(function.name, "add-one")
        self.assertEqual(function.sha256, image.sha256)
        self.assertEqual(function.key, KEY_ID)
        # The node fetches what was uploaded, the SAS with it.
        self.assertEqual(function.address, f"{self.store.url}{put}?{SAS}")
        self.assertEqual(self.store.objects[put], b"an image of add-one")
        upload = self.store.requests[-1]
        self.assertEqual((upload.method, upload.query), ("PUT", SAS))
        self.assertEqual(upload.headers["x-ms-blob-type"], "BlockBlob")
        self.assertEqual(self.listed()["add-one"], function)

    def test_every_form_of_the_key_signs_alike(self) -> None:
        image = self.archive(b"keys")
        path = self.scratch / "operator.key"
        path.write_bytes(pem(KEY_DER))
        for i, key in enumerate([KEY_DER, pem(KEY_DER).decode(), path, str(path)]):
            function = self.publish(image, f"keyed-{i}", key=key)
            self.assertEqual(function.key, KEY_ID)

    def test_a_key_node_config_does_not_hold_is_refused_by_the_node(self) -> None:
        from cryptography.hazmat.primitives import serialization  # noqa: PLC0415
        from cryptography.hazmat.primitives.asymmetric import ed25519  # noqa: PLC0415

        other = ed25519.Ed25519PrivateKey.generate().private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
        with self.assertRaises(api.ApiError) as refused:
            self.publish(self.archive(b"signed by another"), "unknown-key", key=other)
        said = refused.exception
        self.assertEqual((said.status, said.code), (400, api.ErrorCode.INVALID))
        self.assertIn("does not verify", said.message)
        self.assertNotIn("unknown-key", self.listed())

    def test_an_upload_that_fails_registers_nothing(self) -> None:
        self.store.status = 403
        with self.assertRaises(_register.Refused) as refused:
            self.publish(self.archive(b"never stored"), "unstored")
        said = str(refused.exception)
        self.assertIn("HTTP 403", said)
        self.assertIn(f"{self.store.url}/functions/unstored-", said)
        # The SAS is the store's credential: never in a message.
        self.assertNotIn("sig=", said)
        self.assertNotIn("unstored", self.listed())
        self.assertEqual(self.store.objects, {})

    def test_registering_a_name_again_replaces_it(self) -> None:
        first = self.publish(self.archive(b"version one"), "scored")
        second = self.publish(self.archive(b"version two"), "scored")
        self.assertNotEqual(first.sha256, second.sha256)
        self.assertNotEqual(first.address, second.address)
        listed = self.listed()
        self.assertEqual(listed["scored"], second)
        # Both objects stay: the store's, not the node's, to collect.
        self.assertEqual(len(self.store.objects), 2)

    def test_a_store_of_pre_signed_urls_registers_its_fetch_url(self) -> None:
        image = self.archive(b"pre-signed")
        seen: list[str] = []

        def presign(name: str) -> tuple[str, str]:
            seen.append(name)
            return (
                f"{self.store.url}/bucket/{name}?X-Amz-Signature=put",
                f"{self.store.url}/bucket/{name}?X-Amz-Signature=get",
            )

        function = self.publish(image, "presigned", store=presign)
        name = f"presigned-{image.sha256}.tar"
        self.assertEqual(seen, [name])
        self.assertEqual(function.address, f"{self.store.url}/bucket/{name}?X-Amz-Signature=get")
        self.assertEqual(self.store.requests[-1].query, "X-Amz-Signature=put")


if __name__ == "__main__":
    unittest.main()
