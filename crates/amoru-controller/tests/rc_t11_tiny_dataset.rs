//! RC-T11 tiny_dataset. A dataset that fits four times over in the budget has nothing to adapt
//! to: there is no probe, the morsel target is the largest the configuration allows, the
//! read-ahead is the whole plan up to eight splits, and the report says so. Proves f.10.

mod common;

use amoru_controller::PlanSummary;
use amoru_kernel::KernelHints;
use amoru_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, config, kernel, morsel_targets, read_aheads, steady};

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
    assert_eq!(
        morsel_targets(&writes),
        vec![(1, morsel_max)],
        "f.10: every target is the maximum"
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
