//! The arena under a budget that follows the machine (MH 4.4):
//! grow_adds_a_region, shrink_drains, drain_then_unmap. AR-I3.
//!
//! A test binary of its own, and every test in it takes one gate, so the process-wide syscall
//! counters of the AR-T3 shim describe the arena the test is looking at.

mod common;

use std::sync::{Arc, Mutex, MutexGuard};

use moruna_arena::{Arena, ArenaConfig, REGION_ROUNDING};
use moruna_kernel::{Allocator, Guarantee, MorunaError, Tier, TierKind};

const MIB: u64 = 1024 * 1024;

fn gate() -> MutexGuard<'static, ()> {
    static GATE: Mutex<()> = Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// grow_adds_a_region. `grow` maps one region rounded up to the huge page, touched like
/// the first, and the allocator serves from it once the first is full; an arena that never grows
/// made exactly one `mmap` (AR-I3), and each grow makes exactly one more.
#[test]
fn grow_adds_a_region() {
    let _gate = gate();
    let before = moruna_arena::region::syscall_counts();
    let arena = common::host_arena(8 * MIB);
    let after_new = moruna_arena::region::syscall_counts();
    assert_eq!(after_new.mmap - before.mmap, 1, "one mmap at new (AR-I3)");
    assert_eq!(arena.arena_stats().host_regions, 1);
    assert_eq!(arena.host_capacity(), 8 * MIB);

    // Fill the first region.
    let mut live = Vec::new();
    while let Ok(buffer) = arena.alloc((MIB) as usize, Tier::Host) {
        live.push(buffer);
    }
    let refused = arena.alloc(MIB as usize, Tier::Host).expect_err("full");
    assert!(matches!(refused, MorunaError::Alloc { budget, .. } if budget == 8 * MIB));

    // Grow by a figure that is not a whole huge page: it is rounded up.
    let added = arena.grow(3 * MIB + 1).expect("grow");
    assert_eq!(added, 4 * MIB, "rounded up to {REGION_ROUNDING} bytes");
    let after_grow = moruna_arena::region::syscall_counts();
    assert_eq!(after_grow.mmap - after_new.mmap, 1, "one mmap per grow");
    assert_eq!(arena.region_bytes(Tier::Host), 12 * MIB);
    assert_eq!(arena.host_capacity(), 12 * MIB);
    assert_eq!(arena.arena_stats().host_regions, 2);
    assert!(arena.largest_free(Tier::Host) >= MIB);

    // The new region serves, and a buffer from it is an ordinary host buffer.
    let grown = arena
        .alloc(MIB as usize, Tier::Host)
        .expect("served by the new region");
    assert_eq!(grown.tier(), Tier::Host);
    assert!(arena.contains(grown.host_ptr().expect("host")));
    assert_eq!(
        arena.tier_of(grown.host_ptr().expect("host")),
        Some(Tier::Host)
    );
    let in_use = arena.stats().host_in_use;
    drop(grown);
    assert_eq!(
        arena.stats().host_in_use,
        in_use - MIB,
        "accounting is exact (AR-I5)"
    );
    drop(live);
    assert_eq!(arena.stats().host_in_use, 0);

    // Zero is no region at all.
    assert_eq!(arena.grow(0).expect("nothing to add"), 0);
    assert_eq!(arena.arena_stats().host_regions, 2);
    assert!(format!("{arena:?}").contains("host_bytes"));
}

/// shrink_drains. `shrink` marks the newest grown regions draining until the bytes asked
/// for are covered; a draining region takes no new allocation while the older ones still
/// serve, a region that is already empty is unmapped at once, and the region `new` made never
/// drains.
#[test]
fn shrink_drains() {
    let _gate = gate();
    let arena = common::host_arena(8 * MIB);
    arena.grow(4 * MIB).expect("second region");
    arena.grow(4 * MIB).expect("third region");
    assert_eq!(arena.arena_stats().host_regions, 3);

    // Fill everything, so a buffer sits in every region.
    let mut live = Vec::new();
    while let Ok(buffer) = arena.alloc(MIB as usize, Tier::Host) {
        live.push(buffer);
    }
    assert_eq!(live.len(), 16);

    // Ask for less than a region: the newest region is marked, whole.
    let marked = arena.shrink(MIB);
    assert_eq!(marked, 4 * MIB);
    assert_eq!(arena.draining_bytes(), 4 * MIB);
    assert_eq!(arena.host_capacity(), 12 * MIB);
    assert_eq!(
        arena.region_bytes(Tier::Host),
        16 * MIB,
        "a draining region is still resident until its buffers come home"
    );

    // Free one buffer from every region: only the older regions serve it again.
    let freed_newest = live.pop().expect("a buffer in the newest region");
    drop(freed_newest);
    assert!(
        arena.alloc(MIB as usize, Tier::Host).is_err(),
        "the slot freed in the draining region is not handed out again"
    );
    let from_base = live.remove(0);
    drop(from_base);
    let again = arena
        .alloc(MIB as usize, Tier::Host)
        .expect("the base region serves");
    live.insert(0, again);

    // Asking for more than every grown region marks them all and never the base.
    let marked = arena.shrink(64 * MIB);
    assert_eq!(marked, 4 * MIB, "only the one grown region left to mark");
    assert_eq!(arena.draining_bytes(), 8 * MIB);
    assert_eq!(arena.host_capacity(), 8 * MIB);
    assert_eq!(arena.shrink(64 * MIB), 0, "nothing left that can drain");

    // An empty grown region is unmapped at the shrink that marks it.
    let fresh = common::host_arena(8 * MIB);
    fresh.grow(2 * MIB).expect("grow");
    assert_eq!(fresh.shrink(2 * MIB), 2 * MIB);
    let stats = fresh.arena_stats();
    assert_eq!(
        stats.host_regions, 1,
        "the empty region was unmapped at once"
    );
    assert_eq!(stats.draining_bytes, 0);
    assert_eq!(stats.retired_regions, 1);
    assert_eq!(stats.retired_bytes, 2 * MIB);
    drop(live);
}

/// drain_then_unmap. A draining region is unmapped when its last buffer is released,
/// including the second half of a split buffer, and not before; `draining_bytes` reaches zero at
/// that moment, which is how the facade knows the drain is complete.
#[test]
fn drain_then_unmap() {
    let _gate = gate();
    let arena = common::host_arena(4 * MIB);
    let base = arena
        .alloc((4 * MIB) as usize, Tier::Host)
        .expect("fills the base region");
    arena.grow(2 * MIB).expect("grow");
    let grown = arena
        .alloc((2 * MIB) as usize, Tier::Host)
        .expect("in the grown region");
    let (left, right) = grown.split_at(MIB as usize);
    assert_eq!(arena.shrink(2 * MIB), 2 * MIB);
    assert_eq!(arena.arena_stats().host_regions, 2);

    drop(left);
    assert_eq!(
        arena.draining_bytes(),
        2 * MIB,
        "half the slot is still live, so the region stays mapped"
    );
    drop(right);
    let stats = arena.arena_stats();
    assert_eq!(stats.draining_bytes, 0, "the drain is complete");
    assert_eq!(stats.host_regions, 1);
    assert_eq!(stats.retired_regions, 1);
    assert_eq!(stats.double_release, 0);
    assert_eq!(arena.region_bytes(Tier::Host), 4 * MIB);

    // Growing again after a drain works as the first grow did.
    drop(base);
    assert_eq!(arena.grow(2 * MIB).expect("grow again"), 2 * MIB);
    assert_eq!(arena.host_capacity(), 6 * MIB);
    let buffers: Vec<_> = (0..6)
        .map(|_| arena.alloc(MIB as usize, Tier::Host).expect("served"))
        .collect();
    assert_eq!(arena.stats().host_in_use, 6 * MIB);
    drop(buffers);
}

/// AR-I6 under growth: a pinned arena grows pinned regions only, and an unpinned one never locks
/// a grown region. The pinned half needs memlock, which a development host usually allows for a
/// few MiB; where it does not, the arena was never pinned and the check is the unpinned one.
#[test]
fn grown_regions_keep_the_one_host_tier() {
    let _gate = gate();
    let pinned = Arena::new(ArenaConfig {
        host_bytes: 2 * MIB,
        host_tier: TierKind::PinnedHost,
        device_bytes: Vec::new(),
        page_bytes: 4096,
        huge_pages: Guarantee::Probed(false),
        memlock: Guarantee::Probed(true),
        register_rdma: false,
    })
    .expect("arena");
    let tier = if pinned.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    };
    let _full = pinned.alloc((2 * MIB) as usize, tier).expect("base");
    match pinned.grow(2 * MIB) {
        Ok(added) => {
            assert_eq!(added, 2 * MIB);
            let buffer = pinned.alloc(MIB as usize, tier).expect("grown region");
            assert_eq!(buffer.tier(), tier, "one host tier for the run (AR-I6)");
            let other = if tier == Tier::Host {
                Tier::PinnedHost
            } else {
                Tier::Host
            };
            assert!(pinned.alloc(MIB as usize, other).is_err());
        }
        Err(error) => {
            // A lock limit that allowed the first region but not the second: refused, not added
            // unlocked.
            assert!(matches!(
                error,
                MorunaError::Config {
                    name: "arena.pin",
                    ..
                }
            ));
            assert_eq!(pinned.arena_stats().host_regions, 1);
        }
    }
    let _ = Arc::strong_count(&pinned);
}
