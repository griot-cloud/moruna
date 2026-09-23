//! RC-T7 workers_bounded_by_memory. A large amplification against a small budget leaves room
//! for fewer in-flight morsels than there are workers, and the worker count follows the memory
//! and not the CPU count. Three stages with three different amplifications receive the one
//! common target f.3 solves for, clamped into the morsel range. Proves RC-I7 and f.3.

mod common;

use common::{GIB, MIB, active_workers, config, kernel, morsel_targets, probe, steady};
use moruna_kernel::KernelHints;
use moruna_testkit::{FakeKnobs, FakeSampler};

#[test]
fn rc_t7_workers_bounded_by_memory() {
    let cfg = config(2 * GIB, 32);
    let probe_bytes = cfg.probe_bytes;
    let knobs = FakeKnobs::new()
        .probe_result(1, probe(probe_bytes, 20.0))
        .probe_result(2, probe(probe_bytes, 8.0))
        .probe_result(3, probe(probe_bytes, 2.0));
    let kernels = vec![
        kernel(1, KernelHints::default()),
        kernel(2, KernelHints::default()),
        kernel(3, KernelHints::default()),
    ];
    let rig = common::Rig::new(
        cfg,
        kernels,
        knobs,
        FakeSampler::new().scripted(steady(200 * MIB, 4)),
    );
    rig.run_up();

    let writes = rig.writes();
    let active = *active_workers(&writes).last().expect("a worker count");
    assert!(
        active < 32,
        "RC-I7: 32 workers cannot each hold a morsel of this amplification in a 2 GiB budget, \
         but the controller allowed {active}"
    );
    assert!(active >= 1, "at least one worker is always allowed");

    let targets = morsel_targets(&writes);
    assert_eq!(targets.len(), 3, "one target per stage");
    let first = targets[0].1;
    assert!(
        targets.iter().all(|(_, bytes)| *bytes == first),
        "f.3 solves for one common target and clamps it per stage: {targets:?}"
    );
    rig.controller.stop();
}
