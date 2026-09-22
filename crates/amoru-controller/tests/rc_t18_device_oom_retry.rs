//! RC-T18 device_oom_retry. A device allocation failure halves the failing stage and takes the
//! input queue's device high water to zero inside `on_record`, so that the scheduler's one
//! retry has somewhere to allocate; the next tick puts the high water back; a second failure on
//! the same stage is a device budget the stage cannot be sized into, and says so. Proves f.11
//! (architecture 7).

mod common;

use amoru_kernel::{KernelHints, Outcome, TierKind};
use amoru_testkit::{FakeKnobs, FakeSampler};
use common::{
    GIB, MIB, config, high_waters, kernel, limits_with_device, morsel_targets, probe, record,
    steady,
};

#[test]
fn rc_t18_device_oom_retry() {
    let mut cfg = config(8 * GIB, 4);
    cfg.limits = limits_with_device(8 * GIB, 8 * GIB);
    let probe_bytes = cfg.probe_bytes;
    let knobs = FakeKnobs::new()
        .probe_result(1, probe(probe_bytes, 2.0))
        .probe_result(2, probe(probe_bytes, 2.0))
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
        FakeSampler::new().scripted(steady(400 * MIB, 8)),
    );
    rig.run_up();
    let before = morsel_targets(&rig.writes())
        .into_iter()
        .find(|(stage, _)| *stage == 3)
        .map(|(_, bytes)| bytes)
        .expect("stage 3 has a target");

    let mut failure = record(1, 3, before, 0);
    failure.outcome = Outcome::Error;
    failure.error = Some("alloc 1 GiB bytes in Device(0)".into());
    failure.dev_mem_peak = GIB;
    let mark = rig.writes().len();
    rig.feed(&failure);

    let fresh = &rig.writes()[mark..];
    assert_eq!(
        morsel_targets(fresh),
        vec![(3, before / 2)],
        "f.11: the failing stage is halved at once"
    );
    assert!(
        high_waters(fresh)
            .iter()
            .any(|(stage, tier, bytes)| *stage == 2 && *tier == TierKind::Device && *bytes == 0),
        "f.11: the input queue's device high water goes to zero so the retry has room"
    );
    assert!(
        rig.knobs.terminated().is_none(),
        "one failure is a retry, not a diagnostic"
    );

    let mark = rig.writes().len();
    rig.controller.tick_once();
    assert!(
        high_waters(&rig.writes()[mark..])
            .iter()
            .any(|(stage, tier, bytes)| *stage == 2 && *tier == TierKind::Device && *bytes > 0),
        "f.11: the next tick restores the device high water"
    );

    let mut second = record(2, 3, before / 2, 0);
    second.outcome = Outcome::Error;
    second.error = Some("alloc 1 GiB bytes in Device(0)".into());
    second.dev_mem_peak = GIB;
    rig.feed(&second);
    let diagnostic = rig.knobs.terminated().expect("a second failure terminates");
    assert!(
        diagnostic.contains("stage 3") && diagnostic.contains(&format!("footprint {GIB}")),
        "f.11: the diagnostic carries the device figures: {diagnostic}"
    );

    let summary = rig.controller.stop();
    assert!(
        summary
            .notes
            .iter()
            .any(|note| note == "device OOM retry on stage 3"),
        "f.11: the note is in the report: {:?}",
        summary.notes
    );
}
