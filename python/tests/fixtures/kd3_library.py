"""One actual KD3 caller-owned adapter run; same canonical matrix on both adapters.

python kd3_library.py MATRIX CASE linux|native EXPLICIT_STAGING_ROOT
Requires the real installed Moruna/PyArrow runtime; nothing is mocked or installed here.
Each invocation runs exactly one job (and one native assigned startup identity).
"""
import json
import pathlib
import sys

import moruna
import pyarrow as pa


def main():
    if len(sys.argv) not in (5, 6):
        raise ValueError("usage: kd3_library.py MATRIX CASE ADAPTER STAGING [rows|adapter-refusal]")
    matrix_path, case_id, adapter, staging = sys.argv[1:5]
    mode = sys.argv[5] if len(sys.argv) == 6 else "rows"
    if mode not in ("rows", "adapter-refusal"):
        raise ValueError("invalid real fixture mode")
    matrix = json.loads(pathlib.Path(matrix_path).read_text())
    case = next(case for case in matrix["cases"] if case["id"] == case_id)
    if case["runner"] != "python-library" or adapter not in ("linux", "native"):
        raise ValueError("wrong fixture runner/adapter")
    if sys.platform != "linux":
        raise RuntimeError("KD3 requires actual Linux/native execution")
    # The exact same shared values and non-nullable schema are used by the Rust fixture.
    expected = matrix["rows"]
    schema = pa.schema([pa.field("x", pa.int64(), nullable=False)])
    batch = pa.RecordBatch.from_arrays([pa.array(expected, type=pa.int64())], schema=schema)

    class Input(moruna.Source):
        def schema(self):
            return schema

        def plan(self):
            # Exercise nonuniform actual split geometry, not a fake row count.
            return [moruna.Split(0, 17), moruna.Split(1, len(expected) - 17)]

        def read(self, split_id, start, end):
            if mode == "adapter-refusal" and case_id == "library.python.source":
                raise ConnectionError("KD3 real source refusal")
            base = 0 if split_id == 0 else 17
            return batch.slice(base + start, end - start)

    class Output(moruna.Sink):
        def __init__(self):
            self.values = []
            self.finished = 0

        def write(self, actual):
            if mode == "adapter-refusal" and case_id == "library.python.sink":
                raise OSError("KD3 real sink refusal")
            if actual.schema != schema:
                raise ValueError(f"actual schema changed: {actual.schema}")
            self.values.extend(actual.column("x").to_pylist())

        def finish(self):
            self.finished += 1

    @moruna.kernel
    def identity(actual):
        if mode == "adapter-refusal":
            raise RuntimeError("KD3 real kernel refusal")
        return actual

    def batches():
        yield batch.slice(0, 17)
        if mode == "adapter-refusal":
            raise ConnectionError("KD3 real iterator refusal")
        yield batch.slice(17)

    source = batches() if case_id == "library.iterator" else Input()
    kernels = [identity] if case_id == "library.python.kernel" else []
    output = Output()
    # Native startup authority is mandatory; off-kernel caller embedding remains explicit.
    try:
        ports = moruna.EnginePorts.from_guest()
    except moruna.ConfigError as error:
        # Only the canonical absence of a native start page proves a host run.
        # A missing/denied native binding is NOT permission to label it Linux.
        absent = error.kind == "Config" and error.diagnostic == {
            "name": "engine.bindings",
            "message": "no actual Moruna start page; native bootstrap cannot use host fallback",
        }
        unsupported_target = error.kind == "Config" and error.diagnostic == {
            "name": "engine.bindings",
            "message": "native guest bootstrap requires a real Linux x86_64 Moruna start page",
        }
        if adapter != "linux" or not (absent or unsupported_target):
            raise
        ports = None
    else:
        if adapter != "native":
            raise RuntimeError("adapter label contradicts actual native startup grants")
    try:
        report = moruna.run(
            source, kernels, output, budget=384 << 20, cpu=1.0,
            staging_dir=staging, staging_limit=128 << 20, ordered=True,
            checkpoint=False, engine_ports=ports,
        )
    except moruna.MorunaError as error:
        if mode != "adapter-refusal" or "KD3 real" not in str(error):
            raise
        print(json.dumps({"case": case_id, "adapter": adapter, "phase": "execute", "mode": mode,
                          "outcome": "refused", "diagnostic": str(error),
                          "partial_rows": output.values, "finish": output.finished, "run_id": error.run_id}))
        return
    if mode == "adapter-refusal":
        raise AssertionError("actual source/kernel/sink failure was not propagated")
    if report.exit != "Completed" or output.finished != 1 or sorted(output.values) != sorted(expected):
        raise AssertionError(f"actual execution differs: {report.exit}, finish={output.finished}, rows={output.values}")
    print(json.dumps({"case": case_id, "adapter": adapter, "phase": "execute", "mode": "rows",
                      "outcome": "rows", "rows": sorted(output.values), "finish": output.finished,
                      "committed": len(output.values), "run_id": report.run_id}))


if __name__ == "__main__":
    main()
