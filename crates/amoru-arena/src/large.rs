//! The top of a region: the slab bump pointer, the large-allocation area that grows down to
//! meet it, and the free list that coalesces large blocks (e.1, f.3, f.4).
//!
//! Everything here is offsets inside one region, so this module needs no `unsafe`; the
//! pointer arithmetic that turns an offset into an address is in `classes.rs`.

use std::collections::BTreeMap;

/// One live large allocation: what it was charged and how many of its bytes are still held
/// by a `Buffer`. `split_at` hands out two buffers over one allocation, so the block is
/// returned only when every byte of it has been released (e.3).
#[derive(Copy, Clone, Debug)]
struct Live {
    charged: u64,
    outstanding: u64,
}

/// What a release did to the large area.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum LargeRelease {
    /// The whole block is back; its charged bytes and its offset range.
    Freed { offset: u64, charged: u64 },
    /// Some bytes of the block are still held by another `Buffer`.
    Partial,
    /// No live block covers this pointer: a double release (h).
    Unknown,
}

/// The slab bump pointer, the large area and the gap between them (e.1).
#[derive(Debug)]
pub(crate) struct TopArea {
    slab_top: u64,
    large_bottom: u64,
    free: BTreeMap<u64, u64>,
    live: BTreeMap<u64, Live>,
}

impl TopArea {
    /// An empty top area over a region of `bytes`: slabs grow up from 0, large allocations
    /// down from the end.
    pub(crate) fn new(bytes: u64) -> TopArea {
        TopArea {
            slab_top: 0,
            large_bottom: bytes,
            free: BTreeMap::new(),
            live: BTreeMap::new(),
        }
    }

    /// A top area with no gap at all: the small-budget mode hands the whole region to the
    /// size classes at `new`, so no slab can be claimed and no large allocation can be made
    /// (e.1).
    pub(crate) fn exhausted(bytes: u64) -> TopArea {
        TopArea {
            slab_top: bytes,
            large_bottom: bytes,
            free: BTreeMap::new(),
            live: BTreeMap::new(),
        }
    }

    /// Bytes between the highest slab and the lowest large allocation.
    pub(crate) fn gap(&self) -> u64 {
        self.large_bottom.saturating_sub(self.slab_top)
    }

    /// Claim `slab` bytes from the low end for a size class; `None` when the gap cannot
    /// serve it (AR-I2).
    pub(crate) fn claim_slab(&mut self, slab: u64) -> Option<u64> {
        if self.gap() < slab {
            return None;
        }
        let off = self.slab_top;
        self.slab_top += slab;
        Some(off)
    }

    /// Carve `charged` bytes from the top, reusing a free large block when one fits
    /// (f.3); `requested` is what the caller asked for and is what the block's outstanding
    /// byte count starts at.
    pub(crate) fn alloc_large(&mut self, charged: u64, requested: u64) -> Option<u64> {
        let reuse = self
            .free
            .iter()
            .find(|(_, len)| **len >= charged)
            .map(|(off, len)| (*off, *len));
        let off = match reuse {
            Some((off, len)) => {
                self.free.remove(&off);
                if len > charged {
                    self.free.insert(off + charged, len - charged);
                }
                off
            }
            None => {
                if self.gap() < charged {
                    return None;
                }
                self.large_bottom -= charged;
                self.large_bottom
            }
        };
        self.live.insert(
            off,
            Live {
                charged,
                outstanding: requested,
            },
        );
        Some(off)
    }

    /// Release `len` bytes at `off` from whichever live block covers it, coalescing and
    /// retreating the top when the block is wholly free (f.4).
    pub(crate) fn release_large(&mut self, off: u64, len: u64) -> LargeRelease {
        let Some((base, live)) = self
            .live
            .range(..=off)
            .next_back()
            .map(|(b, l)| (*b, *l))
            .filter(|(b, l)| off < b + l.charged)
        else {
            return LargeRelease::Unknown;
        };
        let rest = live.outstanding.saturating_sub(len);
        if rest > 0 {
            self.live.insert(
                base,
                Live {
                    charged: live.charged,
                    outstanding: rest,
                },
            );
            return LargeRelease::Partial;
        }
        self.live.remove(&base);
        self.insert_free(base, live.charged);
        LargeRelease::Freed {
            offset: base,
            charged: live.charged,
        }
    }

    /// Put `[off, off + len)` back on the free list, merge it with its neighbours, and
    /// retreat the top if the merged block starts there (f.4).
    fn insert_free(&mut self, off: u64, len: u64) {
        let mut start = off;
        let mut end = off + len;
        if let Some((p_off, p_len)) = self.free.range(..start).next_back().map(|(o, l)| (*o, *l))
            && p_off + p_len == start
        {
            self.free.remove(&p_off);
            start = p_off;
        }
        if let Some((n_off, n_len)) = self.free.range(end..).next().map(|(o, l)| (*o, *l))
            && n_off == end
        {
            self.free.remove(&n_off);
            end = n_off + n_len;
        }
        if start == self.large_bottom {
            self.large_bottom = end;
        } else {
            self.free.insert(start, end - start);
        }
    }

    /// The largest free large block, ignoring the gap.
    pub(crate) fn largest_free_block(&self) -> u64 {
        self.free.values().copied().max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn slabs_and_large_meet_in_the_middle() {
        let mut t = TopArea::new(1024 * MIB);
        assert_eq!(t.claim_slab(512 * MIB), Some(0));
        assert_eq!(t.gap(), 512 * MIB);
        assert_eq!(t.alloc_large(512 * MIB, 512 * MIB), Some(512 * MIB));
        assert_eq!(t.gap(), 0);
        assert_eq!(t.claim_slab(512 * MIB), None);
        assert_eq!(t.alloc_large(MIB, MIB), None);
    }

    #[test]
    fn an_exhausted_top_serves_nothing() {
        let mut t = TopArea::exhausted(256 * MIB);
        assert_eq!(t.gap(), 0);
        assert_eq!(t.claim_slab(1), None);
        assert_eq!(t.alloc_large(1, 1), None);
    }

    #[test]
    fn three_large_blocks_coalesce_back_into_the_gap() {
        let region = 2048 * MIB;
        let mut t = TopArea::new(region);
        let a = t.alloc_large(600 * MIB, 600 * MIB).expect("a");
        let b = t.alloc_large(600 * MIB, 600 * MIB).expect("b");
        let c = t.alloc_large(600 * MIB, 600 * MIB).expect("c");
        assert!(a > b && b > c);
        assert_eq!(t.gap(), region - 1800 * MIB);
        // free the middle, then the ends
        assert_eq!(
            t.release_large(b, 600 * MIB),
            LargeRelease::Freed {
                offset: b,
                charged: 600 * MIB
            }
        );
        assert_eq!(t.largest_free_block(), 600 * MIB);
        assert_eq!(
            t.release_large(a, 600 * MIB),
            LargeRelease::Freed {
                offset: a,
                charged: 600 * MIB
            }
        );
        assert_eq!(t.largest_free_block(), 1200 * MIB);
        assert_eq!(
            t.release_large(c, 600 * MIB),
            LargeRelease::Freed {
                offset: c,
                charged: 600 * MIB
            }
        );
        assert_eq!(t.largest_free_block(), 0);
        assert_eq!(t.gap(), region);
    }

    #[test]
    fn a_free_block_is_reused_and_split() {
        let mut t = TopArea::new(2048 * MIB);
        let a = t.alloc_large(600 * MIB, 600 * MIB).expect("a");
        let _b = t.alloc_large(600 * MIB, 600 * MIB).expect("b");
        t.release_large(a, 600 * MIB);
        let c = t.alloc_large(520 * MIB, 520 * MIB).expect("c");
        assert_eq!(c, a);
        assert_eq!(t.largest_free_block(), 80 * MIB);
    }

    #[test]
    fn a_split_block_returns_only_when_both_halves_do() {
        let mut t = TopArea::new(2048 * MIB);
        let a = t.alloc_large(600 * MIB, 600 * MIB).expect("a");
        assert_eq!(t.release_large(a, 200 * MIB), LargeRelease::Partial);
        assert_eq!(
            t.release_large(a + 200 * MIB, 400 * MIB),
            LargeRelease::Freed {
                offset: a,
                charged: 600 * MIB
            }
        );
        assert_eq!(t.release_large(a, 1), LargeRelease::Unknown);
        assert_eq!(t.release_large(0, 1), LargeRelease::Unknown);
    }
}
