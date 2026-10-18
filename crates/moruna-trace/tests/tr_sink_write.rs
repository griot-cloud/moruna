//! What the engine behind a run's sink said about the write it committed reaches the report:
//! carried from the meta, written to the JSON as `{"engine", "report"}` with the report exactly
//! as the engine serialised it, with `"excluded"` beside it when the sink kept rows the engine
//! handed back, and absent from the JSON of a run whose sink has no such engine, so that report
//! is unchanged. What the engine behind its source said about the read it planned reaches it
//! the same way, as `"read"`.

mod common;

use common::{TempDir, config, limits, meta, synthetic_trace};
use moruna_kernel::{ExcludedRows, SinkWrite, SourceRead, TraceSink};
use moruna_trace::{ExitReason, RunReport, TraceWriter};

fn compute(tag: &str, write: Option<SinkWrite>) -> RunReport {
    compute_with(tag, write, None)
}

fn compute_with(tag: &str, write: Option<SinkWrite>, read: Option<SourceRead>) -> RunReport {
    let dir = TempDir::new(tag);
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in synthetic_trace() {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    let mut meta = meta(ExitReason::Completed);
    meta.write = write;
    meta.read = read;
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
        excluded: None,
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
    assert!(json.get("read").is_none(), "{json}");
}

#[test]
fn rows_a_write_excluded_are_in_the_report_beside_what_its_engine_said() {
    let said = serde_json::json!({
        "rows_written": 15,
        "on_violation": "exclude",
        "violations": {"non_negative": 5},
        "rows_excluded": 5,
    });
    let excluded = ExcludedRows {
        url: "/scratch/excluded".into(),
        files: vec!["/scratch/excluded/excluded-00000000000000000000.parquet".into()],
        rows: 5,
    };
    let write = SinkWrite {
        engine: "peql".into(),
        report: said.clone(),
        excluded: Some(excluded.clone()),
    };
    let report = compute("write-excluded", Some(write.clone()));
    assert_eq!(report.write, Some(write));
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("json");
    assert_eq!(
        json["write"],
        serde_json::json!({
            "engine": "peql",
            "report": said,
            "excluded": {"url": excluded.url, "files": excluded.files, "rows": 5},
        })
    );
}

#[test]
fn a_governed_read_is_in_the_report_as_its_engine_said() {
    let said = serde_json::json!({
        "contracts": [{"contract": "demo/readings", "excluded": {"non_negative": 5}}],
    });
    let read = SourceRead {
        engine: "peql".into(),
        report: said.clone(),
    };
    let report = compute_with("read-some", None, Some(read.clone()));
    assert_eq!(report.read, Some(read));
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("json");
    assert_eq!(
        json["read"],
        serde_json::json!({"engine": "peql", "report": said})
    );
    assert!(json.get("write").is_none(), "{json}");
}
