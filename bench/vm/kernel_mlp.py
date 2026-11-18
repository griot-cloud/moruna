"""`mlp_forward` against Score (system design 4G.1, work item 4.6): the measurement and the row check.

`crates/moruna-kernels/examples/mlp_score.rs` runs Score's features (plain Arrow, until `expr`)
and network (`mlp_forward`'s CPU reference); this script feeds it Arrow IPC, times NumPy's
network as `workloads.score` computes it, and checks the rows.

    kernel_mlp.py export OUT                       Score's model and embedding as JSON
    kernel_mlp.py measure BIN NORM_PARQUET OUT [--rows 10000,100000,1000000] [--reps N]
    kernel_mlp.py score BIN REFERENCE_JSON|numpy OUT PARQUET... [--gate-canon BIN]

`measure`: the first 10^6 rows of a month's features, as `workloads.score` builds them; for each
row count, NumPy's float64 network and `mlp_forward` in float32 and float64, each on one thread
(BLAS held to one thread here, the example single-threaded), best and median of REPS runs after
one warm run; and each precision's error against NumPy's float64: the largest absolute and
relative error, the largest error in units of the declared tolerance (`u * (s(x) + 4|y64|)`, spec
`crates/moruna-kernels/spec/mlp_forward.md`), and how many rows are equal bit for bit, and after
Score's `np.round(y, 6)`. One JSON line per measurement, and a table at the end.

`score`: Score over the trips of the given months through the example, written as Parquet and
reduced by `gate-canon` (gate-canon/1); float64 must give the reference's `hash_all` (every
column bit for bit), float32 its `hash_exact`, and its predictions are compared with float64's
row by row. With `numpy` for the reference (a machine without gate-canon), the float64
predictions are compared bit for bit, row for row, with `workloads.score` on this machine's
NumPy, in the same 64Ki-row batches.

Run with BLAS on one thread from the start: this script sets OPENBLAS_NUM_THREADS,
VECLIB_MAXIMUM_THREADS and OMP_NUM_THREADS to 1 before it imports NumPy.
"""
import argparse
import json
import os
import platform
import subprocess
import sys
import time

for var in ("OPENBLAS_NUM_THREADS", "VECLIB_MAXIMUM_THREADS", "OMP_NUM_THREADS", "MKL_NUM_THREADS"):
    os.environ[var] = "1"

import numpy as np  # noqa: E402
import pyarrow as pa  # noqa: E402
import pyarrow.compute as pc  # noqa: E402
import pyarrow.ipc as ipc  # noqa: E402
import pyarrow.parquet as pq  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import workloads as w  # noqa: E402

COLS = ["vendor_id", "pickup_at", "dropoff_at", "passenger_count", "trip_distance", "ratecode_id",
        "pu_location_id", "do_location_id", "fare_amount"]
UNIT = {"float32": 2.0 ** -24, "float64": 2.0 ** -53}


def model():
    """Score's network in mlp_forward's JSON form: weights inputs x outputs (x @ W), no bias on
    the last layer. json.dump writes each float64 by repr, which reads back exactly."""
    return {"layers": [
        {"weights": w._W1.tolist(), "bias": w._B1.tolist(), "activation": "relu"},
        {"weights": w._W2.tolist(), "bias": w._B2.tolist(), "activation": "relu"},
        {"weights": w._W3.tolist(), "activation": "identity"},
    ]}


def export(out):
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "score-model.json"), "w") as f:
        json.dump(model(), f)
    with open(os.path.join(out, "score-embedding.json"), "w") as f:
        json.dump(w._EMB.tolist(), f)
    return os.path.join(out, "score-model.json"), os.path.join(out, "score-embedding.json")


def features(batch):
    """X exactly as workloads.score builds it."""
    n = batch.num_rows
    a = batch["pickup_at"].cast(pa.int64()).to_numpy(zero_copy_only=False)
    b = batch["dropoff_at"].cast(pa.int64()).to_numpy(zero_copy_only=False)
    num = lambda c, d: pc.fill_null(batch[c], d).to_numpy(zero_copy_only=False).astype(np.float64)
    dist = np.clip(num("trip_distance", 0.0), 0.0, 200.0)
    dur = np.clip((b - a) / 60e6, 0.0, 240.0)
    hour = ((a // 3_600_000_000) % 24).astype(np.float64)
    X = np.empty((n, 23))
    X[:, 0] = np.log1p(dist); X[:, 1] = np.log1p(dur)
    X[:, 2] = np.sin(hour * (np.pi / 12)); X[:, 3] = np.cos(hour * (np.pi / 12))
    X[:, 4] = ((a // 86_400_000_000) % 7) / 6.0
    X[:, 5] = num("passenger_count", 1.0) / 4.0; X[:, 6] = num("ratecode_id", 1.0) / 6.0
    X[:, 7:15] = w._EMB[np.clip(num("pu_location_id", 0.0).astype(np.int64), 0, 299)]
    X[:, 15:23] = w._EMB[np.clip(num("do_location_id", 0.0).astype(np.int64), 0, 299)]
    return X


def network(X):
    """The network exactly as workloads.score computes it, before its round."""
    h = np.maximum(X @ w._W1 + w._B1, 0.0)
    h = np.maximum(h @ w._W2 + w._B2, 0.0)
    return (h @ w._W3)[:, 0]


def scale(X):
    """s(x): the network with |W|, |b| and |x| and no activations (the spec's tolerance scale)."""
    s = np.abs(X) @ np.abs(w._W1) + np.abs(w._B1)
    s = s @ np.abs(w._W2) + np.abs(w._B2)
    return (s @ np.abs(w._W3))[:, 0]


def write_ipc(path, table, chunk=None):
    with ipc.new_file(path, table.schema) as f:
        for b in table.to_batches(max_chunksize=chunk):
            f.write_batch(b)


def run(cmd):
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        raise SystemExit(f"{' '.join(cmd)} failed: {p.stderr.strip()[-2000:]}")
    return json.loads(p.stdout.strip().splitlines()[-1])


def machine():
    return {"machine": platform.machine(), "system": platform.system(), "node": platform.node(),
            "python": platform.python_version(), "numpy": np.__version__,
            "blas": blas_name()}


def blas_name():
    try:
        cfg = np.show_config(mode="dicts")
        b = cfg["Build Dependencies"]["blas"]
        return f"{b.get('name')} {b.get('openblas configuration', '')}".strip()
    except Exception:  # noqa: BLE001 (older NumPy)
        return "unknown"


def measure(a):
    os.makedirs(a.out, exist_ok=True)
    model_json, _ = export(a.out)
    rows = [int(r) for r in a.rows.split(",")]
    most = max(rows)
    t = pq.read_table(a.parquet, columns=COLS).slice(0, most).combine_chunks()
    X = features(t.to_batches()[0])
    if len(X) < most:
        raise SystemExit(f"the month has {len(X)} rows, fewer than {most}")
    feats = os.path.join(a.out, "features.arrow")
    write_ipc(feats, pa.table({f"x{i}": X[:, i].copy() for i in range(23)}))
    print(json.dumps({"machine": machine()}), flush=True)
    results = []
    for n in rows:
        x = X[:n]
        y = network(x)  # warm
        times = []
        for _ in range(a.reps):
            t0 = time.perf_counter(); y = network(x); times.append(time.perf_counter() - t0)
        times.sort()
        s = scale(x)
        r = {"rows": n, "impl": "numpy float64", "best_s": times[0], "median_s": times[len(times) // 2]}
        print(json.dumps(r), flush=True)
        results.append(r)
        for precision in ("float32", "float64"):
            out = os.path.join(a.out, f"y-{precision}-{n}.arrow")
            r = run([a.bin, "time", model_json, precision, feats, str(n), str(a.reps), out])
            got = ipc.open_file(out).read_all()["y"].to_numpy().astype(np.float64)
            err = np.abs(got - y)
            r.update(impl=f"mlp_forward {precision}",
                     max_abs=float(err.max()),
                     max_rel=float((err / np.maximum(np.abs(y), np.finfo(np.float64).tiny)).max()),
                     max_of_tolerance=float((err / (UNIT[precision] * (s + 4.0 * np.abs(y)))).max()),
                     bit_equal=int((got.view(np.int64) == y.view(np.int64)).sum()),
                     round6_equal=int((np.round(got, 6) == np.round(y, 6)).sum()))
            print(json.dumps(r), flush=True)
            results.append(r)
    with open(os.path.join(a.out, "measure.json"), "w") as f:
        json.dump({"machine": machine(), "results": results}, f, indent=1)
    print(table(results))


def table(results):
    lines = ["| rows | implementation | best s | median s | rows/s | max abs err vs NumPy f64 | "
             "max err / tolerance | bit-equal rows | equal after round(6) |",
             "|---:|---|---:|---:|---:|---:|---:|---:|---:|"]
    for r in results:
        fmt = lambda k, f: (f % r[k]) if k in r else "-"
        lines.append(f"| {r['rows']:,} | {r['impl']} | {r['best_s']:.4f} | {r['median_s']:.4f} | "
                     f"{r['rows'] / r['best_s']:,.0f} | {fmt('max_abs', '%.2e')} | "
                     f"{fmt('max_of_tolerance', '%.3f')} | {fmt('bit_equal', '%d')} | "
                     f"{fmt('round6_equal', '%d')} |")
    return "\n".join(lines)


def digest(gate_canon, outdir):
    p = subprocess.run([gate_canon, "digest", "score", outdir], capture_output=True, text=True)
    if p.returncode != 0:
        raise SystemExit(f"gate-canon: {p.stderr.strip()[-500:]}")
    return json.loads(p.stdout)


def numpy_score(src):
    """workloads.score's predictions on a month in 64Ki-row batches, in row order, on this machine."""
    t = pq.read_table(src, columns=COLS)
    return np.concatenate([w.score(b).column(3).to_numpy() for b in t.to_batches(max_chunksize=65536)])


def score(a):
    os.makedirs(a.out, exist_ok=True)
    model_json, emb_json = export(a.out)
    ref = json.load(open(a.reference)) if a.reference != "numpy" else None
    verdict = {"reference": a.reference, "machine": machine()}
    preds = {}
    for precision in ("float64", "float32"):
        outdir = os.path.join(a.out, f"score-{precision}")
        os.makedirs(outdir, exist_ok=True)
        network_s, pred = 0.0, []
        for i, src in enumerate(a.parquet):
            trips = os.path.join(a.out, "trips.arrow")
            write_ipc(trips, pq.read_table(src, columns=COLS), chunk=65536)
            got = os.path.join(a.out, "score.arrow")
            r = run([a.bin, "score", model_json, emb_json, precision, trips, got])
            network_s += r["network_s"]
            t = ipc.open_file(got).read_all()
            pred.append(t["predicted_tip"].to_numpy())
            pq.write_table(t, os.path.join(outdir, f"part-{i:05d}.parquet"))
            os.remove(trips); os.remove(got)
        preds[precision] = np.concatenate(pred)
        v = {"rows": len(preds[precision]), "network_s": round(network_s, 3)}
        if ref is not None:
            d = digest(a.gate_canon, outdir)
            v.update(rows=d["rows"], hash_exact=d["hash_exact"] == ref["hash_exact"],
                     hash_all=d["hash_all"] == ref["hash_all"])
        verdict[precision] = v
        print(json.dumps({precision: v}), flush=True)
    if ref is None:
        # No gate-canon here: workloads.score on this machine's NumPy, row for row, is the reference.
        mine = np.concatenate([numpy_score(src) for src in a.parquet])
        verdict["rows"] = len(mine)
        verdict["float64"]["bit_equal_rows"] = int((preds["float64"].view(np.int64) == mine.view(np.int64)).sum())
        verdict["float64"]["hash_all"] = verdict["float64"]["bit_equal_rows"] == len(mine)
        verdict["float32"]["hash_exact"] = True  # the exact columns are the input's own, passed through
        ref = {"rows": len(mine)}
    else:
        verdict["rows"] = ref["rows"]
    # float32 against float64 (which is 0.6.6's rows when its hash_all matches), row for row: the
    # declared tolerance on the network's output, plus one step of the sixth decimal for the round.
    diff = np.abs(preds["float32"] - preds["float64"])
    verdict["float32"]["max_abs_vs_float64_rows"] = float(diff.max())
    verdict["float32"]["rows_past_gate_tolerance_1.1e-6"] = int((diff > 1.1e-6).sum())
    verdict["ok"] = bool(verdict["float64"]["hash_all"] and verdict["float32"]["hash_exact"]
                         and verdict["float64"]["rows"] == ref["rows"])
    with open(os.path.join(a.out, "score.json"), "w") as f:
        json.dump(verdict, f, indent=1)
    print(json.dumps(verdict))


# The conformance corpus (crates/moruna-kernels/conformance/mlp_forward/, in the catalogue's
# format, crates/moruna-kernels/src/conformance.rs): the expected values are NumPy's float64
# forward pass, independent of the CPU reference, and each element's allowed error is the
# specification's u * (s(x) + 4|y64|).

ACTS = {"identity": lambda z: z, "relu": lambda z: np.where((z > 0) | np.isnan(z), z, 0.0),
        "sigmoid": lambda z: 1.0 / (1.0 + np.exp(-z)), "tanh": np.tanh}
CORPUS = os.path.join(HERE, "..", "..", "crates", "moruna-kernels", "conformance", "mlp_forward")


def numpy_forward(layers, X):
    """The specification's arithmetic in float64: (x @ W) + b, then the activation; a layer with
    no bias adds nothing. `layers` is [(W inputs x outputs, b or None, activation)]."""
    h = X
    for W, b, act in layers:
        z = h @ W
        if b is not None:
            z = z + b
        h = ACTS[act](z)
    return h


def numpy_scale(layers, X):
    """s(x): |x| through |W| and |b|, no activations, in float64."""
    s = np.abs(X)
    for W, b, _ in layers:
        s = s @ np.abs(W)
        if b is not None:
            s = s + np.abs(b)
    return s


def score_layers():
    return [(w._W1, w._B1, "relu"), (w._W2, w._B2, "relu"), (w._W3, None, "identity")]


def random_layers(seed, widths, activations, bias=True):
    rng = np.random.default_rng(seed)
    return [(rng.normal(0.0, 1 / np.sqrt(i), (i, o)), rng.normal(0.0, 0.1, o) if bias else None, a)
            for (i, o), a in zip(zip(widths, widths[1:]), activations)]


class Corpus:
    """The cases and the files they name, written into `dir`."""

    def __init__(self, dir):
        self.dir, self.cases, self.files = dir, [], set()
        os.makedirs(dir, exist_ok=True)

    def write(self, name, columns, batches=None):
        """An IPC file of one batch of `columns` ({name: array}), or of `batches`."""
        if name not in self.files:
            batches = batches or [pa.record_batch(list(columns.values()), names=list(columns))]
            with ipc.new_file(os.path.join(self.dir, name), batches[0].schema) as f:
                for b in batches:
                    f.write_batch(b)
            self.files.add(name)
        return name

    def model(self, prefix, layers):
        """The model handle: one batch per layer, one row per output unit."""
        arrays = {}
        for l, (W, b, _) in enumerate(layers, 1):
            cols = {"weights": pa.FixedSizeListArray.from_arrays(
                pa.array(W.T.reshape(-1), pa.float64()), W.shape[0])}
            if b is not None:
                cols["bias"] = pa.array(b, pa.float64())
            arrays[f"layer_{l}"] = {"file": self.write(f"{prefix}.layer_{l}.arrow", cols)}
        return arrays

    def add(self, name, tests, about, params, arrays, inputs, expect=None, allowed=None, error=None):
        case = {"name": name, "version": 1, "tests": tests, "about": about, "params": params}
        if arrays:
            case["arrays"] = arrays
        case["inputs"] = inputs
        if error:
            case["error"] = error
        else:
            case["expected"] = expect
            if allowed:
                case["allowed"] = {params.get("output", "y"): {"file": allowed}}
        self.cases.append(case)

    def finish(self):
        index = {"format": "moruna-kernel-corpus/1", "kernel": "moruna.mlp_forward",
                 "cases": self.cases}
        with open(os.path.join(self.dir, "index.json"), "w") as f:
            json.dump(index, f, indent=2)
            f.write("\n")


def expected_columns(layers, precision, X, null_rows=(), output="y"):
    """The expected output batch and each element's allowed error: NumPy's float64 pass on X,
    zeros under null rows, written in the precision's type; u * (s(x) + 4|y64|), plus, in float32, the
    distance from y64 to the float32 value written. Null rows and non-finite values allow nothing:
    their allowed error is null, bit for bit (a null must be null, a NaN NaN)."""
    y = numpy_forward(layers, X)
    with np.errstate(invalid="ignore", over="ignore"):
        tol = UNIT[precision] * (numpy_scale(layers, X) + 4.0 * np.abs(y))
    valid = np.ones(len(X), dtype=bool)
    valid[list(null_rows)] = False
    y[~valid] = 0.0
    if precision == "float32":
        with np.errstate(over="ignore", invalid="ignore"):
            tol = tol + np.abs(y.astype(np.float32).astype(np.float64) - y)
    # Null rows, and non-finite values or bounds, allow nothing: null, compared bit for bit.
    exact = ~valid[:, None] | ~np.isfinite(y) | ~np.isfinite(tol)
    tol[exact] = 0.0
    width = y.shape[1]
    ftype = pa.float32() if precision == "float32" else pa.float64()
    if width == 1:
        out = pa.array(y[:, 0].astype(ftype.to_pandas_dtype()), ftype, mask=~valid)
        allowed = pa.array(tol[:, 0], pa.float64(), mask=exact[:, 0])
    else:
        values = pa.array(y.reshape(-1).astype(ftype.to_pandas_dtype()), ftype)
        out = pa.FixedSizeListArray.from_arrays(
            values, type=pa.list_(pa.field("item", ftype, nullable=False), width),
            mask=pa.array(~valid))
        allowed = pa.FixedSizeListArray.from_arrays(
            pa.array(tol.reshape(-1), pa.float64(), mask=exact.reshape(-1)), width)
    schema = pa.schema([pa.field(output, out.type, nullable=True)])
    return pa.record_batch([out], schema=schema), allowed


def columns_of(X, names, dtype=pa.float64(), nulls=None):
    """{name: Float array of dtype}; `nulls` maps a column index to its null rows."""
    out = {}
    for j, n in enumerate(names):
        mask = np.zeros(len(X), dtype=bool)
        mask[list((nulls or {}).get(j, []))] = True
        out[n] = pa.array(X[:, j].astype(dtype.to_pandas_dtype()), dtype, mask=mask)
    return out


def list_of(X, dtype=pa.float64(), null_rows=(), null_values=()):
    """One FixedSizeList<dtype, k> array of X's rows; whole rows and single values may be null."""
    n, k = X.shape
    vmask = np.zeros(n * k, dtype=bool)
    for r, j in null_values:
        vmask[r * k + j] = True
    values = pa.array(X.reshape(-1).astype(dtype.to_pandas_dtype()), dtype, mask=vmask)
    rmask = np.zeros(n, dtype=bool)
    rmask[list(null_rows)] = True
    return pa.FixedSizeListArray.from_arrays(values, k, mask=pa.array(rmask))


def corpus(norm_parquet, dir=CORPUS):
    """Write mlp_forward's corpus. Score's cases read January 2024's first 600 trips."""
    c = Corpus(dir)

    def case(name, tests, about, layers, precision, inputs_file, X, prefix, input_params,
             null_rows=(), slice_=None):
        params = {"precision": precision, "activations": [a for _, _, a in layers], **input_params}
        out = params.get("output", "y")
        at = slice_[0] if slice_ else 0
        Xe = X if slice_ is None else X[at:at + slice_[1]]
        y, allowed = expected_columns(layers, precision, Xe, [r - at for r in null_rows], out)
        source = {"file": inputs_file}
        if slice_ is not None:
            source["slice"] = list(slice_)
        c.add(name, tests, about, params, c.model(prefix, layers), [source],
              expect=c.write(f"{name}.expected.arrow", None, [y]),
              allowed=c.write(f"{name}.allowed.arrow", {out: allowed}))

    # Score's network, its features exactly as workloads.score builds them.
    t = pq.read_table(norm_parquet, columns=COLS).slice(0, 600).combine_chunks()
    X = features(t.to_batches()[0])
    names = [f"x{i}" for i in range(23)]
    score_in = c.write("score.in.arrow", columns_of(X, names))
    score = {"inputs": names, "output": "predicted_tip"}
    for p in ("float64", "float32"):
        case(f"score_{p}", ["params", "rounding"],
             f"Score's 23-128-128-1 network on January 2024's first 600 trips in {p}, across the "
             "CPU reference's 256-row blocks.", score_layers(), p, score_in, X, "score", score)
    case("score_sliced", ["sliced"], "Score's network on a batch whose arrays start at row 250 of "
         "the file (rows 250 to 529).", score_layers(), "float64", score_in, X, "score", score,
         slice_=(250, 280))

    rng = np.random.default_rng(46)
    # Every activation; a list output of width 3; layers with and without a bias.
    for i, a in enumerate(["identity", "relu", "sigmoid", "tanh"]):
        layers = random_layers(100 + i, [4, 6, 3], [a, a], bias=a != "tanh")
        Xa = rng.normal(0.0, 2.0, (40, 4))
        nm = [f"f{j}" for j in range(4)]
        no_bias = " with no bias" if a == "tanh" else ""
        case(f"activation_{a}", ["params", "nested"],
             f"{a} on both layers{no_bias}, a FixedSizeList<Float64, 3> output.", layers,
             "float64", c.write(f"activation_{a}.in.arrow", columns_of(Xa, nm)), Xa,
             f"activation_{a}", {"inputs": nm})
    small = random_layers(7, [3, 8, 1], ["relu", "identity"])
    Xs = rng.normal(0.0, 1.0, (300, 3))
    nm = ["a", "b", "c"]
    case("list_input", ["params"], "One FixedSizeList<Float64, 3> column; the other column is "
         "ignored.", small, "float64",
         c.write("list_input.in.arrow", {"id": pa.array(np.arange(300)), "f": list_of(Xs)}),
         Xs, "small", {"input": "f"})
    X32 = Xs.astype(np.float32).astype(np.float64)
    case("float32_columns", ["params", "rounding"], "Float32 columns, widened exactly to float64.",
         small, "float64", c.write("float32_columns.in.arrow", columns_of(X32, nm, pa.float32())),
         X32, "small", {"inputs": nm})
    X16 = Xs.astype(np.float16).astype(np.float64)
    case("float16_columns", ["params", "rounding"], "Float16 columns, widened exactly to float64.",
         small, "float64", c.write("float16_columns.in.arrow", columns_of(X16, nm, pa.float16())),
         X16, "small", {"inputs": nm})
    case("float32_list", ["params", "rounding"], "A FixedSizeList<Float32, 3> column computed in "
         "float32.", small, "float32",
         c.write("float32_list.in.arrow", {"f": list_of(X32, pa.float32())}), X32, "small",
         {"input": "f"})
    case("narrowed_to_float32", ["params", "rounding"], "Float64 columns narrowed to float32, to "
         "nearest.", small, "float32", c.write("narrowed.in.arrow", columns_of(Xs, nm)), Xs,
         "small", {"inputs": nm})
    # Nulls.
    Xn = rng.normal(0.0, 1.0, (12, 3))
    Xz = Xn.copy(); Xz[[4]] = 0.0
    case("null_column_value", ["nulls"], "A null in one column gives a null row of zeros; the "
         "other rows are as without it.", small, "float64",
         c.write("null_column.in.arrow", columns_of(Xn, nm, nulls={1: [4]})), Xz, "small",
         {"inputs": nm}, null_rows=[4])
    Xz = Xn.copy(); Xz[[2, 9]] = 0.0
    case("null_list_and_value", ["nulls"], "A null list and a null value inside a list each give "
         "a null row.", small, "float64",
         c.write("null_list.in.arrow", {"f": list_of(Xn, null_rows=[2], null_values=[(9, 0)])}),
         Xz, "small", {"input": "f"}, null_rows=[2, 9])
    # NaN, overflow, empty, two batches.
    Xnan = rng.normal(0.0, 1.0, (5, 3)); Xnan[1, 2] = np.nan
    case("nan_row", ["overflow"], "A NaN makes its own row NaN and no other.", small, "float64",
         c.write("nan.in.arrow", columns_of(Xnan, nm)), Xnan, "small", {"inputs": nm})
    ten = [(np.array([[10.0]]), None, "identity")]
    Xo = np.array([[1e308], [-1e308], [1.0], [np.inf]])
    case("overflow", ["overflow"], "A product past Float64's range is an infinity of its sign, "
         "never an error.", ten, "float64", c.write("overflow.in.arrow", columns_of(Xo, ["v"])), Xo,
         "ten", {"inputs": ["v"]})
    case("empty", ["empty"], "A batch of zero rows gives a batch of zero rows.", small,
         "float64", c.write("empty.in.arrow", columns_of(np.empty((0, 3)), nm)),
         np.empty((0, 3)), "small", {"inputs": nm})
    two = [pa.record_batch(list(columns_of(Xs[a:b], nm).values()), names=nm)
           for a, b in ((0, 100), (100, 300))]
    case("two_batches", ["multi_batch"], "Two batches give their rows in order.", small,
         "float64", c.write("two.in.arrow", None, two), Xs, "small", {"inputs": nm})
    # Refusals.
    model = c.model("small", small)
    base = {"precision": "float64", "activations": ["relu", "identity"], "inputs": nm}
    src = [{"file": "float32_columns.in.arrow"}]
    c.add("unknown_activation", ["error", "params"], "An activation the specification does not "
          "name is refused at configure.", {**base, "activations": ["relu", "softmax"]}, model,
          src, error={"at": "configure", "contains": "softmax"})
    bad = c.write("bad.layer_2.arrow", {
        "weights": pa.FixedSizeListArray.from_arrays(pa.array([1.0, 2.0]), 2)})
    c.add("layers_do_not_chain", ["error", "params"], "A layer whose input count is not the "
          "previous layer's output count is refused at configure.", base,
          {"layer_1": model["layer_1"], "layer_2": {"file": bad}}, src,
          error={"at": "configure", "contains": "layer 1 gives 8 outputs"})
    c.add("missing_column", ["error"], "A named column the input has not is refused at plan.",
          {**base, "inputs": ["a", "b", "z"]}, model, src,
          error={"at": "schema", "contains": "`z`"})
    wide = c.write("wide_list.in.arrow", {"f": list_of(rng.normal(0.0, 1.0, (2, 4)))})
    c.add("wrong_list_size", ["error"], "A list column of another size is refused at plan.",
          {"precision": "float64", "activations": ["relu", "identity"], "input": "f"}, model,
          [{"file": wide}], error={"at": "schema", "contains": "FixedSizeList"})
    c.finish()
    return c


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("export"); e.add_argument("out")
    k = sub.add_parser("corpus"); k.add_argument("parquet"); k.add_argument("--dir", default=CORPUS)
    m = sub.add_parser("measure")
    m.add_argument("bin"); m.add_argument("parquet"); m.add_argument("out")
    m.add_argument("--rows", default="10000,100000,1000000"); m.add_argument("--reps", type=int, default=5)
    s = sub.add_parser("score")
    s.add_argument("bin"); s.add_argument("reference"); s.add_argument("out"); s.add_argument("parquet", nargs="+")
    s.add_argument("--gate-canon", default=os.path.expanduser("~/moruna-data/bin/gate-canon"))
    a = p.parse_args()
    if a.cmd == "export":
        export(a.out)
    elif a.cmd == "corpus":
        corpus(a.parquet, a.dir)
    elif a.cmd == "measure":
        measure(a)
    else:
        score(a)


if __name__ == "__main__":
    main()
