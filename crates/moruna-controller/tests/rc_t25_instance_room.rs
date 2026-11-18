//! RC-T25 (6.3g, system design 3A): a stage's target never passes what one of its instances can
//! take. The scheduler's counters carry the largest morsel the stage's kernel says one instance
//! holds inside its room (piece <= (room - fixed) / per-byte); the controller probes at it, holds
//! every target to it, below `morsel_min` where it is less, and follows it down on the next tick
//! when it falls (a warm-up measured the function). Proves RC-I11.

mod common;

use common::{GIB, MIB, config, kernel, probe, steady};
use moruna_kernel::{KernelHints, SchedulerStats, StageStats};
use moruna_testkit::{FakeKnobs, FakeSampler};

/// One stage's counters, its instances taking at most `cap`.
fn counters(cap: Option<u64>) -> SchedulerStats {
    SchedulerStats {
        per_stage: vec![StageStats {
            stage: 1,
            instances_live: 1,
            morsel_bytes_max: cap,
            ..StageStats::default()
        }],
        workers_active: 2,
        workers_busy: 2,
        ..SchedulerStats::default()
    }
}

/// A run's start over one stage whose instances take at most `cap`: the rig, run up.
fn started(cap: Option<u64>) -> common::Rig {
    let cfg = config(8 * GIB, 2);
    let probe_bytes = cfg.probe_bytes.min(cap.unwrap_or(u64::MAX));
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new()
            .probe_result(1, probe(probe_bytes, 4.0))
            .stats(counters(cap)),
        FakeSampler::new().scripted(steady(400 * MIB, 64)),
    );
    rig.run_up();
    rig
}

#[test]
fn rc_t25_a_target_never_passes_what_an_instance_takes() {
    assert_eq!(
        config(8 * GIB, 2).morsel_min,
        4 * MIB,
        "the cap here is below the least morsel"
    );
    let rig = started(Some(2 * MIB));
    assert_eq!(
        rig.prober.calls(),
        vec![(1, 2 * MIB)],
        "the probe is at the cap, below the least morsel"
    );
    let targets = common::morsel_targets(&rig.writes());
    assert!(!targets.is_empty(), "a target at start");
    assert!(
        targets.iter().all(|(_, bytes)| *bytes <= 2 * MIB),
        "no target past the cap: {targets:?}"
    );
    // The cap falls (a warm-up measured the function): the next tick takes the target to it.
    rig.knobs.set_stats(counters(Some(MIB)));
    rig.controller.tick_once();
    assert_eq!(
        common::morsel_targets(&rig.writes()).last().copied(),
        Some((1, MIB))
    );
    rig.controller.stop();
}

#[test]
fn rc_t25_a_stage_with_no_cap_is_sized_as_before() {
    let uncapped = started(None);
    let far = started(Some(1 << 40));
    assert_eq!(
        common::morsel_targets(&uncapped.writes()),
        common::morsel_targets(&far.writes()),
        "a cap above everything changes nothing"
    );
    assert_eq!(uncapped.prober.calls(), far.prober.calls());
    uncapped.controller.stop();
    far.controller.stop();
}
