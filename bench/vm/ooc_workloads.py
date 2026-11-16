"""Out-of-core benchmark workloads (2026-11-14): two datasets far larger than the node's memory,
two functions each, as different from each other and from the NYC set as can be.

FineWeb (HuggingFaceFW/fineweb sample/100BT, web text for LLM pre-training):
  fw_clean  pure Python per document: Gopher/C4-style quality heuristics, then a regex scrub of
            e-mail addresses, IPs and phone numbers; drops documents that fail. String heavy.
  fw_minhash numpy per document: word 5-gram shingles hashed with crc32, 64-permutation MinHash
            signature and 8 LSH band keys, the near-duplicate step of every LLM data pipeline.
            Temporary arrays scale with document length, so memory per batch swings widely.

Overture Maps buildings (2026-10-21.0, ~2.5 B footprints, GeoParquet WKB):
  ov_shape  vectorised geometry: decode WKB with shapely, area and perimeter in metres from a
            local equal-area scaling, centroid, vertex count, compactness, and the z15 web-map
            tile of the centroid. C and numpy heavy; GEOS objects live outside Python's heap.
  ov_attrs  pure Python over nested columns: the primary name normalised, the source datasets
            counted and the best one picked, a height estimated from floors and class rules, a
            usage bucket, and a volume from the bounding box. Struct and list heavy.

Every function takes and returns a pyarrow RecordBatch, as the NYC set does.
"""
import math
import re
import unicodedata
import zlib

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc

# ---------------------------------------------------------------------------------------------
# FineWeb

_STOP = frozenset("the be to of and that have with".split())
_BULLETS = ("•", "-", "*", "·", "‣", "◦")
_EMAIL = re.compile(r"[\w.+-]+@[\w-]+\.[\w.-]+")
_IPV4 = re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b")
_PHONE = re.compile(r"\b(?:\+?\d{1,2}[ .-]?)?\(?\d{3}\)?[ .-]?\d{3}[ .-]?\d{4}\b")


def _quality(text: str):
    words = text.split()
    n = len(words)
    if n < 50 or n > 100_000:
        return None
    mean_len = sum(len(w) for w in words) / n
    if mean_len < 3 or mean_len > 10:
        return None
    lower = [w.lower() for w in words]
    if sum(1 for w in lower if w in _STOP) < 2:
        return None
    symbols = text.count("#") + text.count("...") + text.count("…")
    if symbols / n > 0.1:
        return None
    lines = [ln for ln in text.split("\n") if ln.strip()]
    if not lines:
        return None
    bullets = sum(1 for ln in lines if ln.lstrip().startswith(_BULLETS)) / len(lines)
    if bullets > 0.9:
        return None
    ellipsis = sum(1 for ln in lines if ln.rstrip().endswith(("...", "…"))) / len(lines)
    if ellipsis > 0.3:
        return None
    dup_lines = 1.0 - len(set(lines)) / len(lines)
    if dup_lines > 0.3:
        return None
    alpha = sum(1 for w in words if any(c.isalpha() for c in w)) / n
    if alpha < 0.8:
        return None
    punct_end = sum(1 for ln in lines if ln.rstrip()[-1:] in ".!?\"'") / len(lines)
    score = 0.4 * punct_end + 0.3 * alpha + 0.3 * (1.0 - dup_lines)
    return score, n, mean_len


def fw_clean(batch: pa.RecordBatch) -> pa.RecordBatch:
    texts = batch["text"].to_pylist()
    keep, scrubbed, scores, nwords = [], [], [], []
    for i, text in enumerate(texts):
        q = _quality(text or "")
        if q is None:
            continue
        clean = _EMAIL.sub("<EMAIL>", text)
        clean = _IPV4.sub("<IP>", clean)
        clean = _PHONE.sub("<PHONE>", clean)
        keep.append(i)
        scrubbed.append(clean)
        scores.append(q[0])
        nwords.append(q[1])
    idx = pa.array(keep, pa.int64())
    return pa.record_batch(
        {
            "id": pc.take(batch["id"], idx),
            "url": pc.take(batch["url"], idx),
            "dump": pc.take(batch["dump"], idx),
            "text": pa.array(scrubbed, pa.large_string()),
            "quality": pa.array(scores, pa.float64()),
            "words": pa.array(nwords, pa.int32()),
        }
    )


_PERMS = 64
_BANDS = 8
_PRIME = np.uint64((1 << 61) - 1)
_rng = np.random.default_rng(20261114)
_A = _rng.integers(1, (1 << 61) - 1, size=_PERMS, dtype=np.uint64)
_B = _rng.integers(0, (1 << 61) - 1, size=_PERMS, dtype=np.uint64)
_MAXU32 = np.uint64(0xFFFFFFFF)


def _signature(text: str) -> np.ndarray:
    words = text.lower().split()
    if len(words) < 5:
        words = words + [""] * (5 - len(words))
    shingles = {" ".join(words[i : i + 5]) for i in range(len(words) - 4)}
    h = np.fromiter((zlib.crc32(s.encode()) for s in shingles), dtype=np.uint64, count=len(shingles))
    # (a * h + b) mod p over every permutation at once: len(shingles) x 64 uint64.
    perm = (np.multiply.outer(h, _A) + _B) % _PRIME
    return (perm.min(axis=0) & _MAXU32).astype(np.uint32)


def fw_minhash(batch: pa.RecordBatch) -> pa.RecordBatch:
    texts = batch["text"].to_pylist()
    sigs = np.empty((len(texts), _PERMS), dtype=np.uint32)
    for i, text in enumerate(texts):
        sigs[i] = _signature(text or "")
    rows = _PERMS // _BANDS
    bands = sigs.reshape(len(texts), _BANDS, rows)
    keys = np.empty((len(texts), _BANDS), dtype=np.uint64)
    for band in range(_BANDS):
        keys[:, band] = np.bitwise_xor.reduce(
            bands[:, band, :].astype(np.uint64) * np.uint64(0x9E3779B1), axis=1
        ) ^ np.uint64(band)
    sig_list = pa.FixedSizeListArray.from_arrays(pa.array(sigs.ravel(), pa.uint32()), _PERMS)
    key_list = pa.FixedSizeListArray.from_arrays(pa.array(keys.ravel(), pa.uint64()), _BANDS)
    return pa.record_batch(
        {"id": batch["id"], "url": batch["url"], "minhash": sig_list, "bands": key_list}
    )


# ---------------------------------------------------------------------------------------------
# Overture buildings

_EARTH = 6_371_008.8
_Z = 15


def ov_shape(batch: pa.RecordBatch) -> pa.RecordBatch:
    import shapely

    geoms = shapely.from_wkb(batch["geometry"].to_numpy(zero_copy_only=False))
    cent = shapely.centroid(geoms)
    lon = shapely.get_x(cent)
    lat = shapely.get_y(cent)
    # A local equal-area scaling about each centroid: metres per degree of longitude shrink with
    # the cosine of the latitude. Good to a fraction of a percent for a building.
    kx = np.radians(1.0) * _EARTH * np.cos(np.radians(lat))
    ky = np.radians(1.0) * _EARTH
    area = shapely.area(geoms) * kx * ky
    # Perimeter: scale the coordinates, then measure; done per coordinate array, vectorised.
    coords, index = shapely.get_coordinates(geoms, return_index=True)
    xs = coords[:, 0] * kx[index]
    ys = coords[:, 1] * ky
    step = np.hypot(np.diff(xs), np.diff(ys))
    same = index[1:] == index[:-1]
    perim = np.bincount(index[1:][same], weights=step[same], minlength=len(geoms))
    nverts = shapely.get_num_coordinates(geoms)
    compact = np.where(perim > 0, 4 * np.pi * area / np.maximum(perim, 1e-9) ** 2, 0.0)
    n = 1 << _Z
    tx = np.floor((lon + 180.0) / 360.0 * n).astype(np.int32)
    lat_r = np.radians(np.clip(lat, -85.05112878, 85.05112878))
    ty = np.floor((1.0 - np.log(np.tan(lat_r) + 1.0 / np.cos(lat_r)) / np.pi) / 2.0 * n).astype(np.int32)
    return pa.record_batch(
        {
            "id": batch["id"],
            "class": batch["class"],
            "subtype": batch["subtype"],
            "height": batch["height"],
            "area_m2": pa.array(area),
            "perimeter_m": pa.array(perim),
            "lon": pa.array(lon),
            "lat": pa.array(lat),
            "vertices": pa.array(nverts.astype(np.int32)),
            "compactness": pa.array(compact),
            "tile_x": pa.array(tx),
            "tile_y": pa.array(ty),
        }
    )


_FLOOR_M = {"residential": 3.0, "commercial": 4.0, "industrial": 5.5, "education": 3.8,
            "religious": 6.0, "agricultural": 4.5, "transportation": 5.0, "medical": 4.0}
_USAGE = {"house": "home", "apartments": "home", "detached": "home", "semidetached_house": "home",
          "residential": "home", "terrace": "home", "garage": "parking", "parking": "parking",
          "carport": "parking", "commercial": "work", "office": "work", "retail": "work",
          "industrial": "work", "warehouse": "work", "school": "civic", "university": "civic",
          "hospital": "civic", "church": "civic", "mosque": "civic", "temple": "civic",
          "barn": "farm", "farm": "farm", "greenhouse": "farm", "shed": "outbuilding",
          "hut": "outbuilding", "roof": "outbuilding"}


def _norm_name(name):
    if not name:
        return None
    s = unicodedata.normalize("NFKC", name).strip().lower()
    s = " ".join(s.split())
    return s or None


def ov_attrs(batch: pa.RecordBatch) -> pa.RecordBatch:
    names = batch["names"].to_pylist()
    sources = batch["sources"].to_pylist()
    heights = batch["height"].to_pylist()
    floors = batch["num_floors"].to_pylist()
    classes = batch["class"].to_pylist()
    subtypes = batch["subtype"].to_pylist()
    bboxes = batch["bbox"].to_pylist()
    out_name, out_nsrc, out_src, out_h, out_usage, out_vol = [], [], [], [], [], []
    for nm, src, h, fl, cl, st, bb in zip(names, sources, heights, floors, classes, subtypes, bboxes):
        out_name.append(_norm_name(nm.get("primary") if nm else None))
        datasets = [s.get("dataset") for s in (src or []) if s]
        out_nsrc.append(len(datasets))
        best = None
        for s in src or []:
            if s and (best is None or (s.get("confidence") or 0) > (best.get("confidence") or 0)):
                best = s
        out_src.append(best.get("dataset") if best else None)
        if h is None:
            per = _FLOOR_M.get(st or "", 3.2)
            h = (fl or 1) * per if fl or st else None
        out_h.append(h)
        out_usage.append(_USAGE.get(cl or "", _USAGE.get(st or "", "other")))
        if bb and h:
            lat = (bb["ymin"] + bb["ymax"]) / 2.0
            w = math.radians(bb["xmax"] - bb["xmin"]) * _EARTH * math.cos(math.radians(lat))
            d = math.radians(bb["ymax"] - bb["ymin"]) * _EARTH
            out_vol.append(w * d * h)
        else:
            out_vol.append(None)
    return pa.record_batch(
        {
            "id": batch["id"],
            "name": pa.array(out_name, pa.string()),
            "sources": pa.array(out_nsrc, pa.int32()),
            "best_source": pa.array(out_src, pa.string()),
            "height_m": pa.array(out_h, pa.float64()),
            "usage": pa.array(out_usage, pa.string()),
            "volume_m3": pa.array(out_vol, pa.float64()),
        }
    )
