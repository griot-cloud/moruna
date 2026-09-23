//! PL-T9 close_drains (PL-I10): after `close`, pushes error and pops return the remaining
//! entries in order, some of them from disk, then `None`; no entry is lost.

mod common;

use moruna_kernel::{Locality, Placement, TierKind};
use moruna_testkit::{FakeAllocator, FakeReactor};

#[test]
fn pl_t9_close_drains() {
    let scratch = common::Scratch::new("t9");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 64);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes * 2, bytes * 3);

    for seq in 0..100u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 64))
            .expect("push");
    }
    common::settle(&reactor);
    let on_disk = engine.detailed_stats().per_queue[0].entries_by_state[4];
    assert!(on_disk > 0, "some entries must be on disk for this test");

    engine.close(0);
    assert!(
        engine
            .push(0, common::table_morsel(&alloc, 1000, 0, 64))
            .is_err(),
        "a push after close errors (PL-I10)"
    );

    let mut popped = Vec::new();
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        popped.push(morsel.seq);
    }
    assert_eq!(popped, (0..100u64).collect::<Vec<_>>(), "all 100, in order");
    assert!(
        engine
            .pop_blocking(0, common::want_host(), Locality::Any)
            .expect("pop_blocking")
            .is_none(),
        "then None"
    );
}
