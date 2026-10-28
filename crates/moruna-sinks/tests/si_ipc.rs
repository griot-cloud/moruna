//! `ArrowIpcSink`: SI-T1, SI-T4 and SI-T9 (08 k).

mod common;

use std::sync::Arc;

use common::{
    CountingAlloc, Scratch, arena_batch, arena_payload, block_on, table_source_schema, written,
};
use moruna_kernel::arrow::ipc::reader::FileReader;
use moruna_kernel::arrow::ipc::root_as_footer;
use moruna_kernel::{Allocator, MorunaError, Sink, Tier, ipc};
use moruna_sinks::{ArrowIpcSink, ArrowIpcSinkConfig};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

const PAGE: u64 = 4096;

fn new_sink(scratch: &Scratch, file_bytes: u64) -> (ArrowIpcSink, FakeReactor, Arc<CountingAlloc>) {
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let sink = ArrowIpcSink::new(
        ArrowIpcSinkConfig {
            path: scratch.path().to_path_buf(),
            file_bytes,
        },
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("arrow ipc sink");
    (sink, reactor, alloc)
}

/// SI-T1 for `ArrowIpcSink`. The payload is dropped after the last completion over its buffers
/// resolves, never before, so the arena gets its bytes back exactly once. SI-I1.
#[test]
fn si_t1_ownership_once_ipc() {
    let scratch = Scratch::new("t1-ipc");
    let (mut sink, reactor, alloc) = new_sink(&scratch, 1 << 30);
    sink.open(&table_source_schema()).expect("open");
    let baseline = alloc.fake().in_use(Tier::Host);

    let payload = arena_payload(alloc.fake(), 512, 0);
    let bytes = payload.bytes();
    assert!(alloc.fake().in_use(Tier::Host) > baseline);
    block_on(sink.write(0, payload)).expect("write");
    assert_eq!(
        alloc.fake().in_use(Tier::Host),
        baseline,
        "the payload's {bytes} bytes were still held after write resolved"
    );
    for op in reactor.ops() {
        assert!(
            op.t_resolve.is_some(),
            "every write over the payload's buffers resolved before the drop: {op:?}"
        );
    }
    sink.finish().expect("finish");
}

/// SI-T4 for `ArrowIpcSink`. The sink copies no payload bytes: nothing is noted against the
/// allocator, `encode_bytes` is zero, and every write is either the framing or one of the
/// payload's own buffers. SI-I4, G-I2.
#[test]
fn si_t4_encode_only_copy_ipc() {
    let scratch = Scratch::new("t4-ipc");
    let (mut sink, reactor, alloc) = new_sink(&scratch, 1 << 30);
    sink.open(&table_source_schema()).expect("open");

    let mut body_lengths = Vec::new();
    for seq in 0..4 {
        let batch = arena_batch(alloc.fake(), 256, seq as i64);
        for column in batch.columns() {
            for buffer in column.to_data().buffers() {
                body_lengths.push(buffer.len() as u64);
            }
        }
        block_on(sink.write(seq, moruna_kernel::Payload::table(batch).expect("payload")))
            .expect("write");
    }
    sink.finish().expect("finish");

    assert_eq!(alloc.payload_copies(), 0, "the ipc sink copies nothing");
    assert_eq!(sink.stats().encode_bytes, 0);
    let writes: Vec<u64> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteFile)
        .map(|op| op.len)
        .collect();
    for length in &body_lengths {
        assert!(
            writes.contains(length),
            "no write moved a payload buffer of {length} bytes"
        );
    }
    for op in reactor.ops() {
        assert_eq!(op.src_tier, Some(Tier::Host), "{op:?}");
    }
}

/// SI-T9. Every body lands on a page boundary, `arrow`'s own reader reads the file back equal,
/// the contracts crate's decoder reads each record without copying, and a file with two records
/// carries one Schema message. e.3, f.2.
#[test]
fn si_t9_ipc_page_aligned() {
    let scratch = Scratch::new("t9");
    let (mut sink, reactor, alloc) = new_sink(&scratch, 1 << 30);
    sink.open(&table_source_schema()).expect("open");

    let inputs: Vec<_> = (0..2)
        .map(|seq| arena_batch(alloc.fake(), 300, seq as i64 * 1_000))
        .collect();
    for (seq, batch) in inputs.iter().enumerate() {
        block_on(sink.write(
            seq as u64,
            moruna_kernel::Payload::table(batch.clone()).expect("payload"),
        ))
        .expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.files, vec!["part-00000.arrow".to_string()]);

    let bytes = written(&reactor, scratch.path(), "part-00000.arrow");

    // The bodies were written at page boundaries and the framings were not (f.2, e.3).
    let framing_writes: Vec<_> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteFile && op.offset >= PAGE)
        .collect();
    assert!(!framing_writes.is_empty());

    // `arrow`'s own file reader reads the batches back equal.
    let reader = FileReader::try_new(std::io::Cursor::new(bytes.clone()), None)
        .expect("arrow reads the file back");
    let read: Vec<_> = reader.map(|b| b.expect("record batch")).collect();
    assert_eq!(read.len(), 2);
    for (expected, found) in inputs.iter().zip(read.iter()) {
        assert_eq!(expected, found);
    }

    // One Schema message: the first record wrote the whole framing, the second only its
    // record batch message, which is strictly shorter (f.2).
    let footer_len = u32::from_le_bytes([
        bytes[bytes.len() - 10],
        bytes[bytes.len() - 9],
        bytes[bytes.len() - 8],
        bytes[bytes.len() - 7],
    ]) as usize;
    let footer = &bytes[bytes.len() - 10 - footer_len..bytes.len() - 10];
    let parsed = root_as_footer(footer).expect("the footer parses");
    let blocks = parsed.recordBatches().expect("record batch blocks");
    assert_eq!(blocks.len(), 2);
    let schema_len = blocks.get(0).offset() as u64 - PAGE;
    assert!(
        schema_len > 0,
        "the first record carries the schema message"
    );
    assert_eq!(
        blocks.get(1).offset() as u64 % PAGE,
        schema_len % PAGE,
        "a later record's base is page aligned and its message sits one schema length in"
    );

    // Each record decodes through the contracts crate's decoder into arrays that point into
    // the buffer it was read into, with no copy (contracts e.7, CT-T18's check).
    for (i, expected) in inputs.iter().enumerate().take(blocks.len()) {
        let block = blocks.get(i);
        let base = block.offset() as u64 - schema_len;
        let end = block.offset() as u64 + block.metaDataLength() as u64 + block.bodyLength() as u64;
        let span = (end - base) as usize;
        let mut buffer = alloc
            .fake()
            .buffer(span.next_multiple_of(PAGE as usize), Tier::Host);
        buffer[..schema_len as usize]
            .copy_from_slice(&bytes[PAGE as usize..(PAGE + schema_len) as usize]);
        buffer[schema_len as usize..span]
            .copy_from_slice(&bytes[(base + schema_len) as usize..end as usize]);
        let arrow_buffer = buffer.into_arrow_buffer().expect("arrow buffer");
        let low = arrow_buffer.as_ptr() as usize;
        let high = low + arrow_buffer.len();
        let batch = ipc::decode(arrow_buffer, PAGE as usize).expect("the record decodes");
        assert_eq!(&batch, expected);
        for column in batch.columns() {
            for buffer in column.to_data().buffers() {
                let at = buffer.as_ptr() as usize;
                assert!(
                    at >= low && at < high,
                    "column {i} was copied out of the record buffer"
                );
            }
        }
    }
}

/// Files roll at `file_bytes` and each is committed whole (e.3, SI-I3).
#[test]
fn ipc_files_roll_and_commit() {
    let scratch = Scratch::new("ipc-roll");
    let (mut sink, reactor, alloc) = new_sink(&scratch, 64 << 10);
    sink.open(&table_source_schema()).expect("open");
    for seq in 0..12 {
        block_on(sink.write(seq, arena_payload(alloc.fake(), 512, seq as i64))).expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert!(summary.files.len() > 1, "{:?}", summary.files);
    assert_eq!(sink.stats().rolls as usize, summary.files.len() - 1);
    for name in &summary.files {
        let bytes = written(&reactor, scratch.path(), name);
        FileReader::try_new(std::io::Cursor::new(bytes), None).expect("a committed file is valid");
    }
    assert_eq!(sink.committed_seq(), Some(11));
}

/// A failed write moves the sink to `Failed`, and `finish` returns the original error naming
/// the files that were committed before it (h, SI-I3).
#[test]
fn a_failed_ipc_write_is_reported_by_finish() {
    let scratch = Scratch::new("ipc-fail");
    let reactor = FakeReactor::new().fail_next(OpKind::WriteFile, 8);
    let alloc = FakeAllocator::new();
    let mut sink = ArrowIpcSink::new(
        ArrowIpcSinkConfig {
            path: scratch.path().to_path_buf(),
            file_bytes: 1 << 30,
        },
        Arc::new(reactor),
        Arc::new(alloc.clone()),
    )
    .expect("arrow ipc sink");
    // The magic write at `open` is the first refusal.
    let outcome = sink.open(&table_source_schema());
    assert!(
        matches!(outcome, Err(MorunaError::Io { .. })),
        "{outcome:?}"
    );

    let scratch = Scratch::new("ipc-fail-write");
    let reactor = FakeReactor::new();
    let mut sink = ArrowIpcSink::new(
        ArrowIpcSinkConfig {
            path: scratch.path().to_path_buf(),
            file_bytes: 1 << 30,
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("arrow ipc sink");
    sink.open(&table_source_schema()).expect("open");
    let reactor = reactor.fail_next(OpKind::WriteFile, 1);
    let outcome = block_on(sink.write(0, arena_payload(&alloc, 64, 0)));
    assert!(
        matches!(outcome, Err(MorunaError::Io { .. })),
        "{outcome:?}"
    );
    let _ = reactor;
    let outcome = sink.finish();
    let Err(MorunaError::Sink(msg)) = outcome else {
        panic!("finish must return the original error, got {outcome:?}");
    };
    assert!(msg.contains("committed files:"), "{msg}");
}

/// A batch whose buffers have no 8-byte multiple of bytes: a nullable `Int32`, a string, a
/// boolean and a list of `Int32`, so the last buffer of the body is unpadded for most `rows`.
fn ragged_batch(rows: usize, base: i32) -> moruna_kernel::arrow::record_batch::RecordBatch {
    use moruna_kernel::arrow::array::{
        ArrayRef, BooleanArray, Int32Array, ListArray, StringArray, types::Int32Type,
    };
    use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
    let ints: ArrayRef = Arc::new(Int32Array::from_iter(
        (0..rows).map(|i| (i % 4 != 0).then_some(base + i as i32)),
    ));
    let text: ArrayRef = Arc::new(StringArray::from_iter_values(
        (0..rows).map(|i| format!("row {base}-{i}")),
    ));
    let flags: ArrayRef = Arc::new(BooleanArray::from_iter((0..rows).map(|i| Some(i % 3 == 0))));
    let lists: ArrayRef = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(
        (0..rows).map(|i| Some((0..(i % 3)).map(|j| Some(j as i32)).collect::<Vec<_>>())),
    ));
    let schema = Arc::new(Schema::new(vec![
        Field::new("n", DataType::Int32, true),
        Field::new("s", DataType::Utf8, false),
        Field::new("f", DataType::Boolean, false),
        Field::new("l", lists.data_type().clone(), true),
    ]));
    moruna_kernel::arrow::record_batch::RecordBatch::try_new(schema, vec![ints, text, flags, lists])
        .expect("ragged batch")
}

/// SI-T27. Every block of the footer is what the Arrow IPC file format requires and what
/// pyarrow (Arrow C++) refuses a file for lacking: an offset, a `metaDataLength` and a
/// `bodyLength` that are multiples of 8, and an offset that points at a continuation marker.
/// Row counts that are odd make the last buffer of a body unpadded, which is where the lengths
/// went wrong. e.3, f.2, KA11.
#[test]
fn si_t27_ipc_blocks_are_eight_aligned() {
    for rows in [3usize, 7, 301, 1001, 1] {
        let scratch = Scratch::new("t27");
        let (mut sink, reactor, _alloc) = new_sink(&scratch, 1 << 30);
        sink.open(&table_source_schema()).expect("open");
        for seq in 0..5u64 {
            let batch = ragged_batch(rows, seq as i32 * 7);
            block_on(sink.write(seq, moruna_kernel::Payload::table(batch).expect("payload")))
                .expect("write");
        }
        sink.finish().expect("finish");
        let bytes = written(&reactor, scratch.path(), "part-00000.arrow");

        let footer_len = u32::from_le_bytes([
            bytes[bytes.len() - 10],
            bytes[bytes.len() - 9],
            bytes[bytes.len() - 8],
            bytes[bytes.len() - 7],
        ]) as usize;
        assert_eq!(&bytes[bytes.len() - 6..], b"ARROW1");
        let footer = &bytes[bytes.len() - 10 - footer_len..bytes.len() - 10];
        let parsed = root_as_footer(footer).expect("the footer parses");
        let blocks = parsed.recordBatches().expect("record batch blocks");
        assert_eq!(blocks.len(), 5);
        for i in 0..blocks.len() {
            let block = blocks.get(i);
            let at = block.offset() as usize;
            assert_eq!(block.offset() % 8, 0, "rows {rows} block {i}: offset");
            assert_eq!(
                block.metaDataLength() % 8,
                0,
                "rows {rows} block {i}: metaDataLength"
            );
            assert_eq!(
                block.bodyLength() % 8,
                0,
                "rows {rows} block {i}: bodyLength"
            );
            assert_eq!(
                &bytes[at..at + 4],
                &[0xff; 4],
                "rows {rows} block {i}: the offset is not at a continuation marker"
            );
            // Arrow C++ holds the block to the message's own body length.
            let message = moruna_kernel::arrow::ipc::root_as_message(
                &bytes[at + 8..at + block.metaDataLength() as usize],
            )
            .expect("the record batch message parses");
            assert_eq!(
                message.bodyLength(),
                block.bodyLength(),
                "rows {rows} block {i}: the message and the block disagree about the body"
            );
            let end = at + block.metaDataLength() as usize + block.bodyLength() as usize;
            assert!(
                end <= bytes.len() - 10 - footer_len,
                "block {i} runs into the footer"
            );
        }
        let reader = FileReader::try_new(std::io::Cursor::new(bytes), None).expect("arrow reads");
        assert_eq!(
            reader.map(|b| b.expect("batch").num_rows()).sum::<usize>(),
            rows * 5
        );
    }
}
