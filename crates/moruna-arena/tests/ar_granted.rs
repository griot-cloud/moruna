//! The arena on granted memory (`architecture/data-os.md` §3, C4): host regions carved from a
//! `HostBacking` in place of mappings of the arena's own, at most one backing region each,
//! never given back and never drained.

use std::sync::atomic::{AtomicU64, Ordering};
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
    refuse: bool,
}

impl Heap {
    fn new(max: u64) -> Arc<Heap> {
        Arc::new(Heap {
            max,
            huge: true,
            given: Mutex::new(Vec::new()),
            released: Arc::new(AtomicU64::new(0)),
            refuse: false,
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
        if self.refuse || bytes > self.max {
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

#[test]
fn the_budget_is_carved_into_regions_of_at_most_one_grant() {
    let heap = Heap::new(4 * MIB);
    let before = moruna_arena::region::granted_regions();
    let arena = Arena::with_backing(config(10 * MIB), heap.clone()).expect("a granted arena");
    assert!(arena.granted());
    assert!(arena.huge_pages_active(), "every grant was huge");
    // Each region asks for its bytes and one alignment more, within the grant's largest.
    let given = heap.given();
    assert_eq!(given.len(), 3, "{given:?}");
    assert!(given.iter().all(|b| *b <= 4 * MIB));
    assert!(moruna_arena::region::granted_regions() >= before + 3);
    assert_eq!(arena.host_capacity(), 10 * MIB);
    assert_eq!(arena.available(Tier::Host), Some(10 * MIB));

    // Buffers land in every region, and are writable and readable where they land. Granule
    // buffers fill the regions exactly.
    let mut held = Vec::new();
    for i in 0..160u32 {
        let mut b = arena.alloc(64 * 1024, Tier::Host).expect("room");
        b.fill(i as u8);
        assert!(arena.contains(b.as_ptr()));
        held.push(b);
    }
    assert!(held.iter().enumerate().all(|(i, b)| b[0] == i as u8));
    assert_eq!(arena.available(Tier::Host), Some(0));
    assert!(matches!(
        arena.alloc(64 * 1024, Tier::Host),
        Err(MorunaError::Alloc { .. })
    ));
    drop(held);
    assert_eq!(arena.available(Tier::Host), Some(10 * MIB));
    // Nothing is given back while the run lasts (C4: no zeroing on reuse).
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
        refuse: true,
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
