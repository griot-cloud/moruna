//! SI-T33 parallel_encodes (RB1, `architecture/worker-io.md`): several workers encode into one
//! Parquet sink at once, each into a lane of its own; every row lands in exactly one committed,
//! readable file, and the watermark reaches the last sequence number.

mod common;

use std::sync::{Arc, Barrier};

use common::{CountingAlloc, Scratch, arena_payload, block_on, object_bytes, table_source_schema};
use moruna_kernel::{Allocator, Encoded, Sink};
use moruna_sinks::{ParquetSink, ParquetSinkConfig, WriterGrant};
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
    .expect("parquet sink")
    .with_lanes(THREADS as usize);
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

/// 6.3d: in a program whose allocator does not count the heap (this test binary installs
/// none), a sink with a writer heap estimates it from parquet's own figure, says so, and writes
/// every row.
#[test]
fn without_a_counting_allocator_the_writers_heap_is_estimated() {
    assert!(
        !moruna_kernel::held::heap_counting(),
        "this binary counts nothing"
    );
    let scratch = Scratch::new("lanes-estimated");
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("parquet sink")
    .with_lanes(1)
    .with_writer_heap(Some(8 << 20));
    sink.open(&table_source_schema()).expect("open");
    for seq in 0..20u64 {
        let encoded = sink
            .encode(seq, arena_payload(alloc.fake(), ROWS, seq as i64 * 1_000))
            .expect("encode");
        block_on(sink.put(encoded)).expect("put");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.rows, 20 * ROWS as u64);
    assert!(!sink.stats().writer_heap_measured, "estimated, and said so");
}

/// A sink over a grant, written with 20 morsels: its lanes and the files it wrote.
fn granted(name: &str, grant: &WriterGrant, set: Option<(Option<u64>, u64)>) -> (u64, usize) {
    let scratch = Scratch::new(name);
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            row_group_bytes: 16 << 10,
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("parquet sink")
    .with_lanes(4)
    .with_grant(grant.clone());
    if let Some((writers, file_buffers)) = set {
        grant.grant(writers, file_buffers);
    }
    sink.open(&table_source_schema()).expect("open");
    for seq in 0..20u64 {
        let encoded = sink
            .encode(seq, arena_payload(alloc.fake(), ROWS, seq as i64 * 1_000))
            .expect("encode");
        block_on(sink.put(encoded)).expect("put");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.rows, 20 * ROWS as u64);
    (sink.stats().lanes, summary.files.len())
}

/// 3A (6.3d): the run grants a sink its writers' heap and its file buffers once it knows the
/// sink's schema, before the sink opens, and the sink reads the grant at `open`. A grant not
/// set changes nothing; one lane's need is one lane; file buffers of a few KiB make small files.
#[test]
fn a_grant_set_before_open_gives_the_writers_and_the_file_buffers() {
    let unset = granted("grant-unset", &WriterGrant::new(), None);
    assert_eq!(unset.0, 4, "no grant: parquet's limits on every lane");
    let leaves = moruna_sinks::leaves_of(match &table_source_schema() {
        moruna_kernel::SourceSchema::Table(schema) => schema,
        _ => unreachable!(),
    })
    .expect("leaves");
    let zstd = parquet::basic::Compression::ZSTD(Default::default());
    let one = moruna_sinks::least_lane_bytes(&leaves, zstd);
    let grant = WriterGrant::new();
    assert_eq!((grant.writers(), grant.file_buffers()), (None, None));
    let lanes = granted("grant-one-lane", &grant, Some((Some(one), 1 << 30)));
    assert_eq!(lanes.0, 1, "one lane's need is one lane");
    assert_eq!(grant.writers(), Some(one));
    let small = granted(
        "grant-small-files",
        &WriterGrant::new(),
        Some((None, 64 << 10)),
    );
    assert_eq!(small.0, 4, "no writers' heap: parquet's limits");
    assert!(
        small.1 > unset.1,
        "small file buffers make more files: {} against {}",
        small.1,
        unset.1
    );
}
