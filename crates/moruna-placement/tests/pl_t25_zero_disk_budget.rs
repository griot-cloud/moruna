//! PL-T25 a_zero_disk_budget_has_no_disk_tier (PL-I7, f.2): `budget.disk = 0` means the run
//! has no disk tier, so no demotion to disk is ever planned and no `Staging` error reaches a
//! `pop`. The queue reports full and the run stalls on admission instead.
//!
//! The defect: a host with no writable staging directory, or a `budget.disk` nobody computed,
//! planned a demotion anyway; the segment roll then refused the charge and the run died mid
//! pass with "staging is at its disk bound: 0 bytes held, budget 0".

mod common;

use moruna_kernel::{MorunaError, Locality, Placement, TierKind};
use moruna_testkit::{FakeAllocator, FakeReactor};

#[test]
fn pl_t25_a_zero_disk_budget_has_no_disk_tier() {
    let scratch = common::Scratch::new("t25");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        2,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 0),
    );
    cfg.segment_bytes = 64 * 1024;
    cfg.disk_budget = 0;
    let engine = common::engine(cfg, &alloc, &reactor);
    // Stage 1 is a kernel output: not recomputable, and staging is on by default (b).
    engine.set_promotion_window(1, 1);
    let sample = common::table_morsel(&alloc, 0, 1, 512);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(1, TierKind::Host, bytes, bytes);

    for seq in 0..32u64 {
        engine
            .push(1, common::table_morsel(&alloc, seq, 1, 512))
            .expect("push");
        common::settle(&reactor);
        let detail = engine.detailed_stats();
        assert_eq!(
            detail.disk_bytes, 0,
            "nothing was staged with no disk budget"
        );
        assert_eq!(detail.segments_live, 0, "no segment was ever opened");
        if engine.is_full(1) {
            break;
        }
    }
    assert!(
        engine.is_full(1),
        "f.14: the queue reports full rather than spilling"
    );

    // And the failure the defect produced never happens: `pop` returns the head, not a
    // `Staging` error carrying a budget of zero.
    match engine.pop(1, common::want_host(), Locality::Any) {
        Ok(_) => {}
        Err(MorunaError::Staging(msg)) => {
            panic!("a zero disk budget failed the run instead of leaving the tier unused: {msg}")
        }
        Err(other) => panic!("unexpected error: {other}"),
    }
}
