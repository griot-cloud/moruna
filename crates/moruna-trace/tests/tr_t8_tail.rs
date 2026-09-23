//! TR-T8 tail. `tail(stage, n)` through `Arc<dyn TraceTail>` returns the newest n records
//! for that stage, oldest first, including records still in the builder's partial chunk.
//! Proves f.3.

mod common;

use std::sync::Arc;

use common::{TempDir, config, record};
use moruna_kernel::{TraceSink, TraceTail};
use moruna_trace::TraceWriter;

#[test]
fn tr_t8_tail() {
    let dir = TempDir::new("t8");
    let mut cfg = config(dir.path());
    // Small enough that the later half of the trace leaves memory, so the window is
    // demonstrably what memory holds and not what the file holds.
    cfg.memory_limit = 256 * 1024;
    let writer = TraceWriter::start(cfg).expect("start");
    // The controller never names this crate: it holds the writer as a trait object.
    let tail: Arc<dyn TraceTail> = writer.clone();

    assert!(tail.tail(1, 8).is_empty(), "an empty trace has no tail");

    // Fewer records than a chunk, so every one of them is still in the builders.
    for seq in 0..100 {
        writer.record(record(seq, 1));
        writer.record(record(seq, 2));
    }
    // The writer drains within its 100 ms tick; wait for the records to be appended.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while tail.tail(1, 32).len() < 32 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let got = tail.tail(1, 32);
    assert_eq!(got.len(), 32, "the partial chunk is part of the window");
    assert!(
        got.iter().all(|r| r.stage == 1),
        "the window is filtered by stage"
    );
    let seqs: Vec<u64> = got.iter().map(|r| r.seq).collect();
    assert_eq!(
        seqs,
        (68..100).collect::<Vec<u64>>(),
        "the newest n, oldest first"
    );

    // More records than fit in memory: the window is what memory holds, never the overflow.
    for seq in 100..20_000 {
        writer.record(record(seq, 1));
    }
    writer.flush().expect("flush");
    let got = tail.tail(1, 16);
    assert_eq!(got.len(), 16);
    assert_eq!(
        got.iter().map(|r| r.seq).collect::<Vec<u64>>(),
        (19_984..20_000).collect::<Vec<u64>>()
    );

    // A window larger than memory holds returns fewer records rather than reading the file.
    let huge = tail.tail(1, 10_000_000);
    assert!(
        (huge.len() as u64) < 20_000,
        "tail does not read the overflow file"
    );
    assert!(!huge.is_empty());
    assert_eq!(tail.tail(1, 0).len(), 0);
    assert!(tail.tail(9, 4).is_empty(), "a stage with no records");

    // The same walk is available on the view (d.1).
    let view = writer.finish().expect("finish");
    let from_view = view.tail(1, 4);
    assert_eq!(from_view.len(), 4);
    assert!(from_view.iter().all(|r| r.stage == 1));
    assert_eq!(
        from_view.iter().map(|r| r.seq).collect::<Vec<u64>>(),
        (19_996..20_000).collect::<Vec<u64>>()
    );
    assert!(view.tail(1, 0).is_empty());
    assert!(!view.is_empty());
    // A stage the trace never saw, and a window wider than the whole in-memory set, both
    // walk every chunk and return what there is.
    assert!(view.tail(9, 5).is_empty());
    assert!(view.tail(2, 1_000_000).len() < 20_000);
}
