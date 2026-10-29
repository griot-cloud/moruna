//! PL-T27 and PL-T28 (f.17, contracts d.16): the queues of a run's DAG.
//!
//! PL-T27 fan_out_is_counted_once: a batch pushed into several queues with `push_to` is one
//! residency, counted once in the tier's total and in each queue's own figure, and released when
//! the last of its consumers takes it. PL-T28 lineage_by_root: a source morsel's lineage follows
//! it into every queue it fans out to, a sink's commit releases only what every sink it reaches
//! has committed, and a manifest restores a morsel whose every position is on disk at each of
//! them and lists any other for its source to read again.

mod common;

use moruna_kernel::{
    CheckpointExtras, Consumer, Link, Locality, Morsel, Placement, Producer, SourceCursor, Tier,
    TierKind, Topology,
};
use moruna_testkit::{FakeAllocator, FakeReactor};

fn k(stage: u16, port: u16) -> Consumer {
    Consumer::Kernel { stage, port }
}

/// One source fanning out to two stages, each to its own sink: queues 0 and 1 from the source,
/// 2 and 3 to the sinks.
fn fan_out() -> Topology {
    Topology::new(
        1,
        2,
        2,
        vec![
            Link {
                from: Producer::Source(0),
                to: k(1, 0),
            },
            Link {
                from: Producer::Source(0),
                to: k(2, 0),
            },
            Link {
                from: Producer::Kernel(1),
                to: Consumer::Sink(0),
            },
            Link {
                from: Producer::Kernel(2),
                to: Consumer::Sink(1),
            },
        ],
    )
    .expect("a fan-out")
}

fn host_bytes(engine: &moruna_placement::PlacementEngine, queue: u16) -> u64 {
    engine.stats().queues[usize::from(queue)].bytes_by_tier[TierKind::Host.index()]
}

#[test]
fn pl_t27_fan_out_is_counted_once() {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(1, None, common::budgets(1 << 30, 0, 0));
    cfg.topology = fan_out();
    cfg.fingerprints = vec![
        moruna_kernel::Fingerprint::compute("a", b"1"),
        moruna_kernel::Fingerprint::compute("b", b"1"),
    ];
    let engine = common::engine(cfg, &alloc, &reactor);
    let morsel = common::table_morsel(&alloc, 0, 0, 1000);
    let bytes = morsel.bytes;
    engine.push_to(&[0, 1], morsel).expect("fans out");
    let stats = engine.stats();
    assert_eq!(host_bytes(&engine, 0), bytes, "queue 0 counts the batch");
    assert_eq!(host_bytes(&engine, 1), bytes, "queue 1 counts the batch");
    assert_eq!(
        stats.shared_by_tier[TierKind::Host.index()],
        bytes,
        "and the second count is the share"
    );
    // The two consumers are handed the same bytes.
    let a = engine
        .pop(0, common::want_host(), Locality::Any)
        .expect("pop")
        .expect("the head");
    assert_eq!(
        engine.stats().shared_by_tier[TierKind::Host.index()],
        0,
        "one consumer took its share: the batch is resident once, in queue 1"
    );
    assert_eq!(host_bytes(&engine, 1), bytes);
    let b = engine
        .pop(1, common::want_host(), Locality::Any)
        .expect("pop")
        .expect("the head");
    let address = |m: &Morsel| match &m.payload {
        moruna_kernel::Payload::Table(batch, _) => {
            batch.column(0).to_data().buffers()[0].as_ptr() as usize
        }
        moruna_kernel::Payload::Tensor(..) => 0,
    };
    assert_eq!(address(&a), address(&b), "shared, never copied");
    assert_eq!(host_bytes(&engine, 1), 0);
    // A fan-out into a closed queue pushes nothing anywhere.
    engine.close(1);
    let refused = engine.push_to(&[0, 1], common::table_morsel(&alloc, 1, 0, 10));
    assert!(refused.is_err());
    assert_eq!(engine.stats().queues[0].count, 0, "nothing reached queue 0");
    assert!(
        engine
            .push_to(&[], common::table_morsel(&alloc, 2, 0, 10))
            .is_err()
    );
    // A batch in a host tier this run does not have is refused.
    let mut device = common::table_morsel(&alloc, 3, 0, 10);
    device.payload = match device.payload {
        moruna_kernel::Payload::Table(batch, _) => moruna_kernel::Payload::Table(
            batch,
            Tier::Disk(moruna_kernel::SegmentRef {
                segment: 0,
                offset: 0,
                len: 0,
            }),
        ),
        other => other,
    };
    assert!(engine.push_to(&[0, 2], device).is_err());
}

/// The testkit's default push: a share of the payload into each queue, in order.
#[test]
fn pl_t27_the_contract_default_pushes_a_share_into_each() {
    let fake = moruna_testkit::FakePlacement::new();
    let alloc = FakeAllocator::new();
    fake.push_to(&[3, 4], common::table_morsel(&alloc, 9, 0, 10))
        .expect("pushed");
    assert_eq!(fake.pushed(3), vec![9]);
    assert_eq!(fake.pushed(4), vec![9]);
    assert!(
        fake.push_to(&[], common::table_morsel(&alloc, 9, 0, 10))
            .is_err()
    );
}

#[test]
fn pl_t28_lineage_by_root() {
    let scratch = common::Scratch::new("t28");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    cfg.topology = fan_out();
    cfg.fingerprints = vec![
        moruna_kernel::Fingerprint::compute("a", b"1"),
        moruna_kernel::Fingerprint::compute("b", b"1"),
    ];
    cfg.resume_policy = vec![moruna_kernel::ResumePolicy::Reinit; 2];
    let engine = common::engine(cfg.clone(), &alloc, &reactor);
    for seq in 0..4 {
        engine
            .push_to(&[0, 1], common::table_morsel(&alloc, seq, 0, 100))
            .expect("fans out");
    }
    // Morsel 0 goes through both stages to both sinks' queues.
    for (from, to) in [(0u16, 2u16), (1, 3)] {
        let popped = engine
            .pop(from, common::want_host(), Locality::Any)
            .expect("pop")
            .expect("the head");
        let next = Morsel::new(popped.seq, to - 1, popped.payload, popped.origin);
        engine.push(to, next).expect("pushed downstream");
    }
    let detail = engine.detailed_stats();
    assert_eq!(detail.lineage_len, 4, "one record per source morsel");
    // Sink 0 commits 0; sink 1 has not: nothing is released.
    engine.set_committed(0, 0);
    assert_eq!(engine.detailed_stats().source_committed, vec![None]);
    engine.set_committed(1, 0);
    assert_eq!(engine.detailed_stats().source_committed, vec![Some(0)]);
    assert_eq!(engine.detailed_stats().committed, vec![Some(0), Some(0)]);

    // Morsel 1 is demoted to disk in both of its queues; morsel 2 in one only.
    let extras = CheckpointExtras {
        kernel_states: Vec::new(),
        sink_states: vec![None, None],
        committed: vec![Some(0), Some(0)],
        source_cursors: vec![SourceCursor {
            split_index: 0,
            row_offset: 400,
            next_seq: 4,
        }],
        issued: vec![Vec::new()],
    };
    let path = engine.checkpoint(&extras).expect("a manifest");
    let text = std::fs::read_to_string(&path).expect("the manifest");
    let mut manifest: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let rows = manifest["lineage"].as_array().expect("rows").clone();
    let roots: Vec<u64> = rows
        .iter()
        .map(|r| r["root"].as_u64().unwrap_or(9))
        .collect();
    assert_eq!(
        roots,
        vec![1, 1, 2, 2, 3, 3],
        "two positions per morsel above the watermark"
    );
    // Hand-place disk copies, as demotions would have: both of morsel 1's, one of morsel 2's.
    let segment = serde_json::json!({"segment": 0, "offset": 0, "len": 4096});
    for row in manifest["lineage"].as_array_mut().expect("rows") {
        let root = row["root"].as_u64().unwrap_or(9);
        let stage = row["stage"].as_u64().unwrap_or(9);
        if root == 1 || (root == 2 && stage == 0) {
            row["disk"] = segment.clone();
        }
    }
    manifest["segments"] = serde_json::json!([{"stage": 0, "segment": 0, "bytes": 8192}]);
    let segment_path = engine
        .manifest_path()
        .and_then(|m| m.parent().map(|d| d.join("seg-000000.seg")))
        .expect("a run directory");
    std::fs::write(&segment_path, vec![0u8; 8192]).expect("a segment file");
    std::fs::write(&path, manifest.to_string()).expect("rewritten");

    let fresh = common::engine(cfg.clone(), &FakeAllocator::new(), &FakeReactor::new());
    let point = fresh
        .restore(&path, &[common::plan()], &cfg.fingerprints)
        .expect("restores");
    let recompute: Vec<u64> = point.to_recompute[0].iter().map(|(s, _)| *s).collect();
    assert_eq!(
        recompute,
        vec![2, 3],
        "a morsel with a position off disk is read again; one all on disk is not"
    );
    assert_eq!(
        fresh.stats().queues[0].count,
        1,
        "morsel 1 is back in queue 0"
    );
    assert_eq!(fresh.stats().queues[1].count, 1, "and in queue 1");
    assert_eq!(point.extras.committed, vec![Some(0), Some(0)]);
    assert_eq!(point.extras.source_cursors[0].next_seq, 4);

    // A manifest of another shape is refused by name.
    let mut other = cfg.clone();
    other.topology = Topology::chain(2);
    let engine = common::engine(other, &FakeAllocator::new(), &FakeReactor::new());
    let refused = engine
        .restore(&path, &[common::plan()], &cfg.fingerprints)
        .expect_err("refused")
        .to_string();
    assert!(
        refused.contains("written for a run of 4 queues"),
        "{refused}"
    );
}
