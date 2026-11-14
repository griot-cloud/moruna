//! RC-T10 profile_roundtrip. The profile store is the controller's memory across runs: a
//! completed run writes what it measured, a second run of the same kernel merges into it rather
//! than replacing it, a file that cannot be read is ignored with a note instead of failing the
//! run, and a probe that disagrees with the stored probe by more than half is drift, which the
//! probe wins. A repeat run of a kernel the store has seen enough of is sized from it and not
//! probed (RC-T10b). Proves e.3, f.2 and f.9.
//!
//! The store is written under a scratch directory named for this process, because several
//! gates run on one machine at a time (preamble 6.7).

mod common;

use common::{GIB, MIB, Scratch, config, kernel, probe, record, steady};
use moruna_kernel::{Fingerprint, KernelHints, SchedulerStats, StageStats};
use moruna_testkit::{FakeKnobs, FakeSampler};
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
    complete_run_seen(dir, amplification, records);
}

/// `complete_run`, giving back the probes the controller asked for and the run's notes.
fn complete_run_seen(
    dir: &std::path::Path,
    amplification: f64,
    records: u64,
) -> (Vec<(u16, u64)>, Vec<String>) {
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
    let summary = rig.controller.stop();
    (rig.prober.calls(), summary.notes)
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
        "prediction_error_p95": 0.1,
        "probe_a_anon": 1.0
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

/// A stored row for kernel 1 under the given input schema, as `moruna check` writes one: the
/// controller's fields plus the harness's own, which the controller ignores.
fn check_row(dir: &std::path::Path, schema: &str, a_k: f64) -> std::path::PathBuf {
    let fingerprint = Fingerprint::compute("test kernel 1", b"");
    let row = serde_json::json!({
        "version": 1,
        "fingerprint": fingerprint.to_hex(),
        "schema_hash": schema,
        "updated": "2026-09-26T00:00:00Z",
        "a_k_p50": a_k,
        "a_k_p95": a_k,
        "a_k_dev_p95": 0.0,
        "a_k_samples": 40_000,
        "a_k_var": 0.0,
        "state_bytes_max": 0,
        "final_target": 64 * MIB,
        "final_workers": 4,
        "final_safety": 1.2,
        "runs": 1,
        "prediction_error_p95": 0.0,
        "probe_a_anon": a_k,
        "wall_ns_per_row": 12.5,
        "gil": "none"
    });
    let path = dir.join(format!("{}-{schema}.json", fingerprint.to_hex()));
    std::fs::write(&path, serde_json::to_string(&row).expect("json")).expect("write");
    path
}

fn external_state_profile(dir: &std::path::Path, state_bytes: u64) -> common::Rig {
    let file = check_row(dir, &"01".repeat(32), 1.0);
    let mut row: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    row["state_bytes_max"] = state_bytes.into();
    std::fs::write(file, serde_json::to_vec(&row).unwrap()).unwrap();

    let mut cfg = config(4 * GIB, 2);
    cfg.arena_bytes = GIB;
    cfg.baseline_bytes = 64 * MIB;
    cfg.profiles_dir = Some(dir.to_path_buf());
    cfg.plan.total_bytes = MIB;
    cfg.plan.total_rows = 1000;
    let rig = common::Rig::new(
        cfg,
        vec![common::stateful_kernel(1, 1, KernelHints::default())],
        FakeKnobs::new(),
        FakeSampler::new().scripted(steady(GIB + 64 * MIB, 8)),
    );
    rig.controller.set_external_state(&[1]).unwrap();
    rig
}

#[test]
fn rc_t10_external_profile_state_does_not_spend_the_placement_arena() {
    let scratch = Scratch::new("external-state-profile");
    let rig = external_state_profile(scratch.path(), 2 * GIB);
    assert!(rig.controller.set_external_state(&[0]).is_err());
    assert!(rig.controller.set_external_state(&[2]).is_err());
    rig.run_up();
    assert!(rig.controller.set_external_state(&[]).is_err());
    let budgets = common::last_budgets(&rig.placement).unwrap();
    assert!(
        budgets.host > 0,
        "state outside the arena must leave room for queued batches"
    );
    assert!(budgets.host < GIB, "placement remains bounded by its arena");
    rig.controller.stop();
}

#[test]
fn rc_t10_external_profile_state_is_still_funded_below_the_run_ceiling() {
    let scratch = Scratch::new("external-state-ceiling");
    let rig = external_state_profile(scratch.path(), 4 * GIB);
    rig.controller.prepare().unwrap();
    rig.controller.probe_all().unwrap();
    let error = rig.controller.start().unwrap_err();
    assert!(error.to_string().contains("budget cannot hold this job"));
    rig.controller.stop();
}

/// The notes of a run of kernel 1 over the store, probed at `probed`.
fn seeded_notes(dir: &std::path::Path, probed: f64) -> Vec<String> {
    let mut cfg = config(8 * GIB, 2);
    cfg.profiles_dir = Some(dir.to_path_buf());
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, probed)),
        FakeSampler::new().scripted(steady(400 * MIB, 8)),
    );
    rig.run_up();
    rig.controller.stop().notes
}

/// A kernel checked against a declared column subset has its first row under that subset's
/// schema. A later run of the same kernel over a wider source has no exact row, so the newest row
/// with the same fingerprint seeds it: the probe is judged against it (drift when they disagree),
/// and a note names the schema it came from. An exact row, when there is one, wins.
#[test]
fn rc_t10_a_subset_check_row_seeds_a_wider_run() {
    let scratch = Scratch::new("profile-subset");
    let subset: String = "ab".repeat(32);
    check_row(scratch.path(), &subset, 1.0);

    let notes = seeded_notes(scratch.path(), 1.0);
    assert!(
        notes.iter().any(
            |note| note.contains("seeded from the same kernel's row") && note.contains(&subset)
        ),
        "the subset row seeds the wider run: {notes:?}"
    );
    let notes = seeded_notes(scratch.path(), 4.0);
    assert!(
        notes.iter().any(|note| note == "profile drift on stage 1"),
        "the probe is judged against the subset row, which proves it was loaded: {notes:?}"
    );

    // Of two rows under other schemas, the newest seeds.
    let older = check_row(scratch.path(), &"cd".repeat(32), 1.0);
    let old = std::fs::File::options()
        .write(true)
        .open(&older)
        .expect("open");
    old.set_modified(std::time::SystemTime::UNIX_EPOCH)
        .expect("backdate");
    let notes = seeded_notes(scratch.path(), 1.0);
    assert!(
        notes.iter().any(|note| note.contains(&subset)),
        "the newest row seeds: {notes:?}"
    );

    // An exact row wins over any other, and no note is made.
    let exact = profile_path(scratch.path(), 1);
    let exact_schema: String = (0..32).map(|_| "01".to_string()).collect();
    check_row(scratch.path(), &exact_schema, 1.0);
    assert!(exact.exists());
    let notes = seeded_notes(scratch.path(), 1.0);
    assert!(
        !notes
            .iter()
            .any(|note| note.contains("seeded from the same kernel's row")),
        "the exact row is used: {notes:?}"
    );
}

/// RC-T10b (f.2, RC-I6, 2026-11-14): a run of a kernel a completed run measured over the same
/// input schema, from enough records, starts from what that run learned: no probe, a note that
/// says so, and no drift. The figures the solve needs are in the file for it.
#[test]
fn rc_t10b_a_repeat_run_is_sized_from_what_the_last_one_learned() {
    let scratch = Scratch::new("profile-learned");
    let (first_probes, first_notes) = complete_run_seen(scratch.path(), 4.0, 40);
    assert_eq!(first_probes.len(), 1, "the first run measures itself");
    assert!(
        !first_notes.iter().any(|n| n.contains("not probed")),
        "{first_notes:?}"
    );
    let stored: Value = serde_json::from_str(
        &std::fs::read_to_string(profile_path(scratch.path(), 1)).expect("written"),
    )
    .expect("json");
    for figure in ["a_k_arena", "a_anon", "probe_a_anon"] {
        assert!(
            stored[figure].as_f64().unwrap_or(0.0) > 0.0,
            "e.3: the profile keeps {figure}: {stored}"
        );
    }

    let (probes, notes) = complete_run_seen(scratch.path(), 4.0, 40);
    assert!(probes.is_empty(), "a repeat is not probed: {probes:?}");
    assert!(
        notes
            .iter()
            .any(|n| n == "stage 1 sized from 1 earlier runs (40 records); not probed"),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("drift")),
        "a repeat is not drift: {notes:?}"
    );
    let merged: Value = serde_json::from_str(
        &std::fs::read_to_string(profile_path(scratch.path(), 1)).expect("written"),
    )
    .expect("json");
    assert_eq!(merged["runs"], 2);
    assert_eq!(
        merged["probe_a_anon"], stored["probe_a_anon"],
        "a run that did not probe carries the probe's figure forward"
    );
}

/// RC-T10b: too little evidence is not trusted; the run probes, and an identical probe is not
/// drift (it was, compared with the records' steady-state figure).
#[test]
fn rc_t10b_too_little_evidence_is_probed_and_an_identical_probe_is_not_drift() {
    let scratch = Scratch::new("profile-thin");
    complete_run_seen(scratch.path(), 4.0, 8);
    let (probes, notes) = complete_run_seen(scratch.path(), 4.0, 8);
    assert_eq!(probes.len(), 1, "eight records do not stand in for a probe");
    assert!(!notes.iter().any(|n| n.contains("drift")), "{notes:?}");
}
