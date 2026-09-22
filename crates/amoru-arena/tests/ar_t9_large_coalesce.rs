//! AR-T9 large_coalesce (02 k): three adjacent large allocations, free the middle then the
//! ends; `largest_free` returns the full gap. f.4.

mod common;

use amoru_kernel::{Allocator, Tier};

/// The smallest allocation the large path serves: one granule above the largest class (e.2).
const UNIT: u64 = (512 << 20) + (64 << 10);

#[test]
fn ar_t9_large_coalesce() {
    let region = 3 * UNIT;
    let arena = common::host_arena(region);
    assert_eq!(arena.region_bytes(Tier::Host), region);
    assert_eq!(arena.largest_free(Tier::Host), region);

    let a = arena.alloc(UNIT as usize, Tier::Host).expect("a");
    let b = arena.alloc(UNIT as usize, Tier::Host).expect("b");
    let c = arena.alloc(UNIT as usize, Tier::Host).expect("c");
    assert_eq!(arena.stats().host_in_use, region);
    assert_eq!(arena.largest_free(Tier::Host), 0);
    let (pa, pb, pc) = (
        a.host_ptr().expect("a"),
        b.host_ptr().expect("b"),
        c.host_ptr().expect("c"),
    );
    assert!(pa > pb && pb > pc, "large allocations grow downward (e.1)");

    drop(b);
    assert_eq!(arena.largest_free(Tier::Host), UNIT, "the middle block");
    drop(a);
    assert_eq!(
        arena.largest_free(Tier::Host),
        2 * UNIT,
        "merged with its neighbour"
    );
    drop(c);
    assert_eq!(
        arena.largest_free(Tier::Host),
        region,
        "the whole gap is back (f.4)"
    );
    assert_eq!(arena.stats().host_in_use, 0);
    assert_eq!(arena.arena_stats().double_release, 0);

    // And the gap serves a new large allocation of the whole region.
    let d = arena
        .alloc(region as usize, Tier::Host)
        .expect("the full gap");
    assert_eq!(d.len() as u64, region);
}
