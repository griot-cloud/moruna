//! Stream P (2026-11-17): the controller's fit and its decisions are in the run's report. A run
//! of the NYC gate sat seven seconds at bottleneck Idle and its report could not say which knob
//! held it: no anon fit, no worker bound, no decision was left behind. The summary's notes now
//! carry each stage's fit, what bounds the workers, and the knobs moved, in order.

mod common;

use common::{GIB, MIB, config, kernel, steady};
use moruna_controller::PlanSummary;
use moruna_kernel::KernelHints;
use moruna_testkit::{FakeKnobs, FakeSampler};

#[test]
fn rc_p_the_report_says_the_fit_the_worker_bounds_and_the_decisions() {
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
    let notes = rig.controller.stop().notes;
    let find = |prefix: &str| {
        notes
            .iter()
            .find(|n| n.starts_with(prefix))
            .cloned()
            .unwrap_or_else(|| panic!("no note `{prefix}`: {notes:?}"))
    };
    let stage = find("controller stage 1:");
    for part in [
        "anon fit",
        "bytes fixed plus",
        "a morsel byte",
        "morsel target",
        "safety",
    ] {
        assert!(stage.contains(part), "{part}: {stage}");
    }
    let workers = find("controller workers:");
    for part in [
        "active at the end",
        "the CPU allows",
        "the arena's half",
        "the anon fit",
        "breach line",
    ] {
        assert!(workers.contains(part), "{part}: {workers}");
    }
    let decisions = find("controller decisions:");
    assert!(decisions.contains("workers "), "{decisions}");
    assert!(decisions.contains("stage 1 morsel "), "{decisions}");
    assert!(
        decisions.contains(" s "),
        "each decision at its second: {decisions}"
    );
}
