"""PY-T5 report_object: the report's attributes are exactly the fields of the trace crate's
`RunReport`, `__str__` is at most forty lines, `to_json` round-trips, and `_core.pyi` names every
public symbol of `moruna._core` and nothing else (PY-I5, e.3).
"""

from __future__ import annotations

import ast
import json
import pathlib

import moruna
from moruna import _core

STUB = pathlib.Path(moruna.__file__).with_name("_core.pyi")


@moruna.kernel
def identity(batch):  # noqa: ANN001, ANN202
    return batch


def test_py_t5_report_object(
    dataset: tuple[str, str, int], small_sink, staging
) -> None:  # noqa: ANN001
    src_url, out_url, _ = dataset
    report = moruna.run(moruna.ParquetSource(src_url), identity, small_sink(out_url), **staging)

    # The attributes are the fields `to_json` serialises, which is the field list of
    # `moruna_trace::RunReport` (04 d.1), plus `trace_path`, which PY-I5 adds.
    as_json = json.loads(report.to_json())
    for field in as_json:
        assert hasattr(report, field), f"the report has no attribute `{field}`"
    assert report.run_id == as_json["run_id"]
    assert report.trace_path is None

    text = str(report)
    assert len(text.splitlines()) <= 40, text
    assert report.run_id in text
    assert repr(report).startswith("RunReport(")


def test_the_trace_path_is_on_the_report(
    dataset: tuple[str, str, int], scratch, small_sink, staging
) -> None:  # noqa: ANN001
    src_url, out_url, _ = dataset
    trace = scratch / "trace.arrow"
    report = moruna.run(
        moruna.ParquetSource(src_url),
        identity,
        small_sink(out_url),
        trace=str(trace),
        **staging,
    )
    assert report.trace_path == str(trace)


def test_py_t5_the_stub_names_every_public_symbol() -> None:
    """`_core.pyi` is hand written and kept in step by this test (e.3)."""
    stub = ast.parse(STUB.read_text())
    declared = {
        node.name
        for node in stub.body
        if isinstance(node, ast.ClassDef | ast.FunctionDef)
    } | {
        target.id
        for node in stub.body
        if isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name)
        for target in [node.target]
    }
    declared = {name for name in declared if not name.startswith("_")}

    public = {name for name in dir(_core) if not name.startswith("_")}
    assert public <= declared, f"the stub does not name {sorted(public - declared)}"
    assert declared <= public | {"__version__"}, (
        f"the stub names what the module does not have: {sorted(declared - public)}"
    )


def test_py_t5_the_module_has_a_version() -> None:
    assert moruna.__version__ == _core.__version__
    assert moruna.__version__.count(".") == 2


def test_the_pyclasses_are_frozen(dataset: tuple[str, str, int]) -> None:
    """PY-I7: no Python-visible object has interior mutability the interpreter must guard."""
    src_url, out_url, _ = dataset
    for obj in (
        moruna.ParquetSource(src_url),
        moruna.ParquetSink(out_url),
        moruna.TensorSink("/tmp/moruna-not-written"),  # noqa: S108
        identity,
    ):
        try:
            obj.some_new_attribute = 1
        except AttributeError:
            continue
        raise AssertionError(f"{type(obj).__name__} accepted an attribute")
