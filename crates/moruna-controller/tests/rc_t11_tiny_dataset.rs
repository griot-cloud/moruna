//! RC-T11 tiny_dataset. A dataset that fits four times over in the budget has nothing to adapt
//! to: there is no probe, the morsel target is the largest the anon inequality of f.3 allows, the
//! read-ahead is the whole plan up to eight splits, and the report says so. Proves f.10.
//!
//! `morsel_max` is not the target here and f.10 never promised it would be: the path skips the
//! probe and the adaptation, not RC-I1. With no probe the amplification is the 4.0 default, so
//! one 512 MiB morsel is charged 3 GiB of anonymous memory against the 1.6 GiB this ceiling
//! leaves above the arena, and the largest target that fits on all eight workers is 34 MiB. The
//! run that made this explicit reached 1.11 x its ceiling on a tiny dataset with every worker
//! running at the maximum morsel (PM, 2026-09-23).

mod common;

use common::{GIB, MIB, active_workers, config, kernel, morsel_targets, read_aheads, steady};
use moruna_controller::PlanSummary;
use moruna_kernel::KernelHints;
use moruna_testkit::{FakeKnobs, FakeSampler};

#[test]
fn rc_t11_tiny_dataset() {
    let mut cfg = config(16 * GIB, 8);
    cfg.plan = PlanSummary {
        total_bytes: GIB,
        total_rows: 10_000_000,
        splits: 4,
        max_split_bytes: 256 * MIB,
        sub_splittable_all: true,
    };
    let morsel_max = cfg.morsel_max;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new(),
        FakeSampler::new().scripted(steady(400 * MIB, 4)),
    );
    rig.run_up();

    assert!(
        rig.prober.calls().is_empty(),
        "f.10: a tiny dataset is not probed"
    );
    let writes = rig.writes();
    // 1.6 GiB of headroom above the arena, eight workers on one stage, the 4.0 default
    // amplification and the 1.5 initial safety: `1.6 GiB / (8 x 4 x 1.5)`.
    let allowed = 1_717_986_918u64 / 48;
    assert_eq!(
        morsel_targets(&writes),
        vec![(1, allowed)],
        "f.10: every target is the largest the anon inequality allows, not morsel_max {morsel_max}"
    );
    assert_eq!(
        active_workers(&writes).last().copied(),
        Some(8),
        "f.10: capping the target keeps the workers; capping the workers would have left one"
    );
    assert_eq!(
        read_aheads(&writes).last().copied(),
        Some(4),
        "f.10: the read-ahead is min(splits, 8)"
    );
    let summary = rig.controller.stop();
    assert!(
        summary
            .notes
            .iter()
            .any(|note| note == "small dataset: no adaptation"),
        "f.10: the note is in the report, {:?}",
        summary.notes
    );
}
