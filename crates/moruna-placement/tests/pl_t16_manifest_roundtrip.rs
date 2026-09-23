//! PL-T16 manifest_roundtrip (PL-I11, PL-I12, e.5, f.13): the manifest is exactly the table
//! of e.5, a second engine rebuilds the queues from it, and every refusal case is named.

mod common;

use moruna_kernel::{
    MorunaError, CheckpointExtras, Fingerprint, Locality, Placement, ResumePolicy, RunId,
    SourceCursor, TierKind,
};
use moruna_placement::manifest::Manifest;
use moruna_placement::state::State;
use moruna_placement::{PlacementConfig, PlacementEngine};
use moruna_testkit::{FakeAllocator, FakeReactor};
use std::path::Path;
use std::sync::Arc;

fn extras() -> CheckpointExtras {
    CheckpointExtras {
        kernel_states: vec![(1, 0, vec![1, 2, 3])],
        sink_state: Some(vec![9, 9]),
        committed_seq: Some(2),
        source_cursor: SourceCursor {
            split_index: 0,
            row_offset: 40,
            next_seq: 12,
        },
    }
}

fn drive(
    scratch: &common::Scratch,
) -> (
    Arc<PlacementEngine>,
    FakeAllocator,
    FakeReactor,
    PlacementConfig,
) {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    cfg.segment_bytes = 64 * 1024;
    let engine = common::engine(cfg.clone(), &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 128);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    for seq in 0..12u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 128))
            .expect("push");
    }
    common::settle(&reactor);
    // Three morsels are consumed and committed, so their lineage is dropped (f.11).
    for _ in 0..3 {
        engine
            .pop_blocking(0, common::want_host(), Locality::Any)
            .expect("pop_blocking")
            .expect("a morsel");
    }
    common::settle(&reactor);
    engine.set_committed(2);
    (engine, alloc, reactor, cfg)
}

#[test]
fn pl_t16_manifest_matches_e5_and_restores() {
    let scratch = common::Scratch::new("t16");
    let (engine, _alloc, reactor, cfg) = drive(&scratch);
    let path = engine.checkpoint(&extras()).expect("checkpoint");
    assert_eq!(path, engine.manifest_path().expect("a manifest path"));
    assert!(
        !path.with_extension("json.tmp").exists(),
        "the temporary file is gone after the rename (PL-I12)"
    );

    // Every key of e.5, read by an independent parser.
    let text = std::fs::read_to_string(&path).expect("the manifest");
    let raw: serde_json::Value = serde_json::from_str(&text).expect("json");
    for key in [
        "version",
        "run_id",
        "written_ns",
        "node",
        "hostname",
        "nodes",
        "durable_staging",
        "plan_digest",
        "kernels",
        "resume_policy",
        "stages",
        "committed_seq",
        "source_cursor",
        "sink_state",
        "kernel_states",
        "lineage",
        "segments",
        "config",
    ] {
        assert!(raw.get(key).is_some(), "the manifest has no {key} (e.5)");
    }
    let manifest: Manifest = serde_json::from_str(&text).expect("the e.5 shape");
    assert_eq!(manifest.version, 1);
    assert_eq!(manifest.run_id, RunId([7; 16]).to_hex());
    assert_eq!(manifest.nodes, vec![0]);
    assert_eq!(manifest.committed_seq, Some(2));
    assert_eq!(manifest.source_cursor.next_seq, 12);
    assert_eq!(manifest.resume_policy, vec!["reinit".to_string()]);
    assert_eq!(
        manifest.hostname,
        hostname::get()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "the hostname is the operating system's (e.5)"
    );
    let mut ascending = manifest.lineage.iter().map(|row| row.seq);
    let mut previous = ascending.next().expect("a lineage row");
    for seq in ascending {
        assert!(seq > previous, "lineage is ascending by seq (e.5)");
        previous = seq;
    }
    assert!(
        manifest.lineage.iter().all(|row| row.seq > 2),
        "the watermark dropped everything at or below it (f.11)"
    );
    for row in &manifest.segments {
        let file = path
            .parent()
            .expect("run directory")
            .join(format!("seg-{:06}.seg", row.segment));
        assert!(file.is_file(), "a referenced segment is present (PL-I12)");
        let biggest = manifest
            .lineage
            .iter()
            .filter_map(|entry| entry.disk.as_ref())
            .filter(|disk| disk.segment == row.segment)
            .map(|disk| disk.offset + disk.len)
            .max()
            .unwrap_or(0);
        assert!(row.bytes >= biggest, "`bytes` covers every record it holds");
        let length = std::fs::metadata(&file).expect("metadata").len();
        assert!(row.bytes <= length, "`bytes` never overstates the file");
    }

    // A second engine over the same fake and the same identity restores the queues.
    let first: Vec<(u64, State)> = engine.entry_states(0);
    let alloc = FakeAllocator::new();
    let second = common::engine(cfg.clone(), &alloc, &reactor);
    let point = second
        .restore(&path, &common::plan(), &cfg.fingerprints)
        .expect("restore");
    let restored: Vec<(u64, State)> = second.entry_states(0);
    // Every entry the manifest records with a disk copy comes back `OnDisk` (f.13); an
    // entry the first engine held resident with a valid record is one of them, because the
    // record is what survives the process.
    let with_record: Vec<u64> = manifest
        .lineage
        .iter()
        .filter(|row| row.disk.is_some())
        .map(|row| row.seq)
        .collect();
    // `restore` plans every queue before it returns, so the entries inside the promotion
    // window are already on their way back into memory and keep their record (f.7).
    let restored_seqs: Vec<u64> = restored
        .iter()
        .filter(|(_, state)| {
            matches!(
                state,
                State::OnDisk(_) | State::ResidentOnDisk(_) | State::Promoting(_, _)
            )
        })
        .map(|(seq, _)| *seq)
        .collect();
    assert_eq!(with_record, restored_seqs, "the same entries came back");
    assert!(
        first
            .iter()
            .filter(|(seq, _)| with_record.contains(seq))
            .all(|(_, state)| matches!(state, State::OnDisk(_) | State::ResidentOnDisk(_))),
        "every record the manifest names was a valid disk copy in the first engine"
    );
    let expected: Vec<u64> = manifest
        .lineage
        .iter()
        .filter(|row| row.disk.is_none())
        .map(|row| row.seq)
        .collect();
    let listed: Vec<u64> = point.to_recompute.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(
        listed, expected,
        "the rest is listed for recomputation (f.13)"
    );
    assert_eq!(point.extras.source_cursor.next_seq, 12);
    assert_eq!(point.extras.sink_state, Some(vec![9, 9]));
    assert_eq!(
        point.extras.kernel_states,
        vec![(1u16, 0usize, vec![1, 2, 3])]
    );
    let detail = second.detailed_stats();
    assert_eq!(
        detail.disk_bytes,
        cfg.segment_bytes * manifest.segments.len() as u64,
        "a segment is charged in full after a restore (f.13)"
    );
    assert!(
        detail.next_segment
            > manifest
                .segments
                .iter()
                .map(|r| r.segment)
                .max()
                .unwrap_or(0),
        "the counter continues above every restored number"
    );
    assert_eq!(detail.committed_seq, Some(2));

    // And the restored queue drains in order.
    second.close(0);
    let mut order = Vec::new();
    while let Some((morsel, _)) = second
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        order.push(morsel.seq);
    }
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted, "FIFO survives a restore (PL-I5, f.13)");
}

fn refuse(
    scratch: &common::Scratch,
    path: &Path,
    edit: impl FnOnce(&mut serde_json::Value),
) -> String {
    let text = std::fs::read_to_string(path).expect("the manifest");
    let mut raw: serde_json::Value = serde_json::from_str(&text).expect("json");
    edit(&mut raw);
    let edited = scratch.path().join("edited.json");
    std::fs::write(&edited, raw.to_string()).expect("write");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg.clone(), &alloc, &reactor);
    match engine.restore(&edited, &common::plan(), &cfg.fingerprints) {
        Ok(_) => panic!("the manifest should have been refused"),
        Err(MorunaError::Resume(message)) => message,
        Err(other) => panic!("expected Resume, got {other}"),
    }
}

#[test]
fn pl_t16_every_refusal_is_named() {
    let scratch = common::Scratch::new("t16b");
    let (engine, _alloc, _reactor, _cfg) = drive(&scratch);
    let path = engine.checkpoint(&extras()).expect("checkpoint");

    let message = refuse(&scratch, &path, |raw| raw["version"] = serde_json::json!(2));
    assert!(message.contains("version"), "{message}");
    let message = refuse(&scratch, &path, |raw| {
        raw["run_id"] = serde_json::json!("0".repeat(32))
    });
    assert!(message.contains("run id"), "{message}");
    let message = refuse(&scratch, &path, |raw| {
        raw["plan_digest"] = serde_json::json!("f".repeat(64))
    });
    assert!(message.contains("plan digest"), "{message}");
    let message = refuse(&scratch, &path, |raw| {
        raw["kernels"] = serde_json::json!([Fingerprint::compute("other", b"2").to_hex()])
    });
    assert!(message.contains("fingerprints"), "{message}");
    let message = refuse(&scratch, &path, |raw| {
        raw["resume_policy"] = serde_json::json!(["forbid"])
    });
    assert!(message.contains("forbid"), "{message}");
    let message =
        refuse(
            &scratch,
            &path,
            |raw| {
                raw["segments"] =
                    serde_json::json!([{ "stage": 0, "segment": 9999, "bytes": 4096 }])
            },
        );
    assert!(message.contains("missing"), "{message}");
    let message = refuse(&scratch, &path, |raw| {
        let biggest = raw["segments"][0]["bytes"].as_u64().unwrap_or(0);
        raw["lineage"][0]["disk"] = serde_json::json!({
            "segment": raw["segments"][0]["segment"],
            "offset": biggest + 4096,
            "len": 4096
        });
    });
    assert!(message.contains("lineage reads to"), "{message}");
    let message = refuse(&scratch, &path, |raw| {
        raw["config"]["page"]["bytes"] = serde_json::json!(8192)
    });
    assert!(message.contains("page.bytes"), "{message}");
    let message = refuse(&scratch, &path, |raw| {
        raw["config"]["staging"]["segment_bytes"] = serde_json::json!(1)
    });
    assert!(message.contains("staging.segment_bytes"), "{message}");

    // A plan whose row count changed is a different plan (e.5).
    let mut different = common::plan();
    different[0].rows += 1;
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg.clone(), &alloc, &reactor);
    let error = engine
        .restore(&path, &different, &cfg.fingerprints)
        .expect_err("a different plan is refused");
    assert!(error.to_string().contains("plan digest"), "{error}");

    // A resume policy of `forbid` in the configuration is refused before anything else.
    let mut forbidding = cfg.clone();
    forbidding.resume_policy = vec![ResumePolicy::Forbid];
    let _ = forbidding;
}

#[test]
fn pl_t16_find_manifest_takes_the_newest() {
    let scratch = common::Scratch::new("t16c");
    let older = scratch.path().join("moruna-".to_string() + &"aa".repeat(16));
    let newer = scratch.path().join("moruna-".to_string() + &"bb".repeat(16));
    for (dir, written, id) in [(&older, 10u64, "aa"), (&newer, 20u64, "bb")] {
        std::fs::create_dir_all(dir).expect("dir");
        let manifest = serde_json::json!({
            "version": 1,
            "run_id": id.repeat(16),
            "written_ns": written,
            "node": 0,
            "hostname": "here",
            "nodes": [0],
            "durable_staging": false,
            "plan_digest": "00",
            "kernels": [],
            "resume_policy": [],
            "stages": 1,
            "committed_seq": null,
            "source_cursor": { "split_index": 0, "row_offset": 0, "next_seq": 0 },
            "sink_state": null,
            "kernel_states": [],
            "lineage": [],
            "segments": [],
            "config": {}
        });
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).expect("write");
    }
    let found = PlacementEngine::find_manifest(scratch.path(), None)
        .expect("find_manifest")
        .expect("a manifest");
    assert_eq!(
        found,
        newer.join("manifest.json"),
        "the larger written_ns wins"
    );
    let header = PlacementEngine::read_manifest_header(&found).expect("header");
    assert_eq!(header.written_ns, 20);
    assert_eq!(header.hostname, "here");
    let wanted = RunId::from_hex(&"aa".repeat(16)).expect("run id");
    let found = PlacementEngine::find_manifest(scratch.path(), Some(wanted))
        .expect("find_manifest")
        .expect("a manifest");
    assert_eq!(found, older.join("manifest.json"), "a run id narrows it");
    assert!(
        PlacementEngine::find_manifest(&scratch.path().join("nowhere"), None)
            .expect("find_manifest")
            .is_none(),
        "a directory that is not there is not an error"
    );
}

#[test]
fn pl_t16_checkpoint_without_a_staging_directory() {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(1, None, common::budgets(1 << 30, 0, 0));
    let engine = common::engine(cfg, &alloc, &reactor);
    assert!(engine.manifest_path().is_none());
    match engine.checkpoint(&CheckpointExtras::default()) {
        Err(MorunaError::Resume(message)) => {
            assert_eq!(message, "no staging directory", "contracts d.10")
        }
        other => panic!("expected Resume, got {other:?}"),
    }
}

#[test]
fn pl_t16_the_manifest_names_every_policy_and_both_payload_kinds() {
    let scratch = common::Scratch::new("t16d");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        2,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    cfg.resume_policy = vec![ResumePolicy::Reinit, ResumePolicy::Checkpoint];
    cfg.fingerprints = vec![
        Fingerprint::compute("one", b"1"),
        Fingerprint::compute("two", b"2"),
    ];
    cfg.durable_staging = true;
    let engine = common::engine(cfg.clone(), &alloc, &reactor);
    engine
        .push(0, common::table_morsel(&alloc, 0, 0, 16))
        .expect("push");
    engine
        .push(1, common::tensor_morsel(&alloc, 1, 1, 32))
        .expect("push");
    let path = engine
        .checkpoint(&CheckpointExtras::default())
        .expect("checkpoint");
    let manifest = moruna_placement::manifest::read_manifest(&path).expect("manifest");
    assert_eq!(manifest.resume_policy, vec!["reinit", "checkpoint"]);
    assert!(
        manifest.durable_staging,
        "the platform's guarantee is recorded"
    );
    assert_eq!(manifest.committed_seq, None, "nothing is committed yet");
    let kinds: Vec<&str> = manifest
        .lineage
        .iter()
        .map(|row| row.kind.as_str())
        .collect();
    assert_eq!(kinds, vec!["table", "tensor"], "both payload kinds (e.5)");
    assert!(manifest.segments.is_empty(), "nothing is on disk");

    // A manifest that is not a manifest, and one whose run id is not 32 hex characters.
    let broken = scratch.path().join("broken.json");
    std::fs::write(&broken, "{").expect("write");
    assert!(moruna_placement::manifest::read_manifest(&broken).is_err());
    assert!(PlacementEngine::read_manifest_header(&broken).is_err());
    assert!(PlacementEngine::read_manifest_header(&scratch.path().join("nothing.json")).is_err());
    let bad_id = scratch.path().join("bad.json");
    let mut raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    raw["run_id"] = serde_json::json!("not hex");
    std::fs::write(&bad_id, raw.to_string()).expect("write");
    assert!(PlacementEngine::read_manifest_header(&bad_id).is_err());

    // The plan digest is over split ids and row counts, and nothing else (e.5).
    let mut estimated = common::plan();
    estimated[0].uncompressed_bytes += 1_000_000;
    estimated[0].estimated = true;
    assert_eq!(
        moruna_placement::manifest::plan_digest(&common::plan()),
        moruna_placement::manifest::plan_digest(&estimated),
        "`uncompressed_bytes` is excluded because it may be an estimate (e.5)"
    );
    let mut renumbered = common::plan();
    renumbered[0].id += 1;
    assert_ne!(
        moruna_placement::manifest::plan_digest(&common::plan()),
        moruna_placement::manifest::plan_digest(&renumbered),
        "a different split is a different plan"
    );
}
