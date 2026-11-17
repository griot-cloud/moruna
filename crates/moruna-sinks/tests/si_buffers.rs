//! Stream C (2026-11-17): a Parquet sink's file buffers are made as its files need them. A lane
//! makes its buffer when it opens its first file, starting small and growing with the file up
//! to its share, so a run that writes a few megabytes holds a few megabytes, a lane that never
//! opens a file holds nothing, and a buffer the arena cannot grow rolls its file at what it
//! holds. The sink used to take every lane's share at `open`: on the gate's eight lanes and
//! 1 GiB files that was 1 GiB of the arena, made before the first row, for 28 MB of output.

mod common;

use std::sync::Arc;

use common::{Scratch, arena_payload, block_on, object_bytes, table_schema, table_source_schema};
use moruna_kernel::arrow::array::{ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::datatypes::DataType;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{AllocStats, Allocator, Buffer, Payload, Sink, Tier};
use moruna_sinks::{ParquetSink, ParquetSinkConfig};
use moruna_testkit::{FakeAllocator, FakeReactor};
use parquet::file::reader::{FileReader, SerializedFileReader};

const MIB: u64 = 1 << 20;
/// The bytes a lane's buffer starts with (`file_buf::START_BYTES`).
const START: u64 = 8 * MIB;

fn sink(
    scratch: &Scratch,
    file_bytes: u64,
    reactor: &FakeReactor,
    alloc: Arc<dyn Allocator>,
) -> ParquetSink {
    ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            row_group_bytes: 128 * MIB,
            file_bytes,
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        alloc,
    )
    .expect("parquet sink")
}

/// The rows of every file the sink committed, read back.
fn rows_written(
    files: &[String],
    scratch: &Scratch,
    reactor: &FakeReactor,
    alloc: &FakeAllocator,
) -> i64 {
    files
        .iter()
        .map(|name| {
            let url = format!("{}/{}", scratch.url(), name);
            let len = reactor
                .ops()
                .iter()
                .rev()
                .find(|op| op.path_or_url == url)
                .map(|op| op.len as usize)
                .expect("the file was written");
            let bytes = bytes::Bytes::from(object_bytes(reactor, alloc, &url, len));
            let reader = SerializedFileReader::new(bytes).expect("a readable parquet file");
            reader.metadata().file_metadata().num_rows()
        })
        .sum()
}

/// Rows of about `bytes` decoded bytes: two Int64 columns.
fn rows_of(bytes: u64) -> usize {
    (bytes / 16) as usize
}

/// A table payload of `rows` rows of values no encoder compresses, over arena memory, so the
/// files are as large as their rows.
fn noisy_payload(alloc: &FakeAllocator, rows: usize, seed: u64) -> Payload {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut column = || -> ArrayRef {
        let mut bytes = Vec::with_capacity(rows * 8);
        for _ in 0..rows {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        let buffer = alloc.arrow_buffer(&bytes, alloc.host_tier());
        let data = ArrayData::builder(DataType::Int64)
            .len(rows)
            .add_buffer(buffer)
            .build()
            .expect("array data");
        make_array(data)
    };
    let batch = RecordBatch::try_new(table_schema(), vec![column(), column()]).expect("batch");
    Payload::table(batch).expect("payload")
}

#[test]
fn a_small_output_holds_one_small_buffer_and_open_holds_none() {
    let scratch = Scratch::new("buffers-small");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    // The gate's shape: 1 GiB files on eight lanes.
    let mut sink = sink(&scratch, 1 << 30, &reactor, Arc::new(alloc.clone())).with_lanes(8);
    sink.open(&table_source_schema()).expect("open");
    assert_eq!(alloc.in_use(Tier::Host), 0, "open takes nothing");
    for seq in 0..3 {
        block_on(sink.write(seq, arena_payload(&alloc, 1_000, seq as i64 * 1_000))).expect("write");
    }
    let held = alloc.in_use(Tier::Host);
    assert!(
        held <= START,
        "a few rows hold one small buffer, not a share: {held}"
    );
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.files.len(), 1);
    assert_eq!(
        rows_written(&summary.files, &scratch, &reactor, &alloc),
        3_000
    );
}

#[test]
fn a_large_output_grows_its_buffer_to_the_lanes_share_and_rolls_there() {
    let scratch = Scratch::new("buffers-large");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    // Two lanes of 32 MiB files: each lane's share is 32 MiB, four times the first buffer.
    let mut sink = sink(&scratch, 32 * MIB, &reactor, Arc::new(alloc.clone()));
    sink.open(&table_source_schema()).expect("open");
    let rows = rows_of(4 * MIB);
    let morsels = 12u64;
    let mut grew = false;
    for seq in 0..morsels {
        block_on(sink.write(seq, noisy_payload(&alloc, rows, seq + 1))).expect("write");
        grew |= alloc.in_use(Tier::Host) > START;
    }
    assert!(grew, "the buffer grew past its first size");
    assert_eq!(
        sink.stats().roll_bytes,
        32 * MIB - MIB,
        "the file rolls at its share"
    );
    let summary = sink.finish().expect("finish");
    assert!(summary.files.len() >= 2, "{:?}", summary.files);
    assert_eq!(
        rows_written(&summary.files, &scratch, &reactor, &alloc),
        (morsels * rows as u64) as i64
    );
}

#[test]
fn a_lane_that_never_opens_a_file_holds_nothing() {
    let scratch = Scratch::new("buffers-one-lane");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    let mut sink = sink(&scratch, 64 * MIB, &reactor, Arc::new(alloc.clone())).with_lanes(8);
    sink.open(&table_source_schema()).expect("open");
    // One worker encodes: one lane opens a file, the other seven never do.
    let mut encoded = Vec::new();
    for seq in 0..4 {
        encoded.push(
            sink.encode(seq, arena_payload(&alloc, 2_000, seq as i64 * 2_000))
                .expect("encode"),
        );
    }
    let held = alloc.in_use(Tier::Host);
    assert!(
        held <= START + 4 * MIB,
        "one lane's buffer, not eight: {held}"
    );
    for one in encoded {
        block_on(sink.put(one)).expect("put");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(
        rows_written(&summary.files, &scratch, &reactor, &alloc),
        8_000
    );
}

/// An allocator that refuses any buffer larger than `max`, as a full arena refuses the class a
/// growing buffer asks for.
struct NoLarger {
    inner: FakeAllocator,
    max: usize,
}

impl Allocator for NoLarger {
    fn available(&self, tier: Tier) -> Option<u64> {
        self.inner.available(tier)
    }
    fn alloc(&self, bytes: usize, tier: Tier) -> moruna_kernel::Result<Buffer> {
        if bytes > self.max {
            return Err(moruna_kernel::MorunaError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: self.max as u64,
                in_use: 0,
            });
        }
        self.inner.alloc(bytes, tier)
    }
    fn page_bytes(&self) -> usize {
        Allocator::page_bytes(&self.inner)
    }
    fn stats(&self) -> AllocStats {
        self.inner.stats()
    }
    fn contains(&self, ptr: *const u8) -> bool {
        self.inner.contains(ptr)
    }
    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        self.inner.tier_of(ptr)
    }
    fn is_pinned(&self) -> bool {
        self.inner.is_pinned()
    }
}

#[test]
fn a_buffer_the_arena_cannot_grow_rolls_its_file_at_what_it_holds() {
    let scratch = Scratch::new("buffers-no-growth");
    let reactor = FakeReactor::new();
    let fake = FakeAllocator::new();
    let alloc = Arc::new(NoLarger {
        inner: fake.clone(),
        max: START as usize,
    });
    let mut sink = sink(&scratch, 64 * MIB, &reactor, alloc);
    sink.open(&table_source_schema()).expect("open");
    let rows = rows_of(2 * MIB);
    let morsels = 10u64;
    for seq in 0..morsels {
        block_on(sink.write(seq, noisy_payload(&fake, rows, seq + 1)))
            .expect("a full buffer rolls its file rather than failing the run");
    }
    let summary = sink.finish().expect("finish");
    assert!(summary.files.len() >= 2, "{:?}", summary.files);
    assert_eq!(
        rows_written(&summary.files, &scratch, &reactor, &fake),
        (morsels * rows as u64) as i64
    );
}
