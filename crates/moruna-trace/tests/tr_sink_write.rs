//! What the engine behind a run's sink said about the write it committed reaches the report:
//! carried from the meta, written to the JSON as `{"engine", "report"}` with the report exactly
//! as the engine serialised it, and absent from the JSON of a run whose sink has no such
//! engine, so that report is unchanged.

mod common;

use common::{TempDir, config, limits, meta, synthetic_trace};
use moruna_kernel::{SinkWrite, TraceSink};
use moruna_trace::{ExitReason, RunReport, TraceWriter};

fn compute(tag: &str, write: Option<SinkWrite>) -> RunReport {
    let dir = TempDir::new(tag);
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in synthetic_trace() {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    let mut meta = meta(ExitReason::Completed);
    meta.write = write;
    RunReport::compute(&view, &limits(), &meta)
}

#[test]
fn a_committed_write_is_in_the_report_as_its_engine_said() {
    let said = serde_json::json!({
        "rows_written": 20,
        "verdict": {"valid": false, "breached": ["non_negative"], "row_count": 20,
                    "stats": {"kwh_min": -5}, "data_hash": "sha256:ab"},
        "snapshot": {"snapshot_id": 7, "parent_snapshot_id": 6},
    });
    let write = SinkWrite {
        engine: "peql".into(),
        report: said.clone(),
    };
    let report = compute("write-some", Some(write.clone()));
    assert_eq!(report.write, Some(write));
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("json");
    assert_eq!(
        json["write"],
        serde_json::json!({"engine": "peql", "report": said})
    );
}

#[test]
fn a_run_without_an_engine_write_has_no_write_field() {
    let report = compute("write-none", None);
    assert_eq!(report.write, None);
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("json");
    assert!(json.get("write").is_none(), "{json}");
    assert!(json.get("snapshot").is_none(), "{json}");
}
