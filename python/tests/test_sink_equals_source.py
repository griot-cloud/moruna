"""PY-T14 sink_equals_source: a sink that would write where the source reads is refused with
`PlanError`, before anything starts (f.3).
"""

from __future__ import annotations

import pytest

import amoru


@pytest.mark.parametrize(
    ("sink_url", "source_url"),
    [
        ("s3://b/in/", "s3://b/in/"),
        ("s3://b/", "s3://b/in/"),
        ("file:///data/out", "/data/out/part.parquet"),
        ("S3://B/IN", "s3://B/IN/part.parquet"),
    ],
)
def test_py_t14_refused(sink_url: str, source_url: str) -> None:
    with pytest.raises(amoru.PlanError) as caught:
        amoru.run(
            amoru.ParquetSource(source_url), [], amoru.ParquetSink(sink_url)
        )
    assert caught.value.kind == "Plan"
    assert "the sink writes where the source reads" in caught.value.message


def test_py_t14_a_sibling_prefix_is_allowed(
    dataset: tuple[str, str, int], small_sink, staging
) -> None:  # noqa: ANN001
    """`s3://b/in2/` beside `s3://b/in/` is a different place and is not refused."""
    src_url, out_url, _ = dataset
    # The run itself completes, which is the proof that nothing refused it.
    report = amoru.run(amoru.ParquetSource(src_url), [], small_sink(out_url), **staging)
    assert report.exit == "Completed"

    with pytest.raises(amoru.PlanError):
        amoru.run(amoru.ParquetSource(src_url), [], amoru.ParquetSink(src_url))
