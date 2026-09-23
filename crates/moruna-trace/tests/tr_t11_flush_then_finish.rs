//! TR-T11 flush_then_finish. From four threads, record 100,000 records each, then each
//! thread calls `flush` concurrently; after every `flush` returns, `snapshot().len()` is
//! 400,000; `finish` once afterwards writes the footer; a second `finish` is a no-op;
//! `record` after `finish` counts in `late_records`. Proves TR-I5 and e.1.

mod common;

use std::sync::{Arc, Barrier};

use moruna_kernel::TraceSink;
use moruna_trace::TraceWriter;
use common::{TempDir, config, lean_record};

const THREADS: u64 = 4;
const PER_THREAD: u64 = 100_000;
const TOTAL: u64 = THREADS * PER_THREAD;

#[test]
fn tr_t11_flush_then_finish() {
    let dir = TempDir::new("t11");
    let mut cfg = config(dir.path());
    cfg.path = Some(dir.path().join("trace.arrow"));
    cfg.channel_capacity = 1024;
    cfg.memory_limit = 2 * 1024 * 1024;
    let writer = TraceWriter::start(cfg).expect("start");

    // Every record is on the channel before any thread flushes, so the flush markers
    // rendezvous with a writer that has real work queued in front of them.
    let all_recorded = Arc::new(Barrier::new(THREADS as usize));
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let w = Arc::clone(&writer);
            let barrier = Arc::clone(&all_recorded);
            scope.spawn(move || {
                let sink: &dyn TraceSink = w.as_ref();
                for i in 0..PER_THREAD {
                    sink.record(lean_record(t * PER_THREAD + i, t as u16));
                }
                barrier.wait();
                sink.flush().expect("a concurrent flush");
                // After this thread's flush returned, every record recorded before it,
                // which is all of them, is in a chunk.
                assert_eq!(
                    w.snapshot().len(),
                    TOTAL,
                    "a flush that returned leaves nothing queued"
                );
            });
        }
    });

    assert_eq!(writer.snapshot().len(), TOTAL);

    let view = writer.finish().expect("finish");
    assert_eq!(view.len(), TOTAL);
    // The footer is there: the file reads back through the plain IPC file reader.
    let file = std::fs::File::open(dir.path().join("trace.arrow")).expect("the final file");
    let reader = arrow::ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None)
        .expect("a footer");
    let rows: usize = reader.map(|b| b.expect("batch").num_rows()).sum();
    assert_eq!(rows as u64, TOTAL);

    // A second finish is a no-op and returns the same view.
    let again = writer.finish().expect("finish is idempotent");
    assert_eq!(again.len(), TOTAL);
    assert_eq!(again.late_records(), 0);

    // A record after finish is counted, not appended (e.1), and a flush after finish is a
    // no-op rather than a hang.
    writer.record(lean_record(TOTAL, 0));
    writer.record(lean_record(TOTAL + 1, 0));
    writer.flush().expect("a flush after finish");
    let after = writer.finish().expect("finish");
    assert_eq!(after.len(), TOTAL, "a late record is never appended");
    assert_eq!(after.late_records(), 2, "a late record is counted");

    let r = moruna_trace::RunReport::compute(
        &after,
        &common::limits(),
        &common::meta(moruna_trace::ExitReason::Cancelled),
    );
    assert_eq!(r.late_records, 2);
    assert!(format!("{r}").contains("late records 2"));
}
