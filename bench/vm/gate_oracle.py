"""The reference oracle for the gate's Rust ports (architecture/arrow-core-0.7.md section 9).

It builds synthetic input batches in each job's real input schema, runs the Python function
0.6.6 ran on them (workloads.py, ooc_workloads.py), and writes the inputs and the expected
outputs as Arrow IPC files under images/gate/fixtures/, with a manifest of their canonical
row counts and hashes (canon.py). The Rust tests read those files and need no Python.

It also exports the constants the functions draw from NumPy's generator (Score's weights,
FineWeb's MinHash permutations) bit for bit, so the ports embed them instead of reimplementing
the generator, and a table of Python's round(x, 2) for Audit.

    python3 -I bench/vm/gate_oracle.py [JOB...]   # from the repository root
    python3 -I bench/vm/gate_oracle.py --check-constants   # on the benchmark VM: the
        # constants this machine's NumPy draws are the ones committed

Every input is seeded; a rerun on the same Python, NumPy and pyarrow writes the same bytes.
"""
import hashlib
import json
import os
import platform
import struct
import sys

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.ipc as ipc

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, HERE)
import canon  # noqa: E402
import workloads as w  # noqa: E402

FIX = os.path.join(ROOT, "images", "gate", "fixtures")
DATA = os.path.join(ROOT, "images", "gate", "functions", "data")

# ---------------------------------------------------------------------------------------------
# Constants drawn from NumPy's generator, exported once.


def score_weights() -> bytes:
    parts = [w._EMB, w._W1, w._B1, w._W2, w._B2, w._W3]
    return b"".join(np.ascontiguousarray(p, dtype="<f8").tobytes() for p in parts)


def score_trig() -> bytes:
    """sin and cos of hour * (pi / 12) for hours 0 to 23, as NumPy computes them in score().
    Taken over a long array, so the vector loop computes them as it does on a real batch."""
    hour = np.tile(np.arange(24, dtype=np.float64), 4096)
    s = np.sin(hour * (np.pi / 12))
    c = np.cos(hour * (np.pi / 12))
    s24, c24 = s[:24], c[:24]
    assert all((s[i * 24:(i + 1) * 24] == s24).all() for i in range(4096)), "sin differs by place"
    assert all((c[i * 24:(i + 1) * 24] == c24).all() for i in range(4096)), "cos differs by place"
    return s24.astype("<f8").tobytes() + c24.astype("<f8").tobytes()


def minhash_ab() -> bytes:
    import ooc_workloads as ooc
    return ooc._A.astype("<u8").tobytes() + ooc._B.astype("<u8").tobytes()


CONSTANTS = {"score_weights.bin": score_weights, "score_trig.bin": score_trig,
             "minhash_ab.bin": minhash_ab}


def write_constants() -> dict:
    os.makedirs(DATA, exist_ok=True)
    out = {}
    for name, make in CONSTANTS.items():
        try:
            data = make()
        except ImportError as e:  # ooc_workloads needs nothing beyond numpy and pyarrow
            print(f"skip {name}: {e}")
            continue
        open(os.path.join(DATA, name), "wb").write(data)
        out[name] = hashlib.sha256(data).hexdigest()
    return out


def check_constants() -> int:
    bad = 0
    for name, make in CONSTANTS.items():
        have = open(os.path.join(DATA, name), "rb").read()
        now = make()
        same = have == now
        bad += not same
        print(f"{name}: {'same' if same else 'DIFFERENT'} ({hashlib.sha256(now).hexdigest()[:16]})")
    return 1 if bad else 0


# ---------------------------------------------------------------------------------------------
# Python's round(x, 2), the one Audit calls on every trip.


def pyround_table() -> bytes:
    rng = np.random.default_rng(1)
    xs = []
    for scale in (1e-3, 1.0, 10.0, 100.0, 1e4, 1e8, 1e12, 4e13, 1e14, 1e15, 4.4e15):
        xs += list(rng.uniform(-scale, scale, 2000))
    for k in range(-4000, 4000):  # the exact ties a double can hold: odd multiples of 1/8
        xs.append(k / 8.0)
        xs.append(k / 8.0 + 1e6)
    ties = [0.125, 0.375, 0.625, 0.875, 2.675, 1.005, 1.015, 1.025, 0.005, 0.015, 0.045]
    for t in ties:
        for s in (1.0, -1.0):
            x = s * t
            xs += [x, np.nextafter(x, np.inf), np.nextafter(x, -np.inf)]
    xs += [0.0, -0.0, 1e-320, -1e-320, 5e-324, 0.004999999999999999, 0.005, -0.005,
           2.0 ** 52, 2.0 ** 52 + 0.5, 2.0 ** 53, 1e300, -1e300, 1.7976931348623157e308,
           float("inf"), float("-inf"), float("nan"), 4503599627370495.5, 45035996273704.96,
           90071992547409.91, 70368744177663.99, 70368744177663.995]
    out = bytearray()
    for x in xs:
        x = float(x)  # a Python float: NumPy's float64 rounds by its own rule
        out += struct.pack("<dd", x, round(x, 2))
    return bytes(out)


# ---------------------------------------------------------------------------------------------
# NYC: the normalized yellow-taxi schema of normalize.py.

US = 1_000_000
BASE_2024 = 1_704_067_200 * US


def _maybe_null(rng, values, frac, typ):
    mask = rng.random(len(values)) < frac
    return pa.array([None if m else v for v, m in zip(values, mask)], typ)


def nyc_batch(seed: int, n: int, *, null_times=False, null_zone=False, huge=False) -> pa.RecordBatch:
    """Trips in normalize.py's schema: realistic values, then edge values and nulls. The
    timestamps stay within +/- 2**53 microseconds, the domain in which 0.6.6's integer and
    float paths agree (see images/gate/README.md)."""
    from normalize import SCHEMA
    rng = np.random.default_rng(seed)
    pickup = BASE_2024 + rng.integers(0, 366 * 86400, n) * US + rng.integers(0, US, n)
    kind = rng.random(n)
    dur = np.where(kind < 0.80, rng.integers(60 * US, 3600 * US, n),          # ordinary trips
          np.where(kind < 0.85, rng.integers(0, 60 * US, n),                  # under a minute
          np.where(kind < 0.90, rng.integers(-3600 * US, 0, n),               # dropoff first
          np.where(kind < 0.95, rng.integers(4 * 3600 * US, 48 * 3600 * US, n),  # long
                   rng.integers(3600 * US, 5 * 3600 * US, n)))))
    dropoff = pickup + dur
    # Edge instants: the epoch, before it, round hours, the 16:00 and 20:00 borders of the
    # surcharge, minute borders, and the ends of the 2**53 domain.
    edges = [0, -1, -US, -3600 * US * 24 * 365 * 30, 16 * 3600 * US - 1, 16 * 3600 * US,
             20 * 3600 * US - 1, 20 * 3600 * US, 60 * US - 1, 60 * US, 2 ** 52, -(2 ** 52)]
    for i, e in enumerate(edges):
        pickup[i] = e
        dropoff[i] = e + [0, 1, 60 * US, 241 * 60 * US, -1, 3600 * US][i % 6]
    if huge:
        dropoff[len(edges)] = pickup[len(edges)] + 2 ** 53 + 1
    dist = rng.exponential(3.0, n)
    dist[rng.random(n) < 0.03] = 0.0
    dist[rng.random(n) < 0.02] *= -1
    dist[rng.random(n) < 0.02] = rng.uniform(100, 400)
    dist[rng.random(n) < 0.01] = np.nan
    dist[:6] = [0.0, -0.0, 99.99999999999999, 100.0, 200.0, 1e-300]
    fare = np.round(3.0 + dist * 2.5 + rng.normal(0, 3, n), 2)
    fare[rng.random(n) < 0.03] = 0.0
    fare[rng.random(n) < 0.03] *= -1
    fare[rng.random(n) < 0.01] = np.nan
    fare[rng.random(n) < 0.01] = rng.uniform(500, 5000)
    rate = rng.choice([1.0, 1.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 99.0, np.nan], n)
    pas = rng.choice([0.0, 1.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, np.nan], n)
    zone_pu = rng.integers(1, 266, n)
    zone_do = rng.integers(1, 266, n)
    zone_pu[:8] = [0, -1, 264, 265, 299, 300, 1_000_000, -1_000_000]
    zone_do[:8] = [300, 299, 0, -5, 1, 2, 3, 2 ** 40]
    flags = rng.choice(["N", "Y", "é", "日本"], n).tolist()
    nullable = 0.03
    cols = {
        "vendor_id": _maybe_null(rng, rng.choice([1, 2, 6], n).tolist(), nullable, pa.int64()),
        "pickup_at": pa.array(pickup, pa.int64()).cast(pa.timestamp("us")),
        "dropoff_at": pa.array(dropoff, pa.int64()).cast(pa.timestamp("us")),
        "passenger_count": _maybe_null(rng, pas.tolist(), nullable, pa.float64()),
        "trip_distance": _maybe_null(rng, dist.tolist(), nullable, pa.float64()),
        "ratecode_id": _maybe_null(rng, rate.tolist(), nullable, pa.float64()),
        "store_and_fwd_flag": _maybe_null(rng, flags, nullable, pa.string()),
        "pu_location_id": (_maybe_null(rng, zone_pu.tolist(), nullable, pa.int64()) if null_zone
                           else pa.array(zone_pu, pa.int64())),
        "do_location_id": _maybe_null(rng, zone_do.tolist(), nullable, pa.int64()),
        "payment_type": _maybe_null(rng, rng.integers(0, 6, n).tolist(), nullable, pa.int64()),
        "fare_amount": _maybe_null(rng, fare.tolist(), nullable, pa.float64()),
    }
    for c in ("extra", "mta_tax", "tip_amount", "tolls_amount", "improvement_surcharge",
              "total_amount", "congestion_surcharge"):
        v = np.round(rng.exponential(2.0, n), 2)
        v[rng.random(n) < 0.01] = np.nan
        cols[c] = _maybe_null(rng, v.tolist(), nullable, pa.float64())
    cols["airport_fee"] = pa.nulls(n, pa.float64())  # a column a month lacks, as normalize.py fills it
    if null_times:
        mask = rng.random(n) < 0.02
        mask[3] = True
        cols["pickup_at"] = pa.array([None if m else v for v, m in zip(pickup.tolist(), mask)],
                                     pa.int64()).cast(pa.timestamp("us"))
    arrays = [cols[f.name].cast(f.type) for f in SCHEMA]
    return pa.RecordBatch.from_arrays(arrays, schema=SCHEMA)


def nyc_cases(job: str):
    """(case name, input batch) for one NYC job. Cases a 0.6.6 function raises on are kept: the
    port must refuse them too."""
    big = nyc_batch(7, 4000)
    yield "main", big
    yield "empty", big.slice(0, 0)
    yield "one", big.slice(20, 1)
    yield "offset", big.slice(1001, 517)  # arrays that start mid-buffer
    # Audit and Occupancy raise on a null timestamp; Enrich and Score compute through it.
    yield "null_times", nyc_batch(9, 600, null_times=True)
    if job == "occupancy":
        yield "null_zone", nyc_batch(11, 600, null_zone=True)  # 0.6.6 returns float columns
    if job == "enrich":
        yield "huge_duration", nyc_batch(13, 100, huge=True)  # pyarrow refuses the cast


# ---------------------------------------------------------------------------------------------
# FineWeb: the sample/100BT parquet schema.

FW_SCHEMA = pa.schema([("text", pa.string()), ("id", pa.string()), ("dump", pa.string()), ("url", pa.string()),
                       ("date", pa.string()), ("file_path", pa.string()), ("language", pa.string()),
                       ("language_score", pa.float64()), ("token_count", pa.int64())])
WORDS = ("the of and to in a is that for it as was with be by on not he I this are or his from at which "
         "but have an they you were her she there been one all we their has would when if so no will more "
         "data model river Street café naïve Straße İstanbul ΟΔΟΣ Σίσυφος 東京 데이터 🙂 x1 2024 e-mail").split()
SPACES = [" ", " ", " ", "  ", "\n", "\t", "\u00a0", "\u3000", "\u001c", "\u2028", " \r\n "]


def fw_batch(seed: int, n: int) -> pa.RecordBatch:
    rng = np.random.default_rng(seed)
    texts = []
    for i in range(n):
        k = int(rng.choice([0, 1, 3, 4, 5, 6, 12, 50, 300, 2000], p=[.03, .03, .04, .04, .04, .05, .17, .3, .27, .03]))
        words = [str(rng.choice(WORDS)) for _ in range(k)]
        if rng.random() < 0.2:
            words = [w.upper() for w in words]
        sep = [str(rng.choice(SPACES)) for _ in range(k)]
        texts.append("".join(w + s for w, s in zip(words, sep)) if rng.random() < 0.5 else " ".join(words))
    texts[:6] = [None, "", "   ", "ΟΔΟΣ ΟΔΟΣ ΟΔΟΣ ΟΔΟΣ ΟΔΟΣ", "İI ıi ß ẞ ǅ", "a\u001cb\u001dc\u001ed\u001fe f g"]
    mask = [t is None for t in texts]
    cols = {
        "text": pa.array(texts, pa.string()),
        "id": pa.array([f"<urn:uuid:{seed}-{i}>" for i in range(n)]),
        "dump": pa.array(["CC-MAIN-2024-10"] * n),
        "url": pa.array([None if i % 17 == 0 else f"https://example.org/{i}" for i in range(n)]),
        "date": pa.array(["2024-02-21T00:00:00Z"] * n),
        "file_path": pa.array(["s3://commoncrawl/x.warc.gz"] * n),
        "language": pa.array(["en"] * n),
        "language_score": pa.array(rng.random(n)),
        "token_count": pa.array([0 if m else len(t.split()) for t, m in zip(texts, mask)], pa.int64()),
    }
    return pa.RecordBatch.from_arrays([cols[f.name] for f in FW_SCHEMA], schema=FW_SCHEMA)


def fw_cases(job: str):
    big = fw_batch(21, 400)
    yield "main", big
    yield "empty", big.slice(0, 0)
    yield "offset", big.slice(37, 101)


# ---------------------------------------------------------------------------------------------
# Overture buildings: the 2026-10-21.0 release's schema, the columns the two functions read and a
# few beside them (a map, a list of structs of lists) that a reader must step over.

SOURCE = pa.struct([("property", pa.string()), ("dataset", pa.string()), ("record_id", pa.string()),
                    ("update_time", pa.string()), ("confidence", pa.float64())])
RULE = pa.struct([("variant", pa.string()), ("language", pa.string()), ("value", pa.string()),
                  ("between", pa.list_(pa.float64())), ("side", pa.string())])
OV_SCHEMA = pa.schema([
    ("id", pa.string()), ("geometry", pa.binary()),
    ("bbox", pa.struct([("xmin", pa.float32()), ("xmax", pa.float32()), ("ymin", pa.float32()), ("ymax", pa.float32())])),
    ("version", pa.int32()), ("sources", pa.list_(SOURCE)), ("level", pa.int32()), ("subtype", pa.string()),
    ("class", pa.string()), ("height", pa.float64()),
    ("names", pa.struct([("primary", pa.string()), ("common", pa.map_(pa.string(), pa.string())),
                         ("rules", pa.list_(RULE))])),
    ("has_parts", pa.bool_()), ("num_floors", pa.int32()), ("roof_shape", pa.string()),
])
SUBTYPES = [None, "", "residential", "commercial", "industrial", "education", "religious", "agricultural",
            "transportation", "medical", "outbuilding", "unknown"]
CLASSES = [None, "", "house", "apartments", "garage", "office", "school", "barn", "shed", "roof", "hangar", "unknown"]
NAMES = [None, "", "   ", "Town Hall", "  ＴＯＷＥＲ\u3000Ｏｎｅ ", "ΟΔΟΣ ΕΡΜΟΥ", "Straße", "İSTANBUL Kulesi",
         "ﬁre station", "Ⅻ Apostles", "a\u001cb", "東京タワー"]


def _shapes(rng, n, kinds):
    import shapely
    out = []
    for i in range(n):
        kind = kinds[i % len(kinds)]
        lon = float(rng.uniform(-179.9, 179.9))
        lat = float(rng.uniform(-60, 70))
        if i % 23 == 0:
            lat = float(rng.choice([89.0, -88.0, 85.06, -85.06]))
        w, h = float(rng.uniform(5e-5, 5e-4)), float(rng.uniform(5e-5, 5e-4))
        box = shapely.box(lon, lat, lon + w, lat + h, ccw=bool(rng.random() < 0.5))
        if kind == "box":
            g = shapely.affinity.rotate(box, float(rng.uniform(0, 90)), origin="centroid")
        elif kind == "hole":
            inner = shapely.box(lon + w / 4, lat + h / 4, lon + w / 2, lat + h / 2, ccw=bool(rng.random() < 0.5))
            g = shapely.Polygon(box.exterior.coords, [inner.exterior.coords])
        elif kind == "multi":
            other = shapely.box(lon + 2 * w, lat, lon + 3 * w, lat + h)
            g = shapely.MultiPolygon([box, other])
        elif kind == "ell":
            g = shapely.Polygon([(lon, lat), (lon + w, lat), (lon + w, lat + h / 2), (lon + w / 2, lat + h / 2),
                                 (lon + w / 2, lat + h), (lon, lat + h), (lon, lat)])
        elif kind == "flat":  # a flat top, and a collinear point on it
            g = shapely.Polygon([(lon, lat), (lon + w, lat), (lon + w, lat + h), (lon + w / 2, lat + h),
                                 (lon, lat + h), (lon, lat)])
        elif kind == "zero":  # no area: a line traced out and back
            g = shapely.Polygon([(lon, lat), (lon + w, lat + h), (lon + 2 * w, lat + 2 * h), (lon, lat)])
        elif kind == "many":
            t = np.linspace(0, 2 * np.pi, 40)[:-1]
            pts = [(lon + w * np.cos(a), lat + h * np.sin(a)) for a in t]
            g = shapely.Polygon(pts)
        else:
            raise ValueError(kind)
        out.append(g)
    return out


def ov_batch(seed: int, n: int, *, other_types=False, null_bound=False, bad_wkb=False,
             empty_geom=False) -> pa.RecordBatch:
    import shapely
    rng = np.random.default_rng(seed)
    geoms = _shapes(rng, n, ["box", "hole", "multi", "ell", "flat", "zero", "many", "box", "box"])
    wkb = [shapely.to_wkb(g) for g in geoms]
    # Big-endian, EWKB with an SRID, and three dimensions: what shapely reads too.
    wkb[1] = shapely.to_wkb(geoms[1], byte_order=0)
    wkb[2] = shapely.to_wkb(shapely.set_srid(geoms[2], 4326), include_srid=True)
    wkb[3] = shapely.to_wkb(shapely.force_3d(geoms[3], 7.0), output_dimension=3)
    wkb[4] = None
    if empty_geom:  # shapely's get_x raises on the empty centroid
        wkb[5] = shapely.to_wkb(shapely.from_wkt("POLYGON EMPTY"))
    if other_types:
        extra = ["POINT (10 20)", "LINESTRING (0 0, 3 4)", "MULTIPOINT ((0 0), (2 2))", "MULTIPOINT ((5 5))",
                 "GEOMETRYCOLLECTION (POINT (1 1), POLYGON ((0 0, 1 0, 1 1, 0 0)))", "LINESTRING (1 1, 1 1)",
                 "MULTILINESTRING ((0 0, 1 1), (2 2, 3 3))"]
        for i, t in enumerate(extra):
            wkb[6 + i] = shapely.to_wkb(shapely.from_wkt(t))
    if bad_wkb:
        wkb[7] = wkb[7][:-3]
    bounds = [shapely.bounds(g) if w is not None else None for g, w in zip(geoms, wkb)]
    bbox = [None if b is None or i % 31 == 0 else {"xmin": b[0], "xmax": b[2], "ymin": b[1], "ymax": b[3]}
            for i, b in enumerate(bounds)]
    if null_bound:
        bbox[1] = {"xmin": None, "xmax": 1.0, "ymin": 0.0, "ymax": 1.0}  # row 1 has a height
    def sources(i):
        k = i % 7
        if k == 0:
            return None
        if k == 1:
            return []
        items = []
        for j in range(k - 1):
            c = rng.choice([None, 0.0, 0.5, 0.9, 0.9, float("nan"), 1.0])
            items.append(None if (i + j) % 5 == 0 else
                         {"property": "", "dataset": rng.choice(["OpenStreetMap", "Microsoft ML Buildings", "Esri", None]),
                          "record_id": f"r{i}-{j}", "update_time": "2024-01-01", "confidence": c})
        return items
    names = []
    for i in range(n):
        if i % 13 == 0:
            names.append(None)
        else:
            names.append({"primary": NAMES[i % len(NAMES)], "common": [("en", "x"), ("fr", "y")] if i % 3 == 0 else None,
                          "rules": [{"variant": "short", "language": None, "value": "v", "between": [0.0, 0.5], "side": None}]
                          if i % 4 == 0 else None})
    height = [None if i % 3 == 0 else float(rng.uniform(2, 120)) for i in range(n)]
    height[6:10] = [float("nan"), 0.0, -0.0, None]
    floors = [None if i % 4 == 0 else int(rng.integers(0, 40)) for i in range(n)]
    cols = {
        "id": pa.array([f"08b{seed:04x}{i:010x}" for i in range(n)]),
        "geometry": pa.array(wkb, pa.binary()),
        "bbox": pa.array(bbox, OV_SCHEMA.field("bbox").type),
        "version": pa.array([int(rng.integers(0, 5)) for _ in range(n)], pa.int32()),
        "sources": pa.array([sources(i) for i in range(n)], pa.list_(SOURCE)),
        "level": pa.array([None if i % 2 else 1 for i in range(n)], pa.int32()),
        "subtype": pa.array([SUBTYPES[i % len(SUBTYPES)] for i in range(n)]),
        "class": pa.array([CLASSES[(i * 5) % len(CLASSES)] for i in range(n)]),
        "height": pa.array(height, pa.float64()),
        "names": pa.array(names, OV_SCHEMA.field("names").type),
        "has_parts": pa.array([bool(i % 2) for i in range(n)]),
        "num_floors": pa.array(floors, pa.int32()),
        "roof_shape": pa.array([None if i % 2 else "flat" for i in range(n)]),
    }
    return pa.RecordBatch.from_arrays([cols[f.name] for f in OV_SCHEMA], schema=OV_SCHEMA)


def ov_cases(job: str):
    big = ov_batch(31, 600)
    yield "main", big
    yield "empty", big.slice(0, 0)
    yield "offset", big.slice(101, 211)
    if job == "ov_shape":
        yield "other_types", ov_batch(33, 60, other_types=True)
        yield "bad_wkb", ov_batch(35, 20, bad_wkb=True)  # shapely raises
        yield "empty_geom", ov_batch(39, 20, empty_geom=True)  # get_x raises on an empty centroid
    if job == "ov_attrs":
        yield "null_bound", ov_batch(37, 20, null_bound=True)  # Python raises on the None bound


# ---------------------------------------------------------------------------------------------


def write_ipc(path: str, batch: pa.RecordBatch) -> None:
    with ipc.new_file(path, batch.schema) as f:
        f.write_batch(batch)


def as_on_x86(job: str, out: pa.RecordBatch) -> pa.RecordBatch:
    """The output as the gate's machine (x86-64) computes it, where NumPy's float-to-integer
    cast is the CPU's: x86-64 gives INT_MIN for NaN, arm64 gives 0. Only ov_shape's tiles of a
    NaN centroid (a null geometry) cast a NaN."""
    if job != "ov_shape" or platform.machine() in ("x86_64", "AMD64"):
        return out
    nan = pc.is_nan(out["lon"])
    cols = {n: out[n] for n in out.schema.names}
    for n in ("tile_x", "tile_y"):
        cols[n] = pc.if_else(nan, pa.scalar(-2 ** 31, pa.int32()), out[n])
    return pa.record_batch([cols[n] for n in out.schema.names], schema=out.schema)


def run_case(job: str, fn, case: str, batch: pa.RecordBatch, dataset: str) -> dict:
    """Runs one case; the input is written once per dataset and shared by its jobs."""
    d = os.path.join(FIX, job)
    os.makedirs(d, exist_ok=True)
    inputs = os.path.join(FIX, "inputs", dataset)
    os.makedirs(inputs, exist_ok=True)
    write_ipc(os.path.join(inputs, f"{case}.arrow"), batch)
    rec = {"rows_in": batch.num_rows, "input": f"inputs/{dataset}/{case}.arrow"}
    expected = os.path.join(d, f"{case}.expected.arrow")
    try:
        out = fn(batch)
    except Exception as e:  # the port must refuse what the function raised on
        rec["error"] = f"{type(e).__name__}: {e}"[:300]
        if os.path.exists(expected):
            os.remove(expected)
        return rec
    out = as_on_x86(job, out)
    write_ipc(expected, out)
    c = canon.canonical(job, pa.Table.from_batches([out]))
    rec.update(rows_out=out.num_rows, rows=c["rows"], hash_exact=c["hash_exact"],
               hash_all=c["hash_all"], schema=[f"{f.name}:{f.type}" for f in out.schema])
    return rec


# job: (function, its cases, the dataset its inputs come from)
import ooc_workloads as ooc  # noqa: E402

JOBS = {"enrich": (w.enrich, nyc_cases, "nyc"), "audit": (w.audit, nyc_cases, "nyc"),
        "occupancy": (w.occupancy, nyc_cases, "nyc"), "score": (w.score, nyc_cases, "nyc"),
        "fw_minhash": (ooc.fw_minhash, fw_cases, "fineweb"),
        "ov_shape": (ooc.ov_shape, ov_cases, "overture"), "ov_attrs": (ooc.ov_attrs, ov_cases, "overture")}


def main(argv):
    if argv[:1] == ["--check-constants"]:
        return check_constants()
    os.makedirs(FIX, exist_ok=True)
    mpath = os.path.join(FIX, "manifest.json")
    manifest = json.load(open(mpath)) if os.path.exists(mpath) else {"jobs": {}}
    import shapely
    manifest["oracle"] = {"python": sys.version.split()[0], "numpy": np.__version__,
                          "pyarrow": pa.__version__, "shapely": shapely.__version__,
                          "geos": ".".join(map(str, shapely.geos_version)),
                          "machine": platform.machine(), "system": platform.system()}
    manifest["constants"] = write_constants()
    open(os.path.join(FIX, "pyround.bin"), "wb").write(pyround_table())
    for job in argv or list(JOBS):
        fn, cases, dataset = JOBS[job]
        manifest["jobs"][job] = {case: run_case(job, fn, case, b, dataset) for case, b in cases(job)}
        print(job, {k: v.get("rows_out", v.get("error", "")[:40]) for k, v in manifest["jobs"][job].items()})
    json.dump(manifest, open(mpath, "w"), indent=1, sort_keys=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
