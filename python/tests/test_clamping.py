"""PY-T13 configuration_clamping, through `moruna.run`'s own arguments (PY-I10, f.3).

Every row of the preamble's configuration table whose owner is `user` and whose range is not
`fixed` is clamped here, once, and each clamp is named in `report.notes`. The bounds themselves,
and the rows that are not reachable through a completed run, are asserted in Rust
(`crates/moruna-py/src/translate.rs`); what is asserted here is that the note reaches the report.
"""

from __future__ import annotations

import pytest

import moruna


@moruna.kernel
def identity(batch):  # noqa: ANN001, ANN202
    return batch


def test_py_t13_checkpoint_interval_is_clamped(
    dataset: tuple[str, str, int], small_sink, staging
) -> None:  # noqa: ANN001
    src_url, out_url, _ = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url),
        identity,
        small_sink(out_url),
        checkpoint_interval=0.1,
        **staging,
    )
    clamps = [n for n in report.notes if n.startswith("clamped checkpoint.interval_ms")]
    assert clamps == ["clamped checkpoint.interval_ms from 100 to 500"], report.notes


def test_py_t13_sink_sizes_are_clamped(
    dataset: tuple[str, str, int], staging
) -> None:  # noqa: ANN001
    src_url, out_url, _ = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url),
        identity,
        moruna.ParquetSink(out_url, row_group_bytes="1MiB", file_bytes="1MiB"),
        **staging,
    )
    clamps = [n for n in report.notes if n.startswith("clamped sink.")]
    assert clamps == [
        f"clamped sink.row_group_bytes from {1 << 20} to {16 << 20}",
        f"clamped sink.file_bytes from {1 << 20} to {64 << 20}",
    ], report.notes


def test_py_t13_an_in_range_value_is_not_clamped(
    dataset: tuple[str, str, int], staging
) -> None:  # noqa: ANN001
    src_url, out_url, _ = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url),
        identity,
        moruna.ParquetSink(out_url, row_group_bytes="32MiB", file_bytes="64MiB"),
        checkpoint_interval=5.0,
        **staging,
    )
    assert not [n for n in report.notes if n.startswith("clamped ")], report.notes


def test_py_t13_budget_reaches_discovery_unclamped(dataset: tuple[str, str, int]) -> None:
    """`budget.host` is discovery's to clamp, and it reports the clamp in its own notes."""
    src_url, out_url, _ = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url),
        identity,
        moruna.ParquetSink(out_url, row_group_bytes="16MiB", file_bytes="64MiB"),
        budget=1 << 62,
    )
    assert not [n for n in report.notes if n.startswith("clamped budget.host")]
    assert report.limits["memory_ceiling"] < (1 << 62), "discovery clamped it, not the surface"


def test_py_t13_a_size_string_is_parsed(
    dataset: tuple[str, str, int], small_sink, staging
) -> None:  # noqa: ANN001
    """`"4GiB"` goes through discovery's size parser and becomes the ceiling (f.3).

    A copy, with no kernel stage, so the assertion is about the argument and not about how much
    room a kernel leaves.
    """
    src_url, out_url, _ = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url),
        [],
        small_sink(out_url),
        budget="4GiB",
        **staging,
    )
    assert report.limits["memory_ceiling"] <= 4 << 30


def test_py_t13_a_bad_size_string_is_refused(dataset: tuple[str, str, int]) -> None:
    src_url, out_url, _ = dataset
    with pytest.raises(moruna.ConfigError):
        moruna.run(
            moruna.ParquetSource(src_url),
            [],
            moruna.ParquetSink(out_url),
            budget="eight gigs",
        )


def test_py_t13_a_trace_path_that_cannot_be_written_is_refused(
    dataset: tuple[str, str, int], scratch
) -> None:  # noqa: ANN001
    src_url, out_url, _ = dataset
    with pytest.raises(moruna.ConfigError) as caught:
        moruna.run(
            moruna.ParquetSource(src_url),
            [],
            moruna.ParquetSink(out_url),
            trace=str(scratch / "no-such-directory" / "t.arrow"),
        )
    assert "trace.path" in caught.value.diagnostic["name"]
