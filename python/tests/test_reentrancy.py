"""PY-T9 reentrancy: a second concurrent `run` raises `ConfigError` (g).

The arena is a process-wide reservation, so two runs in one process cannot overlap. The second
run is started from a thread while the first is in flight; whichever loses the race is the one
that raises, so the test asserts that exactly one of the two did.
"""

from __future__ import annotations

import threading
import time

import amoru


def test_py_t9_reentrancy(
    dataset: tuple[str, str, int], scratch, small_sink, staging
) -> None:  # noqa: ANN001
    src_url, out_url, _ = dataset
    second_out = scratch / "out2"
    second_out.mkdir()

    @amoru.kernel
    def slow(batch):  # noqa: ANN001, ANN202
        time.sleep(0.5)
        return batch

    refusals: list[BaseException] = []
    others: list[BaseException] = []
    completions: list[object] = []

    def go(out: str) -> None:
        try:
            completions.append(
                amoru.run(amoru.ParquetSource(src_url), slow, small_sink(out), **staging)
            )
        except amoru.ConfigError as e:  # the re-entrancy refusal
            if "already active" in e.message:
                refusals.append(e)
            else:
                others.append(e)
        except amoru.AmoruError as e:
            others.append(e)

    threads = [
        threading.Thread(target=go, args=(out_url,)),
        threading.Thread(target=go, args=(f"file://{second_out}",)),
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=120)

    assert len(refusals) == 1, (
        f"expected exactly one refusal, got {len(refusals)}; other errors: "
        f"{[type(e).__name__ + ': ' + e.message for e in others]}"
    )
    assert "already active" in refusals[0].message
    assert refusals[0].kind == "Config"
    assert len(completions) + len(others) == 1
