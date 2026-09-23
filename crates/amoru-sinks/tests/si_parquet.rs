//! `ParquetSink`: SI-T1, SI-T2, SI-T3, SI-T4, SI-T7, SI-T8, SI-T16 (08 k).

mod common;

use std::sync::Arc;

use amoru_kernel::{Allocator, AmoruError, Sink, Tier};
use amoru_sinks::{ParquetSink, ParquetSinkConfig};
use amoru_testkit::{FakeAllocator, FakeReactor, OpKind};
use common::{CountingAlloc, Scratch, arena_payload, block_on, object_bytes, table_source_schema};

fn config(scratch: &Scratch, file_bytes: u64) -> ParquetSinkConfig {
    ParquetSinkConfig {
        url: scratch.url(),
        row_group_bytes: 64 << 10,
        file_bytes,
        ..ParquetSinkConfig::default()
    }
}

fn new_sink(scratch: &Scratch, file_bytes: u64) -> (ParquetSink, FakeReactor, Arc<CountingAlloc>) {
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let sink = ParquetSink::new(
        config(scratch, file_bytes),
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("parquet sink");
    (sink, reactor, alloc)
}

/// SI-T1. After `write` resolves the payload's arena bytes are back with the arena, net of the
/// file buffer, and the future resolved exactly once. SI-I1.
#[test]
fn si_t1_ownership_once() {
    let scratch = Scratch::new("t1-parquet");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, 1 << 20);
    sink.open(&table_source_schema()).expect("open");
    // The file buffer arrives with the first payload (f.1), so the baseline is taken after it.
    block_on(sink.write(0, arena_payload(alloc.fake(), 8, 0))).expect("the first morsel");
    let baseline = alloc.fake().in_use(Tier::Host);

    let payload = arena_payload(alloc.fake(), 1_000, 1);
    let bytes = payload.bytes();
    assert!(alloc.fake().in_use(Tier::Host) > baseline);
    let future = sink.write(1, payload);
    assert!(block_on(future).is_ok());
    assert_eq!(
        alloc.fake().in_use(Tier::Host),
        baseline,
        "the payload's {bytes} bytes are still held after write resolved"
    );
    sink.finish().expect("finish");
}

/// SI-T2. `write` after `finish` and a second `finish` are errors. SI-I2.
#[test]
fn si_t2_finish_once() {
    let scratch = Scratch::new("t2");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, 1 << 20);
    sink.open(&table_source_schema()).expect("open");
    block_on(sink.write(0, arena_payload(alloc.fake(), 8, 0))).expect("write");
    sink.finish().expect("finish");

    let after = block_on(sink.write(1, arena_payload(alloc.fake(), 8, 0)));
    assert!(matches!(after, Err(AmoruError::Sink(_))), "{after:?}");
    assert!(matches!(sink.finish(), Err(AmoruError::Sink(_))));

    // A sink that was never opened refuses both as well (e.1).
    let scratch = Scratch::new("t2-created");
    let (mut fresh, _r, a) = new_sink(&scratch, 1 << 20);
    assert!(block_on(fresh.write(0, arena_payload(a.fake(), 8, 0))).is_err());
    assert!(fresh.finish().is_err());
    assert!(fresh.open(&table_source_schema()).is_ok());
    assert!(fresh.open(&table_source_schema()).is_err());
}

/// SI-T3. A failed object write leaves no complete file behind: the sink fails, `finish`
/// returns the original error naming the committed files, and no later operation resolved for
/// the file that failed. SI-I3.
#[test]
fn si_t3_no_partial_final() {
    let scratch = Scratch::new("t3");
    let reactor = FakeReactor::new().fail_next(OpKind::WriteObject, 1);
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut sink = ParquetSink::new(
        config(&scratch, 16 << 10),
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");

    let before = alloc.fake().in_use(Tier::Host);
    let mut failed = None;
    for seq in 0..64 {
        if let Err(e) = block_on(sink.write(seq, arena_payload(alloc.fake(), 4_000, seq as i64))) {
            failed = Some(e);
            break;
        }
    }
    let failed = failed.expect("a roll must have been attempted and refused");
    assert!(matches!(failed, AmoruError::Io { .. }), "{failed:?}");

    let outcome = sink.finish();
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("finish must return the original error, got {outcome:?}");
    };
    assert!(msg.contains("committed files:"), "{msg}");
    assert!(
        alloc.fake().in_use(Tier::Host) <= before,
        "the rolled file's arena buffer was not released"
    );
    let resolved: Vec<_> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteObject && op.path_or_url.ends_with(".parquet"))
        .collect();
    assert_eq!(resolved.len(), 1, "only the refused write was issued");
    assert!(scratch.names().is_empty(), "no final file exists");
}

/// SI-T4. The encode is the only CPU copy a Parquet sink makes: one `note_payload_copy` per
/// morsel, `encode_bytes` equal to the input bytes, and every object write out of the host
/// tier. SI-I4, G-I2.
#[test]
fn si_t4_encode_only_copy() {
    let scratch = Scratch::new("t4");
    let (mut sink, reactor, alloc) = new_sink(&scratch, 1 << 20);
    sink.open(&table_source_schema()).expect("open");

    let mut input = 0u64;
    for seq in 0..8 {
        let payload = arena_payload(alloc.fake(), 512, seq as i64);
        input += payload.bytes();
        block_on(sink.write(seq, payload)).expect("write");
    }
    sink.finish().expect("finish");

    assert_eq!(alloc.payload_copies(), 8, "one encode copy per morsel");
    assert_eq!(alloc.payload_copy_bytes(), input);
    assert_eq!(sink.stats().encode_bytes, input);
    assert_eq!(sink.stats().writes, 8);
    assert_eq!(sink.stats().multipart_parts, 0, "the reactor owns parts");
    for op in reactor.ops() {
        if op.kind == OpKind::WriteObject {
            assert_eq!(op.src_tier, Some(Tier::Host), "{op:?}");
        }
    }
}

/// SI-T7. Rows, bytes and the file list match what was written. SI-I7.
#[test]
fn si_t7_summary_exact() {
    let scratch = Scratch::new("t7");
    let (mut sink, reactor, alloc) = new_sink(&scratch, 32 << 10);
    sink.open(&table_source_schema()).expect("open");

    let mut rows = 0u64;
    for seq in 0..24 {
        let payload = arena_payload(alloc.fake(), 1_000, seq as i64);
        rows += payload.rows();
        block_on(sink.write(seq, payload)).expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.rows, rows);
    assert!(
        summary.files.len() > 1,
        "the run rolled: {:?}",
        summary.files
    );
    for (i, name) in summary.files.iter().enumerate() {
        assert_eq!(name, &format!("part-{i:05}.parquet"));
    }
    let written: u64 = reactor
        .ops()
        .iter()
        .filter(|op| op.kind == OpKind::WriteObject && op.path_or_url.ends_with(".parquet"))
        .map(|op| op.len)
        .sum();
    assert_eq!(
        summary.bytes, written,
        "summary bytes are the bytes written"
    );
    assert_eq!(sink.stats().files_committed as usize, summary.files.len());

    // Every file is a readable Parquet file whose footer carries its sequence range (e.2).
    let url = format!("{}/{}", scratch.url(), summary.files[0]);
    let len = reactor
        .ops()
        .iter()
        .find(|op| op.path_or_url == url)
        .map(|op| op.len as usize)
        .expect("the first file was written");
    let bytes = object_bytes(&reactor, alloc.fake(), &url, len);
    let reader = parquet_metadata(&bytes);
    let kv = reader
        .file_metadata()
        .key_value_metadata()
        .expect("footer metadata");
    assert!(kv.iter().any(|k| k.key == "amoru.run_id"));
    assert!(kv.iter().any(|k| k.key == "amoru.seq_min"));
    assert!(kv.iter().any(|k| k.key == "amoru.seq_max"));
}

fn parquet_metadata(bytes: &[u8]) -> parquet::file::metadata::ParquetMetaData {
    use parquet::file::reader::FileReader;
    let reader = parquet::file::serialized_reader::SerializedFileReader::new(
        bytes::Bytes::copy_from_slice(bytes),
    )
    .expect("parquet file");
    reader.metadata().clone()
}

/// SI-T8. (integration, closes in wave 3) Files read back through `ParquetSource` equal the
/// input batches and the row group and file sizes are within 10% of their targets. e.2.
#[test]
#[ignore = "integration, closes in wave 3: ParquetSource is component 7"]
fn si_t8_parquet_roundtrip() {
    unimplemented!("needs amoru-sources ParquetSource, which lands in the same wave");
}

/// SI-T16. The encoder's output lands in arena memory: one buffer per open file, one
/// `write_object` per roll out of the host tier, and a budget too small to hold a file buffer
/// fails at `open` rather than mid-run. f.1.
#[test]
fn si_t16_parquet_encodes_into_arena() {
    let scratch = Scratch::new("t16");
    let reactor = FakeReactor::new();
    let fake = FakeAllocator::new().with_limit(Tier::Host, 2 << 30);
    let alloc = CountingAlloc::new(fake);
    let mut sink = ParquetSink::new(
        config(&scratch, 32 << 10),
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("parquet sink");

    let before = alloc.fake().allocations_total();
    sink.open(&table_source_schema()).expect("open");
    assert_eq!(
        alloc.fake().allocations_total(),
        before,
        "f.1: `open` allocates nothing; the writer waits for the first payload's schema"
    );

    for seq in 0..24 {
        block_on(sink.write(seq, arena_payload(alloc.fake(), 1_000, seq as i64))).expect("write");
    }
    let summary = sink.finish().expect("finish");
    let rolls: Vec<_> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteObject && op.path_or_url.ends_with(".parquet"))
        .collect();
    assert_eq!(
        rolls.len(),
        summary.files.len(),
        "one write_object per file"
    );
    for op in &rolls {
        assert_eq!(op.src_tier, Some(Tier::Host));
        assert!(op.len > 0);
    }

    // A host budget under one row group is an `Alloc` at open, not a surprise mid-run (f.1).
    let scratch = Scratch::new("t16-budget");
    let tight = FakeAllocator::new().with_limit(Tier::Host, 8 << 20);
    let tight_fake = tight.clone();
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            file_bytes: 1 << 30,
            row_group_bytes: 128 << 20,
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(tight),
    )
    .expect("parquet sink");
    sink.open(&table_source_schema())
        .expect("open takes no memory");
    // The buffer arrives with the first payload (f.1), and that is where the budget is met.
    let outcome = block_on(sink.write(0, arena_payload(&tight_fake, 8, 0)));
    assert!(
        matches!(outcome, Err(AmoruError::Alloc { .. })),
        "a sink that cannot hold one row group cannot encode one, got {outcome:?}"
    );
}

/// SI-T16, f.1. The file buffer is one whole size class of 02 e.2 with the footer inside it, so
/// a default sink opens in the 128 MiB class and not the 256 MiB one.
///
/// Until 2026-09-23 it asked for `row_group_bytes + 1 MiB of footer`, which is 129 MiB at the
/// default and is served out of the 256 MiB class: the sink reserved twice what it wanted, that
/// class had to be free all at once, and no 512 MiB budget could open it (PM, 2026-09-23).
#[test]
fn si_t16_the_file_buffer_is_one_size_class() {
    let scratch = Scratch::new("t16-class");
    let row_group: u64 = 128 << 20;
    // Exactly the 129 MiB the sink used to ask for, so a sink that still asked for
    // `row_group_bytes + footer` could not open here once the payload is in hand.
    let limit = FakeAllocator::new().with_limit(Tier::Host, row_group + (1 << 20));
    let fake = limit.clone();
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            file_bytes: 1 << 30,
            row_group_bytes: row_group,
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(limit),
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");
    block_on(sink.write(0, arena_payload(&fake, 8, 0))).expect("the class fits, so the sink opens");

    let roll = sink.stats().roll_bytes;
    let requested = roll + (1 << 20);
    assert_eq!(
        requested, row_group,
        "f.1: the request is the {row_group} byte class with the footer inside it, not {requested}"
    );
    assert!(
        requested.is_power_of_two(),
        "{requested} is not a size class"
    );
    sink.finish().expect("finish");
}

/// Zero writes then `finish` still produces one file with the schema and the marker (h).
#[test]
fn empty_run_writes_one_file_and_the_marker() {
    let scratch = Scratch::new("empty");
    let (mut sink, reactor, _alloc) = new_sink(&scratch, 1 << 20);
    sink.open(&table_source_schema()).expect("open");
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.rows, 0);
    assert_eq!(summary.files, vec!["part-00000.parquet".to_string()]);
    assert!(
        reactor
            .ops()
            .iter()
            .any(|op| op.path_or_url.ends_with("_SUCCESS")),
        "the marker is written after every file is committed"
    );
}

/// A morsel whose schema is not the one the sink opened with is refused by name (h).
#[test]
fn schema_drift_names_the_field() {
    use amoru_kernel::Payload;
    use amoru_kernel::arrow::array::{ArrayData, make_array};
    use amoru_kernel::arrow::datatypes::{DataType, Field, Schema};
    use amoru_kernel::arrow::record_batch::RecordBatch;

    let scratch = Scratch::new("drift");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, 1 << 20);
    sink.open(&table_source_schema()).expect("open");
    // f.1: the first morsel settles the output schema, so the drift is the second morsel's.
    block_on(sink.write(0, arena_payload(alloc.fake(), 8, 0))).expect("the first morsel");

    let buffer = alloc.fake().arrow_buffer(&[0u8; 32], Tier::Host);
    let data = ArrayData::builder(DataType::Int32)
        .len(8)
        .add_buffer(buffer)
        .build()
        .expect("array");
    let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int32, false)]));
    let batch = RecordBatch::try_new(schema, vec![make_array(data)]).expect("batch");
    let outcome = block_on(sink.write(1, Payload::table(batch).expect("payload")));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("schema drift must be refused, got {outcome:?}");
    };
    assert!(msg.contains("schema drift"), "{msg}");
    assert!(msg.contains('a'), "{msg}");
}

/// A sink whose destination is a store this crate cannot enumerate says so rather than
/// guessing, which is what a write-only credential looks like (f.8).
#[test]
fn resume_refuses_a_destination_it_cannot_list() {
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: "s3://bucket/run".to_string(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor),
        Arc::new(alloc),
    )
    .expect("parquet sink");
    let state = br#"{"version":1,"kind":"parquet","next_index":0,"committed":[]}"#;
    let outcome = sink.resume(&table_source_schema(), state, None);
    let Err(AmoruError::Resume(msg)) = outcome else {
        panic!("a store that cannot be listed must refuse, got {outcome:?}");
    };
    assert_eq!(msg, "cannot list destination");
}

/// SI-T14. A sink with the default `file_bytes` of 1 GiB opens inside a 256 MiB arena, rolls
/// several files and finishes: `sink.file_bytes` is an upper bound on the file, not a
/// reservation the arena has to fit before the run can start (08 f.1).
#[test]
fn si_t14_opens_inside_a_small_arena() {
    const ARENA: u64 = 256 << 20;
    let scratch = Scratch::new("t14-small-arena");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new().with_limit(Tier::Host, ARENA);
    let defaults = ParquetSinkConfig::default();
    let file_bytes = defaults.file_bytes;
    assert_eq!(file_bytes, 1 << 30, "the preamble's default");
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            // A row group small enough that several files roll in a test's worth of rows.
            row_group_bytes: 256 << 10,
            ..defaults
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");

    // Most of the arena is morsels, as it is in a real run: the sink gets what is left, and
    // that is what its files roll at.
    let held = alloc
        .alloc((248 << 20) as usize, Tier::Host)
        .expect("morsels in flight");

    sink.open(&table_source_schema())
        .expect("open inside 256 MiB");
    block_on(sink.write(0, arena_payload(&alloc, 8, 0))).expect("the first morsel");
    assert!(
        alloc.in_use(Tier::Host) < file_bytes,
        "the sink reserved {} bytes for a {file_bytes} byte file",
        alloc.in_use(Tier::Host)
    );
    let roll_bytes = sink.stats().roll_bytes;
    assert!(
        roll_bytes > 0 && roll_bytes < file_bytes,
        "f.1: the file rolls at {roll_bytes}, below the {file_bytes} byte upper bound"
    );

    for seq in 1..400 {
        block_on(sink.write(seq, arena_payload(&alloc, 8_000, seq as i64))).expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert!(
        summary.files.len() > 1,
        "several files roll at {roll_bytes} bytes each: {:?}",
        summary.files
    );
    drop(held);
    let rolls: Vec<_> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteObject && op.path_or_url.ends_with(".parquet"))
        .collect();
    assert_eq!(rolls.len(), summary.files.len(), "one object per file");
    for op in &rolls {
        assert!(
            op.len <= roll_bytes + (1 << 20),
            "a file of {} bytes above the roll size {roll_bytes}",
            op.len
        );
    }
}
