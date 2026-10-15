//! The data area as leased regions (17 e.4, SB-I13): an input is placed in a free region, which
//! is leased to the child until the child says every Arrow buffer over it has been released; a
//! region is reused only after that.

use std::collections::BTreeMap;

/// Every region starts on this boundary, Arrow's.
pub const ALIGN: u64 = 64;

/// The data area's free ranges and its leases.
#[derive(Debug, Default)]
pub struct Regions {
    /// Free ranges, offset to length, never adjacent (coalesced).
    free: BTreeMap<u64, u64>,
    /// Leases, id to `(offset, length)`.
    leased: BTreeMap<u64, (u64, u64)>,
    next: u64,
}

fn align_up(x: u64) -> u64 {
    x.div_ceil(ALIGN) * ALIGN
}

impl Regions {
    /// A data area of `offset..offset + len`, all free.
    pub fn new(offset: u64, len: u64) -> Regions {
        let mut regions = Regions::default();
        regions.give_back(offset, len);
        regions
    }

    /// The largest free range, its start aligned: `(offset, length)`.
    pub fn largest_free(&self) -> Option<(u64, u64)> {
        self.free
            .iter()
            .filter_map(|(&o, &l)| {
                let start = align_up(o);
                let end = o + l;
                (end > start).then(|| (start, end - start))
            })
            .max_by_key(|&(o, l)| (l, std::cmp::Reverse(o)))
    }

    /// Lease `offset..offset + len`, which must lie in one free range; returns the lease's id.
    pub fn lease(&mut self, offset: u64, len: u64) -> Result<u64, String> {
        let (&start, &free_len) = self
            .free
            .range(..=offset)
            .next_back()
            .ok_or_else(|| format!("{offset}..+{len} is not free"))?;
        let end = start + free_len;
        if offset + len > end {
            return Err(format!("{offset}..+{len} is not free"));
        }
        self.free.remove(&start);
        if offset > start {
            self.free.insert(start, offset - start);
        }
        if end > offset + len {
            self.free.insert(offset + len, end - offset - len);
        }
        self.next += 1;
        self.leased.insert(self.next, (offset, len));
        Ok(self.next)
    }

    /// The child released lease `id`: its range is free again; returns the free range it is
    /// now part of, whose whole pages may be given back to the operating system. An id that is
    /// not leased (released twice) is `None`.
    pub fn release(&mut self, id: u64) -> Option<(u64, u64)> {
        let (offset, len) = self.leased.remove(&id)?;
        Some(self.give_back(offset, len))
    }

    /// The free range holding `offset`, if it is free.
    pub fn free_at(&self, offset: u64) -> Option<(u64, u64)> {
        self.free
            .range(..=offset)
            .next_back()
            .filter(|&(&o, &l)| offset < o + l)
            .map(|(&o, &l)| (o, l))
    }

    /// Add `offset..offset + len` to the free ranges, joining its neighbours; the range it is
    /// now part of.
    pub fn give_back(&mut self, mut offset: u64, mut len: u64) -> (u64, u64) {
        if len == 0 {
            return (offset, 0);
        }
        if let Some((&before, &before_len)) = self.free.range(..offset).next_back()
            && before + before_len == offset
        {
            self.free.remove(&before);
            offset = before;
            len += before_len;
        }
        if let Some(&after_len) = self.free.get(&(offset + len)) {
            self.free.remove(&(offset + len));
            len += after_len;
        }
        self.free.insert(offset, len);
        (offset, len)
    }

    /// Bytes leased to the child now.
    pub fn leased_bytes(&self) -> u64 {
        self.leased.values().map(|&(_, l)| l).sum()
    }

    /// Leases held by the child now.
    pub fn leased_count(&self) -> usize {
        self.leased.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SB-T23: leases split the free ranges, releases join them back, the largest free range is
    /// aligned, and a release twice is nothing.
    #[test]
    fn sb_t23_regions() {
        let mut r = Regions::new(4096, 10_000);
        assert_eq!(r.largest_free(), Some((4096, 10_000)));
        let a = r.lease(4096, 100).unwrap();
        assert_eq!(
            r.largest_free(),
            Some((4224, 9_872)),
            "aligned past the lease"
        );
        let b = r.lease(4224, 1000).unwrap();
        assert_eq!((r.leased_count(), r.leased_bytes()), (2, 1100));
        assert!(r.lease(4100, 10).is_err(), "inside a lease");
        assert!(r.lease(4224 + 1000, 1_000_000).is_err(), "past the area");
        assert!(r.lease(0, 1).is_err(), "before the area");
        assert_eq!(r.free_at(4100), None, "leased");
        assert_eq!(r.free_at(4200), Some((4196, 28)), "the gap before `b`");
        assert_eq!(
            r.release(a),
            Some((4096, 128)),
            "joined with the gap before `b`"
        );
        assert_eq!(r.release(a), None);
        // The released hole is smaller than what follows `b`.
        assert_eq!(r.largest_free().unwrap().0, 5248);
        assert_eq!(r.release(b), Some((4096, 10_000)));
        assert_eq!(r.free_at(14_096), None, "past the area");
        assert_eq!(
            r.largest_free(),
            Some((4096, 10_000)),
            "joined back into one range"
        );
        assert_eq!(r.leased_count(), 0);
        r.give_back(0, 0);
        // A full area has no free range.
        let mut full = Regions::new(64, 64);
        full.lease(64, 64).unwrap();
        assert_eq!(full.largest_free(), None);
        // An unaligned free range that is all alignment offers nothing.
        let mut tiny = Regions::new(65, 10);
        assert_eq!(tiny.largest_free(), None);
        tiny.give_back(200, 100);
        assert_eq!(tiny.largest_free(), Some((256, 44)));
    }
}
