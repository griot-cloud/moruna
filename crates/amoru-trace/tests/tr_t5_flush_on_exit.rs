//! TR-T5 flush_on_exit. For each exit reason the final file is complete and readable by the
//! `arrow` IPC reader. Proves TR-I5.

mod common;

use amoru_kernel::{TraceRecord, TraceSink};
use amoru_trace::{ExitReason, RunReport, TraceWriter};
use common::{TempDir, config, limits, meta, record};

const RECORDS: u64 = 12_000;

fn exercise(tag: &str, exit: ExitReason) {
    let dir = TempDir::new(tag);
    let mut cfg = config(dir.path());
    let path = dir.path().join("trace.arrow");
    cfg.path = Some(path.clone());
    // Small enough that the trace overflows and the final file is the only complete copy.
    cfg.memory_limit = 256 * 1024;
    let writer = TraceWriter::start(cfg).expect("start");
    for seq in 0..RECORDS {
        writer.record(record(seq, (seq % 3) as u16));
    }

    // The scheduler flushes at every exit (SC f.10, e.2); the facade then finishes once.
    writer.flush().expect("flush at the exit");
    let view = writer.finish().expect("finish");
    assert_eq!(view.len(), RECORDS);

    // The file has its footer and reads back through the plain Arrow IPC file reader.
    let file = std::fs::File::open(&path).expect("the final file exists");
    let reader = arrow::ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None)
        .expect("a complete IPC file with a footer");
    assert_eq!(*reader.schema(), *TraceRecord::arrow_schema());
    let mut rows = 0usize;
    for batch in reader {
        rows += batch.expect("a readable batch").num_rows();
    }
    assert_eq!(rows as u64, RECORDS, "the final file holds every record");

    // The view returned by `finish` reads the final file, so the report is produced from a
    // trace that is complete whichever way the run ended.
    let r = RunReport::compute(&view, &limits(), &meta(exit.clone()));
    assert_eq!(r.exit, exit);
    assert_eq!(
        r.stages.iter().map(|s| s.morsels).sum::<u64>(),
        RECORDS,
        "the report sees every record"
    );

    // e.3: the rendering names how the run ended, and stays inside its forty lines.
    let shown = format!("{r}");
    let lines = shown.lines().count();
    assert!(lines <= 40, "the layout is at most 40 lines, got {lines}");
    match &exit {
        ExitReason::Completed => assert!(shown.starts_with("amoru run"), "{shown}"),
        ExitReason::Cancelled => assert!(shown.contains("cancelled"), "{shown}"),
        ExitReason::Terminated { diagnostic } => {
            assert!(shown.contains(diagnostic), "{shown}")
        }
    }

    // The overflow copy is gone once the final file holds the whole trace (e.2).
    let overflow = dir
        .path()
        .join(format!("trace-overflow-{}.arrow", "ab".repeat(16)));
    assert!(!overflow.exists(), "the overflow file is removed on finish");
}

#[test]
fn tr_t5_flush_on_exit_completed() {
    exercise("t5-completed", ExitReason::Completed);
}

#[test]
fn tr_t5_flush_on_exit_terminated() {
    exercise(
        "t5-terminated",
        ExitReason::Terminated {
            diagnostic: "budget: morsel 42 stage 2 footprint 9 exceeds budget 8".to_string(),
        },
    );
}

#[test]
fn tr_t5_flush_on_exit_cancelled() {
    exercise("t5-cancelled", ExitReason::Cancelled);
}

/// With no final path the trace is still complete: the view reads the overflow file, which
/// is kept for exactly that reason (e.2).
#[test]
fn tr_t5_flush_on_exit_without_a_path() {
    let dir = TempDir::new("t5-nopath");
    let mut cfg = config(dir.path());
    cfg.memory_limit = 256 * 1024;
    let writer = TraceWriter::start(cfg).expect("start");
    for seq in 0..RECORDS {
        writer.record(record(seq, 1));
    }
    writer.flush().expect("flush");
    let view = writer.finish().expect("finish");
    assert_eq!(view.len(), RECORDS);
    assert_eq!(view.records().len() as u64, RECORDS);
    let overflow = dir
        .path()
        .join(format!("trace-overflow-{}.arrow", "ab".repeat(16)));
    assert!(overflow.exists(), "the overflow file is kept for the view");

    // And the view can still be written out as an IPC file on request (d.1).
    let out = dir.path().join("out.arrow");
    view.to_ipc_file(&out).expect("to_ipc_file");
    let file = std::fs::File::open(&out).expect("the written file");
    let reader = arrow::ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None)
        .expect("a complete IPC file");
    let rows: usize = reader.map(|b| b.expect("batch").num_rows()).sum();
    assert_eq!(rows as u64, RECORDS);

    // A view whose overflow file has gone, or is truncated, returns what is still in memory
    // rather than failing: the report is produced from an incomplete trace, never from none.
    let truncated = view.records().len();
    std::fs::write(&overflow, b"not an arrow stream").expect("truncate the overflow file");
    assert!(view.records().len() < truncated);
    std::fs::remove_file(&overflow).expect("remove the overflow file");
    assert!(view.records().len() < truncated);
}
