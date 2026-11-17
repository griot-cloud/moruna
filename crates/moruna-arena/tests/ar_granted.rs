//! The arena on granted memory (`architecture/data-os.md` §3, C4): host regions carved from a
//! `HostBacking` in place of mappings of the arena's own, at most one backing region each,
//! made as the run first needs them, never given back and never drained.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_arena::{Arena, ArenaConfig, Granted, HostBacking};
use moruna_kernel::{Allocator, Guarantee, MorunaError, Tier, TierKind};

const MIB: u64 = 1024 * 1024;

/// What a fake grant keeps: a heap block, freed (and counted) when the arena lets it go.
struct Block {
    bytes: Vec<u8>,
    released: Arc<AtomicU64>,
}

impl Drop for Block {
    fn drop(&mut self) {
        self.released.fetch_add(1, Ordering::SeqCst);
        let _ = self.bytes.len();
    }
}

/// A backing over the heap, as the data OS's memory objects are: at most `max` bytes a
/// region, every byte present when it is given.
struct Heap {
    max: u64,
    huge: bool,
    given: Mutex<Vec<u64>>,
    released: Arc<AtomicU64>,
    refuse: AtomicBool,
}

impl Heap {
    fn new(max: u64) -> Arc<Heap> {
        Arc::new(Heap {
            max,
            huge: true,
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
        let mut bytes = vec![0u8; bytes as usize];
        let base = bytes.as_mut_ptr();
        let len = bytes.len() as u64;
        Ok(Granted {
            base,
            bytes: len,
            huge_pages: self.huge,
            keep: Box::new(Block {
                bytes,
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
    // One region now, of the most one grant holds less one alignment; the rest is the arena's
    // but not yet made.
    let chunk = 4 * MIB - 64 * 1024;
    assert_eq!(heap.given(), vec![4 * MIB]);
    assert!(moruna_arena::region::granted_regions() > before);
    assert_eq!(arena.uncommitted_bytes(), 10 * MIB - chunk);
    assert_eq!(arena.host_capacity(), 10 * MIB);
    assert_eq!(arena.regions_capacity(), 10 * MIB);
    assert_eq!(arena.region_bytes(Tier::Host), 10 * MIB);
    assert_eq!(arena.available(Tier::Host), Some(10 * MIB));

    // Buffers fill the first region before a second is made, and so on. Granule buffers fill
    // the regions exactly.
    let per_region = (chunk / GRANULE as u64) as usize;
    let mut held = Vec::new();
    for i in 0..160u32 {
        let mut b = arena.alloc(GRANULE, Tier::Host).expect("room");
        b.fill(i as u8);
        assert!(arena.contains(b.as_ptr()));
        held.push(b);
        let made = heap.given().len();
        let want = if held.len() <= per_region {
            1
        } else if held.len() <= 2 * per_region {
            2
        } else {
            3
        };
        assert_eq!(made, want, "after {} buffers", held.len());
    }
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
    // 100 granules need two regions of 63.
    assert_eq!(heap.given().len(), 2);
    assert_eq!(arena.host_capacity(), 1024 * MIB);
    assert_eq!(
        arena.available(Tier::Host),
        Some(1024 * MIB - 100 * GRANULE as u64)
    );
}

#[test]
fn allocations_that_find_the_regions_full_together_make_each_region_once() {
    let heap = Heap::new(4 * MIB);
    let budget = 64 * MIB;
    let arena = Arena::with_backing(config(budget), heap.clone()).expect("a granted arena");
    let chunk = 4 * MIB - 64 * 1024;
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
        2,
        "the cap's last granule needed a second region"
    );
    assert_eq!(arena.host_capacity(), 4 * MIB);
    arena.cap_host(None);
    assert_eq!(arena.host_capacity(), 16 * MIB);
}

#[test]
fn a_grant_refused_during_the_run_refuses_the_allocation_and_the_arena_goes_on() {
    let heap = Heap::new(4 * MIB);
    let arena = Arena::with_backing(config(8 * MIB), heap.clone()).expect("a granted arena");
    let chunk = 4 * MIB - 64 * 1024;
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
    let chunk = 4 * MIB - 64 * 1024;
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
    // A region of 4032 KiB, then 1088 KiB left to make.
    let arena = Arena::with_backing(config(5 * MIB), heap.clone()).expect("a granted arena");
    let small = arena.alloc(GRANULE, Tier::Host).expect("room");
    let big = arena
        .alloc(2 * MIB as usize, Tier::Host)
        .expect("first region");
    // Room by charge, but neither the first region nor the 1088 KiB left holds 2 MiB in one
    // piece: a side grant.
    let side = arena.alloc(2 * MIB as usize, Tier::Host);
    assert!(side.is_ok(), "{side:?}");
    assert_eq!(arena.side_regions(), 1);
    assert_eq!(heap.given().len(), 2);
    drop((small, big, side));
}

#[test]
fn a_backing_whose_region_holds_no_class_is_refused() {
    let heap = Heap::new(64 * 1024);
    let refused = Arena::with_backing(config(MIB), heap).unwrap_err();
    assert!(matches!(refused, MorunaError::Config { name, .. } if name == "budget.host"));
}

#[test]
fn a_refused_grant_is_the_arenas_refusal() {
    let heap = Arc::new(Heap {
        max: 4 * MIB,
        huge: false,
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
    let heap = Heap::new(8 * MIB);
    // Two regions of about 8 MiB; 6 MiB taken from each leaves 4 MiB free, 2 in each.
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
            assert_eq!(heap.given().len(), grants + 1);
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
