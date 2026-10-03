//! The snapshot a run's sink committed reaches the report: carried from the meta, written to
//! the JSON as `{"snapshot_id", "parent_snapshot_id"}`, and absent from the JSON of a run
//! whose sink keeps no snapshots, so that report is unchanged.

mod common;

use common::{TempDir, config, limits, meta, synthetic_trace};
use moruna_kernel::{SinkSnapshot, TraceSink};
use moruna_trace::{ExitReason, RunReport, TraceWriter};

fn compute(tag: &str, snapshot: Option<SinkSnapshot>) -> RunReport {
    let dir = TempDir::new(tag);
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in synthetic_trace() {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    let mut meta = meta(ExitReason::Completed);
    meta.snapshot = snapshot;
    RunReport::compute(&view, &limits(), &meta)
}

#[test]
fn a_committed_snapshot_is_in_the_report() {
    let snapshot = SinkSnapshot {
        snapshot_id: 7,
        parent_snapshot_id: Some(6),
    };
    let report = compute("snap-some", Some(snapshot));
    assert_eq!(report.snapshot, Some(snapshot));
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("json");
    assert_eq!(
        json["snapshot"],
        serde_json::json!({"snapshot_id": 7, "parent_snapshot_id": 6})
    );

    let first = compute(
        "snap-first",
        Some(SinkSnapshot {
            snapshot_id: 1,
            parent_snapshot_id: None,
        }),
    );
    let json: serde_json::Value = serde_json::from_str(&first.to_json()).expect("json");
    assert_eq!(
        json["snapshot"],
        serde_json::json!({"snapshot_id": 1, "parent_snapshot_id": null})
    );
}

#[test]
fn a_run_without_a_snapshot_has_no_snapshot_field() {
    let report = compute("snap-none", None);
    assert_eq!(report.snapshot, None);
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("json");
    assert!(json.get("snapshot").is_none(), "{json}");
}
