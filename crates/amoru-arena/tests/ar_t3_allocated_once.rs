//! AR-T3 allocated_once (02 k): a counting shim around `mmap`/`mlock` (feature
//! `test-shim`, which is on by default so this test runs in the standard gate) sees exactly
//! one `mmap` and at most one `mlock` per run. AR-I3.
//!
//! This is a test binary of its own so that the process-wide counters describe one arena.

mod common;

use amoru_kernel::{Allocator, Tier};

#[test]
fn ar_t3_allocated_once() {
    let before = amoru_arena::region::syscall_counts();
    assert_eq!(before.mmap, 0, "this binary reserves exactly one region");

    let arena = common::host_arena(64 << 20);
    let after_new = amoru_arena::region::syscall_counts();
    assert_eq!(after_new.mmap, 1, "one mmap for the run (AR-I3)");
    assert!(
        after_new.mlock <= 1,
        "at most one mlock for the run (AR-I3)"
    );

    // Every allocation after `new` is served from the region: no syscall, whatever the mix
    // of classes, slab claims and releases.
    let mut live = Vec::new();
    for i in 0..1_000u64 {
        live.push(
            arena
                .alloc((1 + i % 4096) as usize, Tier::Host)
                .expect("buffer"),
        );
        if live.len() == 64 {
            live.clear();
        }
    }
    let after_alloc = amoru_arena::region::syscall_counts();
    assert_eq!(
        after_alloc, after_new,
        "no syscall after Arena::new (AR-I3)"
    );
}
