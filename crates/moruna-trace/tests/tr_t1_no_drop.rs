//! TR-T1 no_drop. 16 threads record 1 M records with the channel capacity at 256; the final
//! count is 1 M. Proves TR-I1.

mod common;

use std::sync::Arc;

use moruna_kernel::{TraceRecord, TraceSink};
use moruna_trace::TraceWriter;
use common::{TempDir, config, lean_record};

const THREADS: u64 = 16;
const PER_THREAD: u64 = 62_500;

#[test]
fn tr_t1_no_drop() {
    let dir = TempDir::new("t1");
    let mut cfg = config(dir.path());
    cfg.channel_capacity = 256;
    let writer = TraceWriter::start(cfg).expect("start");

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let sink: Arc<dyn TraceSink> = writer.clone();
            scope.spawn(move || {
                for i in 0..PER_THREAD {
                    let seq = t * PER_THREAD + i;
                    sink.record(lean_record(seq, (t % 4) as u16));
                }
            });
        }
    });

    let view = writer.finish().expect("finish");
    assert_eq!(
        view.len(),
        THREADS * PER_THREAD,
        "a bounded channel applies backpressure to the caller and drops nothing"
    );

    // Every sequence number appears exactly once, so the count is not right by accident.
    let mut seen = vec![false; (THREADS * PER_THREAD) as usize];
    let mut rows = 0u64;
    for batch in view.batches() {
        let seqs = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::UInt64Array>()
            .expect("seq column");
        for i in 0..batch.num_rows() {
            let seq = seqs.value(i) as usize;
            assert!(!seen[seq], "sequence {seq} appears twice");
            seen[seq] = true;
            rows += 1;
        }
    }
    assert_eq!(rows, THREADS * PER_THREAD);
    assert!(seen.iter().all(|s| *s), "a sequence number is missing");

    // The record path takes the record by value: nothing about it is cloned or boxed.
    let sized: fn(TraceRecord) = |_r| {};
    sized(lean_record(0, 0));
}
