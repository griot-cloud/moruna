//! The host regions' bounds, for `tier_of` with no lock (02 f.7).
//!
//! `tier_of` runs per buffer on every worker. Behind the region list's `RwLock` each lookup
//! wrote the lock's shared word, so eight workers asking at once took about 300 ns a lookup for
//! what one does in 10. [`RangeIndex`] keeps the regions' bounds sorted in arrays of atomics behind
//! one version number, odd while they change (a sequence lock): a lookup reads the version,
//! searches the bounds, and reads the version again, searching again only if a change came in
//! between. It writes nothing shared. The region list changes only under its write lock, and the
//! index is published from that list before the lock is let go (`Inner::write_host`), so the
//! index never lags a region an allocation can come from, and no region is dropped from it while
//! a live buffer is in it (a region is retired only when it holds none).
//!
//! More regions than the index holds is not an error: the index says it cannot answer, and
//! `tier_of` asks the list under its lock, as it always did.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

/// The bounds of the host regions, `[start, end)` each, sorted, none overlapping.
pub(crate) struct RangeIndex {
    /// Even while the bounds are still, odd while the writer changes them.
    version: AtomicU64,
    len: AtomicUsize,
    /// More regions than the index holds: lookups go to the list.
    overflow: AtomicBool,
    starts: Box<[AtomicUsize]>,
    ends: Box<[AtomicUsize]>,
}

impl RangeIndex {
    /// An index of at most `capacity` regions.
    pub(crate) fn new(capacity: usize) -> RangeIndex {
        let atomics = || (0..capacity).map(|_| AtomicUsize::new(0)).collect();
        RangeIndex {
            version: AtomicU64::new(0),
            len: AtomicUsize::new(0),
            overflow: AtomicBool::new(false),
            starts: atomics(),
            ends: atomics(),
        }
    }

    /// Replaces the bounds with `ranges`, in any order. One writer at a time: the caller holds
    /// the region list's write lock.
    pub(crate) fn publish(&self, ranges: impl IntoIterator<Item = (usize, usize)>) {
        let mut ranges: Vec<(usize, usize)> = ranges.into_iter().collect();
        ranges.sort_unstable();
        let v = self.version.load(Ordering::Relaxed);
        self.version.store(v + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        if ranges.len() > self.starts.len() {
            self.overflow.store(true, Ordering::Relaxed);
            self.len.store(0, Ordering::Relaxed);
        } else {
            for (i, (start, end)) in ranges.iter().enumerate() {
                self.starts[i].store(*start, Ordering::Relaxed);
                self.ends[i].store(*end, Ordering::Relaxed);
            }
            self.len.store(ranges.len(), Ordering::Relaxed);
            self.overflow.store(false, Ordering::Relaxed);
        }
        self.version.store(v + 2, Ordering::Release);
    }

    /// Whether `ptr` lies in a region: `Some(true)` or `Some(false)`, or `None` when there are
    /// more regions than the index holds and the list has to be asked.
    pub(crate) fn contains(&self, ptr: *const u8) -> Option<bool> {
        let p = ptr as usize;
        loop {
            let before = self.version.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let overflow = self.overflow.load(Ordering::Relaxed);
            let n = self.len.load(Ordering::Relaxed).min(self.starts.len());
            // The last region starting at or before `p`.
            let (mut lo, mut hi) = (0, n);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if self.starts[mid].load(Ordering::Relaxed) <= p {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            let inside = lo > 0 && p < self.ends[lo - 1].load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.version.load(Ordering::Relaxed) == before {
                return (!overflow).then_some(inside);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn at(n: usize) -> *const u8 {
        n as *const u8
    }

    #[test]
    fn bounds_answer_for_their_regions_alone() {
        let index = RangeIndex::new(4);
        assert_eq!(index.contains(at(0x1000)), Some(false), "empty");
        index.publish([(0x8000, 0x9000), (0x1000, 0x2000), (0x4000, 0x6000)]);
        for (p, want) in [
            (0x0fff, false),
            (0x1000, true),
            (0x1fff, true),
            (0x2000, false),
            (0x5000, true),
            (0x6000, false),
            (0x8fff, true),
            (0x9000, false),
            (usize::MAX, false),
        ] {
            assert_eq!(index.contains(at(p)), Some(want), "{p:#x}");
        }
        index.publish([(0x4000, 0x6000)]);
        assert_eq!(index.contains(at(0x1000)), Some(false), "retired");
        assert_eq!(index.contains(at(0x4000)), Some(true));
    }

    #[test]
    fn more_regions_than_it_holds_sends_lookups_to_the_list() {
        let index = RangeIndex::new(2);
        index.publish([(0x1000, 0x2000), (0x3000, 0x4000), (0x5000, 0x6000)]);
        assert_eq!(index.contains(at(0x1000)), None);
        assert_eq!(index.contains(at(0x7000)), None);
        index.publish([(0x1000, 0x2000)]);
        assert_eq!(index.contains(at(0x1000)), Some(true), "it answers again");
    }

    /// Readers that ask about a region present throughout always find it, and about memory in
    /// no region never do, while a writer adds and removes other regions as fast as it can.
    #[test]
    fn readers_see_the_regions_that_stay_while_others_come_and_go() {
        let index = Arc::new(RangeIndex::new(64));
        let stay = (0x10_0000, 0x20_0000);
        index.publish([stay]);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (index, stop) = (Arc::clone(&index), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let k = n % 40;
                    let others = (0..k).map(|i| {
                        let base = 0x100_0000 + i * 0x10_0000;
                        (base, base + 0x8_0000)
                    });
                    index.publish(std::iter::once(stay).chain(others));
                    n += 1;
                }
                n
            })
        };
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let index = Arc::clone(&index);
                std::thread::spawn(move || {
                    for i in 0..300_000usize {
                        assert_eq!(index.contains(at(0x10_0000 + i % 0x10_0000)), Some(true));
                        assert_eq!(index.contains(at(0x20_0000 + i % 0x1000)), Some(false));
                        assert_eq!(index.contains(at(0xfff)), Some(false));
                    }
                })
            })
            .collect();
        for r in readers {
            r.join().expect("a reader");
        }
        stop.store(true, Ordering::Relaxed);
        assert!(writer.join().expect("the writer") > 0);
    }
}
