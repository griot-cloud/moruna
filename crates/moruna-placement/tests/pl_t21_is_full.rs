//! PL-T21 is_full (f.14): every case the scheduler's admission rule reads.

mod common;

use moruna_kernel::{Locality, Placement, TierKind};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

#[test]
fn pl_t21_is_full() {
    let scratch = common::Scratch::new("t21");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(std::time::Duration::from_millis(30));
    let cfg = common::config(
        2,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    let sample = common::table_morsel(&alloc, 0, 1, 128);
    let bytes = sample.bytes;
    drop(sample);

    // Below high water: not full.
    engine.set_water(1, TierKind::Host, bytes * 8, bytes * 16);
    engine
        .push(1, common::table_morsel(&alloc, 0, 1, 128))
        .expect("push");
    assert!(!engine.is_full(1), "below high water");

    // Above high water with staging on and demotions in flight: draining, not full. Staging is
    // asked for: a queue's own high water writes only then (2026-11-14).
    engine.set_staging(1, true);
    engine.set_promotion_window(1, 1);
    engine.set_water(1, TierKind::Host, bytes, bytes);
    for seq in 1..6u64 {
        engine
            .push(1, common::table_morsel(&alloc, seq, 1, 128))
            .expect("push");
    }
    assert!(
        reactor.in_flight() > 0,
        "demotions are in flight for this case"
    );
    assert!(
        !engine.is_full(1),
        "a queue above high water with demotions in flight is draining, not full (f.14)"
    );
    common::settle(&reactor);

    // Staging off and the entries are not recomputable: no demotion is possible, so full.
    engine.set_staging(1, false);
    for seq in 6..12u64 {
        engine
            .push(1, common::table_morsel(&alloc, seq, 1, 128))
            .expect("push");
    }
    common::settle(&reactor);
    assert!(
        engine.is_full(1),
        "staging off and nothing recomputable: full (PL-I6, f.14)"
    );

    // Staging off on Q0, whose entries are recomputable: its own high water holds the reads back
    // rather than dropping decoded batches to read and decode them again (2026-11-14); it
    // writes nothing either way (PL-I6). A tier short of memory still drops them (`shed`).
    let before = reactor
        .ops()
        .iter()
        .filter(|op| op.kind == OpKind::WriteFile)
        .count();
    engine.set_promotion_window(0, 1);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    for seq in 0..6u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 128))
            .expect("push");
    }
    common::settle(&reactor);
    assert!(
        engine.is_full(0),
        "Q0 at its high water holds the reads back"
    );
    assert_eq!(engine.detailed_stats().per_queue[0].evictions, 0);
    assert!(
        moruna_kernel::Residency::shed(&*engine, TierKind::Host, bytes * 2),
        "memory short: Q0 drops"
    );
    assert!(engine.detailed_stats().per_queue[0].evictions > 0);
    assert_eq!(
        reactor
            .ops()
            .iter()
            .filter(|op| op.kind == OpKind::WriteFile)
            .count(),
        before,
        "and writes nothing"
    );

    // After `close`: full.
    engine.close(0);
    assert!(
        engine.is_full(0),
        "a closed queue takes no more work (f.14)"
    );
}

#[test]
fn pl_t21_a_head_error_holds_until_the_pop_that_takes_it() {
    let scratch = common::Scratch::new("t21b");
    let alloc = FakeAllocator::new().pinned(true);
    let reactor = FakeReactor::new().fail_next(OpKind::Copy, 2);
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 1 << 30, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_consumer(0, common::want_device());
    engine
        .push(0, common::tensor_morsel(&alloc, 0, 0, 512))
        .expect("push");
    common::settle(&reactor);
    assert!(
        engine.is_full(0),
        "a head error makes the queue full (f.14)"
    );
    assert!(engine.detailed_stats().per_queue[0].head_error);
    assert!(engine.pop(0, common::want_device(), Locality::Any).is_err());
    assert!(!engine.is_full(0), "the error is taken by that pop");
    assert!(!engine.detailed_stats().per_queue[0].head_error);
}

#[test]
fn pl_t21_a_stage_outside_the_run_is_full() {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(1, None, common::budgets(1 << 30, 0, 0));
    let engine = common::engine(cfg, &alloc, &reactor);
    assert!(
        engine.is_full(7),
        "a stage that does not exist takes no work"
    );
    assert!(engine.evicted(7).is_empty());
    assert!(!engine.peek_resident(7, common::want_host(), Locality::Any));
    assert!(engine.pop(7, common::want_host(), Locality::Any).is_err());
}

/// PL-T21 (b) (2026-11-14): a kernel output's queue over its high water, with a disk tier and
/// staging allowed by default but not asked for, writes nothing and reports full, so its
/// producers are held back; asking for staging makes the same pressure write.
#[test]
fn pl_t21_high_water_holds_producers_back_unless_staging_is_asked() {
    let scratch = common::Scratch::new("t21b");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        2,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_promotion_window(1, 1);
    let sample = common::table_morsel(&alloc, 0, 1, 128);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(1, TierKind::Host, bytes, bytes);
    for seq in 0..4u64 {
        engine
            .push(1, common::table_morsel(&alloc, seq, 1, 128))
            .expect("push");
    }
    common::settle(&reactor);
    let writes = |reactor: &FakeReactor| {
        reactor
            .ops()
            .into_iter()
            .filter(|op| op.kind == moruna_testkit::OpKind::WriteFile)
            .count()
    };
    assert_eq!(writes(&reactor), 0, "no batch went to disk");
    assert!(engine.is_full(1), "the producers are held back (f.14)");

    engine.set_staging(1, true);
    common::settle(&reactor);
    assert!(writes(&reactor) > 0, "asked for, the same pressure writes");
}
