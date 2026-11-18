"""``moruna.params()`` (item 5.5): the task's params, ``MORUNA_PARAMS``, read the same way by the
package and by the stand-in ``moruna`` module an image Moruna builds holds instead."""

from __future__ import annotations

import importlib.util
import types
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

STANDIN = Path(__file__).resolve().parents[1] / "moruna" / "_standin.py"


def standin() -> types.ModuleType:
    """``_standin.py`` loaded by itself, as an image holds it: ``/opt/moruna/moruna``."""
    spec = importlib.util.spec_from_file_location("moruna_standin_under_test", STANDIN)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.fixture(params=["package", "standin"])
def params(request: pytest.FixtureRequest) -> Callable[[], dict[str, Any]]:
    if request.param == "package":
        import moruna  # noqa: PLC0415, the package only where it is the one under test

        return moruna.params
    return standin().params


def test_the_params_are_the_variables_object(
    params: Callable[[], dict[str, Any]], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("MORUNA_PARAMS", '{"threshold": 0.5, "labels": ["a"]}')
    assert params() == {"threshold": 0.5, "labels": ["a"]}


def test_no_params_is_an_empty_object(
    params: Callable[[], dict[str, Any]], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("MORUNA_PARAMS", raising=False)
    assert params() == {}
    monkeypatch.setenv("MORUNA_PARAMS", "")
    assert params() == {}


def test_anything_else_is_refused_naming_the_variable(
    params: Callable[[], dict[str, Any]], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("MORUNA_PARAMS", "[0.5]")
    with pytest.raises(ValueError, match="MORUNA_PARAMS is a JSON object, not list"):
        params()
    monkeypatch.setenv("MORUNA_PARAMS", "{")
    with pytest.raises(ValueError, match="MORUNA_PARAMS is not JSON"):
        params()


def test_the_standin_says_it_holds_params() -> None:
    module = standin()
    with pytest.raises(AttributeError, match=r"Split, function and params; the engine"):
        _ = module.run
