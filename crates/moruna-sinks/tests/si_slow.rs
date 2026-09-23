//! SI-T11 slow_store: a store that takes its time changes how long a run takes and nothing
//! else (08 k, f.6).

mod common;

use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use moruna_kernel::{Reactor, Sink};
use moruna_sinks::{ParquetSink, ParquetSinkConfig};
use moruna_testkit::{FakeAllocator, FakeReactor};
use common::{Scratch, arena_payload, table_source_schema};

/// The file buffer a sink of this size holds: `file_bytes` plus room for the footer (f.1).
const FILE_BYTES: u64 = 32 << 10;
const FOOTER_HEADROOM: u64 = 1 << 20;

/// SI-T11. Writes against a store with a hundred milliseconds of latency complete correctly,
/// the reactor never holds more operations than the drive admitted, and the sink's arena
/// footprint stays inside one open file buffer plus the rolled files in flight. f.6.
#[test]
fn si_t11_slow_store() {
    let scratch = Scratch::new("t11");
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(100));
    let alloc = FakeAllocator::new();
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            row_group_bytes: 8 << 10,
            file_bytes: FILE_BYTES,
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");

    // The scheduler's sink drive admits `sink.concurrency` writes at a time (preamble 5).
    let concurrency = 2usize;
    let mut cx = Context::from_waker(Waker::noop());
    let mut in_flight_max = 0u64;
    let mut in_use_max = 0u64;
    let mut seq = 0u64;
    while seq < 12 {
        let mut batch: Vec<_> = (0..concurrency)
            .map(|i| {
                Some(sink.write(
                    seq + i as u64,
                    arena_payload(&alloc, 2_000, (seq + i as u64) as i64),
                ))
            })
            .collect();
        seq += concurrency as u64;
        while batch.iter().any(|f| f.is_some()) {
            for slot in batch.iter_mut() {
                let Some(future) = slot.as_mut() else {
                    continue;
                };
                if let Poll::Ready(outcome) = future.as_mut().poll(&mut cx) {
                    outcome.expect("a slow store still writes correctly");
                    *slot = None;
                }
            }
            in_flight_max = in_flight_max.max(reactor.in_flight());
            in_use_max = in_use_max.max(alloc.in_use(moruna_kernel::Tier::Host));
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let summary = sink.finish().expect("finish");

    assert!(
        in_flight_max <= concurrency as u64,
        "the reactor held {in_flight_max} operations, the drive admitted {concurrency}"
    );
    let ceiling = (1 + concurrency as u64) * (FILE_BYTES + FOOTER_HEADROOM) + (4 << 20);
    assert!(
        in_use_max <= ceiling,
        "the sink held {in_use_max} bytes, the bound is one open file buffer plus {concurrency} in flight ({ceiling})"
    );
    assert!(summary.files.len() > 1, "{:?}", summary.files);
    assert_eq!(summary.rows, 12 * 2_000);
    assert_eq!(sink.committed_seq(), Some(11));
    reactor.shutdown();
}
