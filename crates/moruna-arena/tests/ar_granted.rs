//! The arena on granted memory (`architecture/data-os.md` §3, C4): host regions carved from a
//! `HostBacking` in place of mappings of the arena's own, at most one backing region each,
//! made as the run first needs them, never given back and never drained.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_arena::{Arena, ArenaConfig, Granted, HostBacking};
use moruna_kernel::{Allocator, Guarantee, MorunaError, Tier, TierKind};

const MIB: u64 = 1024 * 1024;

/// The arena's alignment: its 64 KiB granule.
const ALIGN: usize = 64 * 1024;

/// What a fake grant keeps: a heap block, freed (and counted) when the arena lets it go.
struct Block {
    ptr: *mut u8,
    layout: std::alloc::Layout,
    released: Arc<AtomicU64>,
}

// SAFETY: the block is the grant's alone; the arena reaches its bytes through the region it
// made of them, never through this.
unsafe impl Send for Block {}
// SAFETY: as above; nothing reads or writes through a shared `Block`.
unsafe impl Sync for Block {}

impl Drop for Block {
    fn drop(&mut self) {
        self.released.fetch_add(1, Ordering::SeqCst);
        // SAFETY: `ptr` came from `alloc_zeroed` with this layout and is freed once, here.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// A backing over the heap, as the data OS's memory objects are: at most `max` bytes a
/// region, every byte present when it is given. Its grants start on the arena's alignment, as
/// the kernel maps a large-page object on a 2 MiB boundary, or a page past it when `skewed`.
struct Heap {
    max: u64,
    huge: bool,
    skewed: bool,
    given: Mutex<Vec<u64>>,
    released: Arc<AtomicU64>,
    refuse: AtomicBool,
}

impl Heap {
    fn new(max: u64) -> Arc<Heap> {
        Arc::new(Heap {
            max,
            huge: true,
            skewed: false,
            given: Mutex::new(Vec::new()),
            released: Arc::new(AtomicU64::new(0)),
            refuse: AtomicBool::new(false),
        })
    }

    /// A backing whose grants start a page past the arena's alignment.
    fn skewed(max: u64) -> Arc<Heap> {
        Arc::new(Heap {
            max,
            huge: true,
            skewed: true,
            given: Mutex::new(Vec::new()),
            released: Arc::new(AtomicU64::new(0)),
            refuse: AtomicBool::new(false),
        })
    }

    fn given(&self) -> Vec<u64> {
        self.given.lock().unwrap().clone()
    }
}

impl HostBacking for Heap {
    fn region_max(&self) -> u64 {
        self.max
    }

    fn region(&self, bytes: u64) -> moruna_kernel::Result<Granted> {
        if self.refuse.load(Ordering::SeqCst) || bytes > self.max {
            return Err(MorunaError::Config {
                name: "budget.host",
                msg: format!("no grant of {bytes} bytes"),
            });
        }
        self.given.lock().unwrap().push(bytes);
        let skew = if self.skewed { 4096 } else { 0 };
        let layout = std::alloc::Layout::from_size_align(bytes as usize + skew, ALIGN).unwrap();
        // SAFETY: the layout is not zero-sized: a grant is at least a page.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Ok(Granted {
            // SAFETY: `skew` is inside the allocation, which is `bytes + skew` long.
            base: unsafe { ptr.add(skew) },
            bytes,
            huge_pages: self.huge,
            keep: Box::new(Block {
                ptr,
                layout,
                released: Arc::clone(&self.released),
            }),
        })
    }
}

fn config(host_bytes: u64) -> ArenaConfig {
    ArenaConfig {
        host_bytes,
        host_tier: TierKind::Host,
        page_bytes: 4096,
        // Neither applies to granted memory, and an unknown guarantee is not consulted.
        huge_pages: Guarantee::Unknown,
        memlock: Guarantee::Unknown,
        ..ArenaConfig::default()
    }
}

const GRANULE: usize = 64 * 1024;

#[test]
fn the_budget_is_made_into_regions_of_at_most_one_grant_as_the_run_needs_them() {
    let heap = Heap::new(4 * MIB);
    let before = moruna_arena::region::granted_regions();
    let arena = Arena::with_backing(config(10 * MIB), heap.clone()).expect("a granted arena");
    assert!(arena.granted());
    assert!(arena.huge_pages_active(), "every grant was huge");
    // One region now, of the most one grant holds; the rest is the arena's but not yet made.
    let chunk = 4 * MIB;
    assert_eq!(heap.given(), vec![4 * MIB]);
    assert!(moruna_arena::region::granted_regions() > before);
    assert_eq!(arena.uncommitted_bytes(), 10 * MIB - chunk);
    assert_eq!(arena.host_capacity(), 10 * MIB);
    assert_eq!(arena.regions_capacity(), 10 * MIB);
    assert_eq!(arena.region_bytes(Tier::Host), 10 * MIB);
    assert_eq!(arena.available(Tier::Host), Some(10 * MIB));

    // Regions are made as the buffers need them, some ahead of need, never past the budget.
    // Granule buffers fill the regions exactly.
    let mut held = Vec::new();
    for i in 0..160u32 {
        let mut b = arena.alloc(GRANULE, Tier::Host).expect("room");
        b.fill(i as u8);
        assert!(arena.contains(b.as_ptr()));
        held.push(b);
        let made: u64 = heap.given().iter().sum();
        assert!(
            made <= 10 * MIB,
            "after {} buffers, {made} made",
            held.len()
        );
    }
    arena.settle();
    assert!(held.iter().enumerate().all(|(i, b)| b[0] == i as u8));
    assert_eq!(heap.given().len(), 3);
    assert!(heap.given().iter().all(|b| *b <= 4 * MIB));
    assert_eq!(arena.uncommitted_bytes(), 0);
    assert_eq!(arena.host_capacity(), 10 * MIB, "the capacity never moved");
    assert_eq!(arena.available(Tier::Host), Some(0));
    assert!(matches!(
        arena.alloc(GRANULE, Tier::Host),
        Err(MorunaError::Alloc { budget, .. }) if budget == 10 * MIB
    ));
    assert_eq!(heap.given().len(), 3, "nothing is made past the budget");
    drop(held);
    assert_eq!(arena.available(Tier::Host), Some(10 * MIB));
    // What the run freed is used again before anything is made, and nothing is given back
    // while the run lasts (C4: no zeroing on reuse).
    let again: Vec<_> = (0..160)
        .map(|_| arena.alloc(GRANULE, Tier::Host).expect("room"))
        .collect();
    assert_eq!(heap.given().len(), 3);
    drop(again);
    assert_eq!(arena.release_free(), 0);
    assert_eq!(arena.shrink(10 * MIB), 0, "the run's regions never drain");
    assert_eq!(heap.released.load(Ordering::SeqCst), 0);
    drop(arena);
    assert_eq!(
        heap.released.load(Ordering::SeqCst),
        3,
        "every grant released once"
    );
}

#[test]
fn a_run_that_needs_little_of_a_large_budget_makes_little() {
    let heap = Heap::new(4 * MIB);
    let arena = Arena::with_backing(config(1024 * MIB), heap.clone()).expect("a granted arena");
    assert_eq!(
        heap.given().len(),
        1,
        "one grant at start, not 1 GiB of them"
    );
    let mut held = Vec::new();
    for _ in 0..100 {
        held.push(arena.alloc(GRANULE, Tier::Host).expect("room"));
    }
    arena.settle();
    // 100 granules hold 6.25 MiB: what is made is that, a little ahead of need, and a step.
    let made: u64 = heap.given().iter().sum();
    let used = 100 * GRANULE as u64;
    assert!(
        made <= used + moruna_arena::GRANTED_AHEAD_BELOW + 4 * MIB,
        "{made} made for {used}"
    );
    assert_eq!(arena.host_capacity(), 1024 * MIB);
    assert_eq!(
        arena.available(Tier::Host),
        Some(1024 * MIB - 100 * GRANULE as u64)
    );
}

#[test]
fn what_is_made_follows_what_the_run_uses_not_its_budget() {
    const GIB: u64 = 1024 * MIB;
    let first = moruna_arena::FIRST_GRANTED_REGION;
    let heap = Heap::new(256 * MIB);
    let arena = Arena::with_backing(config(GIB), heap.clone()).expect("a granted arena");
    // A small region at start, whatever the budget; an aligned grant is the region, whole.
    assert_eq!(heap.given(), vec![first]);
    assert_eq!(arena.host_capacity(), GIB);
    let made = |arena: &Arena| GIB - arena.uncommitted_bytes();
    let step = moruna_arena::GRANTED_STEP_MAX;
    let ahead = moruna_arena::GRANTED_AHEAD_BELOW;
    let mut held = Vec::new();
    for i in 1..=300u64 {
        held.push(arena.alloc(MIB as usize, Tier::Host).expect("room"));
        arena.settle();
        let used = i * MIB;
        // What is made stays within a step and the free bytes kept ahead of what is used.
        assert!(
            made(&arena) <= (used + ahead + step).max(first),
            "{used} bytes used, {} made",
            made(&arena)
        );
    }
    // No region for these buffers is larger than a step, and some were made ahead of need.
    let regions = heap.given();
    assert!(regions.iter().all(|r| *r <= step.max(first)), "{regions:?}");
    assert!(arena.regions_made_ahead() > 0);
    drop(held);
}

#[test]
fn a_large_first_allocation_makes_a_region_that_holds_it() {
    let heap = Heap::new(256 * MIB);
    let arena = Arena::with_backing(config(1024 * MIB), heap.clone()).expect("a granted arena");
    // The Parquet sink's 128 MiB buffer, past the first region.
    let big = arena
        .alloc(128 * MIB as usize, Tier::Host)
        .expect("a region for it");
    assert_eq!(heap.given().len(), 2);
    assert_eq!(heap.given()[1], 128 * MIB);
    assert_eq!(arena.side_regions(), 0);
    drop(big);
}

#[test]
fn allocations_that_find_the_regions_full_together_make_each_region_once() {
    let heap = Heap::new(4 * MIB);
    let budget = 64 * MIB;
    let arena = Arena::with_backing(config(budget), heap.clone()).expect("a granted arena");
    let chunk = 4 * MIB;
    let regions = budget.div_ceil(chunk) as usize;
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let arena = Arc::clone(&arena);
            std::thread::spawn(move || {
                let mut held = Vec::new();
                while let Ok(b) = arena.alloc(GRANULE, Tier::Host) {
                    held.push(b);
                }
                held
            })
        })
        .collect();
    let held: Vec<_> = threads
        .into_iter()
        .flat_map(|t| t.join().expect("a worker"))
        .collect();
    // Every byte of the budget was handed out, and each region was made once.
    assert_eq!(held.len() as u64, budget / GRANULE as u64);
    assert_eq!(heap.given().len(), regions, "{:?}", heap.given());
    assert_eq!(arena.uncommitted_bytes(), 0);
    assert_eq!(arena.side_regions(), 0);
}

#[test]
fn a_ceiling_below_the_budget_makes_nothing_past_it() {
    let heap = Heap::new(4 * MIB);
    let arena = Arena::with_backing(config(16 * MIB), heap.clone()).expect("a granted arena");
    arena.cap_host(Some(4 * MIB));
    let mut held = Vec::new();
    while let Ok(b) = arena.alloc(GRANULE, Tier::Host) {
        held.push(b);
    }
    assert_eq!(held.len() as u64, 4 * MIB / GRANULE as u64);
    assert_eq!(
        heap.given().len(),
        1,
        "the cap fills the first region exactly, and nothing past it is made"
    );
    assert_eq!(arena.host_capacity(), 4 * MIB);
    arena.cap_host(None);
    assert_eq!(arena.host_capacity(), 16 * MIB);
}

#[test]
fn a_grant_refused_during_the_run_refuses_the_allocation_and_the_arena_goes_on() {
    let heap = Heap::new(4 * MIB);
    let arena = Arena::with_backing(config(8 * MIB), heap.clone()).expect("a granted arena");
    let chunk = 4 * MIB;
    let first: Vec<_> = (0..chunk / GRANULE as u64)
        .map(|_| arena.alloc(GRANULE, Tier::Host).expect("room"))
        .collect();
    heap.refuse.store(true, Ordering::SeqCst);
    assert!(matches!(
        arena.alloc(GRANULE, Tier::Host),
        Err(MorunaError::Alloc { .. })
    ));
    assert_eq!(
        arena.uncommitted_bytes(),
        8 * MIB - chunk,
        "nothing was made"
    );
    // Freed room still serves, and the next allocation past it makes the region after all.
    drop(first);
    assert!(arena.alloc(GRANULE, Tier::Host).is_ok());
    heap.refuse.store(false, Ordering::SeqCst);
    let held: Vec<_> = (0..chunk / GRANULE as u64 + 1)
        .map(|_| arena.alloc(GRANULE, Tier::Host).expect("room"))
        .collect();
    assert_eq!(heap.given().len(), 2);
    drop(held);
}

#[test]
fn the_largest_free_counts_the_next_region_to_be_made() {
    let heap = Heap::new(4 * MIB);
    let arena = Arena::with_backing(config(10 * MIB), heap.clone()).expect("a granted arena");
    let chunk = 4 * MIB;
    let held: Vec<_> = (0..chunk / GRANULE as u64)
        .map(|_| arena.alloc(GRANULE, Tier::Host).expect("room"))
        .collect();
    // The made region is full; the next one would hold a chunk.
    assert_eq!(arena.largest_free(Tier::Host), chunk);
    assert_eq!(arena.arena_stats().largest_free, chunk);
    assert_eq!(arena.arena_stats().host_regions, 1);
    drop(held);
    // An arena that maps its own regions has nothing left to make.
    let own = Arena::new(ArenaConfig {
        huge_pages: Guarantee::Absent,
        memlock: Guarantee::Absent,
        ..config(4 * MIB)
    })
    .expect("an arena");
    assert_eq!(own.uncommitted_bytes(), 0);
    assert_eq!(own.region_bytes(Tier::Host), 4 * MIB);
}

#[test]
fn a_large_buffer_past_what_is_left_to_make_gets_a_side_region() {
    let heap = Heap::new(4 * MIB);
    // A region of 4 MiB, then 1 MiB left to make.
    let arena = Arena::with_backing(config(5 * MIB), heap.clone()).expect("a granted arena");
    let small = arena.alloc(GRANULE, Tier::Host).expect("room");
    let big = arena
        .alloc(2 * MIB as usize, Tier::Host)
        .expect("first region");
    // Room by charge, but neither the first region nor the 1 MiB left holds 2 MiB in one
    // piece: a side grant. The 1 MiB left may have been made ahead of need meanwhile.
    let side = arena.alloc(2 * MIB as usize, Tier::Host);
    assert!(side.is_ok(), "{side:?}");
    arena.settle();
    assert_eq!(arena.side_regions(), 1);
    assert!(heap.given().len() <= 3, "{:?}", heap.given());
    assert!(
        heap.given().contains(&(2 * MIB)),
        "the side grant: {:?}",
        heap.given()
    );
    drop((small, big, side));
}

#[test]
fn a_backing_whose_region_holds_no_class_is_refused() {
    let heap = Heap::new(32 * 1024);
    let refused = Arena::with_backing(config(MIB), heap).unwrap_err();
    assert!(matches!(refused, MorunaError::Config { name, .. } if name == "budget.host"));
}

#[test]
fn a_refused_grant_is_the_arenas_refusal() {
    let heap = Arc::new(Heap {
        max: 4 * MIB,
        huge: false,
        skewed: false,
        given: Mutex::new(Vec::new()),
        released: Arc::new(AtomicU64::new(0)),
        refuse: AtomicBool::new(true),
    });
    assert!(Arena::with_backing(config(2 * MIB), heap).is_err());
}

#[test]
fn growing_adds_granted_regions_that_shrink_may_drain() {
    let heap = Heap::new(4 * MIB);
    let arena = Arena::with_backing(config(2 * MIB), heap.clone()).expect("a granted arena");
    assert!(!heap.given().is_empty());
    let added = arena.grow(6 * MIB).expect("grown");
    assert!(added >= 6 * MIB, "{added}");
    assert!(arena.host_capacity() >= 8 * MIB);
    let regions = heap.given().len();
    assert!(regions >= 3, "{regions}");
    // The grown regions are not the run's own: shrink may drain them, and they are released
    // once empty.
    let marked = arena.shrink(added);
    assert!(marked >= added);
    assert_eq!(
        arena.draining_bytes(),
        0,
        "empty regions are released at once"
    );
    assert_eq!(
        heap.released.load(Ordering::SeqCst),
        regions as u64 - 1,
        "every grown grant released"
    );
}

#[test]
fn a_large_buffer_the_regions_cannot_hold_in_one_piece_gets_a_granted_side_region() {
    // Grants a page off the alignment: regions of 8 MiB less 64 KiB and of 6 MiB and 64 KiB,
    // whose free bytes after these three are about 4 MiB, in pieces.
    let heap = Heap::skewed(8 * MIB);
    let arena = Arena::with_backing(config(14 * MIB), heap.clone()).expect("a granted arena");
    let a = arena.alloc(4 * MIB as usize, Tier::Host).expect("first");
    let b = arena.alloc(4 * MIB as usize, Tier::Host).expect("second");
    let c = arena.alloc(2 * MIB as usize, Tier::Host).expect("third");
    let grants = heap.given().len();
    let free = arena.available(Tier::Host).unwrap();
    assert!(free >= 3 * MIB, "{free}");
    // Room by charge, not in one piece: a side grant.
    let side = arena.alloc(3 * MIB as usize, Tier::Host);
    match side {
        Ok(side) => {
            assert_eq!(arena.side_regions(), 1);
            assert!(heap.given().len() > grants);
            drop(side);
            assert_eq!(arena.side_in_use(), 0);
        }
        // Where the regions did hold it in one piece, no grant was needed.
        Err(e) => panic!("refused: {e:?}"),
    }
    // A side region larger than one grant is refused, as the budget refuses.
    assert!(arena.alloc(9 * MIB as usize, Tier::Host).is_err());
    drop((a, b, c));
}

#[test]
fn a_page_locked_run_takes_granted_memory_as_locked() {
    let heap = Heap::new(4 * MIB);
    let arena = Arena::with_backing(
        ArenaConfig {
            host_tier: TierKind::PinnedHost,
            ..config(4 * MIB)
        },
        heap,
    )
    .expect("a granted arena");
    assert!(arena.is_pinned());
    let b = arena
        .alloc(64 * 1024, Tier::PinnedHost)
        .expect("pinned room");
    assert_eq!(arena.tier_of(b.as_ptr()), Some(Tier::PinnedHost));
}

#[test]
fn an_aligned_grant_is_used_whole_so_two_halves_share_one_region() {
    let heap = Heap::new(4 * MIB);
    let arena = Arena::with_backing(config(16 * MIB), heap.clone()).expect("a granted arena");
    let a = arena
        .alloc(2 * MIB as usize, Tier::Host)
        .expect("first half");
    let b = arena
        .alloc(2 * MIB as usize, Tier::Host)
        .expect("second half");
    assert_eq!(heap.given(), vec![4 * MIB], "one grant holds both");
    assert_eq!(arena.uncommitted_bytes(), 12 * MIB);
    assert_eq!(heap.released.load(Ordering::SeqCst), 0);
    drop((a, b));
}

#[test]
fn a_skewed_grant_is_asked_again_or_used_an_alignment_short() {
    // A grant of the backing's largest size cannot be asked for one alignment larger: it is
    // used from its first aligned byte, 64 KiB short.
    let heap = Heap::skewed(4 * MIB);
    let arena = Arena::with_backing(config(16 * MIB), heap.clone()).expect("a granted arena");
    assert_eq!(heap.given(), vec![4 * MIB]);
    assert_eq!(
        arena.uncommitted_bytes(),
        16 * MIB - (4 * MIB - ALIGN as u64)
    );
    assert_eq!(
        arena.host_capacity(),
        16 * MIB,
        "the budget is whole all the same"
    );
    assert_eq!(heap.released.load(Ordering::SeqCst), 0);
    // A smaller one is given back and asked for again one alignment larger, and is usable whole.
    let heap = Heap::skewed(4 * MIB);
    let arena = Arena::with_backing(config(MIB), heap.clone()).expect("a granted arena");
    assert_eq!(heap.given(), vec![MIB, MIB + ALIGN as u64]);
    assert_eq!(
        heap.released.load(Ordering::SeqCst),
        1,
        "the first went back"
    );
    assert_eq!(arena.uncommitted_bytes(), 0);
    let whole = arena
        .alloc(MIB as usize, Tier::Host)
        .expect("the whole region");
    assert!(arena.alloc(GRANULE, Tier::Host).is_err());
    drop(whole);
    // A backing whose largest grant holds nothing once aligned is refused.
    let tiny = Heap::skewed(ALIGN as u64);
    let refused = Arena::with_backing(config(ALIGN as u64), tiny).unwrap_err();
    assert!(matches!(refused, MorunaError::Config { name, .. } if name == "budget.host"));
}

#[test]
fn steps_follow_what_is_in_use_not_the_budget() {
    // The same buffers on a 1 GiB and a 16 GiB budget make the same regions.
    let regions_for = |budget: u64| {
        let heap = Heap::new(256 * MIB);
        let arena = Arena::with_backing(config(budget), heap.clone()).expect("a granted arena");
        let mut held = Vec::new();
        for _ in 0..200 {
            held.push(arena.alloc(MIB as usize, Tier::Host).expect("room"));
            arena.settle();
        }
        drop(held);
        heap.given()
    };
    let small = regions_for(1024 * MIB);
    let large = regions_for(16 * 1024 * MIB);
    assert_eq!(small, large);
    let made: u64 = large.iter().sum();
    assert!(
        made < 2 * 200 * MIB,
        "{made} made for 200 MiB held: {large:?}"
    );
}

#[test]
fn a_request_larger_than_a_step_gets_a_region_of_its_own_size() {
    let heap = Heap::new(256 * MIB);
    let arena = Arena::with_backing(config(4096 * MIB), heap.clone()).expect("a granted arena");
    let big = arena
        .alloc(100 * MIB as usize, Tier::Host)
        .expect("a region of its own");
    arena.settle();
    assert!(heap.given().contains(&(128 * MIB)), "{:?}", heap.given());
    assert_eq!(arena.side_regions(), 0);
    drop(big);
}

#[test]
fn the_next_step_is_made_ahead_of_need_and_serves_without_a_wait() {
    let heap = Heap::new(256 * MIB);
    let arena = Arena::with_backing(config(1024 * MIB), heap.clone()).expect("a granted arena");
    let first = moruna_arena::FIRST_GRANTED_REGION;
    // Fill the first region past the mark: the next step is made on a thread of its own.
    let mut held = Vec::new();
    for _ in 0..(first / MIB) / 2 {
        held.push(arena.alloc(MIB as usize, Tier::Host).expect("room"));
    }
    arena.settle();
    assert!(arena.regions_made_ahead() >= 1);
    let made_before = heap.given().len();
    // The next buffers land in the region made ahead: none of them makes one of its own.
    for _ in 0..(first / MIB) / 2 + 4 {
        held.push(arena.alloc(MIB as usize, Tier::Host).expect("room"));
    }
    let made_after = heap.given().len();
    assert!(
        made_after <= made_before + 1,
        "{made_before} then {made_after}: {:?}",
        heap.given()
    );
    drop(held);
}

#[test]
fn nothing_is_made_ahead_past_the_budget() {
    let heap = Heap::new(256 * MIB);
    let arena = Arena::with_backing(config(40 * MIB), heap.clone()).expect("a granted arena");
    let held = arena.alloc(MIB as usize, Tier::Host).expect("room");
    arena.settle();
    let made: u64 = heap.given().iter().sum();
    assert_eq!(
        made,
        40 * MIB,
        "the first 32 MiB and the 8 MiB left, made ahead"
    );
    assert_eq!(arena.uncommitted_bytes(), 0);
    let again = arena.alloc(MIB as usize, Tier::Host).expect("room");
    arena.settle();
    assert_eq!(
        heap.given().iter().sum::<u64>(),
        40 * MIB,
        "nothing past the budget"
    );
    drop((held, again));
    // An arena that maps its own regions makes nothing ahead.
    let own = Arena::new(ArenaConfig {
        huge_pages: Guarantee::Absent,
        memlock: Guarantee::Absent,
        ..config(4 * MIB)
    })
    .expect("an arena");
    let b = own.alloc(MIB as usize, Tier::Host).expect("room");
    own.settle();
    assert_eq!(own.regions_made_ahead(), 0);
    drop(b);
}
