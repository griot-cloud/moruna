//! Overture building attributes (`bench/vm/ooc_workloads.py`, `ov_attrs` and `_norm_name`): the
//! primary name normalised, the sources counted and the most confident picked, a height from
//! floors and class rules, a usage bucket and a volume from the bounding box.
//!
//! Python's rules, row by row: `x or y` falls through on `None`, `0` and `""`; a struct is truthy
//! when it is not null; `max` and `>` on NaN are false. `math.radians(x)` is `x * (pi / 180)`.
//! `math.cos` is the platform's libm in Python and musl's here, so `volume_m3` is matched within
//! a relative 1e-12 (images/gate/README.md).

use alloc::string::String;
use alloc::vec::Vec;

use unicode_normalization::UnicodeNormalization;

use crate::cols::{Bits, Col, Offsets, Strs};
use crate::error::{Error, Result};
use crate::fineweb::py_split;

/// `_EARTH`, the mean Earth radius in metres.
pub const EARTH: f64 = 6_371_008.8;
const DEG_TO_RAD: f64 = core::f64::consts::PI / 180.0;

/// `math.radians`.
#[inline]
pub fn radians(x: f64) -> f64 {
    x * DEG_TO_RAD
}

/// `_FLOOR_M`: metres a floor by subtype.
pub fn floor_m(subtype: &str) -> Option<f64> {
    Some(match subtype {
        "residential" => 3.0,
        "commercial" => 4.0,
        "industrial" => 5.5,
        "education" => 3.8,
        "religious" => 6.0,
        "agricultural" => 4.5,
        "transportation" => 5.0,
        "medical" => 4.0,
        _ => return None,
    })
}

/// `_USAGE`: a usage bucket by class or subtype.
pub fn usage(key: &str) -> Option<&'static str> {
    Some(match key {
        "house" | "apartments" | "detached" | "semidetached_house" | "residential" | "terrace" => {
            "home"
        }
        "garage" | "parking" | "carport" => "parking",
        "commercial" | "office" | "retail" | "industrial" | "warehouse" => "work",
        "school" | "university" | "hospital" | "church" | "mosque" | "temple" => "civic",
        "barn" | "farm" | "greenhouse" => "farm",
        "shed" | "hut" | "roof" => "outbuilding",
        _ => return None,
    })
}

/// `_norm_name`: NFKC, lowercase, whitespace runs to one space; `None` for a null, empty or blank
/// name.
pub fn norm_name(name: Option<&str>) -> Option<String> {
    let name = name.filter(|n| !n.is_empty())?;
    let nfkc: String = name.nfkc().collect();
    let lower = nfkc.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    for (i, w) in py_split(&lower).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(w);
    }
    (!out.is_empty()).then_some(out)
}

/// A float column of either width, read as doubles (Python widens a float32 exactly).
#[derive(Clone, Copy, Debug)]
pub enum Floats<'a> {
    F32(Col<'a, f32>),
    F64(Col<'a, f64>),
}

impl Floats<'_> {
    pub fn get(&self, i: usize) -> Option<f64> {
        match self {
            Floats::F32(c) => c.get(i).map(f64::from),
            Floats::F64(c) => c.get(i),
        }
    }
}

/// The `sources` column: a list of structs, of which `dataset` and `confidence` are read.
#[derive(Clone, Copy, Debug)]
pub struct Sources<'a> {
    pub offsets: Offsets<'a>,
    pub valid: Option<Bits<'a>>,
    /// The structs' own validity.
    pub item_valid: Option<Bits<'a>>,
    pub dataset: Strs<'a>,
    pub confidence: Floats<'a>,
}

/// The `bbox` struct.
#[derive(Clone, Copy, Debug)]
pub struct BBox<'a> {
    pub valid: Option<Bits<'a>>,
    pub xmin: Floats<'a>,
    pub xmax: Floats<'a>,
    pub ymin: Floats<'a>,
    pub ymax: Floats<'a>,
}

/// The columns `ov_attrs` reads. `primary` is `names.primary` with the null `names` folded in.
#[derive(Clone, Copy, Debug)]
pub struct AttrsIn<'a> {
    pub rows: usize,
    pub names_valid: Option<Bits<'a>>,
    pub primary: Strs<'a>,
    pub sources: Sources<'a>,
    pub height: Col<'a, f64>,
    pub num_floors: Col<'a, i32>,
    pub class: Strs<'a>,
    pub subtype: Strs<'a>,
    pub bbox: BBox<'a>,
}

/// `ov_attrs`' computed columns.
#[derive(Debug, Default)]
pub struct Attrs {
    pub name: Vec<Option<String>>,
    pub sources: Vec<i32>,
    pub best_source: Vec<Option<String>>,
    pub height_m: Vec<Option<f64>>,
    pub usage: Vec<&'static str>,
    pub volume_m3: Vec<Option<f64>>,
}

fn valid(b: &Option<Bits<'_>>, i: usize) -> bool {
    b.is_none_or(|b| b.get(i))
}

/// `(s.get("confidence") or 0)`: 0 for a null or a zero, else the value (NaN stays).
fn conf(v: Option<f64>) -> f64 {
    match v {
        Some(c) if c != 0.0 => c,
        _ => 0.0,
    }
}

pub fn attrs(a: &AttrsIn) -> Result<Attrs> {
    let mut out = Attrs::default();
    for i in 0..a.rows {
        let primary = if valid(&a.names_valid, i) {
            a.primary.get(i)?
        } else {
            None
        };
        out.name.push(norm_name(primary));
        // The sources: the non-null structs counted; the first of the most confident kept.
        let (mut n, mut best): (i32, Option<(f64, Option<&str>)>) = (0, None);
        if valid(&a.sources.valid, i) {
            for j in a.sources.offsets.range(i)? {
                if !valid(&a.sources.item_valid, j) {
                    continue;
                }
                n += 1;
                let c = conf(a.sources.confidence.get(j));
                if best.is_none_or(|(b, _)| c > b) {
                    best = Some((c, a.sources.dataset.get(j)?));
                }
            }
        }
        out.sources.push(n);
        out.best_source
            .push(best.and_then(|(_, d)| d.map(String::from)));
        let st = a.subtype.get(i)?.filter(|s| !s.is_empty());
        let h = match a.height.get(i) {
            Some(h) => Some(h),
            None => {
                let fl = a.num_floors.get(i).filter(|f| *f != 0);
                (fl.is_some() || st.is_some()).then(|| {
                    let per = st.and_then(floor_m).unwrap_or(3.2);
                    f64::from(fl.unwrap_or(1)) * per
                })
            }
        };
        out.height_m.push(h);
        let cl = a.class.get(i)?.unwrap_or("");
        out.usage.push(
            usage(cl)
                .or_else(|| usage(st.unwrap_or("")))
                .unwrap_or("other"),
        );
        // `if bb and h`: a valid box and a height that is not None or 0 (NaN is truthy).
        let vol = match h {
            Some(h) if h != 0.0 && valid(&a.bbox.valid, i) => {
                let b = &a.bbox;
                let (Some(x0), Some(x1), Some(y0), Some(y1)) =
                    (b.xmin.get(i), b.xmax.get(i), b.ymin.get(i), b.ymax.get(i))
                else {
                    return Err(Error::Value(alloc::format!(
                        "TypeError: a null bbox bound at row {i}"
                    )));
                };
                let lat = (y0 + y1) / 2.0;
                let w = radians(x1 - x0) * EARTH * libm::cos(radians(lat));
                let d = radians(y1 - y0) * EARTH;
                Some(w * d * h)
            }
            _ => None,
        };
        out.volume_m3.push(vol);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(norm_name(None), None);
        assert_eq!(norm_name(Some("")), None);
        assert_eq!(norm_name(Some(" \u{3000} ")), None);
        assert_eq!(
            norm_name(Some("  Ｔｏｗｅｒ\u{3000}ΟΔΟΣ  ")).as_deref(),
            Some("tower οδος")
        );
        assert_eq!(usage("shed"), Some("outbuilding"));
        assert_eq!(floor_m("medical"), Some(4.0));
        assert_eq!(floor_m("x"), None);
        assert_eq!(conf(Some(0.0)), 0.0);
        assert!(conf(Some(f64::NAN)).is_nan());
    }
}

/// `ov_shape`'s computed columns, a value a row.
#[derive(Debug, Default)]
pub struct Shapes {
    pub area_m2: Vec<f64>,
    pub perimeter_m: Vec<f64>,
    pub lon: Vec<f64>,
    pub lat: Vec<f64>,
    pub vertices: Vec<i32>,
    pub compactness: Vec<f64>,
    pub tile_x: Vec<i32>,
    pub tile_y: Vec<i32>,
    /// The consecutive coordinate pairs within a geometry, over the batch. With none,
    /// `np.bincount` of nothing is an integer array, so 0.6.6's `perimeter_m` is `int64` zeros.
    pub pairs: usize,
}

/// NumPy's `astype(np.int32)` of a float on x86-64: truncation, `i32::MIN` for NaN and anything
/// out of range.
pub fn np_f64_to_i32(x: f64) -> i32 {
    if x.is_nan() || !(-2_147_483_648.0..2_147_483_648.0).contains(&x) {
        i32::MIN
    } else {
        x as i32
    }
}

const TILES: f64 = (1u32 << 15) as f64;

/// `ov_shape` over a binary column of WKB: centroid, area and perimeter in metres from a local
/// equal-area scaling, vertex count, compactness and the z15 tile of the centroid. A null
/// geometry gives what shapely gives for `None`: a NaN centroid and area, no perimeter; an empty
/// one is refused, as `get_x` raises on its empty centroid.
pub fn shapes(wkb: &Strs) -> Result<Shapes> {
    use crate::geom;
    use crate::pyfloat::np_clip;
    let n = wkb.len();
    let mut out = Shapes::default();
    let ky = radians(1.0) * EARTH;
    let mut coords = Vec::new();
    for i in 0..n {
        let g = if wkb.is_valid(i) {
            let r = wkb.offsets.range(i)?;
            let b = wkb
                .data
                .get(r)
                .ok_or_else(|| Error::Layout(alloc::format!("geometry {i} is outside its data")))?;
            Some(geom::from_wkb(b)?)
        } else {
            None
        };
        // A null geometry's centroid is None, and `get_x` gives NaN; an empty geometry's is an
        // empty point, and `get_x` raises.
        let ((lon, lat), raw_area) = match &g {
            Some(g) => (
                geom::centroid(g)?.ok_or_else(|| {
                    Error::Value(
                        "GEOSException: UnsupportedOperationException: getX called on empty Point"
                            .into(),
                    )
                })?,
                geom::area(g),
            ),
            None => ((f64::NAN, f64::NAN), f64::NAN),
        };
        let kx = radians(1.0) * EARTH * libm::cos(radians(lat));
        let area = raw_area * kx * ky;
        // The perimeter: consecutive coordinates of the geometry, rings one after another, the
        // scaled steps summed in order, as `np.bincount` sums them.
        coords.clear();
        if let Some(g) = &g {
            geom::coordinates(g, &mut coords);
        }
        let mut perim = 0.0;
        out.pairs += coords.len().saturating_sub(1);
        for w in coords.windows(2) {
            let (x0, x1) = (w[0].0 * kx, w[1].0 * kx);
            let (y0, y1) = (w[0].1 * ky, w[1].1 * ky);
            perim += libm::hypot(x1 - x0, y1 - y0);
        }
        let compact = if perim > 0.0 {
            let p = if perim >= 1e-9 { perim } else { 1e-9 };
            4.0 * core::f64::consts::PI * area / (p * p)
        } else {
            0.0
        };
        let tx = libm::floor((lon + 180.0) / 360.0 * TILES);
        let lat_r = radians(np_clip(lat, -85.05112878, 85.05112878));
        let ty = libm::floor(
            (1.0 - libm::log(libm::tan(lat_r) + 1.0 / libm::cos(lat_r)) / core::f64::consts::PI)
                / 2.0
                * TILES,
        );
        out.area_m2.push(area);
        out.perimeter_m.push(perim);
        out.lon.push(lon);
        out.lat.push(lat);
        out.vertices
            .push(i32::try_from(coords.len()).unwrap_or(i32::MAX));
        out.compactness.push(compact);
        out.tile_x.push(np_f64_to_i32(tx));
        out.tile_y.push(np_f64_to_i32(ty));
    }
    Ok(out)
}
