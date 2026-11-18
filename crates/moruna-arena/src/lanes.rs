//! Per-thread lanes (02 f.3): what a buffer holds to find its way home, and the free slots a
//! thread keeps for its next allocations, one lane per thread and arena, so that eight threads
//! allocating at once do not all write the same few words.
//!
//! Every `Buffer` carries an `Arc<dyn ArenaHandle>` (contracts d.3), and every allocation cloned
//! the arena's one `Arc<Inner>` for it, so every allocation and release wrote that count's word:
//! 4 ns a pair with one thread, 422 with eight. A host buffer now holds its thread's
//! [`RegionLane`] for the region it is in, a word no other thread writes unless it releases a
//! buffer this thread allocated, and which names the region, so a release finds it without the
//! region list's lock.
//!
//! Each lane keeps a cache of free slots of the classes up to [`REFILL_QUANTUM`]: an allocation
//! takes one under the lane's own lock, which no other thread takes but to flush it, and a
//! release puts its slot back there. The class lists are touched only to refill the cache, half a
//! quantum at a time, or to flush it, when it would hold more than a quantum. A refusal first
//! flushes every lane and then decides, so nothing is refused while slots sit in a cache
//! (`Inner::alloc_host_buffer`); a shrink flushes every lane of the regions it drains before it
//! retires any, and a thread's lanes are flushed when it ends.
//!
//! A lane is kept by the arena's registry and found through a weak reference in the thread, so
//! nothing a thread keeps outlives the arena: dropping the `Arena` empties and closes the
//! registry, and the lanes then live only as long as the buffers that hold them. A thread's lanes
//! leave their registries when the thread ends.
//!
//! Locks are taken in one order: the region list, then the registry, then a lane's cache, then a
//! class list. Nothing holding a cache waits for the region list.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use moruna_kernel::{ArenaHandle, Tier};

use crate::classes::{GRANULE, Release, Returned, class_for, class_size};
use crate::{HostRegion, Inner};

/// The most bytes of free slots one thread's cache holds (02 f.3). A refill takes at most half
/// of it, and a release that would take the cache past it flushes the cache down to half first.
/// Only classes of at most a quantum are cached, 64 KiB to 1 MiB.
pub const REFILL_QUANTUM: u64 = 1024 * 1024;

/// The classes a cache keeps: 0 to the class of [`REFILL_QUANTUM`].
const CACHED: usize = (REFILL_QUANTUM / GRANULE).trailing_zeros() as usize + 1;

/// The class a cache serves `bytes` from, when it keeps that class. A zero-length request owns
/// nothing and takes no slot.
pub(crate) fn cached_class(bytes: u64) -> Option<usize> {
    if bytes == 0 {
        return None;
    }
    class_for(bytes).filter(|&c| c < CACHED)
}

/// A thread's way into one arena: its cache of free slots, and the handle its buffers of no
/// host region hold.
pub(crate) struct Lane {
    inner: Arc<Inner>,
    /// Free slots this thread keeps, by region and class. Its lock is the thread's own but when
    /// another thread flushes it.
    cache: Mutex<Cache>,
    /// The bytes `cache` holds, written under its lock and read without it.
    cached: AtomicU64,
}

/// The slots a lane keeps, by the region they are in.
#[derive(Default)]
struct Cache {
    regions: Vec<Held>,
}

/// One region's free slots in a lane's cache, and the handle the lane's buffers in it hold.
struct Held {
    handle: Arc<RegionLane>,
    slots: [Vec<u64>; CACHED],
}

/// What a host buffer holds to find its way home (contracts d.3): the thread that allocated it,
/// as the arena's, and the region it is in.
pub(crate) struct RegionLane {
    inner: Arc<Inner>,
    region: Arc<HostRegion>,
}

impl ArenaHandle for RegionLane {
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier) {
        // A zero-length buffer owns nothing and returns nothing, wherever it points (e.3).
        if len == 0 {
            return;
        }
        if tier != self.inner.host_tier || !self.region.space.contains(ptr) {
            self.inner.foreign(tier);
            return;
        }
        self.inner.release_in(&self.region, ptr, len as u64);
    }
}

impl ArenaHandle for Lane {
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier) {
        self.inner.release(ptr, len, tier);
    }
}

impl Lane {
    fn new(inner: &Arc<Inner>) -> Lane {
        Lane {
            inner: Arc::clone(inner),
            cache: Mutex::new(Cache::default()),
            cached: AtomicU64::new(0),
        }
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The bytes this lane's cache holds now.
    pub(crate) fn cached(&self) -> u64 {
        self.cached.load(Ordering::Relaxed)
    }

    /// A cached slot of class `c` given to a buffer of `requested` bytes, and the handle the
    /// buffer holds; `None` when the cache holds no slot of the class.
    pub(crate) fn alloc(&self, c: usize, requested: u64) -> Option<(*mut u8, Arc<RegionLane>)> {
        let mut cache = self.cache();
        let held = cache.regions.iter_mut().find(|h| !h.slots[c].is_empty())?;
        let off = held.slots[c].pop()?;
        let ptr = held.handle.region.space.hand_out(off, requested, c);
        let handle = Arc::clone(&held.handle);
        self.cached
            .store(self.cached() - class_size(c), Ordering::Relaxed);
        Some((ptr, handle))
    }

    /// Refill class `c` from `region`, which is not draining, and give one of the slots to a
    /// buffer of `requested` bytes: at most half a quantum, and never past a quantum cached.
    /// `None` when the region has no slot of the class. The caller holds the region list's read
    /// lock, so no shrink marks the region draining while its slots go into the cache.
    pub(crate) fn refill(
        &self,
        region: &Arc<HostRegion>,
        c: usize,
        requested: u64,
    ) -> Option<(*mut u8, Arc<RegionLane>)> {
        let size = class_size(c);
        let mut cache = self.cache();
        let cached = self.cached();
        let room = REFILL_QUANTUM
            .saturating_sub(cached)
            .min(REFILL_QUANTUM / 2);
        let want = (room / size) as usize + 1;
        let held = cache.entry(&self.inner, region);
        region.space.take(c, want, &mut held.slots[c]);
        let off = held.slots[c].pop()?;
        let ptr = region.space.hand_out(off, requested, c);
        let handle = Arc::clone(&held.handle);
        let kept = held.slots[c].len() as u64 * size;
        self.cached.store(cached + kept, Ordering::Relaxed);
        Some((ptr, handle))
    }

    /// Keep the free class `c` slot at `off` in `region` for this thread's next allocations.
    /// False when it is not the cache's to keep: the region is draining, so the slot goes back
    /// to it and the region can be retired.
    pub(crate) fn keep(&self, region: &Arc<HostRegion>, c: usize, off: u64) -> bool {
        let mut cache = self.cache();
        // Read under the cache's lock: a shrink marks a region draining before it flushes every
        // cache of it, so a slot kept here is flushed by that shrink or never kept.
        if region.draining.load(Ordering::SeqCst) {
            return false;
        }
        let size = class_size(c);
        let mut cached = self.cached();
        if cached + size > REFILL_QUANTUM {
            cached -= cache.shed(cached, (REFILL_QUANTUM / 2).saturating_sub(size), c);
        }
        cache.entry(&self.inner, region).slots[c].push(off);
        self.cached.store(cached + size, Ordering::Relaxed);
        true
    }

    /// The handle a buffer cut from `region` holds. A draining region's is the buffer's own, so
    /// no cache keeps the region once it is retired.
    pub(crate) fn handle_for(&self, region: &Arc<HostRegion>) -> Arc<RegionLane> {
        let mut cache = self.cache();
        if region.draining.load(Ordering::SeqCst) {
            return Arc::new(RegionLane {
                inner: Arc::clone(&self.inner),
                region: Arc::clone(region),
            });
        }
        Arc::clone(&cache.entry(&self.inner, region).handle)
    }

    /// Put back every slot the cache holds of a region `which` picks; with `forget`, drop those
    /// regions from the cache too.
    fn flush(&self, which: &dyn Fn(&HostRegion) -> bool, forget: bool) {
        let mut cache = self.cache();
        let flushed = cache.flush(which, forget);
        self.cached
            .store(self.cached() - flushed, Ordering::Relaxed);
    }
}

impl Cache {
    /// `region`'s entry, made the first time the lane meets the region.
    fn entry(&mut self, inner: &Arc<Inner>, region: &Arc<HostRegion>) -> &mut Held {
        let at = match self
            .regions
            .iter()
            .position(|h| Arc::ptr_eq(&h.handle.region, region))
        {
            Some(at) => at,
            None => {
                self.regions.push(Held {
                    handle: Arc::new(RegionLane {
                        inner: Arc::clone(inner),
                        region: Arc::clone(region),
                    }),
                    slots: Default::default(),
                });
                self.regions.len() - 1
            }
        };
        &mut self.regions[at]
    }

    /// Put slots back until at most `target` of the `cached` bytes are left, other classes than
    /// `keep` first. Returns the bytes put back.
    fn shed(&mut self, cached: u64, target: u64, keep: usize) -> u64 {
        let mut left = cached;
        for others in [true, false] {
            for held in &mut self.regions {
                for c in (0..CACHED).filter(|&c| (c != keep) == others) {
                    if left <= target {
                        return cached - left;
                    }
                    let size = class_size(c);
                    let slots = &mut held.slots[c];
                    let n = ((left - target).div_ceil(size) as usize).min(slots.len());
                    if n > 0 {
                        let at = slots.len() - n;
                        held.handle.region.space.put_back(c, slots.drain(at..));
                        left -= n as u64 * size;
                    }
                }
            }
        }
        cached - left
    }

    /// Put back every slot of a region `which` picks, and with `forget` drop those regions.
    /// Returns the bytes put back.
    fn flush(&mut self, which: &dyn Fn(&HostRegion) -> bool, forget: bool) -> u64 {
        let mut flushed = 0;
        for held in self.regions.iter_mut() {
            if !which(&held.handle.region) {
                continue;
            }
            for (c, slots) in held.slots.iter_mut().enumerate() {
                if !slots.is_empty() {
                    flushed += slots.len() as u64 * class_size(c);
                    held.handle.region.space.put_back(c, slots.drain(..));
                }
            }
        }
        if forget {
            self.regions.retain(|h| !which(&h.handle.region));
        }
        flushed
    }
}

/// The lanes of one arena, each held for its thread.
#[derive(Default)]
pub(crate) struct Registry {
    lanes: Mutex<Lanes>,
}

#[derive(Default)]
struct Lanes {
    /// Set when the arena is dropped: no lane is registered after, so none outlives the arena
    /// in a cycle through it.
    closed: bool,
    list: Vec<Arc<Lane>>,
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, Lanes> {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lanes registered now.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().list.len()
    }

    /// Lets every lane go and registers none after: the arena is dropped. A lane lives on while
    /// a buffer holds it.
    pub(crate) fn clear(&self) {
        let lanes = {
            let mut lanes = self.lock();
            lanes.closed = true;
            std::mem::take(&mut lanes.list)
        };
        drop(lanes);
    }

    /// Every lane puts back what it holds of a region `which` picks, and with `forget` lets go
    /// of those regions: before a refusal is decided, and when a shrink drains regions.
    pub(crate) fn flush(&self, which: &dyn Fn(&HostRegion) -> bool, forget: bool) {
        for lane in self.lock().list.iter() {
            lane.flush(which, forget);
        }
    }

    /// The bytes every lane's cache holds now.
    pub(crate) fn cached(&self) -> u64 {
        self.lock().list.iter().map(|lane| lane.cached()).sum()
    }

    fn register(&self, lane: &Arc<Lane>) -> bool {
        let mut lanes = self.lock();
        if lanes.closed {
            return false;
        }
        lanes.list.push(Arc::clone(lane));
        true
    }

    fn remove(&self, lane: &Arc<Lane>) {
        self.lock().list.retain(|l| !Arc::ptr_eq(l, lane));
    }
}

/// An arena's identity, unlike its address never another's.
pub(crate) fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// This thread's lanes, by the arena's id. When the thread ends each puts back what its cache
/// holds and leaves its registry.
#[derive(Default)]
struct ThreadLanes(Vec<(u64, Weak<Lane>)>);

impl Drop for ThreadLanes {
    fn drop(&mut self) {
        for (_, lane) in self.0.drain(..) {
            if let Some(lane) = lane.upgrade() {
                lane.flush(&|_| true, true);
                lane.inner.lanes.remove(&lane);
            }
        }
    }
}

thread_local! {
    static LANES: RefCell<ThreadLanes> = RefCell::default();
}

impl Inner {
    /// This thread's lane into the arena, made and registered the first time the thread asks;
    /// `None` once the arena is dropped, or while the thread is ending.
    pub(crate) fn lane(self: &Arc<Self>) -> Option<Arc<Lane>> {
        let id = self.id;
        let found = LANES
            .try_with(|lanes| {
                lanes
                    .borrow()
                    .0
                    .iter()
                    .find(|(of, _)| *of == id)
                    .and_then(|(_, lane)| lane.upgrade())
            })
            .ok()?;
        match found {
            Some(lane) => Some(lane),
            None => self.new_lane(),
        }
    }

    fn new_lane(self: &Arc<Self>) -> Option<Arc<Lane>> {
        let lane = Arc::new(Lane::new(self));
        if !self.lanes.register(&lane) {
            return None;
        }
        let id = self.id;
        let recorded = LANES.try_with(|lanes| {
            let mut lanes = lanes.borrow_mut();
            // A lane of an arena gone, or of this one gone before, goes first.
            lanes
                .0
                .retain(|(of, lane)| *of != id && lane.strong_count() > 0);
            lanes.0.push((id, Arc::downgrade(&lane)));
        });
        if recorded.is_err() {
            self.lanes.remove(&lane);
            return None;
        }
        Some(lane)
    }

    /// A host allocation of class `c` from this thread's cache, refilled when it holds no slot
    /// of the class from the oldest region that is not draining and has one, as `alloc_host`
    /// serves. `None` when no region has a slot of the class free.
    pub(crate) fn alloc_cached(
        &self,
        lane: &Lane,
        c: usize,
        bytes: u64,
    ) -> Option<(*mut u8, Arc<RegionLane>)> {
        if let Some(hit) = lane.alloc(c, bytes) {
            return Some(hit);
        }
        let host = self.read_host();
        host.iter()
            .filter(|r| !r.draining.load(Ordering::SeqCst))
            .find_map(|region| lane.refill(region, c, bytes))
    }

    /// Release `len` bytes at `ptr` in `region`, for a buffer's [`RegionLane`]. A class slot
    /// whose last bytes came back goes into this thread's cache when it keeps the class, and
    /// back to its list when not; a draining region whose last buffer came home is retired.
    pub(crate) fn release_in(self: &Arc<Self>, region: &Arc<HostRegion>, ptr: *mut u8, len: u64) {
        let freed = match region.space.give_back(ptr, len) {
            Returned::Done(released) => released == Release::Freed,
            Returned::Slot { class, offset } => {
                if class < CACHED
                    && self
                        .lane()
                        .is_some_and(|lane| lane.keep(region, class, offset))
                {
                    return;
                }
                region.space.put_back(class, [offset]);
                true
            }
        };
        // A draining region whose last buffer just came home is unmapped; that needs the write
        // lock, and nothing here holds a lock now.
        if freed && region.draining.load(Ordering::SeqCst) && region.space.in_use() == 0 {
            let mut host = self.write_host();
            self.retire_drained(&mut host);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arena, ArenaConfig};
    use moruna_kernel::{Allocator, Buffer, Guarantee};

    /// Writes `stamp` at both ends of `buffer`, for [`check`].
    fn stamp(buffer: &mut Buffer, stamp: u64) {
        let (n, bytes) = (buffer.len(), stamp.to_le_bytes());
        let k = n.min(8);
        buffer[..k].copy_from_slice(&bytes[..k]);
        buffer[n - k..].copy_from_slice(&bytes[..k]);
    }

    /// Panics unless `buffer` still holds `stamp` at both ends: no other live buffer was given
    /// its bytes.
    fn check(buffer: &Buffer, stamp: u64) {
        let (n, bytes) = (buffer.len(), stamp.to_le_bytes());
        let k = n.min(8);
        assert_eq!(&buffer[..k], &bytes[..k], "the head of buffer {stamp:#x}");
        assert_eq!(
            &buffer[n - k..],
            &bytes[..k],
            "the tail of buffer {stamp:#x}"
        );
    }

    /// Buffers of every cached class and some above it, allocated on four threads and released
    /// on those and two others, while the arena grows and shrinks under them: every byte comes
    /// home once, no two live buffers ever share a slot, and every drained region is retired.
    #[test]
    fn buffers_cross_threads_and_come_home_once() {
        let a = arena(32 << 20);
        let (tx, rx) = std::sync::mpsc::channel::<(Buffer, u64)>();
        let rx = Arc::new(Mutex::new(rx));
        let producers: Vec<_> = (0..4u64)
            .map(|t| {
                let (a, tx) = (Arc::clone(&a), tx.clone());
                std::thread::spawn(move || {
                    let mut seed = 0x9E37_79B9_7F4A_7C15 ^ (t + 1);
                    let mut kept: Vec<(Buffer, u64)> = Vec::new();
                    for i in 0..2_000u64 {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        let bytes = (seed % (2 << 20)) as usize + 1;
                        let Ok(mut buffer) = a.alloc(bytes, Tier::Host) else {
                            // Full for now: what this thread keeps goes home.
                            kept.clear();
                            continue;
                        };
                        let mark = (t << 32) | i;
                        stamp(&mut buffer, mark);
                        if seed % 3 == 0 {
                            kept.push((buffer, mark));
                            if kept.len() > 8 {
                                let (buffer, mark) = kept.remove(0);
                                check(&buffer, mark);
                            }
                        } else {
                            tx.send((buffer, mark)).expect("a consumer");
                        }
                    }
                    for (buffer, mark) in kept {
                        check(&buffer, mark);
                    }
                })
            })
            .collect();
        drop(tx);
        let consumers: Vec<_> = (0..2)
            .map(|_| {
                let rx = Arc::clone(&rx);
                std::thread::spawn(move || {
                    loop {
                        let next = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
                        match next {
                            Ok((buffer, mark)) => check(&buffer, mark),
                            Err(_) => break,
                        }
                    }
                })
            })
            .collect();
        let mut cycles = 0u32;
        while !producers.iter().all(|p| p.is_finished()) {
            a.grow(8 << 20).expect("grown");
            std::thread::yield_now();
            a.shrink(8 << 20);
            cycles += 1;
        }
        for thread in producers.into_iter().chain(consumers) {
            thread.join().expect("a worker");
        }
        assert!(cycles > 0);
        assert_eq!(a.stats().host_in_use, 0);
        assert_eq!(a.draining_bytes(), 0, "every drained region retired");
        assert_eq!(a.arena_stats().double_release, 0);
        assert_eq!(a.inner.lanes.len(), 0, "every worker's lane left with it");
    }

    fn arena(bytes: u64) -> Arc<Arena> {
        Arena::new(ArenaConfig {
            host_bytes: bytes,
            page_bytes: 4096,
            huge_pages: Guarantee::Probed(false),
            memlock: Guarantee::Absent,
            ..ArenaConfig::default()
        })
        .expect("an arena")
    }

    #[test]
    fn the_cached_classes_are_those_of_at_most_a_quantum() {
        assert_eq!(cached_class(0), None);
        assert_eq!(cached_class(1), Some(0));
        assert_eq!(cached_class(REFILL_QUANTUM), Some(CACHED - 1));
        assert_eq!(class_size(CACHED - 1), REFILL_QUANTUM);
        assert_eq!(cached_class(REFILL_QUANTUM + 1), None);
    }

    #[test]
    fn a_thread_has_one_lane_an_arena_until_it_ends() {
        let a = arena(4 << 20);
        let other = arena(4 << 20);
        let first = a.alloc(64 << 10, Tier::Host).expect("room");
        let second = a.alloc(64 << 10, Tier::Host).expect("room");
        let elsewhere = other.alloc(64 << 10, Tier::Host).expect("room");
        assert_eq!(a.inner.lanes.len(), 1, "one lane for this thread");
        assert_eq!(other.inner.lanes.len(), 1);
        let held = {
            let a = Arc::clone(&a);
            std::thread::spawn(move || {
                let b = a.alloc(64 << 10, Tier::Host).expect("room");
                assert_eq!(a.inner.lanes.len(), 2, "a lane of its own");
                assert!(a.inner.lanes.cached() > 0, "and a cache refilled");
                b
            })
            .join()
            .expect("a thread")
        };
        assert_eq!(
            a.inner.lanes.len(),
            1,
            "its lane left when the thread ended"
        );
        // A buffer of the thread that ended still goes home, into this thread's cache.
        drop(held);
        drop((first, second, elsewhere));
        assert_eq!(a.stats().host_in_use, 0);
    }

    #[test]
    fn the_arena_goes_with_its_last_buffer() {
        let a = arena(4 << 20);
        let inner = Arc::downgrade(&a.inner);
        let held = a.alloc(64 << 10, Tier::Host).expect("room");
        drop(a);
        assert!(inner.upgrade().is_some(), "a live buffer keeps the arena");
        drop(held);
        assert!(inner.upgrade().is_none(), "and nothing else does");
        // A thread that knew a dropped arena makes a new lane for the next one.
        let b = arena(4 << 20);
        let buffer = b.alloc(64 << 10, Tier::Host).expect("room");
        assert_eq!(b.inner.lanes.len(), 1);
        drop(buffer);
    }

    /// A refill takes half a quantum, an allocation and a release go through the cache alone,
    /// and a cache never holds more than a quantum: a release past it puts slots back first.
    #[test]
    fn a_cache_holds_at_most_a_quantum() {
        let a = arena(64 << 20);
        let lane = a.inner.lane().expect("a lane");
        let first = a.alloc(64 << 10, Tier::Host).expect("room");
        assert_eq!(lane.cached(), REFILL_QUANTUM / 2, "half a quantum taken");
        let mut held: Vec<_> = (0..40)
            .map(|_| a.alloc(64 << 10, Tier::Host).expect("room"))
            .collect();
        assert_eq!(a.stats().host_in_use, 41 * (64 << 10));
        held.push(first);
        for (i, buffer) in held.into_iter().enumerate() {
            drop(buffer);
            assert!(lane.cached() <= REFILL_QUANTUM, "release {i}");
        }
        assert_eq!(a.stats().host_in_use, 0);
        // A larger class sheds the smaller one's slots first.
        let big = a.alloc(REFILL_QUANTUM as usize, Tier::Host).expect("room");
        drop(big);
        assert!(lane.cached() <= REFILL_QUANTUM);
        assert_eq!(a.inner.lanes.cached(), lane.cached());
        assert_eq!(a.arena_stats().double_release, 0);
    }

    /// A slot released into a draining region goes back to it, so the region is retired, and a
    /// shrink takes the region out of every cache before it retires it.
    #[test]
    fn no_cache_keeps_a_draining_region() {
        let a = arena(4 << 20);
        a.grow(16 << 20).expect("grown");
        // Fill the first region so the next buffers come from the grown one.
        let first: Vec<_> = (0..4)
            .map(|_| a.alloc(1 << 20, Tier::Host).expect("room"))
            .collect();
        let grown = a.alloc(64 << 10, Tier::Host).expect("the grown region");
        let lane = a.inner.lane().expect("a lane");
        assert!(lane.cached() > 0, "slots of the grown region cached");
        assert_eq!(a.shrink(1), 16 << 20);
        assert_eq!(lane.cached(), 0, "the shrink flushed them");
        assert_eq!(a.draining_bytes(), 16 << 20, "one buffer still holds it");
        drop(grown);
        assert_eq!(a.draining_bytes(), 0, "retired when it came home");
        assert_eq!(a.arena_stats().retired_regions, 1);
        drop(first);
        assert_eq!(a.stats().host_in_use, 0);
    }
}
