//! AR-T12 contains_and_tier_of (02 k): for every live buffer, `contains(ptr)` is true for
//! its first and last byte and false one byte past its end and for a heap pointer;
//! `tier_of` equals the buffer's tier; on a pinned arena every host `tier_of` is
//! `PinnedHost`, on an unpinned one `Host`; `BufferView::of_arrow` over
//! `into_arrow_buffer` of an arena buffer, and over a slice of it, reports that tier.
//! f.7, AR-I6.

mod common;

use amoru_arena::{Arena, ArenaConfig};
use amoru_kernel::{Allocator, BufferView, Guarantee, Tier, TierKind};

fn check_bounds(arena: &Arena, tier: Tier) {
    let sizes = [1usize, 4096, 64 * 1024, 3 * 1024 * 1024];
    let mut live = Vec::new();
    for bytes in sizes {
        let b = arena.alloc(bytes, tier).expect("a buffer");
        let ptr = b.host_ptr().expect("a host buffer has a host pointer");
        assert!(arena.contains(ptr.cast_const()));
        // SAFETY: test-only; the last byte of a live buffer and the address one past it,
        // which is only compared, never read.
        let last = unsafe { ptr.add(bytes - 1) };
        assert!(arena.contains(last.cast_const()));
        assert_eq!(arena.tier_of(ptr.cast_const()), Some(tier));
        assert_eq!(arena.tier_of(last.cast_const()), Some(tier));
        live.push(b);
    }
    // One byte past the region is not in it, and neither is anything on the heap.
    let region = arena.region_bytes(tier);
    let base = live[0].host_ptr().expect("host pointer");
    // SAFETY: test-only; the address is computed and compared, never read.
    let past = unsafe { base.add(region as usize) };
    assert!(!arena.contains(past.cast_const()));
    assert_eq!(arena.tier_of(past.cast_const()), None);
    let heap = [0u8; 64];
    assert!(!arena.contains(heap.as_ptr()));
    assert_eq!(arena.tier_of(heap.as_ptr()), None);
    // The other host tier does not exist in this run (AR-I6, contracts e.1).
    let other = if tier == Tier::Host {
        Tier::PinnedHost
    } else {
        Tier::Host
    };
    assert_eq!(arena.region_bytes(other), 0);
}

fn check_arrow(arena: &Arena, tier: Tier) {
    let b = arena.alloc(8192, tier).expect("a buffer to hand to Arrow");
    let arrow_buffer = b.into_arrow_buffer().expect("a host buffer converts");
    let view = BufferView::of_arrow(&arrow_buffer, arena).expect("an arena-owned allocation");
    assert_eq!(view.tier(), tier);
    assert_eq!(view.len(), 8192);
    let sliced = arrow_buffer.slice(64);
    let view = BufferView::of_arrow(&sliced, arena).expect("a slice shares the allocation");
    assert_eq!(view.tier(), tier, "a slice keeps the arena's tier (d.3)");
}

#[test]
fn ar_t12_contains_and_tier_of() {
    let arena = common::host_arena(64 << 20);
    assert!(!arena.is_pinned());
    check_bounds(&arena, Tier::Host);
    check_arrow(&arena, Tier::Host);

    // The pinned half of the invariant. A host that refuses `mlock` says so through
    // `is_pinned()`; AR-T6 forces both outcomes deterministically, so this half asserts
    // whichever tier this host's arena actually has.
    let pinned = Arena::new(ArenaConfig {
        host_bytes: 1 << 20,
        host_tier: TierKind::PinnedHost,
        device_bytes: Vec::new(),
        page_bytes: 4096,
        huge_pages: Guarantee::Probed(false),
        memlock: Guarantee::Probed(true),
        register_rdma: false,
    })
    .expect("a pinned arena, or its fallback");
    let tier = if pinned.is_pinned() {
        Tier::PinnedHost
    } else {
        println!("AR-T12: this host refused mlock; the arena fell back to Tier::Host (f.1)");
        Tier::Host
    };
    let b = pinned.alloc(4096, tier).expect("the run's host tier");
    let ptr = b.host_ptr().expect("host pointer");
    assert_eq!(pinned.tier_of(ptr.cast_const()), Some(tier));
    assert_eq!(b.tier(), tier);
    check_arrow(&pinned, tier);
}
