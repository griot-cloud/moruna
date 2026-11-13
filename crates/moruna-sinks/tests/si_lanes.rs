//! SI-T33 parallel_encodes (RB1, `architecture/worker-io.md`): several workers encode into one
//! Parquet sink at once, each into a lane of its own; every row lands in exactly one committed,
//! readable file, and the watermark reaches the last sequence number.

mod common;

use std::sync::{Arc, Barrier};

use common::{CountingAlloc, Scratch, arena_payload, block_on, object_bytes, table_source_schema};
use moruna_kernel::{Allocator, Encoded, Sink};
use moruna_sinks::{ParquetSink, ParquetSinkConfig};
use moruna_testkit::{FakeAllocator, FakeReactor};
use parquet::file::reader::{FileReader, SerializedFileReader};

const THREADS: u64 = 8;
const EACH: u64 = 25;
const ROWS: usize = 500;

#[test]
fn si_t33_parallel_encodes() {
    let scratch = Scratch::new("t33-lanes");
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            row_group_bytes: 64 << 10,
            file_bytes: 1 << 20,
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");
    assert!(
        sink.encodes_on_workers(),
        "RB1: the parquet sink encodes on workers"
    );

    // Every thread waits at the barrier so the encodes overlap, which is what makes the sink
    // open more than one lane.
    let barrier = Barrier::new(THREADS as usize);
    let encoded: Vec<Encoded> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|thread| {
                let sink = &sink;
                let barrier = &barrier;
                let alloc = &alloc;
                scope.spawn(move || {
                    barrier.wait();
                    (0..EACH)
                        .map(|i| {
                            let seq = thread * EACH + i;
                            let payload = arena_payload(alloc.fake(), ROWS, seq as i64 * 1_000);
                            sink.encode(seq, payload).expect("encode")
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("an encoder thread"))
            .collect()
    });
    for one in encoded {
        block_on(sink.put(one)).expect("put");
    }
    let summary = sink.finish().expect("finish");
    let total = THREADS * EACH * ROWS as u64;

    assert_eq!(summary.rows, total, "every row is in a committed file");
    assert_eq!(
        sink.committed_seq(),
        Some(THREADS * EACH - 1),
        "the watermark reaches the last morsel"
    );
    let mut read_back = 0i64;
    for name in &summary.files {
        let url = format!("{}/{}", scratch.url(), name);
        let len = reactor
            .ops()
            .iter()
            .find(|op| op.path_or_url == url)
            .map(|op| op.len as usize)
            .expect("the file was written");
        let bytes = bytes::Bytes::from(object_bytes(&reactor, alloc.fake(), &url, len));
        let reader = SerializedFileReader::new(bytes).expect("a readable parquet file");
        read_back += reader.metadata().file_metadata().num_rows();
    }
    assert_eq!(
        read_back as u64, total,
        "the files hold exactly the rows written"
    );
}

/// A write through the drive's own path (`write`, used when no worker takes the encode) is the
/// same encode and put, so one sink serves both.
#[test]
fn si_t33_write_is_encode_then_put() {
    let scratch = Scratch::new("t33-write");
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            row_group_bytes: 64 << 10,
            file_bytes: 1 << 20,
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");
    block_on(sink.write(0, arena_payload(alloc.fake(), ROWS, 0))).expect("write");
    let encoded = sink
        .encode(1, arena_payload(alloc.fake(), ROWS, 1))
        .expect("encode");
    block_on(sink.put(encoded)).expect("put");
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.rows, 2 * ROWS as u64);
    assert_eq!(sink.committed_seq(), Some(1));
}
