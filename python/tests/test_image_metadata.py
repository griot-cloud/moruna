"""OCI metadata is readable without importing/executing user code or the engine."""

import copy
import json

import builder_support as support
import pytest

metadata = support.load("_image_metadata.py", "moruna_image_metadata_tests")
KEY = "sha256:" + "a" * 64


def definition():
    return {
        "name": "score",
        "role": "function",
        "settings": {"type": "object", "properties": {"threshold": {"type": "integer"}}},
        "takes": {"fields": [{"name": "id", "type": "int64", "nullable": False}]},
        "gives": {"fields": [{"name": "score", "type": "int64", "nullable": False}]},
        "requires": [{"name": "warehouse"}],
        "provenance": {"build_key": KEY},
    }


def labels():
    return metadata.labels(
        entry="pipeline:Score", build_key=KEY, definition=definition(), architecture="arm64"
    )


def test_callable_metadata_round_trip_is_canonical_and_has_no_task_values():
    original = definition()
    before = copy.deepcopy(original)
    made = labels()
    assert all(isinstance(k, str) and isinstance(v, str) for k, v in made.items())
    back = metadata.read(made)
    assert back["definition"] == original
    assert back["entry"] == "pipeline:Score"
    assert back["platform"] == {"os": "linux", "architecture": "arm64"}
    assert set(back) == {"schema_version", "entry", "build_key", "definition", "platform"}
    assert "params" not in back and "digest" not in back  # no values or self-digest cycle
    assert original == before
    reordered = dict(reversed(list(original.items())))
    assert (
        metadata.labels(
            entry="pipeline:Score", build_key=KEY, definition=reordered, architecture="arm64"
        )
        == made
    )


@pytest.mark.parametrize("role", ["source", "function", "sink"])
def test_callable_metadata_preserves_its_execution_role(role):
    d = definition()
    d["role"] = role
    made = metadata.labels(
        entry="pipeline:Score", build_key=KEY, definition=d, architecture="amd64"
    )
    assert metadata.read(made)["definition"]["role"] == role


def test_an_ordinary_image_is_not_inferred_to_be_a_function():
    assert metadata.read({"org.opencontainers.image.title": "ordinary"}) is None
    assert (
        metadata.labels(
            entry="pipeline:Score", build_key=KEY, definition=None, architecture="arm64"
        )
        == {}
    )


@pytest.mark.parametrize("version", ["2", "", None])
def test_future_or_missing_metadata_versions_are_not_silently_accepted(version):
    made = labels()
    made[metadata.VERSION_LABEL] = version
    with pytest.raises(ValueError, match="version"):
        metadata.read(made)


@pytest.mark.parametrize(
    "patch",
    [
        {"entry": "not-an-entry"},
        {"build_key": "sha256:bad"},
        {"platform": {"os": "windows", "architecture": "amd64"}},
    ],
)
def test_malformed_metadata_is_refused(patch):
    made = labels()
    body = json.loads(made[metadata.LABEL])
    body.update(patch)
    made[metadata.LABEL] = json.dumps(body)
    with pytest.raises(ValueError):
        metadata.read(made)


def test_metadata_cannot_claim_a_different_build_key_than_its_definition():
    made = labels()
    body = json.loads(made[metadata.LABEL])
    body["definition"]["provenance"]["build_key"] = "sha256:" + "b" * 64
    made[metadata.LABEL] = json.dumps(body)
    with pytest.raises(ValueError, match="differs"):
        metadata.read(made)


def test_metadata_size_is_bounded_on_build_and_read():
    d = definition()
    d["description"] = "x" * metadata.MAX_BYTES
    with pytest.raises(ValueError, match="128 KiB"):
        metadata.labels(entry="pipeline:Score", build_key=KEY, definition=d, architecture="arm64")
    with pytest.raises(ValueError, match="128 KiB"):
        metadata.read({metadata.VERSION_LABEL: "1", metadata.LABEL: "x" * (metadata.MAX_BYTES + 1)})
