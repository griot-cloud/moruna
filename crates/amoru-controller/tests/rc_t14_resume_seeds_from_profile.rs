//! RC-T14 resume_seeds_from_profile. A resumed run does not start again from the probe size:
//! the profile store is the controller's memory of the run that was interrupted, so the stages
//! it covers are seeded from it and only the rest are probed. And because a second crash should
//! not cost what this run has learned either, the store is written while the run is going and
//! not only at the end. Proves f.14 and f.9.

mod common;

use amoru_kernel::{Fingerprint, KernelHints, SchedulerStats, StageStats};
use amoru_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, Scratch, config, kernel, morsel_targets, probe, record, steady};

fn profile_path(dir: &std::path::Path, stage: u16) -> std::path::PathBuf {
    let fingerprint = Fingerprint::compute(&format!("test kernel {stage}"), b"");
    let schema: String = (0..32).map(|_| format!("{:02x}", stage as u8)).collect();
    dir.join(format!("{}-{schema}.json", fingerprint.to_hex()))
}

fn write_profile(dir: &std::path::Path, stage: u16, p95: f64) {
    let stored = serde_json::json!({
        "version": 1,
        "fingerprint": "",
        "schema_hash": "",
        "updated": "2026-09-22T00:00:00Z",
        "a_k_p50": p95,
        "a_k_p95": p95,
        "a_k_dev_p95": 0.0,
        "a_k_samples": 40_000,
        "a_k_var": 0.0,
        "state_bytes_max": 0,
        "final_target": 64 * MIB,
        "final_workers": 4,
        "final_safety": 1.2,
        "runs": 2,
        "prediction_error_p95": 0.1
    });
    std::fs::write(
        profile_path(dir, stage),
        serde_json::to_string(&stored).expect("json"),
    )
    .expect("write");
}

fn running() -> SchedulerStats {
    SchedulerStats {
        per_stage: (1..=3)
            .map(|stage| StageStats {
                stage,
                instances_live: 1,
                ..StageStats::default()
            })
            .collect(),
        workers_active: 4,
        workers_busy: 4,
        resumed: true,
        ..SchedulerStats::default()
    }
}

#[test]
fn rc_t14_resume_seeds_from_profile() {
    let scratch = Scratch::new("resume");
    // The interrupted run got as far as learning stages 1 and 2.
    write_profile(scratch.path(), 1, 3.0);
    write_profile(scratch.path(), 2, 5.0);

    let mut cfg = config(8 * GIB, 4);
    cfg.profiles_dir = Some(scratch.path().to_path_buf());
    cfg.checkpoint_enabled = true;
    cfg.checkpoint_interval_ms = 50;
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![
            kernel(1, KernelHints::default()),
            kernel(2, KernelHints::default()),
            kernel(3, KernelHints::default()),
        ],
        FakeKnobs::new()
            .probe_result(3, probe(probe_bytes, 2.0))
            .stats(running()),
        FakeSampler::new().scripted(steady(400 * MIB, 64)),
    );

    rig.controller.prepare().expect("prepare");
    rig.controller.probe_missing().expect("probe_missing");

    let calls = rig.prober.calls();
    assert_eq!(
        calls.len(),
        1,
        "f.14: only the stage without a profile is probed, got {calls:?}"
    );
    assert_eq!(calls[0].0, 3, "f.14: and it is stage 3");

    rig.controller.start().expect("start");
    let targets = morsel_targets(&rig.writes());
    assert_eq!(targets.len(), 3, "every stage is sized");
    // f.14: the run starts at the size the seeded amplifications imply, not at the probe size.
    assert!(
        targets.iter().all(|(_, bytes)| *bytes != probe_bytes),
        "f.14: a resumed run does not start again at morsel.probe_bytes: {targets:?}"
    );

    // f.9: past fifty records, the store is written while the run is still going.
    let file = profile_path(scratch.path(), 3);
    let _ = std::fs::remove_file(&file);
    let target = targets
        .iter()
        .find(|(stage, _)| *stage == 3)
        .map(|(_, bytes)| *bytes)
        .expect("stage 3 has a target");
    for seq in 1..=51u64 {
        rig.feed(&record(seq, 3, target, target * 2));
    }
    std::thread::sleep(std::time::Duration::from_millis(60));
    rig.controller.tick_once();
    assert!(
        file.exists(),
        "f.9: a run that dies after fifty records keeps what it learned"
    );

    let summary = rig.controller.stop();
    assert!(
        summary
            .notes
            .iter()
            .any(|note| note == "resumed: 2 stages seeded, 1 probed"),
        "f.14: the report says how much was remembered: {:?}",
        summary.notes
    );
}
