//! RC-T10 profile_roundtrip. The profile store is the controller's memory across runs: a
//! completed run writes what it measured, a second run of the same kernel merges into it rather
//! than replacing it, a file that cannot be read is ignored with a note instead of failing the
//! run, and a probe that disagrees with the stored figure by more than half is drift, which the
//! probe wins. Proves e.3, f.2 and f.9.
//!
//! The store is written under a scratch directory named for this process, because several
//! gates run on one machine at a time (preamble 6.7).

mod common;

use moruna_kernel::{Fingerprint, KernelHints, SchedulerStats, StageStats};
use moruna_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, Scratch, config, kernel, probe, record, steady};
use serde_json::Value;

/// The file e.3 names for one kernel: its fingerprint and its input schema hash.
fn profile_path(dir: &std::path::Path, stage: u16) -> std::path::PathBuf {
    let fingerprint = Fingerprint::compute(&format!("test kernel {stage}"), b"");
    let schema: String = (0..32).map(|_| format!("{:02x}", stage as u8)).collect();
    dir.join(format!("{}-{schema}.json", fingerprint.to_hex()))
}

fn exhausted() -> SchedulerStats {
    SchedulerStats {
        per_stage: vec![StageStats {
            stage: 1,
            instances_live: 1,
            ..StageStats::default()
        }],
        workers_active: 2,
        workers_busy: 2,
        source_exhausted: true,
        ..SchedulerStats::default()
    }
}

/// A run that completes, over the given store, reporting the given amplification.
fn complete_run(dir: &std::path::Path, amplification: f64, records: u64) {
    let mut cfg = config(8 * GIB, 2);
    cfg.profiles_dir = Some(dir.to_path_buf());
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new()
            .probe_result(1, probe(probe_bytes, amplification))
            .stats(exhausted()),
        FakeSampler::new().scripted(steady(400 * MIB, 64)),
    );
    rig.run_up();
    for seq in 1..=records {
        let bytes = 8 * MIB;
        rig.feed(&record(
            seq,
            1,
            bytes,
            (bytes as f64 * amplification) as u64,
        ));
    }
    rig.controller.tick_once();
    rig.controller.stop();
}

#[test]
fn rc_t10_write_then_merge() {
    let scratch = Scratch::new("profile-merge");
    complete_run(scratch.path(), 4.0, 8);

    let file = profile_path(scratch.path(), 1);
    let first: Value = serde_json::from_str(
        &std::fs::read_to_string(&file).expect("f.9: a completed run writes its profile"),
    )
    .expect("the profile is JSON");
    assert_eq!(first["version"], 1, "e.3: the format version");
    assert_eq!(first["runs"], 1, "e.3: one run so far");
    assert_eq!(
        first["a_k_samples"], 8,
        "e.3: the records the figures came from"
    );
    let p50 = first["a_k_p50"].as_f64().expect("a median");
    assert!(
        (p50 - 4.0).abs() < 0.01,
        "e.3: the median ratio is what was measured, got {p50}"
    );
    assert!(
        first["updated"].as_str().unwrap_or("").ends_with('Z'),
        "e.3: an RFC 3339 timestamp in UTC: {}",
        first["updated"]
    );

    // A second run of the same kernel, measuring twice the amplification, moves the stored
    // figure by the weight e.3 fixes rather than replacing it.
    complete_run(scratch.path(), 8.0, 8);
    let second: Value = serde_json::from_str(&std::fs::read_to_string(&file).expect("still there"))
        .expect("still JSON");
    assert_eq!(second["runs"], 2, "e.3: two runs now");
    assert_eq!(
        second["a_k_samples"], 16,
        "e.3: evidence adds up across runs"
    );
    let merged = second["a_k_p50"].as_f64().expect("a median");
    let expected = 4.0 * 0.7 + 8.0 * 0.3;
    assert!(
        (merged - expected).abs() < 0.05,
        "e.3: the merge gives the new run a weight of 0.3: expected about {expected}, got {merged}"
    );
}

#[test]
fn rc_t10_corrupt_file_is_ignored() {
    let scratch = Scratch::new("profile-corrupt");
    std::fs::write(profile_path(scratch.path(), 1), "{ this is not json").expect("write");

    let mut cfg = config(8 * GIB, 2);
    cfg.profiles_dir = Some(scratch.path().to_path_buf());
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 8)),
    );
    rig.run_up();
    let summary = rig.controller.stop();
    assert!(
        summary
            .notes
            .iter()
            .any(|note| note.starts_with("profile for stage 1 ignored")),
        "h: a corrupt profile is a note, not a failed run: {:?}",
        summary.notes
    );
}

#[test]
fn rc_t10_drift_is_detected() {
    let scratch = Scratch::new("profile-drift");
    // A stored kernel that used to amplify by one, probed today at four.
    let stored = serde_json::json!({
        "version": 1,
        "fingerprint": "",
        "schema_hash": "",
        "updated": "2026-09-22T00:00:00Z",
        "a_k_p50": 1.0,
        "a_k_p95": 1.0,
        "a_k_dev_p95": 0.0,
        "a_k_samples": 40_000,
        "a_k_var": 0.0,
        "state_bytes_max": 0,
        "final_target": 64 * MIB,
        "final_workers": 4,
        "final_safety": 1.2,
        "runs": 3,
        "prediction_error_p95": 0.1
    });
    std::fs::write(
        profile_path(scratch.path(), 1),
        serde_json::to_string(&stored).expect("json"),
    )
    .expect("write");

    let mut cfg = config(8 * GIB, 2);
    cfg.profiles_dir = Some(scratch.path().to_path_buf());
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 8)),
    );
    rig.run_up();
    let summary = rig.controller.stop();
    assert!(
        summary
            .notes
            .iter()
            .any(|note| note == "profile drift on stage 1"),
        "f.2: a probe more than half away from the profile is drift: {:?}",
        summary.notes
    );
}
