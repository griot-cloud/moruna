//! `FakeAllocator`, the `Allocator` fake of contracts d.15.

use std::alloc::{Layout, alloc, dealloc};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_kernel::arrow;
use moruna_kernel::{
    ALIGNMENT, AllocStats, Allocator, ArenaHandle, Buffer, MorunaError, Result, Tier,
};

/// A region this allocator handed out.
#[derive(Copy, Clone, Debug)]
struct Region {
    base: usize,
    len: usize,
    layout: Layout,
    tier: Tier,
    /// Bytes not yet released. A region goes back to the system when this reaches zero and
    /// not before, because `Buffer::split_at` hands two owners into one allocation and
    /// contracts d.3 frees them independently.
    outstanding: usize,
}

#[derive(Default)]
struct State {
    /// Live regions by the token their buffers carry, which is what a release is attributed
    /// by. Keying this by address and finding the region with a range query is what it used
    /// to do, and that is unsound: this allocator really does hand memory back to the system,
    /// so addresses recycle, and a release that arrives after its region has gone can land
    /// inside a *different* live region, free it under its owner and turn the next read of
    /// those bytes into a use-after-free.
    regions: BTreeMap<u64, Region>,
    /// Base address to token, for `contains` and `tier_of`, which are given an address and
    /// nothing else. A lookup here is a question about live regions only and frees nothing.
    by_addr: BTreeMap<usize, u64>,
    next_token: u64,
    /// Releases this allocator would not honour: no such region, bytes outside the region the
    /// token named, more bytes than it still had outstanding, or no token at all.
    refused: u64,
    host_in_use: u64,
    pinned_in_use: u64,
    device_in_use: [u64; 8],
    fail_next: u64,
}

struct Inner {
    state: Mutex<State>,
    allocations_total: AtomicU64,
    payload_copies: AtomicU64,
    boundary_copies: AtomicU64,
    limits: BTreeMap<usize, u64>,
    page_bytes: usize,
    pinned: bool,
}

/// The arena as a test sees it: real heap allocations tagged with the tier they were asked for,
/// so `Payload::table` tier inference, `Buffer::into_arrow_buffer` and `BufferView::of_arrow`
/// work over them (d.15).
///
/// Knobs: `with_limit(tier, bytes)`, `pinned(bool)`, `page_bytes(n)`, `fail_next(n)`.
/// Observables: `allocations_total()`, `in_use(tier)`, `stats()`.
#[derive(Clone)]
pub struct FakeAllocator {
    inner: Arc<Inner>,
}

impl Default for FakeAllocator {
    fn default() -> Self {
        FakeAllocator::new()
    }
}

impl FakeAllocator {
    /// An allocator with no limits, a 4096-byte page and an unpinned host tier.
    pub fn new() -> FakeAllocator {
        FakeAllocator {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                allocations_total: AtomicU64::new(0),
                payload_copies: AtomicU64::new(0),
                boundary_copies: AtomicU64::new(0),
                limits: BTreeMap::new(),
                page_bytes: 4096,
                pinned: false,
            }),
        }
    }

    /// Knob: cap `tier` at `bytes`; an allocation that would exceed it fails with `Alloc`.
    pub fn with_limit(self, tier: Tier, bytes: u64) -> FakeAllocator {
        self.rebuild(|inner| {
            inner.limits.insert(tier.index(), bytes);
        })
    }

    /// Knob: whether the host tier is page-locked, which makes it `PinnedHost` (e.1).
    pub fn pinned(self, pinned: bool) -> FakeAllocator {
        self.rebuild(|inner| inner.pinned = pinned)
    }

    /// Knob: the page size this allocator reports and aligns large allocations to.
    pub fn page_bytes(self, bytes: usize) -> FakeAllocator {
        self.rebuild(|inner| inner.page_bytes = bytes)
    }

    /// Knob: fail the next `n` allocations with `Alloc`, whatever the tier.
    pub fn fail_next(self, n: u64) -> FakeAllocator {
        {
            let mut state = self.lock();
            state.fail_next = state.fail_next.saturating_add(n);
        }
        self
    }

    /// Observable: allocations made since this allocator was created.
    /// Payload bytes copied with the CPU, as `Allocator::note_payload_copy` was told.
    /// A source decoding Parquet and a sink encoding it are the only callers G-I2
    /// allows, so a test asserting "one decode copy per morsel and none for tensors"
    /// reads this (added 2026-09-22; `stats()` reported a hardcoded zero and the
    /// counter had no observable at all).
    pub fn payload_copies(&self) -> u64 {
        self.inner.payload_copies.load(Ordering::Relaxed)
    }

    /// Bytes an adapter copied once at the kernel boundary (05 AD-I2).
    pub fn boundary_copies(&self) -> u64 {
        self.inner.boundary_copies.load(Ordering::Relaxed)
    }

    /// Every successful `alloc` since this allocator was built.
    pub fn allocations_total(&self) -> u64 {
        self.inner.allocations_total.load(Ordering::SeqCst)
    }

    /// Observable: releases this allocator refused because it could not attribute them with
    /// certainty. Zero is the only acceptable value; anything else is a buffer's bytes being
    /// handed back twice, or after their region has gone, and the message on stderr says
    /// which (d.15, h).
    pub fn refused_releases(&self) -> u64 {
        self.lock().refused
    }

    /// Observable: regions this allocator has handed out and not yet taken back.
    pub fn live_regions(&self) -> usize {
        self.lock().regions.len()
    }

    /// Observable: bytes in use in one tier.
    pub fn in_use(&self, tier: Tier) -> u64 {
        let state = self.lock();
        match tier {
            Tier::Host => state.host_in_use,
            Tier::PinnedHost => state.pinned_in_use,
            Tier::Device(id) => state.device_in_use[id.0 as usize],
            Tier::Disk(_) | Tier::Remote(_, _) => 0,
        }
    }

    /// Allocate and unwrap, for a test that treats a failure here as a broken fixture.
    ///
    /// # Panics
    /// When the allocation fails; the message names the tier and the size.
    pub fn buffer(&self, bytes: usize, tier: Tier) -> Buffer {
        match self.alloc(bytes, tier) {
            Ok(buffer) => buffer,
            Err(e) => panic!("FakeAllocator could not allocate {bytes} bytes in {tier:?}: {e}"),
        }
    }

    /// An Arrow buffer over a region of this allocator, filled from `bytes`, so a test can build
    /// a batch whose buffers the arena owns.
    ///
    /// # Panics
    /// When the allocation or the conversion fails.
    pub fn arrow_buffer(&self, bytes: &[u8], tier: Tier) -> arrow::buffer::Buffer {
        let mut buffer = self.buffer(bytes.len().max(1), tier);
        buffer[..bytes.len()].copy_from_slice(bytes);
        match buffer.into_arrow_buffer() {
            Ok(b) => b.slice_with_length(0, bytes.len()),
            Err(e) => panic!("into_arrow_buffer: {e}"),
        }
    }

    /// The run's one host tier: `PinnedHost` when `pinned`, `Host` otherwise (e.1).
    pub fn host_tier(&self) -> Tier {
        if self.inner.pinned {
            Tier::PinnedHost
        } else {
            Tier::Host
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Apply a builder knob. The knobs are set before the allocator is shared, so rebuilding the
    /// immutable half is enough and no allocation is ever orphaned.
    fn rebuild(self, f: impl FnOnce(&mut InnerBuilder)) -> FakeAllocator {
        let mut builder = InnerBuilder {
            limits: self.inner.limits.clone(),
            page_bytes: self.inner.page_bytes,
            pinned: self.inner.pinned,
        };
        f(&mut builder);
        let state = std::mem::take(&mut *self.lock());
        FakeAllocator {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                allocations_total: AtomicU64::new(self.allocations_total()),
                payload_copies: AtomicU64::new(self.payload_copies()),
                boundary_copies: AtomicU64::new(self.boundary_copies()),
                limits: builder.limits,
                page_bytes: builder.page_bytes,
                pinned: builder.pinned,
            }),
        }
    }

    fn in_use_for(state: &State, tier: Tier) -> u64 {
        match tier {
            Tier::Host => state.host_in_use,
            Tier::PinnedHost => state.pinned_in_use,
            Tier::Device(id) => state.device_in_use[id.0 as usize],
            Tier::Disk(_) | Tier::Remote(_, _) => 0,
        }
    }
}

struct InnerBuilder {
    limits: BTreeMap<usize, u64>,
    page_bytes: usize,
    pinned: bool,
}

impl ArenaHandle for Inner {
    /// A release with no token. Every buffer this allocator hands out carries one, so this
    /// can only be bytes released by hand; it is counted and refused rather than attributed
    /// to whichever region happens to cover the address now.
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier) {
        self.release_token(ptr, len, tier, moruna_kernel::NO_TOKEN);
    }

    /// Return `len` bytes at `ptr` to the region `token` names.
    ///
    /// A `split_at` half releases only its own part of a region, and contracts d.3 says both
    /// halves are freed independently, so the region goes back to the system only when every
    /// byte of it has been released. Which region that is comes from the token the buffer
    /// carries, never from the address: this allocator calls `dealloc`, so an address that
    /// has been freed can be handed back by the system as part of a later, larger region, and
    /// a stale release credited by address would then retire a region a live buffer still
    /// points into. That is a use-after-free, and it is what a SIGSEGV looks like.
    fn release_token(&self, ptr: *mut u8, len: usize, tier: Tier, token: u64) {
        // A zero-length buffer owns no bytes: it returns nothing and can free nothing. This
        // is the half a `split_at` at 0 or at `len` leaves behind, and it may well outlive
        // its region.
        if len == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if token == moruna_kernel::NO_TOKEN {
            state.refused += 1;
            report(&format!(
                "untagged release ptr={ptr:?} len={len} tier={tier:?}"
            ));
            return;
        }
        let Some(region) = state.regions.get(&token).copied() else {
            state.refused += 1;
            report(&format!(
                "release of a region that has gone: token={token} ptr={ptr:?} len={len} tier={tier:?}"
            ));
            return;
        };
        // `wrapping_sub` makes a pointer below the base come out enormous, so one
        // comparison covers both ends of the region.
        let offset = (ptr as usize).wrapping_sub(region.base);
        if offset.saturating_add(len) > region.len {
            state.refused += 1;
            report(&format!(
                "release outside its region: token={token} ptr={ptr:?} base={:#x} len={len} region={}",
                region.base, region.len
            ));
            return;
        }
        let Some(outstanding) = region.outstanding.checked_sub(len) else {
            state.refused += 1;
            report(&format!(
                "release of more bytes than the region has left: token={token} len={len} outstanding={}",
                region.outstanding
            ));
            return;
        };
        match tier {
            Tier::Host => state.host_in_use = state.host_in_use.saturating_sub(len as u64),
            Tier::PinnedHost => {
                state.pinned_in_use = state.pinned_in_use.saturating_sub(len as u64)
            }
            Tier::Device(id) => {
                let slot = &mut state.device_in_use[id.0 as usize];
                *slot = slot.saturating_sub(len as u64);
            }
            Tier::Disk(_) | Tier::Remote(_, _) => {}
        }
        if outstanding > 0 {
            if let Some(entry) = state.regions.get_mut(&token) {
                entry.outstanding = outstanding;
            }
            return;
        }
        state.regions.remove(&token);
        state.by_addr.remove(&region.base);
        // SAFETY: every byte of this region has now been released, the layout is the one
        // `alloc` used, and both index entries are removed before the free, so no later
        // lookup can reach it and no second release can be attributed to it.
        unsafe { dealloc(region.base as *mut u8, region.layout) };
    }
}

/// Report an accounting anomaly straight to stderr, past a test harness's output capture, so
/// the line survives a crash of the process. The first few carry a backtrace; the rest are
/// counted by `FakeAllocator::refused_releases` alone, because a loop that has gone wrong can
/// produce thousands.
fn report(what: &str) {
    use std::io::Write;
    static SEEN: AtomicU64 = AtomicU64::new(0);
    let seen = SEEN.fetch_add(1, Ordering::Relaxed);
    if seen >= 64 {
        return;
    }
    let mut err = std::io::stderr();
    let _ = writeln!(err, "FAKEALLOC {what}");
    if seen < 3 {
        let _ = writeln!(err, "{}", std::backtrace::Backtrace::force_capture());
    }
    let _ = err.flush();
}

impl Allocator for FakeAllocator {
    fn alloc(&self, bytes: usize, tier: Tier) -> Result<Buffer> {
        let len = bytes.max(1);
        let mut state = self.lock();
        if state.fail_next > 0 {
            state.fail_next -= 1;
            return Err(MorunaError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: 0,
                in_use: FakeAllocator::in_use_for(&state, tier),
            });
        }
        if !tier.is_resident() {
            return Err(MorunaError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: 0,
                in_use: 0,
            });
        }
        if let Some(budget) = self.inner.limits.get(&tier.index()).copied() {
            let in_use = FakeAllocator::in_use_for(&state, tier);
            if in_use + len as u64 > budget {
                return Err(MorunaError::Alloc {
                    bytes: bytes as u64,
                    tier,
                    budget,
                    in_use,
                });
            }
        }
        let align = if len >= self.inner.page_bytes {
            self.inner.page_bytes
        } else {
            ALIGNMENT
        };
        let Ok(layout) = Layout::from_size_align(len, align) else {
            return Err(MorunaError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: 0,
                in_use: 0,
            });
        };
        // SAFETY: the layout has a non-zero size and a power-of-two alignment.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Err(MorunaError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: 0,
                in_use: 0,
            });
        }
        // SAFETY: the region was just allocated, so writing zeros initialises every byte of it.
        unsafe { std::ptr::write_bytes(ptr, 0, len) };
        state.next_token += 1;
        let token = state.next_token;
        state.regions.insert(
            token,
            Region {
                base: ptr as usize,
                len,
                layout,
                tier,
                outstanding: len,
            },
        );
        state.by_addr.insert(ptr as usize, token);
        match tier {
            Tier::Host => state.host_in_use += len as u64,
            Tier::PinnedHost => state.pinned_in_use += len as u64,
            Tier::Device(id) => state.device_in_use[id.0 as usize] += len as u64,
            Tier::Disk(_) | Tier::Remote(_, _) => {}
        }
        drop(state);
        self.inner.allocations_total.fetch_add(1, Ordering::SeqCst);
        let handle: Arc<dyn ArenaHandle> = Arc::clone(&self.inner) as Arc<_>;
        // SAFETY: the region is valid for `len` bytes in `tier` and is released to this
        // allocator exactly once, when the buffer (or each half of a split) drops; `token`
        // names the region, so that release is attributed to it and to nothing else.
        Ok(unsafe { Buffer::from_raw_tagged(ptr, len, tier, handle, token) })
    }

    fn note_payload_copy(&self, bytes: u64) {
        self.inner
            .payload_copies
            .fetch_add(bytes, Ordering::Relaxed);
    }

    fn note_boundary_copy(&self, bytes: u64) {
        self.inner
            .boundary_copies
            .fetch_add(bytes, Ordering::Relaxed);
    }

    fn page_bytes(&self) -> usize {
        self.inner.page_bytes
    }

    fn stats(&self) -> AllocStats {
        let state = self.lock();
        AllocStats {
            host_in_use: state.host_in_use,
            pinned_in_use: state.pinned_in_use,
            device_in_use: state.device_in_use,
            allocations_total: self.allocations_total(),
            payload_copies_total: self.payload_copies(),
            boundary_copies_total: self.boundary_copies(),
        }
    }

    fn contains(&self, ptr: *const u8) -> bool {
        self.tier_of(ptr).is_some()
    }

    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        let address = ptr as usize;
        let state = self.lock();
        let (base, token) = state.by_addr.range(..=address).next_back()?;
        let region = state.regions.get(token)?;
        (address < base + region.len).then_some(region.tier)
    }

    fn is_pinned(&self) -> bool {
        self.inner.pinned
    }
}
