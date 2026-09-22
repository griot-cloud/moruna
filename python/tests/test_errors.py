"""PY-T2 exception_mapping, the surface's half: every error reaches Python as one type with a
structured payload, and the subclasses are the ones e.2 names (PY-I2).

The mapping from each `AmoruError` variant is asserted in Rust, where the variants can be
constructed (`crates/amoru-py/src/errors.rs`); what is asserted here is the hierarchy Python sees
and the payload on an exception a real run raised.
"""

from __future__ import annotations

import pytest

import amoru


def test_the_hierarchy_is_the_one_e2_names() -> None:
    for subclass in (
        amoru.PlanError,
        amoru.KernelError,
        amoru.BudgetError,
        amoru.IoError,
        amoru.ConfigError,
        amoru.ResumeError,
        amoru.Cancelled,
    ):
        assert issubclass(subclass, amoru.AmoruError)
        assert issubclass(subclass, Exception)


def test_a_kernel_that_raises_is_a_kernel_error(
    dataset: tuple[str, str, int], small_sink, staging
) -> None:  # noqa: ANN001
    """h, failures: a kernel exception arrives as `KernelError` with the traceback attached."""
    src_url, out_url, _ = dataset

    @amoru.kernel
    def explode(batch):  # noqa: ANN001, ANN202
        raise ValueError("the kernel said no")

    with pytest.raises(amoru.KernelError) as caught:
        amoru.run(amoru.ParquetSource(src_url), explode, small_sink(out_url), **staging)

    error = caught.value
    assert error.kind == "Kernel"
    assert isinstance(error.diagnostic, dict)
    assert "the kernel said no" in error.diagnostic["traceback"]
    assert "ValueError" in error.diagnostic["traceback"]
    assert error.message


def test_an_unknown_argument_is_a_config_error(dataset: tuple[str, str, int]) -> None:
    """f.3: an unknown `sizer` or `on_error` is refused by name, not clamped."""
    src_url, out_url, _ = dataset
    source, sink = amoru.ParquetSource(src_url), amoru.ParquetSink(out_url)

    with pytest.raises(amoru.ConfigError) as caught:
        amoru.run(source, [], sink, sizer="psychic")
    assert caught.value.kind == "Config"
    assert "psychic" in caught.value.message

    with pytest.raises(amoru.ConfigError):
        amoru.run(source, [], sink, on_error="explode")


def test_a_kernel_that_is_neither_decorated_nor_callable_is_a_type_error(
    dataset: tuple[str, str, int],
) -> None:
    """h, failures: a `TypeError` in `run` before anything starts."""
    src_url, out_url, _ = dataset
    with pytest.raises(TypeError):
        amoru.run(amoru.ParquetSource(src_url), 42, amoru.ParquetSink(out_url))
    with pytest.raises(TypeError):
        amoru.run("not a source", [], amoru.ParquetSink(out_url))
    with pytest.raises(TypeError):
        amoru.run(amoru.ParquetSource(src_url), [], "not a sink")
