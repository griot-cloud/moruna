"""``moruna.register``'s steps with no engine built (work item 5.4, system design 3E and 5C): the
image packed into one file, the signature, the store's URLs, the upload and the registration.

The build is the builder's own, over test_build.py's project (hand-made wheels, a stand-in
runtime, no network). The store is a fake over HTTPS (``store_support``), and the node a client
that keeps what it is asked to register. The registration against the API's test node, which
checks the signature as the node does, is ``crates/moruna-api-client/tests/python/register.py``.
"""

from __future__ import annotations

import base64
import hashlib
import json
import sys
import tarfile
import types
from pathlib import Path
from typing import Any

import builder_support as support
import pytest
import store_support
from test_build import X86_64, Project, project  # noqa: F401, the fixture

try:  # `moruna.api`, which `_register` imports, without the engine when it is not built
    import moruna.api  # noqa: F401
except ImportError:
    _stub = types.ModuleType("moruna")
    _stub.__path__ = [str(support.PACKAGE)]
    sys.modules["moruna"] = _stub

serialization = pytest.importorskip("cryptography.hazmat.primitives.serialization")
ed25519 = pytest.importorskip("cryptography.hazmat.primitives.asymmetric.ed25519")
ec = pytest.importorskip("cryptography.hazmat.primitives.asymmetric.ec")

build = support.load("_build.py", "moruna_build")
register = support.load("_register.py", "moruna_register")

#: RFC 8032, section 7.1, TEST 1 to 3: the seed, its public key, the message, the signature.
RFC8032 = [
    (
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ),
    (
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ),
    (
        "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ),
]

SAS = "sv=2026-02-01&sr=c&sp=rcw&sig=c2VjcmV0%3D"


def pkcs8(key: Any, encoding: Any = None, encryption: Any = None) -> bytes:
    return key.private_bytes(
        encoding or serialization.Encoding.PEM,
        serialization.PrivateFormat.PKCS8,
        encryption or serialization.NoEncryption(),
    )


def raw(public: Any) -> bytes:
    return public.public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)


@pytest.fixture
def operator() -> Any:
    """The operator's key."""
    return ed25519.Ed25519PrivateKey.generate()


@pytest.fixture
def store(tmp_path: Path) -> Any:
    made = store_support.Store(tmp_path / "store")
    yield made
    made.close()


class Node:
    """A node that keeps what it is asked to register, as ``moruna.api.Client`` would send it."""

    def __init__(self) -> None:
        self.registered: list[Any] = []

    def functions_register(self, registration: Any) -> Any:
        self.registered.append(registration)
        return registration


def archive(tmp_path: Path, data: bytes) -> Any:
    path = tmp_path / "image.tar"
    path.write_bytes(data)
    return register.Archive(path, hashlib.sha256(data).hexdigest(), len(data))


# The signature --------------------------------------------------------------------------------


@pytest.mark.parametrize(("seed", "public", "message", "signature"), RFC8032)
def test_rfc8032_s_vectors_sign_through_the_key_as_the_operator_holds_it(
    seed: str, public: str, message: str, signature: str
) -> None:
    held = ed25519.Ed25519PrivateKey.from_private_bytes(bytes.fromhex(seed))
    for form in (pkcs8(held), pkcs8(held, serialization.Encoding.DER), pkcs8(held).decode()):
        key = register.signing_key(form)
        assert raw(key.public_key()).hex() == public
        assert key.sign(bytes.fromhex(message)).hex() == signature


def test_the_signature_is_over_the_32_bytes_of_the_sha256_in_base64(operator: Any) -> None:
    digest = hashlib.sha256(b"an image").hexdigest()
    signed = base64.b64decode(register.signature(digest, operator), validate=True)
    operator.public_key().verify(signed, bytes.fromhex(digest))
    assert len(signed) == 64


def test_the_key_is_read_from_its_file(tmp_path: Path, operator: Any) -> None:
    path = tmp_path / "operator.key"
    path.write_bytes(pkcs8(operator))
    for given in (path, str(path)):
        assert raw(register.signing_key(given).public_key()) == raw(operator.public_key())


def test_a_key_that_is_not_an_unencrypted_ed25519_pkcs8_key_is_refused(tmp_path: Path) -> None:
    ed = ed25519.Ed25519PrivateKey.generate()
    for key, said in [
        (pkcs8(ed, encryption=serialization.BestAvailableEncryption(b"pw")), "encrypted"),
        (pkcs8(ec.generate_private_key(ec.SECP256R1())), "not an Ed25519 key"),
        (b"not a key", "not a private key in PKCS#8"),
        (b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n", "not a private key"),
        (tmp_path / "missing.key", "cannot be read"),
    ]:
        with pytest.raises(register.Refused, match=said):
            register.signing_key(key)


def test_signing_without_cryptography_names_the_extra(
    monkeypatch: pytest.MonkeyPatch, operator: Any
) -> None:
    for name in ("cryptography", "cryptography.hazmat.primitives"):
        monkeypatch.setitem(sys.modules, name, None)
    with pytest.raises(register.Refused, match=r"moruna\[register\]"):
        register.signing_key(pkcs8(operator))


# The store ------------------------------------------------------------------------------------


def test_a_prefix_s_object_is_named_by_the_function_and_its_sha256_and_keeps_its_query() -> None:
    sha = "ab" * 32
    want = f"https://acct.blob.core.windows.net/functions/add-one-{sha}.tar?{SAS}"
    for prefix in (
        f"https://acct.blob.core.windows.net/functions?{SAS}",
        f"https://acct.blob.core.windows.net/functions/?{SAS}",
    ):
        assert register.destination(prefix, "add-one", sha) == (want, want)
    bare = register.destination("https://store.example", "add-one", sha)
    assert bare == (f"https://store.example/add-one-{sha}.tar",) * 2


def test_a_callable_store_gives_its_two_urls() -> None:
    names: list[str] = []

    def presign(name: str) -> tuple[str, str]:
        names.append(name)
        return f"https://s3.example/b/{name}?put", f"https://s3.example/b/{name}?get"

    put, address = register.destination(presign, "f", "cd" * 32)
    assert names == [f"f-{'cd' * 32}.tar"]
    assert (put, address) == (
        f"https://s3.example/b/{names[0]}?put",
        f"https://s3.example/b/{names[0]}?get",
    )


def test_a_store_the_node_cannot_fetch_from_is_refused() -> None:
    sha = "ef" * 32
    for store, said in [
        ("http://store.example/f", "not an https:// URL"),
        ("s3://bucket/f", "not an https:// URL"),
        ("https://user:pw@store.example/f", "user or a password"),
        ("https://store.example/f#part", "fragment"),
        (lambda name: ("https://store.example/put", "http://store.example/get"), "address"),
        (lambda name: "https://one-url-only", "no pair of URLs"),
        (42, "https:// prefix or a callable"),
    ]:
        with pytest.raises(register.Refused, match=said):
            register.destination(store, "f", sha)  # type: ignore[arg-type]


def test_a_url_is_shown_without_its_query() -> None:
    assert register.shown(f"https://a.example/f.tar?{SAS}") == "https://a.example/f.tar"


# The upload -----------------------------------------------------------------------------------


def test_an_upload_puts_the_file_as_a_block_blob(tmp_path: Path, store: Any) -> None:
    image = archive(tmp_path, b"bytes of an image")
    register.upload(image, f"{store.url}/f/x.tar?{SAS}", context=store.context())
    assert store.objects == {"/f/x.tar": b"bytes of an image"}
    put = store.requests[-1]
    assert (put.method, put.query) == ("PUT", SAS)
    assert put.headers["x-ms-blob-type"] == "BlockBlob"
    assert put.headers["content-type"] == "application/x-tar"
    assert put.headers["content-length"] == str(image.size)


def test_an_upload_that_is_refused_says_so_without_the_query(tmp_path: Path, store: Any) -> None:
    image = archive(tmp_path, b"refused")
    for status in (403, 500):
        store.status = status
        with pytest.raises(register.Refused, match=f"HTTP {status}") as refused:
            register.upload(image, f"{store.url}/f/x.tar?{SAS}", context=store.context())
        assert "sig=" not in str(refused.value)
    assert store.objects == {}


def test_an_upload_is_never_redirected(tmp_path: Path, store: Any) -> None:
    store.redirect = f"{store.url}/elsewhere"
    with pytest.raises(register.Refused, match="HTTP 307"):
        register.upload(archive(tmp_path, b"x"), f"{store.url}/f/x.tar", context=store.context())
    assert [r.method for r in store.requests] == ["PUT"]


def test_an_upload_to_a_store_that_is_not_there_or_not_trusted_fails(
    tmp_path: Path, store: Any
) -> None:
    image = archive(tmp_path, b"x")
    # A certificate the client does not trust: the default context has never seen this one.
    with pytest.raises(register.Refused, match="failed") as untrusted:
        register.upload(image, f"{store.url}/f/x.tar?{SAS}")
    assert "sig=" not in str(untrusted.value)
    url = store.url
    store.close()
    with pytest.raises(register.Refused, match="failed"):
        register.upload(image, f"{url}/f/x.tar", context=store.context())


# Publishing -----------------------------------------------------------------------------------


def test_publish_signs_uploads_and_registers(tmp_path: Path, store: Any, operator: Any) -> None:
    image = archive(tmp_path, b"an image of add-one")
    node = Node()
    answered = register.publish(
        image, node, name="add-one", store=f"{store.url}/functions?{SAS}", key=pkcs8(operator),
        context=store.context(),
    )  # fmt: skip
    (registration,) = node.registered
    assert answered is registration
    object_path = f"/functions/add-one-{image.sha256}.tar"
    assert registration.name == "add-one"
    assert registration.sha256 == image.sha256
    assert registration.address == f"{store.url}{object_path}?{SAS}"
    operator.public_key().verify(
        base64.b64decode(registration.signature), bytes.fromhex(image.sha256)
    )
    assert store.objects[object_path] == b"an image of add-one"


def test_nothing_is_uploaded_when_the_name_or_the_key_is_refused(
    tmp_path: Path, store: Any, operator: Any
) -> None:
    image = archive(tmp_path, b"x")
    node = Node()
    for name, key, said in [
        ("Add-One", pkcs8(operator), "not a function's name"),
        ("x" * 65, pkcs8(operator), "not a function's name"),
        ("add-one", b"not a key", "not a private key"),
    ]:
        with pytest.raises(register.Refused, match=said):
            register.publish(
                image, node, name=name, store=f"{store.url}/f", key=key, context=store.context()
            )
    assert store.requests == []
    assert node.registered == []


def test_nothing_is_registered_when_the_upload_fails(
    tmp_path: Path, store: Any, operator: Any
) -> None:
    store.status = 403
    node = Node()
    with pytest.raises(register.Refused, match="HTTP 403"):
        register.publish(
            archive(tmp_path, b"x"), node, name="f", store=f"{store.url}/f",
            key=pkcs8(operator), context=store.context(),
        )  # fmt: skip
    assert node.registered == []


# The image as one file ------------------------------------------------------------------------


def members(path: Path) -> dict[str, bytes]:
    with tarfile.open(path) as archive_file:
        return {
            m.name: archive_file.extractfile(m).read()  # type: ignore[union-attr]
            for m in archive_file.getmembers()
            if m.isfile()
        }


def test_an_image_is_one_file_an_oci_layout_of_it_alone(project: Project, tmp_path: Path) -> None:  # noqa: F811
    out = tmp_path / "images"
    first = project.build(project.task("kernel_a", platform=X86_64), out=out, platform=X86_64)
    project.build(project.task("kernel_b", platform=X86_64), out=out, platform=X86_64)
    packed = register.pack(first, tmp_path / "work")
    assert packed.sha256 == hashlib.sha256(packed.path.read_bytes()).hexdigest()
    assert packed.size == packed.path.stat().st_size
    files = members(packed.path)
    assert json.loads(files["oci-layout"]) == {"imageLayoutVersion": "1.0.0"}
    index = json.loads(files["index.json"])
    (manifest_entry,) = index["manifests"]
    assert manifest_entry["digest"] == first.digest
    assert manifest_entry["annotations"]["org.opencontainers.image.ref.name"] == first.key
    manifest = json.loads(files[f"blobs/sha256/{first.digest.split(':')[1]}"])
    layers = [layer["digest"] for layer in manifest["layers"]]
    wanted = {first.digest, manifest["config"]["digest"], *layers}
    blobs = {name.split("/")[-1] for name in files if name.startswith("blobs/")}
    # Its own blobs only, though the layout holds kernel_b's image too.
    assert blobs == {digest.split(":")[1] for digest in wanted}
    for name, data in files.items():
        if name.startswith("blobs/"):
            assert hashlib.sha256(data).hexdigest() == name.split("/")[-1]
    # Every entry as the reproducible tar writes it.
    with tarfile.open(packed.path) as archive_file:
        for member in archive_file.getmembers():
            assert (member.mtime, member.uid, member.gid) == (0, 0, 0)


def test_one_image_packs_to_one_file_bit_for_bit(project: Project, tmp_path: Path) -> None:  # noqa: F811
    task = project.task("kernel_a", platform=X86_64)
    built = project.build(task, out=tmp_path / "images", platform=X86_64)
    again = project.build(task, out=tmp_path / "images", platform=X86_64)
    assert again.reused
    one = register.pack(built, tmp_path / "one")
    two = register.pack(again, tmp_path / "two")
    assert (one.sha256, one.size) == (two.sha256, two.size)
    assert one.path.read_bytes() == two.path.read_bytes()


def test_a_layout_missing_a_blob_or_its_manifest_is_refused(
    project: Project,  # noqa: F811
    tmp_path: Path,
) -> None:
    built = project.build(
        project.task("kernel_a", platform=X86_64), out=tmp_path / "images", platform=X86_64
    )
    manifest = json.loads(
        (built.layout / "blobs" / "sha256" / built.digest.split(":")[1]).read_bytes()
    )
    layer = manifest["layers"][-1]["digest"].split(":")[1]
    (built.layout / "blobs" / "sha256" / layer).unlink()
    with pytest.raises(register.Refused, match="has no blob"):
        register.pack(built, tmp_path / "work")
    (built.layout / "blobs" / "sha256" / built.digest.split(":")[1]).unlink()
    with pytest.raises(register.Refused, match="manifest cannot be read"):
        register.pack(built, tmp_path / "work")


def test_register_built_publishes_the_packed_image(
    project: Project,  # noqa: F811
    tmp_path: Path,
    store: Any,
    operator: Any,
) -> None:
    built = project.build(
        project.task("kernel_a", platform=X86_64), out=tmp_path / "images", platform=X86_64
    )
    node = Node()
    register.register_built(
        built, node, name="kernel-a", store=f"{store.url}/f", key=pkcs8(operator),
        work=tmp_path / "work", context=store.context(),
    )  # fmt: skip
    (registration,) = node.registered
    uploaded = store.objects[f"/f/kernel-a-{registration.sha256}.tar"]
    assert hashlib.sha256(uploaded).hexdigest() == registration.sha256
    saved = tmp_path / "uploaded.tar"
    saved.write_bytes(uploaded)
    index = json.loads(members(saved)["index.json"])
    assert [m["digest"] for m in index["manifests"]] == [built.digest]
