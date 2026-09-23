//! PL-T6 disk_bounded (PL-I7, f.14): staging bytes never exceed the disk budget, and when a
//! non-recomputable queue runs out of room the queue reports full and the next `pop` carries
//! the failure with the totals.

mod common;

use moruna_kernel::{Locality, MorunaError, Placement, TierKind};
use moruna_testkit::{FakeAllocator, FakeReactor};

#[test]
fn pl_t6_disk_bounded() {
    let segment = 64 * 1024u64;
    let budget = 2 * segment;
    let scratch = common::Scratch::new("t6");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        2,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, budget),
    );
    cfg.segment_bytes = segment;
    cfg.disk_budget = budget;
    let engine = common::engine(cfg, &alloc, &reactor);
    // Stage 1 is a kernel output: not recomputable, and staging is on by default (b).
    engine.set_promotion_window(1, 1);
    let sample = common::table_morsel(&alloc, 0, 1, 512);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(1, TierKind::Host, bytes, bytes);

    let mut highest = 0u64;
    for seq in 0..64u64 {
        engine
            .push(1, common::table_morsel(&alloc, seq, 1, 512))
            .expect("push");
        common::settle(&reactor);
        highest = highest.max(engine.detailed_stats().disk_bytes);
        assert!(
            engine.detailed_stats().disk_bytes <= budget,
            "staging bytes exceeded the disk budget (PL-I7)"
        );
        if engine.is_full(1) {
            break;
        }
    }
    let detail = engine.detailed_stats();
    assert_eq!(detail.segments_live, 2, "two segments opened, and no third");
    assert_eq!(
        detail.disk_bytes, budget,
        "both are charged in full (PL-I7)"
    );
    assert_eq!(highest, budget, "and the charge never went above it");
    assert!(engine.is_full(1), "the queue reports full (f.14)");

    let error = engine
        .pop(1, common::want_host(), Locality::Any)
        .expect_err("the next pop carries the failure (f.10)");
    let text = error.to_string();
    assert!(matches!(error, MorunaError::Staging(_)), "got {text}");
    assert!(text.contains(&budget.to_string()), "the budget: {text}");
    assert!(
        text.contains(&segment.to_string()),
        "the segment size: {text}"
    );
    assert!(
        text.contains(&detail.disk_bytes.to_string()),
        "the bytes held: {text}"
    );
}

#[test]
#[ignore = "integration, closes in wave 3: `du` over a real staging directory needs the real reactor"]
fn pl_t6_disk_bounded_on_disk() {
    // The same run against the real reactor, with `du` on the staging directory never
    // exceeding the disk budget. It needs `moruna-reactor`, which this component does not
    // depend on (d.2), so it closes when wave 3's components are wired together.
}

#[test]
fn pl_t6_a_recomputable_entry_is_evicted_at_the_disk_bound() {
    // f.2 step 4: when no segment can open, a recomputable entry is evicted rather than
    // failing the run, and the queue keeps taking work (PL-I6, PL-I7).
    let segment = 64 * 1024u64;
    let budget = segment;
    let scratch = common::Scratch::new("t6c");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, budget),
    );
    cfg.segment_bytes = segment;
    cfg.disk_budget = budget;
    let engine = common::engine(cfg, &alloc, &reactor);
    // Stage 0 is recomputable, and staging is turned on so demotion writes until it cannot.
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 512);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    for seq in 0..40u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 512))
            .expect("push");
        common::settle(&reactor);
        assert!(
            engine.detailed_stats().disk_bytes <= budget,
            "staging never exceeds the budget (PL-I7)"
        );
    }
    let detail = engine.detailed_stats();
    assert_eq!(
        detail.disk_bytes, budget,
        "the one segment is charged in full"
    );
    assert!(
        detail.per_queue[0].evictions > 0,
        "the disk bound evicted a recomputable entry instead of failing (f.2 step 4)"
    );
    assert!(!engine.is_full(0), "and Q0 keeps taking work");
    assert!(
        !engine.evicted(0).is_empty(),
        "the scheduler is told what to re-read"
    );
}
