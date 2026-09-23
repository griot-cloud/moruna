//! TR-T3 pure_report. The same trace and meta produce the same JSON twice, and the same
//! JSON a previous process produced (the golden file beside this test). Proves TR-I3.

mod common;

use moruna_kernel::TraceSink;
use moruna_trace::{ExitReason, RunReport, TraceWriter};
use common::{TempDir, config, limits, meta, synthetic_trace};

/// The report a previous process computed from `synthetic_trace`, `limits` and
/// `meta(Completed)`. Regenerate it deliberately with `MORUNA_UPDATE_GOLDEN=1`, never to make
/// a failing test pass: a change here is a change to the report's formulas or its fields.
const GOLDEN: &str = "tests/data/tr_t3_report.json";

fn compute(tag: &str) -> RunReport {
    let dir = TempDir::new(tag);
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in synthetic_trace() {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    RunReport::compute(&view, &limits(), &meta(ExitReason::Completed))
}

#[test]
fn tr_t3_pure_report() {
    // Twice in this process, over two separate writers, two separate views and two separate
    // chunk layouts: the report is a function of the records, not of how they were stored.
    let first = compute("t3-a");
    let second = compute("t3-b");
    assert_eq!(first.to_json(), second.to_json(), "the report is not pure");
    assert_eq!(
        format!("{first}"),
        format!("{second}"),
        "the rendering is not pure either"
    );

    // Recomputing from one view twice gives the same answer as well.
    let dir = TempDir::new("t3-c");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in synthetic_trace() {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    let a = RunReport::compute(&view, &limits(), &meta(ExitReason::Completed));
    let b = RunReport::compute(&view, &limits(), &meta(ExitReason::Completed));
    assert_eq!(a.to_json(), b.to_json());
    assert_eq!(a.to_json(), first.to_json());

    // And the same as the process that wrote the golden file.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    if std::env::var_os("MORUNA_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, format!("{}\n", first.to_json())).expect("write the golden file");
    }
    let golden = std::fs::read_to_string(&path).expect("the golden file is in the repository");
    assert_eq!(
        first.to_json(),
        golden.trim_end(),
        "the report differs from the one a previous process computed from the same inputs"
    );
}

/// The exit reason and the meta are part of the input, so a different exit gives a
/// different report and the same exit gives the same one.
#[test]
fn tr_t3_pure_report_in_its_meta() {
    let dir = TempDir::new("t3-meta");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    for r in synthetic_trace() {
        writer.record(r);
    }
    let view = writer.finish().expect("finish");
    let completed = RunReport::compute(&view, &limits(), &meta(ExitReason::Completed));
    let cancelled = RunReport::compute(&view, &limits(), &meta(ExitReason::Cancelled));
    assert_ne!(completed.to_json(), cancelled.to_json());
    assert_eq!(
        cancelled.to_json(),
        RunReport::compute(&view, &limits(), &meta(ExitReason::Cancelled)).to_json()
    );
}
