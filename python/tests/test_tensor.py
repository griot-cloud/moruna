"""The tensor path, end to end: a safetensors file through a tensor kernel to a safetensors file.

Every part of this path was built and tested on its own with fakes, and until 2026-09-29 nothing
had run them together: the one round-trip test (SI-T10) had been ignored since wave 3 with an
`unimplemented!()` body. The first real run completed first time; this test keeps it that way.

The input file is written by hand rather than with the `safetensors` package, so the suite gains no
dependency: the format is an 8-byte little-endian header length, a JSON header naming each tensor's
dtype, shape and byte range, and the raw bytes.
"""

from __future__ import annotations

import json
import pathlib
import struct

import numpy as np
import pytest

import moruna

ROWS, DIM = 100_000, 32


def budget() -> int:
    """A budget with room above what this process already holds. The claim here is the tensor
    path, not the budget, and the budget counts the whole process: by the time this file runs,
    earlier in-process runs have left the test process holding over 4 GB (2026-09-29), so a fixed
    figure would be refused or not depending on test order. The floor a run needs is what the
    process holds, plus the arena's floor, plus a reserve of a tenth of the budget itself, so the
    room is added before dividing by 0.9, not after: a flat margin on top of 4.7 GB fell inside
    the reserve and was refused."""
    held = moruna.inspect_host()["anon_bytes"]
    return int((held + (512 << 20)) / 0.9)


def write_safetensors(path: pathlib.Path, name: str, array: np.ndarray) -> None:
    data = np.ascontiguousarray(array).tobytes()
    header = json.dumps(
        {name: {"dtype": "F32", "shape": list(array.shape), "data_offsets": [0, len(data)]}}
    ).encode()
    with path.open("wb") as f:
        f.write(struct.pack("<Q", len(header)))
        f.write(header)
        f.write(data)


def read_safetensors(path: pathlib.Path) -> dict[str, np.ndarray]:
    raw = path.read_bytes()
    (n,) = struct.unpack("<Q", raw[:8])
    header = json.loads(raw[8 : 8 + n])
    body = raw[8 + n :]
    out = {}
    for name, meta in header.items():
        if name == "__metadata__":
            continue
        lo, hi = meta["data_offsets"]
        out[name] = np.frombuffer(body[lo:hi], dtype=np.float32).reshape(meta["shape"])
    return out


@pytest.fixture
def vectors(scratch: pathlib.Path) -> tuple[pathlib.Path, np.ndarray]:
    src = scratch / "in.safetensors"
    array = np.random.default_rng(1).standard_normal((ROWS, DIM), dtype=np.float32)
    write_safetensors(src, "vectors", array)
    return src, array


def test_a_tensor_job_runs_end_to_end_and_the_kernel_sees_a_dlpack_tensor(
    scratch: pathlib.Path, vectors: tuple[pathlib.Path, np.ndarray]
) -> None:
    src, array = vectors
    seen: dict[str, object] = {}

    @moruna.kernel(accepts="tensor")
    def normalise(t):  # noqa: ANN001, ANN202
        # What a tensor kernel receives: an object that speaks DLPack, so NumPy (or torch)
        # takes it without a copy. What it returns is anything that speaks DLPack back.
        seen.setdefault("dlpack", hasattr(t, "__dlpack__"))
        arr = np.from_dlpack(t)
        seen.setdefault("dtype", str(arr.dtype))
        seen.setdefault("ndim", arr.ndim)
        return arr / np.maximum(np.linalg.norm(arr, axis=1, keepdims=True), 1e-9)

    out = scratch / "out"
    report = moruna.run(
        moruna.TensorSource(str(src), tensors=["vectors"]),
        [normalise],
        moruna.TensorSink(str(out), format="safetensors"),
        budget=budget(),
        ordered=True,
    )

    assert report.exit == "Completed", report
    assert report.stages[0]["rows_out"] == ROWS, report.stages
    assert seen == {"dlpack": True, "dtype": "float32", "ndim": 2}, seen

    files = sorted(out.glob("*.safetensors"))
    assert files, f"no safetensors file was written under {out}"
    got = np.concatenate([next(iter(read_safetensors(f).values())) for f in files])
    want = array / np.linalg.norm(array, axis=1, keepdims=True)
    assert got.shape == want.shape
    # Ordered, so row for row: the sink received the morsels in source order.
    assert np.allclose(got, want, atol=1e-5), "the output is not the normalised input"


def test_a_tensor_source_names_files_not_directories(
    scratch: pathlib.Path, vectors: tuple[pathlib.Path, np.ndarray]
) -> None:
    """`TensorSource` takes file paths (07 d.1: local files, v1). A directory is an error that
    names it, not a silent empty run."""
    with pytest.raises(moruna.MorunaError) as failure:
        moruna.run(
            moruna.TensorSource(str(scratch), tensors=["vectors"]),
            [moruna.kernel(lambda t: t, accepts="tensor")],
            moruna.TensorSink(str(scratch / "out-dir"), format="safetensors"),
            budget=budget(),
        )
    assert str(scratch) in str(failure.value), failure.value
