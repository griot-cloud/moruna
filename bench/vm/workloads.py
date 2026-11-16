"""The benchmark's functions: one RecordBatch in, one out. Moruna and every baseline call these same functions."""
import pyarrow as pa, pyarrow.compute as pc, numpy as np

def enrich(batch: pa.RecordBatch) -> pa.RecordBatch:
    """W1, vectorised: trip duration, speed, fare per mile, tip share; drop impossible trips."""
    dur = pc.divide(pc.cast(pc.subtract(batch["dropoff_at"], batch["pickup_at"]), pa.int64()), 1_000_000.0)
    dist, fare, tip = batch["trip_distance"], batch["fare_amount"], batch["tip_amount"]
    ok = pc.and_(pc.and_(pc.greater_equal(dur, 60.0), pc.less_equal(dur, 4 * 3600.0)),
                 pc.and_(pc.and_(pc.greater(dist, 0.0), pc.less(dist, 100.0)), pc.greater(fare, 0.0)))
    out = batch.append_column("duration_s", dur)
    out = out.append_column("mph", pc.divide(dist, pc.divide(dur, 3600.0)))
    out = out.append_column("fare_per_mile", pc.divide(fare, dist))
    out = out.append_column("tip_share", pc.divide(tip, fare))
    return out.filter(ok)

def audit(batch: pa.RecordBatch) -> pa.RecordBatch:
    """W2, pure Python per trip: re-price each trip from a simplified meter rule and flag the gap."""
    dist = batch["trip_distance"].to_pylist(); fare = batch["fare_amount"].to_pylist()
    pu = batch["pickup_at"].cast(pa.int64()).to_pylist(); do = batch["dropoff_at"].cast(pa.int64()).to_pylist()
    rate = batch["ratecode_id"].to_pylist()
    expected, flag = [], []
    for d, f, a, b, r in zip(dist, fare, pu, do, rate):
        minutes = max((b - a) / 60e6, 0.0)
        if r == 2:
            e = 70.0
        else:
            e = 3.0 + 3.5 * (d or 0.0) + 0.35 * max(minutes - (d or 0.0) * 2.0, 0.0)
            if 16 <= (a // 3_600_000_000) % 24 < 20:
                e += 2.5
        expected.append(round(e, 2))
        flag.append(f is not None and abs(f - e) > max(5.0, 0.25 * e))
    return pa.record_batch([batch["vendor_id"], batch["pu_location_id"], batch["fare_amount"],
                            pa.array(expected, pa.float64()), pa.array(flag, pa.bool_())],
                           names=["vendor_id", "pu_location_id", "fare_amount", "expected_fare", "flagged"])

def occupancy(batch: pa.RecordBatch) -> pa.RecordBatch:
    """W3, memory-heavy: expand every trip into one row per minute it ran, then count cars per
    pickup zone per minute-of-day. The working set is ~15-20x the input batch."""
    a = batch["pickup_at"].cast(pa.int64()).to_numpy(zero_copy_only=False) // 60_000_000
    b = batch["dropoff_at"].cast(pa.int64()).to_numpy(zero_copy_only=False) // 60_000_000
    zone = batch["pu_location_id"].to_numpy(zero_copy_only=False)
    n = np.clip(b - a, 0, 240) + 1
    idx = np.repeat(np.arange(len(n)), n)
    minute = a[idx] + (np.arange(len(idx)) - np.repeat(np.cumsum(n) - n, n))
    key = zone[idx] * 1440 + minute % 1440
    keys, counts = np.unique(key, return_counts=True)
    return pa.record_batch([pa.array(keys // 1440), pa.array(keys % 1440), pa.array(counts)],
                           names=["zone", "minute_of_day", "car_minutes"])

# W4, batch model inference: a fixed two-layer network scores every trip (predicted tip).
# Typical of what needs a UDF: a model, not an expression.
_rng = np.random.default_rng(7)
_EMB = _rng.normal(0.0, 0.5, (300, 8))
_W1 = _rng.normal(0.0, 1 / np.sqrt(23), (23, 128)); _B1 = _rng.normal(0.0, 0.1, 128)
_W2 = _rng.normal(0.0, 1 / np.sqrt(128), (128, 128)); _B2 = _rng.normal(0.0, 0.1, 128)
_W3 = _rng.normal(0.0, 1 / np.sqrt(128), (128, 1))

def score(batch: pa.RecordBatch) -> pa.RecordBatch:
    """W4: features from each trip, a 23-128-128-1 network, one prediction per trip."""
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
    X[:, 7:15] = _EMB[np.clip(num("pu_location_id", 0.0).astype(np.int64), 0, 299)]
    X[:, 15:23] = _EMB[np.clip(num("do_location_id", 0.0).astype(np.int64), 0, 299)]
    h = np.maximum(X @ _W1 + _B1, 0.0)
    h = np.maximum(h @ _W2 + _B2, 0.0)
    y = np.round((h @ _W3)[:, 0], 6)
    return pa.record_batch([batch["vendor_id"], batch["pu_location_id"], batch["fare_amount"], pa.array(y)],
                           names=["vendor_id", "pu_location_id", "fare_amount", "predicted_tip"])
