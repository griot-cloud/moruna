//! PL-T25 and PL-T26 (MH 4.7, added by F8.4): the manifest names the reads the scheduler had
//! issued and not pushed, and keeps every morsel above its own watermark even when the sink
//! committed it after the scheduler read that watermark; `find_resumable_manifest` picks the
//! newest manifest a run could resume and passes over the rest. e.5, f.11, f.12, f.13.

mod common;

use moruna_kernel::{
    CheckpointExtras, Fingerprint, Locality, NodeId, Origin, Placement, SourceCursor, TierKind,
};
use moruna_placement::manifest::{Manifest, ManifestMatch, plan_digest};
use moruna_placement::{PlacementConfig, PlacementEngine};
use moruna_testkit::{FakeAllocator, FakeReactor};

fn at(seq: u64, split: u32) -> (u64, Origin) {
    (
        seq,
        Origin {
            split,
            row_start: seq * 10,
            row_end: seq * 10 + 10,
            node: NodeId(0),
        },
    )
}

fn extras(committed: Option<u64>, issued: Vec<(u64, Origin)>) -> CheckpointExtras {
    CheckpointExtras {
        kernel_states: Vec::new(),
        sink_state: None,
        committed_seq: committed,
        source_cursor: SourceCursor {
            split_index: 0,
            row_offset: 0,
            next_seq: 10,
        },
        issued,
    }
}

fn engine_with(
    scratch: &common::Scratch,
    pushed: u64,
) -> (
    std::sync::Arc<PlacementEngine>,
    FakeAllocator,
    FakeReactor,
    PlacementConfig,
) {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg.clone(), &alloc, &reactor);
    for seq in 0..pushed {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 16))
            .expect("push");
    }
    common::settle(&reactor);
    (engine, alloc, reactor, cfg)
}

fn read(path: &std::path::Path) -> Manifest {
    serde_json::from_str(&std::fs::read_to_string(path).expect("the manifest")).expect("e.5")
}

/// PL-T25: `issued` is written ascending above the watermark, restored in `extras`, and handed
/// back in `to_recompute` except where the lineage already names the morsel.
#[test]
fn pl_t25_issued_reads_round_trip() {
    let scratch = common::Scratch::new("t25");
    let (engine, alloc, reactor, cfg) = engine_with(&scratch, 4);
    // 3 was issued and pushed between the scheduler's snapshot and this one, so it is in both;
    // 0 is under the watermark; 6 and 5 arrive out of order.
    let issued = vec![at(6, 1), at(3, 1), at(5, 1), at(0, 1)];
    let path = engine
        .checkpoint(&extras(Some(1), issued))
        .expect("checkpoint");
    let manifest = read(&path);
    let seqs: Vec<u64> = manifest.issued.iter().map(|row| row.seq).collect();
    assert_eq!(seqs, vec![3, 5, 6], "ascending, above the watermark (e.5)");
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("text")).expect("json");
    assert!(raw.get("issued").is_some(), "the key is written");

    let second = common::engine(cfg.clone(), &alloc, &reactor);
    let point = second
        .restore(&path, &common::plan(), &cfg.fingerprints)
        .expect("restore");
    let back: Vec<u64> = point.to_recompute.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(
        back,
        vec![2, 3, 5, 6],
        "the lineage without a disk copy, then the issued reads it did not hold, once each"
    );
    let origin_of_5 = point
        .to_recompute
        .iter()
        .find(|(seq, _)| *seq == 5)
        .map(|(_, origin)| origin.clone())
        .expect("5 is recomputed");
    let expected = at(5, 1).1;
    assert_eq!(
        (
            origin_of_5.split,
            origin_of_5.row_start,
            origin_of_5.row_end
        ),
        (expected.split, expected.row_start, expected.row_end),
        "with the origin the scheduler gave"
    );
    let extras_back: Vec<u64> = point.extras.issued.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(extras_back, vec![3, 5, 6]);

    // A manifest written before F8.4 has no `issued` key and restores as it always did.
    let mut old: serde_json::Value = raw.clone();
    if let Some(map) = old.as_object_mut() {
        map.remove("issued");
    }
    std::fs::write(&path, serde_json::to_string(&old).expect("json")).expect("rewrite");
    let third = common::engine(cfg.clone(), &alloc, &reactor);
    let point = third
        .restore(&path, &common::plan(), &cfg.fingerprints)
        .expect("an old manifest restores");
    assert!(point.extras.issued.is_empty());
    let back: Vec<u64> = point.to_recompute.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(back, vec![2, 3]);
}

/// PL-T26: a morsel committed after the scheduler read the watermark it hands `checkpoint` is
/// still in that manifest's lineage, without a disk copy, and leaves the index only when a
/// manifest whose watermark covers it is written. `lineage_len` counts only what is
/// uncommitted. f.11, f.12, MH H5.
#[test]
fn pl_t26_committed_since_the_watermark_is_kept() {
    let scratch = common::Scratch::new("t26");
    let (engine, alloc, reactor, cfg) = engine_with(&scratch, 6);
    engine.set_staging(0, true);
    let sample_bytes = 64u64;
    engine.set_water(0, TierKind::Host, sample_bytes, sample_bytes);
    common::settle(&reactor);
    for _ in 0..5 {
        engine
            .pop_blocking(0, common::want_host(), Locality::Any)
            .expect("pop_blocking")
            .expect("a morsel");
    }
    engine.set_committed(4);
    assert_eq!(
        engine.detailed_stats().lineage_len,
        1,
        "only 5 is uncommitted"
    );

    // The scheduler read the watermark at 1, before the sink committed 2 to 4.
    let path = engine
        .checkpoint(&extras(Some(1), Vec::new()))
        .expect("one");
    let manifest = read(&path);
    let seqs: Vec<u64> = manifest.lineage.iter().map(|row| row.seq).collect();
    assert_eq!(
        seqs,
        vec![2, 3, 4, 5],
        "everything above the manifest's watermark"
    );
    assert!(
        manifest
            .lineage
            .iter()
            .filter(|row| row.seq <= 4)
            .all(|row| row.disk.is_none()),
        "a committed morsel's disk copy is released; it is recomputed from its origin"
    );
    let second = common::engine(cfg.clone(), &alloc, &reactor);
    let point = second
        .restore(&path, &common::plan(), &cfg.fingerprints)
        .expect("restore");
    let back: Vec<u64> = point.to_recompute.iter().map(|(seq, _)| *seq).collect();
    assert!(
        back.starts_with(&[2, 3, 4]),
        "the morsels the sink state does not hold come back: {back:?}"
    );

    // The next manifest's watermark covers them, and they are gone for good.
    let path = engine
        .checkpoint(&extras(Some(4), Vec::new()))
        .expect("two");
    let seqs: Vec<u64> = read(&path).lineage.iter().map(|row| row.seq).collect();
    assert_eq!(seqs, vec![5]);
    let path = engine
        .checkpoint(&extras(Some(1), Vec::new()))
        .expect("three");
    let seqs: Vec<u64> = read(&path).lineage.iter().map(|row| row.seq).collect();
    assert_eq!(
        seqs,
        vec![5],
        "pruned by the manifest that covered them, never brought back"
    );
}

/// With checkpointing off there is no manifest to wait for, and `set_committed` forgets at once.
#[test]
fn pl_t26_without_checkpoints_committed_lineage_goes_at_once() {
    let scratch = common::Scratch::new("t26-off");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(1, None, common::budgets(1 << 30, 0, 0));
    cfg.checkpoint_enabled = false;
    let engine = common::engine(cfg, &alloc, &reactor);
    for seq in 0..3 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 16))
            .expect("push");
    }
    engine.set_committed(1);
    assert_eq!(engine.detailed_stats().lineage_len, 1);
    drop(scratch);
}

/// `resume = "auto"`: the newest manifest this run could resume, never one it could not.
#[test]
fn find_resumable_manifest_picks_the_newest_that_matches() {
    let scratch = common::Scratch::new("auto");
    let staging = scratch.path().to_path_buf();
    let base = common::config(
        1,
        Some(staging.clone()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let write = |run: u8, fingerprint: &[u8], digest_plan: bool, spec: Option<&str>| {
        let alloc = FakeAllocator::new();
        let reactor = FakeReactor::new();
        let mut cfg = base.clone();
        cfg.run_id = moruna_kernel::RunId([run; 16]);
        cfg.fingerprints = vec![Fingerprint::compute("test", fingerprint)];
        if !digest_plan {
            cfg.plan_digest = [9; 32];
        }
        if let (Some(spec), serde_json::Value::Object(map)) = (spec, &mut cfg.config) {
            map.insert("spec.digest".into(), serde_json::Value::String(spec.into()));
        }
        let engine = common::engine(cfg, &alloc, &reactor);
        let path = engine
            .checkpoint(&extras(None, Vec::new()))
            .expect("checkpoint");
        std::thread::sleep(std::time::Duration::from_millis(2));
        path
    };
    let fingerprints = vec![Fingerprint::compute("test", b"1")];
    let want = ManifestMatch {
        fingerprints: &fingerprints,
        plan_digest: Some(plan_digest(&common::plan())),
        spec_digest: None,
    };
    assert_eq!(
        PlacementEngine::find_resumable_manifest(&staging, &want).expect("empty"),
        None,
        "nothing written yet"
    );
    assert_eq!(
        PlacementEngine::find_resumable_manifest(&staging.join("absent"), &want).expect("absent"),
        None,
        "a directory that does not exist holds nothing"
    );
    let older = write(1, b"1", true, Some("sha256:a"));
    let newer = write(2, b"1", true, Some("sha256:a"));
    let _other_kernels = write(3, b"2", true, Some("sha256:a"));
    let _other_plan = write(4, b"1", false, Some("sha256:a"));
    let _other_spec = write(5, b"1", true, Some("sha256:b"));
    assert_eq!(
        PlacementEngine::find_resumable_manifest(
            &staging,
            &ManifestMatch {
                spec_digest: Some("sha256:a"),
                ..want.clone()
            }
        )
        .expect("found"),
        Some(newer.clone()),
        "the newest whose kernels, plan and job document match"
    );
    let _ = older;
    // Without a plan digest to compare (a source not yet built), a newer manifest of another
    // plan is taken, and restore is left to refuse it by name.
    let loose = ManifestMatch {
        plan_digest: None,
        spec_digest: Some("sha256:a"),
        ..want.clone()
    };
    let picked = PlacementEngine::find_resumable_manifest(&staging, &loose)
        .expect("found")
        .expect("one");
    assert!(
        picked
            .to_string_lossy()
            .contains(&moruna_kernel::RunId([4; 16]).to_hex())
    );

    // A stage that forbids resume, a manifest of another version, and a non-durable one from
    // another host are all passed over.
    let forbid = write(6, b"1", true, Some("sha256:a"));
    let edit = |path: &std::path::Path, key: &str, value: serde_json::Value| {
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("text")).expect("json");
        doc[key] = value;
        std::fs::write(path, serde_json::to_string(&doc).expect("json")).expect("write");
    };
    edit(&forbid, "resume_policy", serde_json::json!(["forbid"]));
    let elsewhere = write(7, b"1", true, Some("sha256:a"));
    edit(&elsewhere, "hostname", serde_json::json!("another-host"));
    let future = write(8, b"1", true, Some("sha256:a"));
    edit(&future, "version", serde_json::json!(2));
    let garbage = staging.join(format!("moruna-{}", "f".repeat(32)));
    std::fs::create_dir_all(&garbage).expect("dir");
    std::fs::write(garbage.join("manifest.json"), "not json").expect("write");
    std::fs::create_dir_all(staging.join("unrelated")).expect("dir");
    std::fs::write(staging.join("unrelated").join("manifest.json"), "{}").expect("write");
    assert_eq!(
        PlacementEngine::find_resumable_manifest(
            &staging,
            &ManifestMatch {
                spec_digest: Some("sha256:a"),
                ..want.clone()
            }
        )
        .expect("found"),
        Some(newer),
        "none of the later ones could be resumed"
    );
}
