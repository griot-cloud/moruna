//! What `ov_shape` asks of shapely, as GEOS 3.13 computes it: a WKB geometry read, its area
//! (`Area::ofRingSigned`'s shoelace, shells less holes), its centroid (`algorithm::Centroid`:
//! triangles from each shell's first point, then segments, then points), its coordinates in
//! order and their count.
//!
//! The arithmetic is GEOS's, operation for operation and unfused, as GEOS is built for
//! x86-64's baseline. A WKB GEOS refuses (a ring of one to three points or not closed, a line of
//! one point, a type it does not know, bytes past the end), the reader refuses too.

use alloc::format;
use alloc::vec::Vec;

use crate::error::{Error, Result};

pub type Pt = (f64, f64);

/// A geometry, as far as `ov_shape` reads one.
#[derive(Clone, Debug, PartialEq)]
pub enum Geom {
    /// `None` for an empty point.
    Point(Option<Pt>),
    LineString(Vec<Pt>),
    /// The shell, then the holes; no rings for an empty polygon.
    Polygon(Vec<Vec<Pt>>),
    /// A multi geometry or a collection, its parts in order.
    Multi(Vec<Geom>),
}

fn bad(m: &str) -> Error {
    Error::Value(format!("GEOSException: ParseException: {m}"))
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
    depth: u8,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let s = self
            .b
            .get(self.at..self.at + N)
            .ok_or_else(|| bad("Unexpected EOF parsing WKB"))?;
        self.at += N;
        let mut out = [0u8; N];
        out.copy_from_slice(s);
        Ok(out)
    }

    fn u32(&mut self, le: bool) -> Result<u32> {
        let b = self.take::<4>()?;
        Ok(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self, le: bool) -> Result<f64> {
        let b = self.take::<8>()?;
        Ok(if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }

    /// A count that the remaining bytes can hold `each` bytes of: refuses a count the bytes
    /// cannot back before anything is allocated for it.
    fn count(&mut self, le: bool, each: usize) -> Result<usize> {
        let n = self.u32(le)? as usize;
        if n.saturating_mul(each) > self.b.len() - self.at.min(self.b.len()) {
            return Err(bad("Unexpected EOF parsing WKB"));
        }
        Ok(n)
    }

    fn point(&mut self, le: bool, dims: usize) -> Result<Pt> {
        let x = self.f64(le)?;
        let y = self.f64(le)?;
        for _ in 2..dims {
            self.f64(le)?;
        }
        Ok((x, y))
    }

    fn points(&mut self, le: bool, dims: usize) -> Result<Vec<Pt>> {
        let n = self.count(le, 8 * dims)?;
        (0..n).map(|_| self.point(le, dims)).collect()
    }

    fn geometry(&mut self) -> Result<Geom> {
        if self.depth > 32 {
            return Err(bad("WKB nests too deeply"));
        }
        let le = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            o => return Err(bad(&format!("Unknown WKB byte order {o}"))),
        };
        let raw = self.u32(le)?;
        let (ewkb_z, ewkb_m, srid) = (
            raw & 0x8000_0000 != 0,
            raw & 0x4000_0000 != 0,
            raw & 0x2000_0000 != 0,
        );
        let code = raw & 0x0FFF_FFFF;
        if srid {
            self.u32(le)?;
        }
        let (base, iso_dims) = (code % 1000, code / 1000);
        let (z, m) = match iso_dims {
            0 => (ewkb_z, ewkb_m),
            1 => (true, false),
            2 => (false, true),
            3 => (true, true),
            _ => return Err(bad(&format!("Unknown WKB type {raw}"))),
        };
        let dims = 2 + usize::from(z) + usize::from(m);
        Ok(match base {
            1 => {
                let p = self.point(le, dims)?;
                Geom::Point((!(p.0.is_nan() && p.1.is_nan())).then_some(p))
            }
            2 => {
                let pts = self.points(le, dims)?;
                if pts.len() == 1 {
                    return Err(bad(
                        "IllegalArgumentException: point array must contain 0 or >1 elements",
                    ));
                }
                Geom::LineString(pts)
            }
            3 => {
                let n = self.count(le, 4)?;
                let mut rings = Vec::with_capacity(n);
                for _ in 0..n {
                    let r = self.points(le, dims)?;
                    if !r.is_empty() && r.len() < 4 {
                        return Err(bad(&format!(
                            "IllegalArgumentException: Invalid number of points in LinearRing found {} - must be 0 or >= 4",
                            r.len()
                        )));
                    }
                    if let (Some(a), Some(b)) = (r.first(), r.last())
                        && !(a.0 == b.0 && a.1 == b.1)
                    {
                        return Err(bad(
                            "IllegalArgumentException: Points of LinearRing do not form a closed linestring",
                        ));
                    }
                    rings.push(r);
                }
                // An empty shell makes an empty polygon; GEOS refuses holes in one.
                if rings.first().is_some_and(Vec::is_empty) {
                    if rings.iter().any(|r| !r.is_empty()) {
                        return Err(bad(
                            "IllegalArgumentException: shell is empty but holes are not",
                        ));
                    }
                    rings.clear();
                }
                Geom::Polygon(rings)
            }
            4..=7 => {
                let n = self.count(le, 5)?;
                self.depth += 1;
                let parts = (0..n)
                    .map(|_| self.geometry())
                    .collect::<Result<Vec<_>>>()?;
                self.depth -= 1;
                let want = match base {
                    4 => Some(1),
                    5 => Some(2),
                    6 => Some(3),
                    _ => None,
                };
                if let Some(w) = want
                    && parts.iter().any(|p| kind(p) != w)
                {
                    return Err(bad("a multi geometry holds a part of another type"));
                }
                Geom::Multi(parts)
            }
            _ => return Err(bad(&format!("Unknown WKB type {raw}"))),
        })
    }
}

fn kind(g: &Geom) -> u32 {
    match g {
        Geom::Point(_) => 1,
        Geom::LineString(_) => 2,
        Geom::Polygon(_) => 3,
        Geom::Multi(_) => 7,
    }
}

/// `shapely.from_wkb` of one value.
pub fn from_wkb(b: &[u8]) -> Result<Geom> {
    let mut r = Reader { b, at: 0, depth: 0 };
    let g = r.geometry()?;
    if r.at != b.len() {
        return Err(bad("trailing bytes after the WKB geometry"));
    }
    Ok(g)
}

/// `Area::ofRingSigned`.
pub fn ring_area_signed(r: &[Pt]) -> f64 {
    let n = r.len();
    if n < 3 {
        return 0.0;
    }
    let mut p1 = r[0];
    let mut p2 = r[1];
    let x0 = p1.0;
    p2.0 -= x0;
    let mut sum = 0.0;
    for &next in &r[2..n] {
        let p0y = p1.1;
        p1 = p2;
        p2 = (next.0 - x0, next.1);
        sum += p1.0 * (p0y - p2.1);
    }
    sum / 2.0
}

/// `Geometry::getArea`.
pub fn area(g: &Geom) -> f64 {
    match g {
        Geom::Point(_) | Geom::LineString(_) => 0.0,
        Geom::Polygon(rings) => {
            let mut a = 0.0;
            for (i, r) in rings.iter().enumerate() {
                let ra = ring_area_signed(r).abs();
                if i == 0 {
                    a += ra;
                } else {
                    a -= ra;
                }
            }
            a
        }
        Geom::Multi(parts) => {
            let mut a = 0.0;
            for p in parts {
                a += area(p);
            }
            a
        }
    }
}

/// `get_coordinates` of one geometry, in order: every ring of every part.
pub fn coordinates(g: &Geom, out: &mut Vec<Pt>) {
    match g {
        Geom::Point(p) => out.extend(p.iter().copied()),
        Geom::LineString(pts) => out.extend_from_slice(pts),
        Geom::Polygon(rings) => rings.iter().for_each(|r| out.extend_from_slice(r)),
        Geom::Multi(parts) => parts.iter().for_each(|p| coordinates(p, out)),
    }
}

fn is_empty(g: &Geom) -> bool {
    match g {
        Geom::Point(p) => p.is_none(),
        Geom::LineString(pts) => pts.is_empty(),
        Geom::Polygon(rings) => rings.is_empty(),
        Geom::Multi(parts) => parts.iter().all(is_empty),
    }
}

/// GEOS's `CGAlgorithmsDD` double-double, the part `orientationIndex` uses.
#[derive(Clone, Copy, Debug)]
struct Dd {
    hi: f64,
    lo: f64,
}

const SPLIT: f64 = 134_217_729.0;

impl Dd {
    fn new(x: f64) -> Dd {
        Dd { hi: x, lo: 0.0 }
    }

    fn add(self, y: Dd) -> Dd {
        let (hi, lo, yhi, ylo) = (self.hi, self.lo, y.hi, y.lo);
        let s_big = hi + yhi;
        let t_big = lo + ylo;
        let mut e = s_big - hi;
        let f = t_big - lo;
        let mut s = s_big - e;
        let mut t = t_big - f;
        s = (yhi - e) + (hi - s);
        t = (ylo - f) + (lo - t);
        e = s + t_big;
        let h_big = s_big + e;
        let h = e + (s_big - h_big);
        e = t + h;
        let zhi = h_big + e;
        let zlo = e + (h_big - zhi);
        Dd { hi: zhi, lo: zlo }
    }

    fn neg(self) -> Dd {
        Dd {
            hi: -self.hi,
            lo: -self.lo,
        }
    }

    fn mul(self, y: Dd) -> Dd {
        let (hi, lo, yhi, ylo) = (self.hi, self.lo, y.hi, y.lo);
        let mut c_big = SPLIT * hi;
        let mut hx = c_big - hi;
        let mut c = SPLIT * yhi;
        hx = c_big - hx;
        let tx = hi - hx;
        let mut hy = c - yhi;
        c_big = hi * yhi;
        hy = c - hy;
        let ty = yhi - hy;
        c = ((((hx * hy - c_big) + hx * ty) + tx * hy) + tx * ty) + (hi * ylo + lo * yhi);
        let zhi = c_big + c;
        hx = c_big - zhi;
        let zlo = c + hx;
        Dd { hi: zhi, lo: zlo }
    }

    fn signum(self) -> i32 {
        if self.hi < 0.0 || (self.hi == 0.0 && self.lo < 0.0) {
            -1
        } else if self.hi > 0.0 || (self.hi == 0.0 && self.lo > 0.0) {
            1
        } else {
            0
        }
    }
}

fn orientation(x: f64) -> i32 {
    if x < 0.0 {
        -1
    } else if x > 0.0 {
        1
    } else {
        0
    }
}

/// `CGAlgorithmsDD::orientationIndex`: 1 counter-clockwise, -1 clockwise, 0 collinear.
pub fn orientation_index(p1: Pt, p2: Pt, q: Pt) -> Result<i32> {
    if !q.0.is_finite() || !q.1.is_finite() {
        return Err(Error::Value(
            "GEOSException: IllegalArgumentException: CGAlgorithmsDD::orientationIndex encountered NaN/Inf numbers".into(),
        ));
    }
    // The filter, as `orientationIndexFilter`.
    let detleft = (p1.0 - q.0) * (p2.1 - q.1);
    let detright = (p1.1 - q.1) * (p2.0 - q.0);
    let det = detleft - detright;
    let detsum = if detleft > 0.0 {
        if detright <= 0.0 {
            return Ok(orientation(det));
        }
        detleft + detright
    } else if detleft < 0.0 {
        if detright >= 0.0 {
            return Ok(orientation(det));
        }
        -detleft - detright
    } else {
        return Ok(orientation(det));
    };
    let errbound = 1e-15 * detsum;
    if det >= errbound || -det >= errbound {
        return Ok(orientation(det));
    }
    let dx1 = Dd::new(p2.0).add(Dd::new(-p1.0));
    let dy1 = Dd::new(p2.1).add(Dd::new(-p1.1));
    let dx2 = Dd::new(q.0).add(Dd::new(-p2.0));
    let dy2 = Dd::new(q.1).add(Dd::new(-p2.1));
    let d = dx1.mul(dy2).add(dy1.mul(dx2).neg());
    Ok(d.signum())
}

/// `Orientation::isCCW` of a closed ring of at least four points.
pub fn is_ccw(ring: &[Pt]) -> Result<bool> {
    let n_pts = ring.len().saturating_sub(1);
    if n_pts < 3 {
        return Err(Error::Value(
            "GEOSException: IllegalArgumentException: Ring has fewer than 4 points, so orientation cannot be determined".into(),
        ));
    }
    let mut up_hi = ring[0];
    let mut prev_y = up_hi.1;
    let mut up_low = None;
    let mut i_up_hi = 0usize;
    for (i, p) in ring.iter().enumerate().take(n_pts + 1).skip(1) {
        let py = p.1;
        if py > prev_y && py >= up_hi.1 {
            i_up_hi = i;
            up_hi = *p;
            up_low = Some(ring[i - 1]);
        }
        prev_y = py;
    }
    let Some(up_low) = up_low.filter(|_| i_up_hi != 0) else {
        return Ok(false);
    };
    let mut i_down_low = i_up_hi;
    loop {
        i_down_low = (i_down_low + 1) % n_pts;
        if i_down_low == i_up_hi || ring[i_down_low].1 != up_hi.1 {
            break;
        }
    }
    let down_low = ring[i_down_low];
    let i_down_hi = if i_down_low > 0 {
        i_down_low - 1
    } else {
        n_pts - 1
    };
    let down_hi = ring[i_down_hi];
    let eq = |a: Pt, b: Pt| a.0 == b.0 && a.1 == b.1;
    if eq(up_hi, down_hi) {
        if eq(up_low, up_hi) || eq(down_low, up_hi) || eq(up_low, down_low) {
            return Ok(false);
        }
        Ok(orientation_index(up_low, up_hi, down_low)? == 1)
    } else {
        Ok(down_hi.0 - up_hi.0 < 0.0)
    }
}

/// `algorithm::Centroid`'s sums.
#[derive(Default)]
struct Centroid {
    base: Option<Pt>,
    areasum2: f64,
    cg3: Pt,
    line: Pt,
    total_length: f64,
    pts: Pt,
    pt_count: u64,
}

impl Centroid {
    fn add(&mut self, g: &Geom) -> Result<()> {
        if is_empty(g) {
            return Ok(());
        }
        match g {
            Geom::Point(Some(p)) => self.add_point(*p),
            Geom::Point(None) => {}
            Geom::LineString(pts) => self.add_line_segments(pts),
            Geom::Polygon(rings) => {
                if let Some((shell, holes)) = rings.split_first() {
                    self.add_shell(shell)?;
                    for h in holes {
                        self.add_hole(h)?;
                    }
                }
            }
            Geom::Multi(parts) => {
                for p in parts {
                    self.add(p)?;
                }
            }
        }
        Ok(())
    }

    fn add_point(&mut self, p: Pt) {
        self.pt_count += 1;
        self.pts.0 += p.0;
        self.pts.1 += p.1;
    }

    fn add_shell(&mut self, pts: &[Pt]) -> Result<()> {
        if let Some(p) = pts.first() {
            self.base = Some(*p);
        }
        let positive = !is_ccw(pts)?;
        let base = self.base.unwrap_or((0.0, 0.0));
        for w in pts.windows(2) {
            self.add_triangle(base, w[0], w[1], positive);
        }
        self.add_line_segments(pts);
        Ok(())
    }

    fn add_hole(&mut self, pts: &[Pt]) -> Result<()> {
        let positive = is_ccw(pts)?;
        let base = self.base.unwrap_or((0.0, 0.0));
        for w in pts.windows(2) {
            self.add_triangle(base, w[0], w[1], positive);
        }
        self.add_line_segments(pts);
        Ok(())
    }

    fn add_triangle(&mut self, p0: Pt, p1: Pt, p2: Pt, positive: bool) {
        let sign = if positive { 1.0 } else { -1.0 };
        let cx = p0.0 + p1.0 + p2.0;
        let cy = p0.1 + p1.1 + p2.1;
        let a2 = (p1.0 - p0.0) * (p2.1 - p0.1) - (p2.0 - p0.0) * (p1.1 - p0.1);
        self.cg3.0 += sign * a2 * cx;
        self.cg3.1 += sign * a2 * cy;
        self.areasum2 += sign * a2;
    }

    fn add_line_segments(&mut self, pts: &[Pt]) {
        let mut len = 0.0;
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (dx, dy) = (a.0 - b.0, a.1 - b.1);
            let seg = libm::sqrt(dx * dx + dy * dy);
            if seg == 0.0 {
                continue;
            }
            len += seg;
            let midx = (a.0 + b.0) / 2.0;
            self.line.0 += seg * midx;
            let midy = (a.1 + b.1) / 2.0;
            self.line.1 += seg * midy;
        }
        self.total_length += len;
        if len == 0.0
            && let Some(p) = pts.first()
        {
            self.add_point(*p);
        }
    }

    fn finish(&self) -> Option<Pt> {
        if self.areasum2.abs() > 0.0 {
            Some((
                self.cg3.0 / 3.0 / self.areasum2,
                self.cg3.1 / 3.0 / self.areasum2,
            ))
        } else if self.total_length > 0.0 {
            Some((
                self.line.0 / self.total_length,
                self.line.1 / self.total_length,
            ))
        } else if self.pt_count > 0 {
            let n = self.pt_count as f64;
            Some((self.pts.0 / n, self.pts.1 / n))
        } else {
            None
        }
    }
}

/// `shapely.centroid`'s point, `None` for an empty geometry (`get_x` then gives NaN).
pub fn centroid(g: &Geom) -> Result<Option<Pt>> {
    let mut c = Centroid::default();
    c.add(g)?;
    Ok(c.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn wkb_polygon(rings: &[&[Pt]]) -> Vec<u8> {
        let mut b = vec![1u8];
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&(rings.len() as u32).to_le_bytes());
        for r in rings {
            b.extend_from_slice(&(r.len() as u32).to_le_bytes());
            for p in *r {
                b.extend_from_slice(&p.0.to_le_bytes());
                b.extend_from_slice(&p.1.to_le_bytes());
            }
        }
        b
    }

    #[test]
    fn a_square_with_a_hole() {
        let shell: &[Pt] = &[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0), (0.0, 0.0)];
        let hole: &[Pt] = &[(1.0, 1.0), (1.0, 2.0), (2.0, 2.0), (2.0, 1.0), (1.0, 1.0)];
        let g = from_wkb(&wkb_polygon(&[shell, hole])).unwrap();
        assert_eq!(area(&g), 15.0);
        let (x, y) = centroid(&g).unwrap().unwrap();
        assert!((x - 61.0 / 30.0).abs() < 1e-12 && (y - 61.0 / 30.0).abs() < 1e-12);
        assert!(is_ccw(shell).unwrap());
        assert!(!is_ccw(hole).unwrap());
        let mut c = Vec::new();
        coordinates(&g, &mut c);
        assert_eq!(c.len(), 10);
    }

    #[test]
    fn refusals() {
        assert!(from_wkb(&[]).is_err());
        assert!(from_wkb(&[2, 0, 0, 0, 0]).is_err());
        let open: &[Pt] = &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        assert!(from_wkb(&wkb_polygon(&[open])).is_err());
        let short: &[Pt] = &[(0.0, 0.0), (1.0, 0.0), (0.0, 0.0)];
        assert!(from_wkb(&wkb_polygon(&[short])).is_err());
        let mut huge = vec![1u8];
        huge.extend_from_slice(&2u32.to_le_bytes());
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(from_wkb(&huge).is_err());
        let mut tail = wkb_polygon(&[]);
        tail.push(0);
        assert!(from_wkb(&tail).is_err());
        assert!(is_ccw(&[(0.0, 0.0), (0.0, 0.0)]).is_err());
        assert!(orientation_index((0.0, 0.0), (1.0, 1.0), (f64::NAN, 0.0)).is_err());
        assert_eq!(
            orientation_index((0.0, 0.0), (1.0, 1.0), (2.0, 2.0)).unwrap(),
            0
        );
        assert_eq!(
            orientation_index((0.0, 0.0), (1.0, 0.0), (1.0, 1.0)).unwrap(),
            1
        );
        // A nearly collinear triple goes to the double-double.
        let p = (0.1, 0.1);
        assert_eq!(
            orientation_index(p, (0.2, 0.2), (0.30000000000000004, 0.3))
                .unwrap()
                .abs(),
            1
        );
    }

    #[test]
    fn empty_and_degenerate() {
        assert_eq!(centroid(&Geom::Polygon(vec![])).unwrap(), None);
        assert_eq!(
            centroid(&Geom::Point(Some((1.0, 2.0)))).unwrap(),
            Some((1.0, 2.0))
        );
        assert_eq!(
            centroid(&Geom::LineString(vec![(0.0, 0.0), (2.0, 0.0)])).unwrap(),
            Some((1.0, 0.0))
        );
        assert_eq!(area(&Geom::Multi(vec![Geom::Point(None)])), 0.0);
        let flat = vec![(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (0.0, 0.0)];
        assert!(!is_ccw(&flat).unwrap());
    }
}
