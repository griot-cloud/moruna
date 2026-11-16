//! How one read or write of a byte range becomes virtio-blk requests: pure arithmetic, so it
//! is tested as ordinary code.
//!
//! A disk moves whole 512-byte sectors. A read of any range is one run of sectors: its first
//! and last sector are read into a slot's bounce sector when the range starts or ends inside
//! them (an edge), and every whole sector between lands by DMA in the destination itself (the
//! body). A body larger than one request takes is split at sector boundaries, the first part
//! keeping the head edge and the last the tail. A write must be whole sectors: the output
//! layer is whole pages.

/// The bytes of a sector.
pub const SECTOR: u64 = 512;

/// Bytes an edge copies between a bounce sector and the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    /// Bytes of the bounce sector before the ones wanted.
    pub skip: usize,
    /// Where they go in the destination.
    pub dst: u64,
    /// How many.
    pub len: usize,
}

/// One virtio-blk request: a run of sectors from `sector`, its data in order: the head edge's
/// bounce sector, the body's bytes of the destination, the tail edge's bounce sector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    /// The first sector.
    pub sector: u64,
    /// A first sector read whole into the bounce, of which `len` bytes are wanted.
    pub head: Option<Edge>,
    /// `(offset in the destination, length)`: whole sectors moved by DMA.
    pub body: Option<(u64, u64)>,
    /// A last sector read whole into the bounce.
    pub tail: Option<Edge>,
}

impl Part {
    /// The sectors the request moves.
    pub fn sectors(&self) -> u64 {
        let edges = u64::from(self.head.is_some()) + u64::from(self.tail.is_some());
        edges + self.body.map_or(0, |(_, len)| len / SECTOR)
    }
}

/// The requests that read `len` bytes at byte `offset` into a destination of `len` bytes, no
/// body larger than `max_body` (a multiple of [`SECTOR`]).
pub fn read(offset: u64, len: u64, max_body: u64) -> Vec<Part> {
    debug_assert!(max_body >= SECTOR && max_body.is_multiple_of(SECTOR));
    if len == 0 {
        return Vec::new();
    }
    let end = offset + len;
    let first = offset / SECTOR;
    let last = (end - 1) / SECTOR;
    if first == last && !(offset.is_multiple_of(SECTOR) && end.is_multiple_of(SECTOR)) {
        // Inside one sector: one bounce, one copy.
        return vec![Part {
            sector: first,
            head: Some(Edge {
                skip: (offset % SECTOR) as usize,
                dst: 0,
                len: len as usize,
            }),
            body: None,
            tail: None,
        }];
    }
    let head = (!offset.is_multiple_of(SECTOR)).then(|| Edge {
        skip: (offset % SECTOR) as usize,
        dst: 0,
        len: (SECTOR - offset % SECTOR) as usize,
    });
    let tail = (!end.is_multiple_of(SECTOR)).then(|| Edge {
        skip: 0,
        dst: end / SECTOR * SECTOR - offset,
        len: (end % SECTOR) as usize,
    });
    let body_start = offset.next_multiple_of(SECTOR);
    let body_end = end / SECTOR * SECTOR;
    let mut parts = Vec::new();
    let mut at = body_start;
    while at < body_end {
        let n = (body_end - at).min(max_body);
        parts.push(Part {
            sector: at / SECTOR,
            head: None,
            body: Some((at - offset, n)),
            tail: None,
        });
        at += n;
    }
    if parts.is_empty() {
        // Two edges and nothing between: one request of two sectors.
        parts.push(Part {
            sector: first,
            head: None,
            body: None,
            tail: None,
        });
    }
    if let Some(h) = head {
        parts[0].head = Some(h);
        parts[0].sector = first;
    }
    if let Some(t) = tail {
        let n = parts.len();
        parts[n - 1].tail = Some(t);
    }
    parts
}

/// The requests that write `len` bytes from a source at byte `offset`, or `None` when the
/// range is not whole sectors.
pub fn write(offset: u64, len: u64, max_body: u64) -> Option<Vec<Part>> {
    if !offset.is_multiple_of(SECTOR) || !len.is_multiple_of(SECTOR) {
        return None;
    }
    let mut parts = Vec::new();
    let mut at = 0;
    while at < len {
        let n = (len - at).min(max_body);
        parts.push(Part {
            sector: (offset + at) / SECTOR,
            head: None,
            body: Some((at, n)),
            tail: None,
        });
        at += n;
    }
    Some(parts)
}

/// The largest body one request takes, in bytes: a body of `b` bytes spans at most
/// `b / page + 2` pages, each one descriptor at worst, beside two edges, the header and the
/// status, within the device's `seg_max` and the queue's `size`; at most `cap`.
pub fn max_body(seg_max: u32, queue: u16, page: u64, cap: u64) -> u64 {
    let segs = u64::from(seg_max).min(u64::from(queue).saturating_sub(2));
    let pages = segs.saturating_sub(4);
    let bytes = (pages * page).min(cap) / SECTOR * SECTOR;
    bytes.max(SECTOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every byte of `[offset, offset + len)` is covered exactly once, in order, by the parts'
    /// edges and bodies, each part is one contiguous run of sectors, and they follow each
    /// other.
    fn check(offset: u64, len: u64, max_body: u64) -> Vec<Part> {
        let parts = read(offset, len, max_body);
        let mut covered = vec![0u8; len as usize];
        let mut next_sector = None;
        for p in &parts {
            if let Some(s) = next_sector {
                assert_eq!(p.sector, s, "{parts:?}");
            }
            let mut sector = p.sector;
            if let Some(h) = p.head {
                assert_eq!(sector * SECTOR + h.skip as u64, offset + h.dst);
                covered[h.dst as usize..h.dst as usize + h.len]
                    .iter_mut()
                    .for_each(|c| *c += 1);
                sector += 1;
            }
            if let Some((at, n)) = p.body {
                assert!(n.is_multiple_of(SECTOR) && n <= max_body && n > 0);
                assert_eq!(sector * SECTOR, offset + at);
                covered[at as usize..(at + n) as usize]
                    .iter_mut()
                    .for_each(|c| *c += 1);
                sector += n / SECTOR;
            }
            if let Some(t) = p.tail {
                assert_eq!(sector * SECTOR + t.skip as u64, offset + t.dst);
                covered[t.dst as usize..t.dst as usize + t.len]
                    .iter_mut()
                    .for_each(|c| *c += 1);
                sector += 1;
            }
            assert_eq!(sector - p.sector, p.sectors());
            next_sector = Some(sector);
        }
        assert!(covered.iter().all(|c| *c == 1), "{offset} {len}: {parts:?}");
        parts
    }

    #[test]
    fn every_range_is_covered_once_by_runs_of_whole_sectors() {
        for offset in [0, 1, 100, 511, 512, 513, 1000, 4096, 4097] {
            for len in [
                1, 10, 411, 412, 511, 512, 513, 1024, 1536, 5000, 12288, 65_537,
            ] {
                for max_body in [512, 4096, 1 << 20] {
                    check(offset, len, max_body);
                }
            }
        }
        assert!(read(7, 0, 512).is_empty());
    }

    #[test]
    fn aligned_reads_have_no_edges_and_split_at_the_body_limit() {
        let parts = check(4096, 3 * 4096, 4096);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.head.is_none() && p.tail.is_none()));
        assert_eq!(parts[1].sector, (4096 + 4096) / 512);
    }

    #[test]
    fn a_range_inside_one_sector_is_one_bounce() {
        let parts = check(700, 20, 4096);
        assert_eq!(
            parts,
            vec![Part {
                sector: 1,
                head: Some(Edge {
                    skip: 188,
                    dst: 0,
                    len: 20
                }),
                body: None,
                tail: None
            }]
        );
        assert_eq!(parts[0].sectors(), 1);
    }

    #[test]
    fn two_edges_with_nothing_between_are_one_request() {
        let parts = check(500, 100, 4096);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].head.is_some() && parts[0].tail.is_some());
        assert_eq!(parts[0].sectors(), 2);
    }

    #[test]
    fn writes_are_whole_sectors() {
        assert_eq!(write(1, 512, 4096), None);
        assert_eq!(write(512, 100, 4096), None);
        let parts = write(4096, 10_240, 4096).expect("whole sectors");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2].body, Some((8192, 2048)));
        assert_eq!(parts[2].sector, (4096 + 8192) / 512);
        assert_eq!(write(0, 0, 4096), Some(Vec::new()));
    }

    #[test]
    fn the_body_limit_follows_the_device_and_the_queue() {
        assert_eq!(max_body(254, 256, 4096, 1 << 20), 250 * 4096);
        assert_eq!(max_body(254, 128, 4096, 1 << 20), 122 * 4096);
        assert_eq!(max_body(254, 256, 4096, 64 * 1024), 64 * 1024);
        assert_eq!(max_body(2, 256, 4096, 1 << 20), 512, "never below a sector");
    }
}
