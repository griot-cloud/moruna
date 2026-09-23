//! The size-class allocator over one region (e.2, e.3, f.3, f.4, f.5).
//!
//! A request rounds up to a power-of-two class from 64 KiB to 512 MiB; a class's free list
//! is refilled by claiming a slab from the low end of the region, sized to the region rather
//! than fixed at 512 MiB (e.1), or, in the small-budget mode, by splitting a larger block
//! buddy-style. Anything above 512 MiB is a large allocation carved from the top
//! (`large.rs`).
//!
//! `unsafe` is permitted in this module for the pointer arithmetic that turns an offset
//! into an address (section l); every block cites AR-I1 or AR-I2.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use moruna_kernel::Tier;

use crate::large::{LargeRelease, TopArea};

/// Class 0, and the granularity every offset in a region is tracked at.
pub(crate) const GRANULE: u64 = 64 * 1024;
/// Classes 0..=13, that is 64 KiB to 512 MiB (e.2).
pub(crate) const CLASS_COUNT: usize = 14;
/// A slab, and the size of class 13 (e.1, e.2).
pub(crate) const SLAB_BYTES: u64 = 512 * 1024 * 1024;
/// Below this host budget the region is served buddy-style from one slab (e.1).
pub(crate) const SMALL_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
/// A region gives one class at most this fraction of itself as a slab (e.1): a slab is
/// `min(SLAB_BYTES, max(class_size, region / SLAB_SHARE))`, so a small region spreads across
/// its classes instead of handing the first two everything.
const SLAB_SHARE: u64 = 8;

/// Granule marker: part of a large allocation.
const MARK_LARGE: u8 = 0xFD;
/// Granule marker: not assigned to any class yet.
const MARK_NONE: u8 = 0xFF;

/// The largest single allocation a free block of `bytes` can serve (f.5). Above a slab the
/// large path takes the request whole; below it the request rounds up to a class, so only the
/// largest class that fits can be served.
fn servable(bytes: u64) -> u64 {
    if bytes >= SLAB_BYTES {
        return bytes;
    }
    let mut size = 0;
    for c in 0..CLASS_COUNT {
        if class_size(c) <= bytes {
            size = class_size(c);
        }
    }
    size
}

/// Bytes in class `c`.
pub(crate) fn class_size(c: usize) -> u64 {
    GRANULE << c
}

/// The class that serves `bytes`, or `None` when the request is a large allocation (e.2).
pub(crate) fn class_for(bytes: u64) -> Option<usize> {
    if bytes <= GRANULE {
        return Some(0);
    }
    if bytes > SLAB_BYTES {
        return None;
    }
    let ceil_log2 = u64::BITS - (bytes - 1).leading_zeros();
    Some(ceil_log2 as usize - 16)
}

/// Why an allocation could not be served.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) struct OutOfSpace;

/// What a release did, so the caller can keep the statistics exact (AR-I5).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum Release {
    /// The allocation is wholly back; `charged` bytes left the in-use total.
    Freed,
    /// Some of the allocation is still held by another `Buffer` (e.3).
    Partial,
    /// Nothing at this address is live: a double release (h, section j).
    Double,
}

/// One region's allocator: the class free lists, the granule maps that turn a pointer back
/// into its slot, and the top area shared by slab claims and large allocations.
pub(crate) struct Space {
    base: *mut u8,
    bytes: u64,
    tier: Tier,
    small_mode: bool,
    /// Per 64 KiB granule: the class of the slot covering it, `MARK_LARGE`, or `MARK_NONE`.
    mark: Box<[AtomicU8]>,
    /// Per 64 KiB granule, meaningful at a slot's first granule: bytes of that slot still
    /// held by a `Buffer`. `split_at` partitions the bytes, so the slot returns to the free
    /// list when the count reaches zero and not before (e.3).
    outstanding: Box<[AtomicU64]>,
    /// Per 64 KiB granule, meaningful at an allocation's first granule: which incarnation of
    /// that slot or block is live. It is bumped every time the allocation is freed, so the
    /// token a `Buffer` carries stops matching the moment its bytes go back on a free list
    /// and a release that arrives after that is refused instead of being credited to
    /// whatever holds the address now (h, AR-I5).
    generation: Box<[AtomicU32]>,
    free: [Mutex<Vec<u64>>; CLASS_COUNT],
    slabs_by_class: [AtomicU32; CLASS_COUNT],
    top: Mutex<TopArea>,
    in_use: AtomicU64,
    double_release: AtomicU64,
    untagged_release: AtomicU64,
}

// SAFETY: `base` is the region's address, which the `Mapping` beside this `Space` owns for
// the life of the arena (AR-I3); every field that mutates is a lock or an atomic, and no
// `&self` method reads or writes the region's contents.
unsafe impl Send for Space {}
// SAFETY: see the `Send` impl.
unsafe impl Sync for Space {}

impl Space {
    /// Build the allocator over `[base, base + bytes)`. `bytes` is a multiple of the
    /// granule and the region's base is aligned to at least the granule, so every class
    /// slot is aligned to its own size and therefore to `ALIGNMENT` and, when the class is
    /// at least that big, to the page size (AR-I1).
    pub(crate) fn new(base: *mut u8, bytes: u64, tier: Tier, small_mode: bool) -> Space {
        let granules = (bytes / GRANULE) as usize;
        let mark: Box<[AtomicU8]> = (0..granules).map(|_| AtomicU8::new(MARK_NONE)).collect();
        let outstanding: Box<[AtomicU64]> = (0..granules).map(|_| AtomicU64::new(0)).collect();
        // Generation 0 is never handed out, so `NO_TOKEN` cannot be mistaken for a token of
        // the region's first granule.
        let generation: Box<[AtomicU32]> = (0..granules).map(|_| AtomicU32::new(1)).collect();
        let space = Space {
            base,
            bytes,
            tier,
            small_mode,
            mark,
            outstanding,
            generation,
            free: std::array::from_fn(|_| Mutex::new(Vec::new())),
            slabs_by_class: std::array::from_fn(|_| AtomicU32::new(0)),
            top: Mutex::new(if small_mode {
                TopArea::exhausted(bytes)
            } else {
                TopArea::new(bytes)
            }),
            in_use: AtomicU64::new(0),
            double_release: AtomicU64::new(0),
            untagged_release: AtomicU64::new(0),
        };
        if small_mode {
            space.seed_small_mode();
        }
        space
    }

    /// The small-budget mode (e.1): the whole region becomes blocks, largest first, and
    /// every class is served by splitting one of them.
    fn seed_small_mode(&self) {
        let mut off = 0u64;
        while off < self.bytes {
            let remaining = self.bytes - off;
            let mut c = CLASS_COUNT - 1;
            while class_size(c) > remaining {
                if c == 0 {
                    return;
                }
                c -= 1;
            }
            self.mark_range(off, class_size(c), c as u8);
            self.lock_free(c).push(off);
            self.slabs_by_class[c].fetch_add(1, Ordering::Relaxed);
            off += class_size(c);
        }
    }

    /// The tier every buffer from this region carries.
    pub(crate) fn tier(&self) -> Tier {
        self.tier
    }

    /// Usable bytes in the region.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Charged bytes currently held by live buffers (AR-I5).
    pub(crate) fn in_use(&self) -> u64 {
        self.in_use.load(Ordering::Relaxed)
    }

    /// Releases that named no live allocation, or named one whose incarnation has gone, or
    /// more bytes than it still had outstanding (section j).
    pub(crate) fn double_releases(&self) -> u64 {
        self.double_release.load(Ordering::Relaxed)
    }

    /// Releases that carried no token. Every buffer this region hands out carries one, so
    /// such a release cannot have come from one of them and is refused rather than guessed
    /// at (section j).
    pub(crate) fn untagged_releases(&self) -> u64 {
        self.untagged_release.load(Ordering::Relaxed)
    }

    /// Count a release that arrived without a token, and refuse it.
    pub(crate) fn refuse_untagged(&self) -> Release {
        self.untagged_release.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "arena.release_untagged", "a release with no allocation token; the arena will not guess which allocation it belongs to");
        Release::Double
    }

    /// True when `ptr` is inside this region (f.7).
    pub(crate) fn contains(&self, ptr: *const u8) -> bool {
        let p = ptr as usize;
        let b = self.base as usize;
        p >= b && (p - b) < self.bytes as usize
    }

    /// Slabs claimed per class, for the report (section j).
    pub(crate) fn slabs_by_class(&self) -> [u32; CLASS_COUNT] {
        std::array::from_fn(|c| self.slabs_by_class[c].load(Ordering::Relaxed))
    }

    /// Bytes sitting on class free lists: claimed against the budget but not in a buffer
    /// (f.5, section j).
    pub(crate) fn stranded_bytes(&self) -> u64 {
        (0..CLASS_COUNT)
            .map(|c| self.lock_free(c).len() as u64 * class_size(c))
            .sum()
    }

    /// The largest single allocation this region can still serve (d.1, f.5).
    ///
    /// A free block of `n` bytes serves a request of `n` through the large path when `n` is
    /// above `SLAB_BYTES`, and otherwise a class request of at most the largest class size
    /// that fits it, because a class request rounds its size up.
    pub(crate) fn largest_free(&self) -> u64 {
        let mut best = (0..CLASS_COUNT)
            .filter(|c| !self.lock_free(*c).is_empty())
            .map(class_size)
            .max()
            .unwrap_or(0);
        let top = self.lock_top();
        best = best.max(servable(top.gap()));
        best = best.max(servable(top.largest_free_block()));
        best
    }

    /// Serve `bytes` (f.3). Returns the address and what it was charged.
    /// Serve `bytes`: the address, what it cost the budget, and the token that names this
    /// allocation for its release (AR-I5).
    pub(crate) fn alloc(&self, bytes: u64) -> Result<(*mut u8, u64, u64), OutOfSpace> {
        // A zero-length request owns nothing: it is served from the region's first byte,
        // charged nothing, and its release is a no-op, so it can never free a live slot
        // (h: `alloc(0)` returns a buffer with `len() == 0` and never a null pointer). It
        // gets no token, because there is no allocation for one to name.
        if bytes == 0 {
            return Ok((self.base, 0, moruna_kernel::NO_TOKEN));
        }
        let offset = match class_for(bytes) {
            Some(c) => self.alloc_class(c, bytes)?,
            None => self.alloc_large(bytes)?,
        };
        // SAFETY: `offset < bytes` and the region is `bytes` long, so the address is inside
        // the mapping this `Space` owns (AR-I2: the allocator never hands out an offset the
        // region does not cover).
        Ok((
            unsafe { self.base.add(offset as usize) },
            self.charged(bytes),
            self.token_for(offset),
        ))
    }

    /// The token for the allocation whose first granule holds `offset`: which granule, and
    /// which incarnation of it. The granule is stored plus one so that no valid token is
    /// `NO_TOKEN`.
    fn token_for(&self, offset: u64) -> u64 {
        let g = (offset / GRANULE) as usize;
        let generation = u64::from(self.generation[g].load(Ordering::Acquire));
        (generation << 32) | (g as u64 + 1)
    }

    /// Retire the incarnation of the allocation at granule `g`, so every token naming it
    /// stops matching and a release that arrives afterwards is refused.
    fn retire(&self, g: usize) {
        self.generation[g].fetch_add(1, Ordering::AcqRel);
    }

    /// What a request of `bytes` costs against the budget (e.2).
    pub(crate) fn charged(&self, bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        match class_for(bytes) {
            Some(c) => class_size(c),
            None => crate::region::align_up(bytes, GRANULE),
        }
    }

    fn alloc_class(&self, c: usize, requested: u64) -> Result<u64, OutOfSpace> {
        let mut fl = self.lock_free(c);
        if let Some(off) = fl.pop() {
            drop(fl);
            self.activate(off, requested, class_size(c));
            return Ok(off);
        }
        // Fresh region first, then a larger class's stranded slot. The second is what keeps
        // f.5's stranding from becoming a refusal in a region that still holds free slots of
        // another class (e.1): a class never returns a slab to the bump area, but a slot on a
        // larger class's free list splits down like any block in the small-budget mode.
        let off = if self.small_mode {
            self.split_down(c, &mut fl).ok_or(OutOfSpace)?
        } else {
            match self.claim_slab(c, &mut fl) {
                Some(off) => off,
                None => self.split_down(c, &mut fl).ok_or(OutOfSpace)?,
            }
        };
        drop(fl);
        self.activate(off, requested, class_size(c));
        Ok(off)
    }

    /// Claim a slab for class `c` and push every slot but the one returned (f.3). The
    /// class lock is held throughout and the bump lock is taken under it, which is the
    /// component's lock order (section g).
    ///
    /// The slab is `slab_bytes(c)` (e.1), and `TopArea::claim_slab` shrinks it to what the gap
    /// can serve when the gap is smaller, down to a single slot. A fixed 512 MiB slab made a
    /// region smaller than 512 MiB times the classes in use refuse allocations with most of
    /// itself free, which AR-I2 and the budget arithmetic of 11 f.1 both forbid.
    fn claim_slab(&self, c: usize, fl: &mut Vec<u64>) -> Option<u64> {
        let size = class_size(c);
        let (off, slab) = self.lock_top().claim_slab(self.slab_bytes(c), size)?;
        self.mark_range(off, slab, c as u8);
        for i in 1..(slab / size) {
            fl.push(off + i * size);
        }
        self.slabs_by_class[c].fetch_add(1, Ordering::Relaxed);
        Some(off)
    }

    /// The slab size class `c` asks for in this region (e.1): `min(512 MiB,
    /// max(class_size(c), region / SLAB_SHARE))`, rounded down to a whole number of slots.
    /// Above about 4 GiB this is 512 MiB again, so a region large enough to have wanted the
    /// fixed slab still gets it.
    fn slab_bytes(&self, c: usize) -> u64 {
        let size = class_size(c);
        let want = (self.bytes / SLAB_SHARE).max(size).min(SLAB_BYTES);
        (want / size * size).max(size)
    }

    /// Small-budget mode: split the smallest free block larger than class `c` down to `c`,
    /// pushing each buddy onto its own class list (e.1). Class locks are taken in
    /// ascending class order, so two splitters cannot deadlock.
    fn split_down(&self, c: usize, fl: &mut Vec<u64>) -> Option<u64> {
        let mut donor = None;
        for d in (c + 1)..CLASS_COUNT {
            if let Some(off) = self.lock_free(d).pop() {
                donor = Some((d, off));
                break;
            }
        }
        let (d, off) = donor?;
        for k in (c..d).rev() {
            let buddy = off + class_size(k);
            self.mark_range(buddy, class_size(k), k as u8);
            if k == c {
                fl.push(buddy);
            } else {
                self.lock_free(k).push(buddy);
            }
        }
        self.mark_range(off, class_size(c), c as u8);
        Some(off)
    }

    fn alloc_large(&self, requested: u64) -> Result<u64, OutOfSpace> {
        let charged = crate::region::align_up(requested, GRANULE);
        let off = self
            .lock_top()
            .alloc_large(charged, requested)
            .ok_or(OutOfSpace)?;
        self.mark_range(off, charged, MARK_LARGE);
        self.in_use.fetch_add(charged, Ordering::Relaxed);
        Ok(off)
    }

    /// Hand a slot out: record how many of its bytes are live and charge the class size.
    fn activate(&self, off: u64, requested: u64, charged: u64) {
        self.outstanding[(off / GRANULE) as usize].store(requested, Ordering::Release);
        self.in_use.fetch_add(charged, Ordering::Relaxed);
    }

    /// Return `len` bytes at `ptr` to the allocation `token` names (f.4). A zero-length
    /// buffer owns nothing and returns nothing, which is what makes `alloc(0)` and a zero
    /// half of `split_at` safe.
    ///
    /// The allocation is named by the token, not worked out from `ptr`. An address says which
    /// granule it is in and nothing about which incarnation of that granule is live, and the
    /// region is never unmapped mid-run (AR-I3), so addresses recycle freely: a release
    /// credited by address alone can take down a block a live `Buffer` still points at, which
    /// is the same bytes handed to two owners with nothing reported. The token carries the
    /// incarnation, so a release of bytes that have already gone back matches nothing and is
    /// refused (h, AR-I5).
    pub(crate) fn release(&self, ptr: *mut u8, len: u64, token: u64) -> Release {
        if len == 0 {
            return Release::Partial;
        }
        if token == moruna_kernel::NO_TOKEN {
            return self.refuse_untagged();
        }
        let g = ((token & 0xFFFF_FFFF) - 1) as usize;
        let generation = (token >> 32) as u32;
        if g >= self.generation.len() || self.generation[g].load(Ordering::Acquire) != generation {
            self.double_release.fetch_add(1, Ordering::Relaxed);
            return Release::Double;
        }
        let off = (ptr as usize - self.base as usize) as u64;
        let base = g as u64 * GRANULE;
        match self.mark[g].load(Ordering::Acquire) {
            MARK_NONE => {
                self.double_release.fetch_add(1, Ordering::Relaxed);
                Release::Double
            }
            MARK_LARGE => self.release_large(g, base, off, len),
            mark => self.release_class(g, base, off, len, mark as usize),
        }
    }

    fn release_class(&self, g: usize, slot: u64, off: u64, len: u64, c: usize) -> Release {
        let size = class_size(c);
        // The released range has to lie inside the slot the token named. A pointer that has
        // drifted out of its own slot is not a release of that slot.
        if off < slot || off.saturating_add(len) > slot + size {
            self.double_release.fetch_add(1, Ordering::Relaxed);
            return Release::Double;
        }
        loop {
            let cur = self.outstanding[g].load(Ordering::Acquire);
            // More bytes than the slot still has outstanding is an over-release, refused
            // rather than saturated: saturating here freed the slot while another `Buffer`
            // held the rest of it.
            let Some(next) = cur.checked_sub(len) else {
                self.double_release.fetch_add(1, Ordering::Relaxed);
                return Release::Double;
            };
            if self.outstanding[g]
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            if next > 0 {
                return Release::Partial;
            }
            self.retire(g);
            self.lock_free(c).push(slot);
            self.in_use.fetch_sub(size, Ordering::Relaxed);
            return Release::Freed;
        }
    }

    fn release_large(&self, g: usize, base: u64, off: u64, len: u64) -> Release {
        match self.lock_top().release_large_at(base, off, len) {
            LargeRelease::Freed { offset, charged } => {
                self.retire(g);
                self.mark_range(offset, charged, MARK_NONE);
                self.in_use.fetch_sub(charged, Ordering::Relaxed);
                Release::Freed
            }
            LargeRelease::Partial => Release::Partial,
            LargeRelease::Unknown => {
                self.double_release.fetch_add(1, Ordering::Relaxed);
                Release::Double
            }
        }
    }

    fn mark_range(&self, off: u64, len: u64, value: u8) {
        let first = (off / GRANULE) as usize;
        let last = ((off + len) / GRANULE) as usize;
        for m in &self.mark[first..last] {
            m.store(value, Ordering::Release);
        }
    }

    fn lock_free(&self, c: usize) -> std::sync::MutexGuard<'_, Vec<u64>> {
        self.free[c].lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_top(&self) -> std::sync::MutexGuard<'_, TopArea> {
        self.top.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// A `Space` over a heap allocation, for the unit tests in this module; the real one
    /// sits on the region `region.rs` maps.
    struct Heap {
        space: Space,
        _backing: Vec<u8>,
    }

    fn heap(bytes: u64, small_mode: bool) -> Heap {
        let mut backing = vec![0u8; (bytes + GRANULE) as usize];
        let raw = backing.as_mut_ptr();
        let base = crate::region::align_up(raw as usize as u64, GRANULE) - raw as usize as u64;
        // SAFETY: test-only; `base` is at most one granule into a buffer that is a granule
        // longer than the space, and the `Vec` outlives the `Space`.
        let ptr = unsafe { raw.add(base as usize) };
        Heap {
            space: Space::new(ptr, bytes, Tier::Host, small_mode),
            _backing: backing,
        }
    }

    #[test]
    fn classes_round_up() {
        assert_eq!(class_for(1), Some(0));
        assert_eq!(class_for(GRANULE), Some(0));
        assert_eq!(class_for(GRANULE + 1), Some(1));
        assert_eq!(class_for(4 * MIB), Some(6));
        assert_eq!(class_for(SLAB_BYTES), Some(13));
        assert_eq!(class_for(SLAB_BYTES + 1), None);
        assert_eq!(class_size(0), GRANULE);
        assert_eq!(class_size(13), SLAB_BYTES);
    }

    #[test]
    fn a_zero_length_request_owns_nothing() {
        let h = heap(4 * MIB, true);
        let (ptr, charged, token) = h.space.alloc(0).expect("zero");
        assert_eq!(charged, 0);
        assert_eq!(token, moruna_kernel::NO_TOKEN);
        assert!(h.space.contains(ptr));
        assert_eq!(h.space.in_use(), 0);
        assert_eq!(h.space.release(ptr, 0, token), Release::Partial);
        assert_eq!(h.space.double_releases(), 0);
        assert_eq!(h.space.untagged_releases(), 0);
    }

    #[test]
    fn small_mode_splits_and_returns() {
        let h = heap(4 * MIB, true);
        let (a, charged, ta) = h.space.alloc(1000).expect("a");
        assert_eq!(charged, GRANULE);
        assert_eq!(h.space.in_use(), GRANULE);
        assert_eq!(a as usize % GRANULE as usize, 0);
        let (b, _, tb) = h.space.alloc(GRANULE + 1).expect("b");
        assert_eq!(h.space.in_use(), GRANULE + 2 * GRANULE);
        assert_eq!(h.space.release(a, 1000, ta), Release::Freed);
        assert_eq!(h.space.release(b, GRANULE + 1, tb), Release::Freed);
        assert_eq!(h.space.in_use(), 0);
        assert!(h.space.stranded_bytes() > 0);
    }

    #[test]
    fn a_split_slot_returns_once_both_halves_do() {
        let h = heap(4 * MIB, true);
        let (a, _, t) = h.space.alloc(GRANULE).expect("a");
        assert_eq!(h.space.release(a, 16, t), Release::Partial);
        // SAFETY: test-only; the second half of a live slot.
        let mid = unsafe { a.add(16) };
        assert_eq!(h.space.release(mid, GRANULE - 16, t), Release::Freed);
        // The slot has gone back, so the token names an incarnation that is over and the
        // release is refused rather than credited to whoever holds the slot next.
        assert_eq!(h.space.release(a, 1, t), Release::Double);
        assert_eq!(h.space.double_releases(), 1);
        // A release with no token at all is refused too: every buffer this region hands out
        // carries one, so the arena has nothing to attribute it to.
        assert_eq!(
            h.space.release(a, 1, moruna_kernel::NO_TOKEN),
            Release::Double
        );
        assert_eq!(h.space.untagged_releases(), 1);
    }

    #[test]
    fn an_unassigned_address_is_a_double_release() {
        let h = heap(4 * MIB, false);
        // Granule 0, incarnation 1: the token an allocation there would have carried.
        assert_eq!(
            h.space.release(h.space.base, 1, (1 << 32) | 1),
            Release::Double
        );
        assert_eq!(h.space.double_releases(), 1);
    }

    #[test]
    fn a_small_region_runs_out() {
        let h = heap(4 * MIB, true);
        let mut live = Vec::new();
        while let Ok((p, c, t)) = h.space.alloc(MIB) {
            live.push((p, c, t));
        }
        assert_eq!(live.len(), 4);
        assert_eq!(h.space.in_use(), 4 * MIB);
        assert_eq!(h.space.alloc(SLAB_BYTES + 1), Err(OutOfSpace));
        assert_eq!(h.space.largest_free(), 0);
        for (p, _, t) in live {
            assert_eq!(h.space.release(p, MIB, t), Release::Freed);
        }
        assert_eq!(h.space.largest_free(), MIB);
    }

    #[test]
    fn big_mode_claims_slabs() {
        let h = heap(2 * SLAB_BYTES, false);
        assert_eq!(h.space.largest_free(), 2 * SLAB_BYTES);
        let (a, charged, t) = h.space.alloc(MIB).expect("a");
        assert_eq!(charged, MIB);
        assert_eq!(h.space.slabs_by_class()[4], 1);
        // The slab is the region's eighth, not a fixed 512 MiB (e.1).
        assert_eq!(h.space.stranded_bytes(), 2 * SLAB_BYTES / 8 - MIB);
        assert_eq!(h.space.tier(), Tier::Host);
        assert_eq!(h.space.bytes(), 2 * SLAB_BYTES);
        assert_eq!(h.space.release(a, MIB, t), Release::Freed);
        assert_eq!(h.space.charged(SLAB_BYTES + 1), SLAB_BYTES + GRANULE);
    }
}
