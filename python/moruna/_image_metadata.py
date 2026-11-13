"""Versioned, passive OCI metadata for Moruna callables.

Readers need only the manifest/config JSON, never an executed image or imported
user code. The config label is authoritative; the manifest marker is discovery
metadata. No Griot identifiers, credentials or per-task argument values belong
here. The image's digest binds this metadata along with its configuration/layers.
"""

from __future__ import annotations

import json
import re
from collections.abc import Mapping
from typing import Any

LABEL = "org.moruna.callable"
VERSION_LABEL = "org.moruna.callable.version"
VERSION = "1"
MAX_BYTES = 128 * 1024


def _validate(value: Any) -> dict[str, Any]:
    if (
        not isinstance(value, dict)
        or type(value.get("schema_version")) is not int
        or value["schema_version"] != 1
    ):
        raise ValueError("unsupported Moruna callable metadata version")
    entry = value.get("entry")
    if not isinstance(entry, str) or not all(entry.partition(":")[::2]):
        raise ValueError("Moruna callable metadata needs a module:entry")
    key = value.get("build_key")
    if not isinstance(key, str) or not re.fullmatch(r"sha256:[a-f0-9]{64}", key):
        raise ValueError("Moruna callable metadata needs a sha256 build key")
    platform = value.get("platform")
    if (
        not isinstance(platform, dict)
        or platform.get("os") != "linux"
        or platform.get("architecture") not in ("arm64", "amd64")
    ):
        raise ValueError("Moruna callable metadata needs a supported OCI platform")
    definition = value.get("definition")
    if (
        not isinstance(definition, dict)
        or not isinstance(definition.get("name"), str)
        or not definition["name"]
    ):
        raise ValueError("Moruna callable metadata needs a named definition")
    if definition.get("role") not in ("source", "function", "sink") or not isinstance(
        definition.get("settings"), dict
    ):
        raise ValueError("Moruna callable metadata needs a role and parameter schema")
    provenance = definition.get("provenance")
    if not isinstance(provenance, dict) or provenance.get("build_key") != key:
        raise ValueError("Moruna callable metadata build key differs from its definition")
    return value


def labels(
    *, entry: str, build_key: str, definition: dict[str, Any] | None, architecture: str
) -> dict[str, str]:
    """The existing Moruna definition, in canonical JSON, without instance params."""
    if definition is None:
        return {}
    value = _validate(
        {
            "schema_version": 1,
            "entry": entry,
            "build_key": build_key,
            "platform": {"os": "linux", "architecture": architecture},
            "definition": definition,
        }
    )
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False)
    if len(encoded.encode("utf-8")) > MAX_BYTES:
        raise ValueError("Moruna callable metadata exceeds 128 KiB")
    return {VERSION_LABEL: VERSION, LABEL: encoded}


def read(labels: Mapping[str, Any]) -> dict[str, Any] | None:
    """Read passive metadata; absent is a plain/older image, malformed is refused.

    This validates the metadata envelope, not the full callable schema, image
    signature, runtime ABI or access policy. Consumers must check those too.
    """
    if LABEL not in labels and VERSION_LABEL not in labels:
        return None
    if labels.get(VERSION_LABEL) != VERSION:
        raise ValueError("unsupported Moruna callable metadata version")
    text = labels.get(LABEL)
    if not isinstance(text, str) or len(text.encode("utf-8")) > MAX_BYTES:
        raise ValueError("Moruna callable metadata is absent or exceeds 128 KiB")
    return _validate(json.loads(text))
