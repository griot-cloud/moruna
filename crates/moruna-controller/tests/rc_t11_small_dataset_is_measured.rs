//! RC-T11 small_dataset_is_measured (RB2, 2026-11-13). A dataset that fits four times over in the
//! budget is probed and sized like any other: there is no small-dataset path. Large morsels on
//! every worker, which is what a small dataset gets on a large machine, is what the solve of f.3
//! yields from the measured amplification, not a rule of its own.
//!
//! The path this replaces (f.10) skipped the probe and sized from the 4.0 default; on the
//! benchmark of 2026-11-13 that sent full 1M-row morsels of a model kernel to every worker at
//! once, and every run of it ended at the ceiling.

mod common;

use common::{GIB, MIB, active_workers, config, kernel, morsel_targets, steady};
use moruna_controller::PlanSummary;
use moruna_kernel::KernelHints;
use moruna_testkit::{FakeKnobs, FakeSampler};

#[test]
fn rc_t11_small_dataset_is_measured() {
    let mut cfg = config(16 * GIB, 8);
    cfg.plan = PlanSummary {
        total_bytes: GIB,
        total_rows: 10_000_000,
        splits: 4,
        max_split_bytes: 256 * MIB,
        sub_splittable_all: true,
    };
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new(),
        FakeSampler::new().scripted(steady(400 * MIB, 4)),
    );
    rig.run_up();

    assert!(
        !rig.prober.calls().is_empty(),
        "RB2: a small dataset is probed like any other"
    );
    let writes = rig.writes();
    assert!(
        !morsel_targets(&writes).is_empty(),
        "RB2: the target comes from the solve, {writes:?}"
    );
    assert_eq!(
        active_workers(&writes).last().copied(),
        Some(8),
        "RB2: the measured kernel funds every worker"
    );
    let summary = rig.controller.stop();
    assert!(
        !summary
            .notes
            .iter()
            .any(|note| note.contains("small dataset")),
        "RB2: there is no small-dataset path to report, {:?}",
        summary.notes
    );
}
