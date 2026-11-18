"""``moruna.register`` over the engine's own objects (work item 5.4): a decorated function and a
``moruna.Function`` subclass built for the node's platform, packed, signed, uploaded to a fake
store and registered with a node that keeps what it is asked, with a tiny runtime layout and
hand-made wheels (no network), as test_build_api.py builds a job. The steps themselves are
test_register.py's; this is the wrapper that reaches them from a function.
"""

from __future__ import annotations

import base64
import hashlib
import json
import sys
import textwrap
from pathlib import Path
from typing import Any

import builder_support as support
import pytest
import store_support

import moruna

serialization = pytest.importorskip("cryptography.hazmat.primitives.serialization")
ed25519 = pytest.importorskip("cryptography.hazmat.primitives.asymmetric.ed25519")

PLATFORM = "linux/x86_64"
#: The page every layer file of an archive starts on.
PAGE = 4096

CODE = '''
import pyarrow as pa

import moruna


@moruna.function
def add_one(batch):
    return batch


class Scored(moruna.Function):
    """Scores rows."""

    def __call__(self, batch):
        return batch
'''


class Node:
    """A node that keeps what it is asked to register."""

    def __init__(self) -> None:
        self.registered: list[Any] = []

    def functions_register(self, registration: Any) -> Any:
        self.registered.append(registration)
        return registration


@pytest.fixture
def project(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    root = tmp_path / "project"
    (root / "scoring").mkdir(parents=True)
    (root / "scoring" / "__init__.py").write_text(textwrap.dedent(CODE))
    wheels = tmp_path / "wheels"
    pyarrow = support.platform_wheels(wheels, "pyarrow", "25.0.1")
    (root / "requirements.lock").write_text(support.lock_line("pyarrow", "25.0.1", pyarrow))
    runtime = support.make_runtime(tmp_path / "runtime", PLATFORM)
    monkeypatch.syspath_prepend(str(root))
    monkeypatch.delitem(sys.modules, "scoring", raising=False)
    import scoring  # noqa: PLC0415

    store = store_support.Store(tmp_path / "store")
    # `moruna.register` takes the system's trusted roots; here they are the store's certificate.
    monkeypatch.setenv("SSL_CERT_FILE", str(store.cert))
    yield root, wheels, runtime, scoring, store
    store.close()


def options(wheels: Path, runtime: Path, tmp_path: Path) -> dict[str, Any]:
    return {
        "runtime": (str(runtime), "python-test"),
        "no_index": True,
        "find_links": [str(wheels)],
        "cache": str(tmp_path / "cache"),
        "out": str(tmp_path / "images"),
    }


def test_a_decorated_function_is_registered_by_its_name(project, tmp_path: Path) -> None:
    _, wheels, runtime, scoring, store = project
    key = ed25519.Ed25519PrivateKey.generate()
    pem = key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()
    )
    node = Node()
    answered = moruna.register(
        scoring.add_one, node, store=f"{store.url}/functions?sig=x", key=pem,
        **options(wheels, runtime, tmp_path),
    )  # fmt: skip
    (registration,) = node.registered
    assert answered is registration
    assert registration.name == "add_one"
    path = f"/functions/add_one-{registration.sha256}.mia"
    assert registration.address == f"{store.url}{path}?sig=x"
    uploaded = store.objects[path]
    assert hashlib.sha256(uploaded).hexdigest() == registration.sha256
    key.public_key().verify(
        base64.b64decode(registration.signature), bytes.fromhex(registration.sha256)
    )
    # The file is a Moruna image archive of the image built for the node's platform: its
    # index, then its layer files, converted here, at page-aligned offsets, every byte counted.
    assert uploaded[:8] == b"MRNIMAG1" and uploaded[12:16] == bytes(4)
    length = int.from_bytes(uploaded[8:12], "little")
    index = json.loads(uploaded[16 : 16 + length])
    assert index["moruna_image_archive"] == 1
    config = json.loads(index["record"]["config"])
    assert config["architecture"] == "amd64"
    manifest = json.loads(index["record"]["manifest"])
    assert [layer["digest"] for layer in index["record"]["layers"]] == [
        layer["digest"] for layer in manifest["layers"]
    ]
    at = -(-(16 + length) // PAGE) * PAGE
    for span, layer in zip(index["layers"], index["record"]["layers"], strict=True):
        assert span["offset"] == at
        header = uploaded[at : at + PAGE]
        assert header[:8] == b"MRNLAYR1"
        assert hashlib.sha256(header).hexdigest() == layer["seal"]
        at += span["pages"] * PAGE
    assert at == len(uploaded)


def test_a_function_class_is_registered_under_the_name_given(project, tmp_path: Path) -> None:
    _, wheels, runtime, scoring, store = project
    key = ed25519.Ed25519PrivateKey.generate().private_bytes(
        serialization.Encoding.DER, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()
    )
    node = Node()
    moruna.register(
        scoring.Scored(), node, store=f"{store.url}/f", key=key, name="scored.v2",
        **options(wheels, runtime, tmp_path),
    )  # fmt: skip
    assert [r.name for r in node.registered] == ["scored.v2"]


def test_what_is_not_a_function_of_the_user_s_code_is_refused(project, tmp_path: Path) -> None:
    _, wheels, runtime, _, store = project
    with pytest.raises(TypeError, match="function of your code"):
        moruna.register(
            moruna.std.filter("id > 0"), Node(), store=store.url, key=b"",
            **options(wheels, runtime, tmp_path),
        )  # fmt: skip


def test_a_build_that_is_refused_registers_nothing(project, tmp_path: Path) -> None:
    root, wheels, runtime, scoring, store = project
    (root / "requirements.lock").unlink()
    node = Node()
    with pytest.raises(moruna.ConfigError, match="register refused"):
        moruna.register(
            scoring.add_one, node, store=store.url, key=b"",
            **options(wheels, runtime, tmp_path),
        )  # fmt: skip
    assert node.registered == [] and store.requests == []


def test_a_refused_key_is_register_refused(project, tmp_path: Path) -> None:
    _, wheels, runtime, scoring, store = project
    with pytest.raises(moruna.RegisterRefused, match="not a private key"):
        moruna.register(
            scoring.add_one, Node(), store=store.url, key=b"not a key",
            **options(wheels, runtime, tmp_path),
        )  # fmt: skip
    assert store.requests == []
