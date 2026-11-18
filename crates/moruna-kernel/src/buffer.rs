//! Buffers and the allocator (contracts d.3, e.2).
//!
//! `unsafe` is permitted in this module for the pointer arithmetic of `split_at`, the host
//! slice views, and the hand-off of a buffer's bytes to an Arrow allocation (section l).

use std::collections::BTreeMap;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use crate::error::MorunaError;
use crate::tier::Tier;
use crate::view::BufferView;

/// The arena token every `Buffer` carries: the object that receives the buffer's bytes back
/// when it drops. Defined here so the arena crate can implement it without a circular
/// dependency. `release` is called once per `Buffer` (twice per allocation after a `split_at`,
/// once per half, with the half's own pointer and length).
pub trait ArenaHandle: Send + Sync {
    /// Return `len` bytes at `ptr` in `tier` to the arena.
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier);
}

/// An owned, aligned region in one tier, returned to its arena on drop.
/// Deref to `[u8]` is provided only for Host and PinnedHost buffers;
/// a Device buffer exposes `device_ptr()` and panics on `as_ref()`.
pub struct Buffer {
    ptr: *mut u8,
    len: usize,
    tier: Tier,
    arena: Arc<dyn ArenaHandle>,
}

// SAFETY: a `Buffer` is the unique owner of its region (CT-I2: the region is addressable in
// `tier` for as long as the buffer lives, and the arena token is `Send + Sync`); moving it to
// another thread moves that ownership. Shared references only read (the `Deref` slice) or hand
// out raw pointers whose use is `unsafe` on the caller, so sharing across threads is sound.
unsafe impl Send for Buffer {}
// SAFETY: see the `Send` impl above; no `&self` method writes through `ptr`.
unsafe impl Sync for Buffer {}

impl Buffer {
    /// Wrap a region the arena owns. The arena crate (and a test allocator) is the only caller.
    ///
    /// # Safety
    /// `ptr` must be valid for `len` bytes in `tier` (readable and writable for a host tier,
    /// a device address for `Device`) until `arena.release(ptr, len, tier)` is called, which
    /// this buffer does exactly once when it drops (CT-I2: the tier tag is truthful).
    pub unsafe fn from_raw(
        ptr: *mut u8,
        len: usize,
        tier: Tier,
        arena: Arc<dyn ArenaHandle>,
    ) -> Buffer {
        Buffer {
            ptr,
            len,
            tier,
            arena,
        }
    }

    /// Byte length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the buffer holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The tier the bytes are in.
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Host-visible pointer; `None` for Device buffers.
    pub fn host_ptr(&self) -> Option<*mut u8> {
        match self.tier {
            Tier::PinnedHost | Tier::Host => Some(self.ptr),
            Tier::Device(_) => None,
            Tier::Disk(_) => None,
            Tier::Remote(_, _) => None,
        }
    }

    /// Device pointer as an integer; `None` for host tiers.
    pub fn device_ptr(&self) -> Option<u64> {
        match self.tier {
            Tier::Device(_) => Some(self.ptr as usize as u64),
            Tier::PinnedHost | Tier::Host => None,
            Tier::Disk(_) => None,
            Tier::Remote(_, _) => None,
        }
    }

    /// The arena token, for the arena's own bookkeeping and for tests.
    pub fn arena(&self) -> &Arc<dyn ArenaHandle> {
        &self.arena
    }

    /// Split off a prefix; both halves keep the arena token and are freed independently.
    /// `mid` at 0 or at `len` is allowed and leaves one half empty.
    ///
    /// # Panics
    /// If `mid > len`, which is a programming error, not an input condition.
    pub fn split_at(self, mid: usize) -> (Buffer, Buffer) {
        assert!(
            mid <= self.len,
            "Buffer::split_at: mid {mid} > len {}",
            self.len
        );
        let this = ManuallyDrop::new(self);
        // The original is `ManuallyDrop`, so its `arena` field is never dropped: take it out
        // rather than cloning beside it, or every split would leak one arena token and the
        // region would stay mapped for the life of the process (CT-I2; found by the
        // `infra/first-run-defects` work, 2026-09-22).
        // SAFETY: `this` is `ManuallyDrop` and is never dropped, and the field is not read
        // again after this point, so the value is moved out exactly once.
        let arena = unsafe { std::ptr::read(&this.arena) };
        // SAFETY: `mid <= len`, so `ptr + mid` is within or one past the region this buffer
        // owns; the two halves partition the region and each releases only its own range.
        let tail_ptr = unsafe { this.ptr.add(mid) };
        let head = Buffer {
            ptr: this.ptr,
            len: mid,
            tier: this.tier,
            arena: Arc::clone(&arena),
        };
        let tail = Buffer {
            ptr: tail_ptr,
            len: this.len - mid,
            tier: this.tier,
            arena,
        };
        (head, tail)
    }

    /// Zero-copy conversion to an Arrow buffer whose deallocation releases to the arena
    /// (host tiers only; Device → error). The arena token survives this conversion and
    /// every Arrow slice of the result (slices share the allocation), so `Payload::table`
    /// on a batch decoded over such a buffer infers the tier correctly (e.2).
    pub fn into_arrow_buffer(self) -> crate::Result<arrow::buffer::Buffer> {
        if !self.tier.is_host() {
            return Err(MorunaError::Staging(format!(
                "into_arrow_buffer: {:?} is not a host tier",
                self.tier
            )));
        }
        let Some(ptr) = NonNull::new(self.ptr) else {
            return Err(MorunaError::Staging(
                "into_arrow_buffer: null pointer".into(),
            ));
        };
        let this = ManuallyDrop::new(self);
        // As in `split_at`: the original is never dropped, so the token moves into the owner
        // instead of being cloned beside a field that nothing will ever release.
        // SAFETY: `this` is `ManuallyDrop` and is never dropped, and the field is not read
        // again after this point, so the value is moved out exactly once.
        let arena = unsafe { std::ptr::read(&this.arena) };
        let owner = Arc::new(ArrowOwner {
            ptr: this.ptr,
            len: this.len,
            tier: this.tier,
            arena,
        });
        registry_insert(this.ptr as usize, this.len, this.tier);
        // SAFETY: the region is valid for `len` bytes (CT-I2) and stays so until `owner`
        // drops, which is when Arrow's last reference to the allocation goes away; `owner`
        // then releases the region to the arena exactly once.
        Ok(unsafe { arrow::buffer::Buffer::from_custom_allocation(ptr, this.len, owner) })
    }

    /// A shareable read-only view of this buffer for use as a DMA source (d.9). The
    /// view keeps the buffer alive; the buffer is not consumed.
    pub fn view(self: &Arc<Buffer>) -> BufferView {
        let owner: Arc<dyn std::any::Any + Send + Sync> = Arc::clone(self) as Arc<_>;
        // SAFETY: the view's bytes are this buffer's region, which `owner` (this `Arc`)
        // keeps alive and in `tier` for the view's lifetime (CT-I2).
        unsafe { BufferView::from_raw(self.ptr.cast_const(), self.len, self.tier, owner) }
    }

    fn host_slice(&self) -> &[u8] {
        let Some(ptr) = self.host_ptr() else {
            panic!(
                "Buffer::as_ref: buffer in tier {:?} has no host-visible bytes",
                self.tier
            );
        };
        if self.len == 0 {
            return &[];
        }
        // SAFETY: a host-tier buffer's region is readable for `len` bytes (CT-I2) and no
        // `&mut` to it exists while `&self` is held.
        unsafe { std::slice::from_raw_parts(ptr.cast_const(), self.len) }
    }

    fn host_slice_mut(&mut self) -> &mut [u8] {
        let Some(ptr) = self.host_ptr() else {
            panic!(
                "Buffer::as_mut: buffer in tier {:?} has no host-visible bytes",
                self.tier
            );
        };
        if self.len == 0 {
            return &mut [];
        }
        // SAFETY: a host-tier buffer's region is writable for `len` bytes (CT-I2) and
        // `&mut self` is the only reference to it.
        unsafe { std::slice::from_raw_parts_mut(ptr, self.len) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.arena.release(self.ptr, self.len, self.tier);
    }
}

impl std::ops::Deref for Buffer {
    type Target = [u8];
    /// Host tiers only.
    ///
    /// # Panics
    /// On a buffer in a tier without host-visible bytes; the message names the tier (h).
    fn deref(&self) -> &[u8] {
        self.host_slice()
    }
}

impl std::ops::DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.host_slice_mut()
    }
}

impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        self.host_slice()
    }
}

impl AsMut<[u8]> for Buffer {
    fn as_mut(&mut self) -> &mut [u8] {
        self.host_slice_mut()
    }
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("ptr", &self.ptr)
            .field("len", &self.len)
            .field("tier", &self.tier)
            .finish()
    }
}

/// The Arrow-side owner of an arena region handed over by `into_arrow_buffer`: releases the
/// region to the arena when Arrow's last reference drops, and keeps the provenance entry
/// alive meanwhile.
struct ArrowOwner {
    ptr: *mut u8,
    len: usize,
    tier: Tier,
    arena: Arc<dyn ArenaHandle>,
}

// SAFETY: as for `Buffer`; the owner never reads or writes the region, it only releases it.
unsafe impl Send for ArrowOwner {}
// SAFETY: as for `Buffer`.
unsafe impl Sync for ArrowOwner {}
impl std::panic::RefUnwindSafe for ArrowOwner {}

impl Drop for ArrowOwner {
    fn drop(&mut self) {
        registry_remove(self.ptr as usize);
        self.arena.release(self.ptr, self.len, self.tier);
    }
}

/// Provenance of Arrow allocations that came from arena buffers (e.2): allocation base
/// address to (length, tier). Arrow does not expose a buffer's custom owner, so the arena
/// token is looked up here by the allocation's base pointer, which every slice of the
/// buffer shares (`arrow::buffer::Buffer::data_ptr`).
static REGISTRY: Mutex<BTreeMap<usize, (usize, Tier)>> = Mutex::new(BTreeMap::new());

fn registry_insert(base: usize, len: usize, tier: Tier) {
    let mut map = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    map.insert(base, (len, tier));
}

fn registry_remove(base: usize) {
    let mut map = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&base);
}

/// The tier of the arena region an Arrow buffer (or any slice of it) was built over by
/// `into_arrow_buffer`; `None` for a buffer that is not arena-owned (e.2 treats it as `Host`).
pub fn arena_tier_of(buf: &arrow::buffer::Buffer) -> Option<Tier> {
    let base = buf.data_ptr().as_ptr() as usize;
    let map = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    map.get(&base).map(|(_, tier)| *tier)
}

/// Allocation statistics the arena maintains; read by the controller and by CT-T tests.
#[derive(Copy, Clone, Default, Debug)]
pub struct AllocStats {
    /// Bytes in use in `Host`.
    pub host_in_use: u64,
    /// Bytes in use in `PinnedHost`.
    pub pinned_in_use: u64,
    /// Bytes in use per device, indexed by `DeviceId`.
    pub device_in_use: [u64; 8],
    /// Allocations made since the arena was created.
    pub allocations_total: u64,
    /// Incremented by any component that copies payload bytes with the CPU; must stay 0
    /// outside sources and sinks.
    pub payload_copies_total: u64,
    /// Adapters' one-time copy of a kernel's non-arena host output into the arena
    /// (05-adapters AD-I2).
    pub boundary_copies_total: u64,
}

/// The memory arena (component 2) as every other component sees it.
pub trait Allocator: Send + Sync {
    /// Allocate `bytes` in `tier`, aligned to ALIGNMENT and to the page size when
    /// `bytes >= page size`. Fails with `MorunaError::Alloc` if the tier's budget
    /// would be exceeded; never blocks.
    fn alloc(&self, bytes: usize, tier: Tier) -> crate::Result<Buffer>;
    /// The page size discovery reported for this host.
    fn page_bytes(&self) -> usize;
    /// Live allocation statistics.
    fn stats(&self) -> AllocStats;
    /// Bytes `tier` could still give, when this allocator has a budget for it; `None` when it
    /// has none. A figure and not a promise: a size class may still not be free all at once. A
    /// component that holds a working set of its own for the whole run (a sink's file buffers)
    /// sizes it from this while the arena is empty, rather than taking whatever one request
    /// happens to be given.
    fn available(&self, tier: Tier) -> Option<u64> {
        let _ = tier;
        None
    }
    /// True if `ptr` lies inside a region this allocator owns (used by adapters to skip the boundary copy).
    fn contains(&self, ptr: *const u8) -> bool {
        let _ = ptr;
        false
    }
    /// The tier of the region containing `ptr`, when `contains(ptr)`.
    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        let _ = ptr;
        None
    }
    /// Record that a component copied `bytes` of payload with the CPU, which only a
    /// source decoding a non-layout-preserving format or a sink encoding to one may
    /// do (G-I2).
    ///
    /// Without this and [`Allocator::note_boundary_copy`], the two counters in
    /// [`AllocStats`] have no writer: the arena owns the struct and no other method
    /// can raise them. The default does nothing, so a fake that does not count is
    /// still a valid allocator.
    fn note_payload_copy(&self, bytes: u64) {
        let _ = bytes;
    }

    /// Record that an adapter copied `bytes` of a kernel's non-arena output into the
    /// arena, once, at the kernel boundary (05 AD-I2).
    fn note_boundary_copy(&self, bytes: u64) {
        let _ = bytes;
    }

    /// True when this allocator's host tier is page-locked. A run has exactly one host
    /// tier: `PinnedHost` when true, `Host` when false; the two never coexist in one
    /// process and no move between them exists (e.1).
    fn is_pinned(&self) -> bool;
}

/// An allocator over another that keeps count of the bytes its buffers still hold: what one
/// read in flight has taken from the arena, its fetch and the buffers its decode reserved
/// (system design 3A, item 6.3c). Every other call is the inner allocator's.
///
/// **The invariant it serves:** a read in flight is counted at `max(charge, held)`, never at
/// `charge + held`. The source drive charges a read its estimate before it takes anything, and
/// the arena's in-use already counts what it has taken, so admission counts only what the read
/// may still take, `charge - held`. A read that holds more than its charge (an estimate that came
/// in short) may still take nothing: that is 0, never negative, and the arena's in-use carries
/// the excess.
///
/// The count moves once per buffer, when it is allocated and when it is released (once per half
/// after a `split_at`), never per byte or per row: a Parquet read's decode moves it a few times a
/// column, for the buffers it reserves, whatever its batches.
pub struct Tally {
    inner: Arc<dyn Allocator>,
    /// The count, the one kind the run measures with (`crate::held`).
    held: crate::held::Held,
}

impl Tally {
    /// A tally of the buffers allocated through it from `inner`, holding nothing yet.
    pub fn new(inner: Arc<dyn Allocator>) -> Tally {
        Tally {
            inner,
            held: crate::held::Held::new(),
        }
    }

    /// The bytes the buffers allocated through this tally still hold.
    pub fn held(&self) -> u64 {
        self.held.held()
    }
}

/// The arena token of a buffer a `Tally` handed out: its release takes the bytes off the
/// tally's count and hands them to the arena it came from.
struct Tallied {
    inner: Arc<dyn ArenaHandle>,
    held: crate::held::Held,
}

impl ArenaHandle for Tallied {
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier) {
        self.held.sub(len as u64);
        self.inner.release(ptr, len, tier);
    }
}

impl Allocator for Tally {
    fn alloc(&self, bytes: usize, tier: Tier) -> crate::Result<Buffer> {
        let buffer = ManuallyDrop::new(self.inner.alloc(bytes, tier)?);
        // As in `split_at`: the original is never dropped, so its token moves under the tally's
        // instead of being cloned beside a field nothing will release.
        // SAFETY: `buffer` is `ManuallyDrop` and is never dropped, and the field is not read
        // again after this point, so the value is moved out exactly once.
        let arena = unsafe { std::ptr::read(&buffer.arena) };
        self.held.add(buffer.len as u64);
        Ok(Buffer {
            ptr: buffer.ptr,
            len: buffer.len,
            tier: buffer.tier,
            arena: Arc::new(Tallied {
                inner: arena,
                held: self.held.clone(),
            }),
        })
    }

    fn page_bytes(&self) -> usize {
        self.inner.page_bytes()
    }

    fn stats(&self) -> AllocStats {
        self.inner.stats()
    }

    fn available(&self, tier: Tier) -> Option<u64> {
        self.inner.available(tier)
    }

    fn contains(&self, ptr: *const u8) -> bool {
        self.inner.contains(ptr)
    }

    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        self.inner.tier_of(ptr)
    }

    fn note_payload_copy(&self, bytes: u64) {
        self.inner.note_payload_copy(bytes);
    }

    fn note_boundary_copy(&self, bytes: u64) {
        self.inner.note_boundary_copy(bytes);
    }

    fn is_pinned(&self) -> bool {
        self.inner.is_pinned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingArena(AtomicUsize);
    impl ArenaHandle for CountingArena {
        fn release(&self, _ptr: *mut u8, len: usize, _tier: Tier) {
            self.0.fetch_add(len, Ordering::SeqCst);
        }
    }

    struct NoDefaults;
    impl Allocator for NoDefaults {
        fn alloc(&self, _bytes: usize, tier: Tier) -> crate::Result<Buffer> {
            Err(MorunaError::Alloc {
                bytes: 0,
                tier,
                budget: 0,
                in_use: 0,
            })
        }
        fn page_bytes(&self) -> usize {
            4096
        }
        fn stats(&self) -> AllocStats {
            AllocStats::default()
        }
        fn is_pinned(&self) -> bool {
            false
        }
    }

    fn heap(len: usize, tier: Tier, arena: &Arc<CountingArena>) -> (Buffer, *mut u8) {
        let mut v = vec![7u8; len];
        let ptr = v.as_mut_ptr();
        std::mem::forget(v);
        let handle: Arc<dyn ArenaHandle> = Arc::clone(arena) as Arc<dyn ArenaHandle>;
        // SAFETY: test-only; the leaked Vec's bytes stay valid for the test.
        (unsafe { Buffer::from_raw(ptr, len, tier, handle) }, ptr)
    }

    /// An allocator of heap buffers released to one `CountingArena`, with a budget it reports.
    struct HeapAlloc(Arc<CountingArena>);
    impl Allocator for HeapAlloc {
        fn alloc(&self, bytes: usize, tier: Tier) -> crate::Result<Buffer> {
            Ok(heap(bytes, tier, &self.0).0)
        }
        fn page_bytes(&self) -> usize {
            16384
        }
        fn stats(&self) -> AllocStats {
            AllocStats {
                allocations_total: 9,
                ..AllocStats::default()
            }
        }
        fn available(&self, _tier: Tier) -> Option<u64> {
            Some(77)
        }
        fn contains(&self, _ptr: *const u8) -> bool {
            true
        }
        fn tier_of(&self, _ptr: *const u8) -> Option<Tier> {
            Some(Tier::Host)
        }
        fn is_pinned(&self) -> bool {
            true
        }
    }

    /// 6.3c: a tally counts what the buffers it handed out still hold, whether they are split,
    /// handed to Arrow or dropped, and returns every byte to the arena they came from.
    #[test]
    fn a_tally_counts_what_its_buffers_still_hold() {
        let arena = Arc::new(CountingArena(AtomicUsize::new(0)));
        let tally = Tally::new(Arc::new(HeapAlloc(Arc::clone(&arena))));
        assert_eq!(tally.held(), 0);
        let whole = tally.alloc(10, Tier::Host).expect("ten");
        let other = tally.alloc(6, Tier::Host).expect("six");
        assert_eq!(tally.held(), 16);
        let (head, tail) = whole.split_at(4);
        drop(tail);
        assert_eq!(tally.held(), 10, "the tail's six bytes went back");
        let arrow = head.into_arrow_buffer().expect("arrow");
        assert_eq!(tally.held(), 10, "Arrow holds the head now");
        drop(arrow);
        drop(other);
        assert_eq!(tally.held(), 0);
        assert_eq!(
            arena.0.load(Ordering::SeqCst),
            16,
            "every byte back to its arena"
        );
        // Everything but the allocation is the inner allocator's.
        assert_eq!(tally.page_bytes(), 16384);
        assert_eq!(tally.stats().allocations_total, 9);
        assert_eq!(tally.available(Tier::Host), Some(77));
        assert!(tally.contains(std::ptr::null()));
        assert_eq!(tally.tier_of(std::ptr::null()), Some(Tier::Host));
        assert!(tally.is_pinned());
        tally.note_payload_copy(1);
        tally.note_boundary_copy(1);
    }

    #[test]
    fn split_releases_each_half_once() {
        let arena = Arc::new(CountingArena(AtomicUsize::new(0)));
        let (b, ptr) = heap(10, Tier::Host, &arena);
        assert_eq!(b.host_ptr(), Some(ptr));
        assert_eq!(b.device_ptr(), None);
        assert!(!b.is_empty());
        let (h, t) = b.split_at(4);
        assert_eq!((h.len(), t.len()), (4, 6));
        assert_eq!(&*h, &[7u8; 4]);
        assert_eq!(&*t, &[7u8; 6]);
        drop(h);
        assert_eq!(arena.0.load(Ordering::SeqCst), 4);
        drop(t);
        assert_eq!(arena.0.load(Ordering::SeqCst), 10);
        let (b, _) = heap(3, Tier::Host, &arena);
        let (h, t) = b.split_at(0);
        assert!(h.is_empty());
        assert_eq!(h.len(), 0);
        assert_eq!(&*h, &[] as &[u8]);
        assert_eq!(t.len(), 3);
        let (h, t) = t.split_at(3);
        assert_eq!(h.len(), 3);
        assert!(t.is_empty());
        let mut m = h;
        m[0] = 1;
        m.as_mut()[1] = 2;
        assert_eq!(m.as_ref(), &[1, 2, 7]);
        assert!(format!("{m:?}").contains("Host"));
        assert!(Arc::ptr_eq(m.arena(), m.arena()));
    }

    /// CT-T20. `split_at` and `into_arrow_buffer` consume the buffer, so neither may leave an
    /// arena token behind: the token is what keeps the arena's region mapped, and one leaked
    /// per morsel kept a whole 1 GiB region resident for the life of the process, which left a
    /// second `Runtime::run` no budget at all (12 f.1, 11 f.1; found 2026-09-22).
    #[test]
    fn the_arena_token_is_not_leaked() {
        let arena = Arc::new(CountingArena(AtomicUsize::new(0)));
        let before = Arc::strong_count(&arena);

        let (b, _) = heap(16, Tier::Host, &arena);
        let (h, t) = b.split_at(8);
        assert_eq!(
            Arc::strong_count(&arena),
            before + 2,
            "a split holds exactly one token per half"
        );
        drop(h);
        drop(t);
        assert_eq!(
            Arc::strong_count(&arena),
            before,
            "split_at leaked an arena token"
        );

        let (b, _) = heap(16, Tier::Host, &arena);
        let arrow = b.into_arrow_buffer().expect("a host buffer converts");
        assert_eq!(
            Arc::strong_count(&arena),
            before + 1,
            "the Arrow owner holds exactly one token"
        );
        drop(arrow);
        assert_eq!(
            Arc::strong_count(&arena),
            before,
            "into_arrow_buffer leaked an arena token"
        );
    }

    #[test]
    #[should_panic(expected = "Device")]
    fn device_buffer_has_no_slice() {
        let arena = Arc::new(CountingArena(AtomicUsize::new(0)));
        let (b, _) = heap(4, Tier::Device(crate::ids::DeviceId(0)), &arena);
        assert!(b.device_ptr().is_some());
        assert!(b.host_ptr().is_none());
        let _ = b.as_ref();
    }

    #[test]
    #[should_panic(expected = "Device")]
    fn device_buffer_has_no_mut_slice() {
        let arena = Arc::new(CountingArena(AtomicUsize::new(0)));
        let (mut b, _) = heap(4, Tier::Device(crate::ids::DeviceId(0)), &arena);
        let _ = b.as_mut();
    }

    #[test]
    #[should_panic(expected = "mid 5 > len 4")]
    fn split_past_end_panics() {
        let arena = Arc::new(CountingArena(AtomicUsize::new(0)));
        let (b, _) = heap(4, Tier::Host, &arena);
        let _ = b.split_at(5);
    }

    #[test]
    fn device_buffer_cannot_become_arrow() {
        let arena = Arc::new(CountingArena(AtomicUsize::new(0)));
        let (b, _) = heap(4, Tier::Device(crate::ids::DeviceId(1)), &arena);
        assert!(matches!(
            b.into_arrow_buffer(),
            Err(MorunaError::Staging(_))
        ));
        let seg = crate::tier::SegmentRef {
            segment: 0,
            offset: 0,
            len: 4,
        };
        let (b, _) = heap(4, Tier::Disk(seg), &arena);
        assert!(b.host_ptr().is_none() && b.device_ptr().is_none());
        let r = crate::tier::RemoteRef {
            addr: 0,
            rkey: 0,
            len: 4,
        };
        let (b, _) = heap(4, Tier::Remote(crate::ids::LOCAL_NODE, r), &arena);
        assert!(b.host_ptr().is_none() && b.device_ptr().is_none());
        let handle: Arc<dyn ArenaHandle> = arena.clone();
        // SAFETY: test-only; a null region of length 0 is never read.
        let null = unsafe { Buffer::from_raw(std::ptr::null_mut(), 0, Tier::Host, handle) };
        assert!(matches!(
            null.into_arrow_buffer(),
            Err(MorunaError::Staging(_))
        ));
    }

    #[test]
    fn allocator_defaults_are_conservative() {
        let a = NoDefaults;
        assert!(!a.contains(std::ptr::null()));
        assert_eq!(a.tier_of(std::ptr::null()), None);
        assert!(a.alloc(1, Tier::Host).is_err());
        assert_eq!(a.page_bytes(), 4096);
        assert_eq!(a.stats().allocations_total, 0);
        assert!(!a.is_pinned());
    }

    #[test]
    fn an_empty_buffer_has_an_empty_slice() {
        let arena = Arc::new(CountingArena(AtomicUsize::new(0)));
        let (b, _) = heap(4, Tier::Host, &arena);
        let (mut head, _tail) = b.split_at(0);
        assert!(head.as_ref().is_empty());
        assert!(head.as_mut().is_empty());
    }
}
