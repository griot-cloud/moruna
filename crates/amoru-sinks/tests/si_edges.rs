//! The edge cases and failures of 08 h and e.1, and the paths the SI-T tests reach past.

mod common;

use std::sync::Arc;

use amoru_kernel::arrow::datatypes::{DataType, Field, Schema};
use amoru_kernel::arrow::record_batch::RecordBatch;
use amoru_kernel::{
    AmoruError, DType, Payload, RunId, SegmentRef, Sink, SourceSchema, Tier, TierPref,
};
use amoru_sinks::{
    ArrowIpcSink, ArrowIpcSinkConfig, ParquetSink, ParquetSinkConfig, ReorderBuffer, SinkHandle,
    SinkStats, TensorFormat, TensorSink, TensorSinkConfig,
};
use amoru_testkit::{FakeAllocator, FakeReactor, FakeSink, OpKind};
use common::{
    Scratch, arena_batch, arena_payload, arena_tensor, arena_tensor_of, arena_wide_batch, block_on,
    materialise, table_schema, table_source_schema, tensor_source_schema, wide_source_schema,
    written,
};

/// A payload whose bytes are not resident in a host tier is refused rather than copied,
/// whichever tier it names. SI-I6.
#[test]
fn a_payload_that_is_not_resident_is_refused() {
    let scratch = Scratch::new("not-resident");
    let alloc = FakeAllocator::new();
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");

    let segment = SegmentRef {
        segment: 0,
        offset: 0,
        len: 8,
    };
    // E9: test code may use `unsafe` to construct a state a test needs.
    let staged = unsafe { Payload::table_in(arena_batch(&alloc, 8, 0), Tier::Disk(segment)) };
    let outcome = block_on(sink.write(0, staged));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("a staged payload must be refused, got {outcome:?}");
    };
    assert_eq!(msg, "disk payload");

    let remote = amoru_kernel::RemoteRef {
        addr: 0,
        rkey: 0,
        len: 8,
    };
    let elsewhere = unsafe {
        Payload::table_in(
            arena_batch(&alloc, 8, 0),
            Tier::Remote(amoru_kernel::LOCAL_NODE, remote),
        )
    };
    let outcome = block_on(sink.write(1, elsewhere));
    assert!(
        matches!(outcome, Err(AmoruError::Unsupported("rdma"))),
        "a reserved tier is an explicit refusal, not a wildcard: {outcome:?}"
    );
}

/// A sink allocates its own buffers in whichever host tier the run has (contracts e.1).
#[test]
fn a_pinned_arena_gives_the_sink_pinned_buffers() {
    let scratch = Scratch::new("pinned");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new().pinned(true);
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            file_bytes: 32 << 10,
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");
    assert!(
        alloc.in_use(Tier::PinnedHost) > 0,
        "the file buffer is pinned"
    );
    block_on(sink.write(0, arena_payload(&alloc, 64, 0))).expect("write");
    sink.finish().expect("finish");
    for op in reactor.ops() {
        assert_eq!(op.src_tier, Some(Tier::PinnedHost), "{op:?}");
    }
}

/// A morsel the encoder cannot fit in the file buffer fails by name, never with a truncated
/// file (h).
#[test]
fn a_morsel_too_large_to_encode_is_named() {
    use parquet::basic::Compression;
    use parquet::file::properties::WriterProperties;

    let scratch = Scratch::new("too-large");
    let alloc = FakeAllocator::new();
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            file_bytes: 4 << 10,
            row_group_bytes: 64 << 10,
            // The escape hatch of d.1, which overrides the compression above.
            writer_props: Some(
                WriterProperties::builder()
                    .set_compression(Compression::UNCOMPRESSED)
                    .build(),
            ),
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");

    // The first morsel of a file never rolls first, so it has to fit the buffer `open` took.
    let outcome = block_on(sink.write(0, arena_payload(&alloc, 400_000, 0)));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("an oversized morsel must be refused, got {outcome:?}");
    };
    assert!(msg.contains("morsel too large to encode"), "{msg}");

    // The sink is `Failed` from here: a later write says so rather than writing anything.
    let refused = block_on(sink.write(1, arena_payload(&alloc, 8, 0)));
    let Err(AmoruError::Sink(msg)) = refused else {
        panic!("a failed sink must refuse later writes, got {refused:?}");
    };
    assert!(msg.contains("the sink failed"), "{msg}");
    assert!(matches!(
        sink.resume(&table_source_schema(), b"{}", None),
        Err(AmoruError::Sink(_))
    ));
}

/// The run id a sink is given goes into every file's footer (e.2, e.3).
#[test]
fn the_run_id_reaches_the_footer() {
    let run_id = RunId([0xab; 16]);
    let expected = "ab".repeat(16);

    let scratch = Scratch::new("run-id-ipc");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    let mut sink = ArrowIpcSink::new(
        ArrowIpcSinkConfig {
            path: scratch.path().to_path_buf(),
            ..ArrowIpcSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("arrow ipc sink")
    .with_run_id(run_id);
    sink.open(&table_source_schema()).expect("open");
    block_on(sink.write(0, arena_payload(&alloc, 32, 0))).expect("write");
    sink.finish().expect("finish");
    let bytes = written(&reactor, scratch.path(), "part-00000.arrow");
    assert!(
        String::from_utf8_lossy(&bytes).contains(&expected),
        "the ipc footer names the run"
    );

    let scratch = Scratch::new("run-id-parquet");
    let reactor = FakeReactor::new();
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink")
    .with_run_id(run_id);
    sink.open(&table_source_schema()).expect("open");
    block_on(sink.write(0, arena_payload(&alloc, 32, 0))).expect("write");
    let summary = sink.finish().expect("finish");
    let url = format!("{}/{}", scratch.url(), summary.files[0]);
    let len = reactor
        .ops()
        .iter()
        .find(|op| op.path_or_url == url)
        .map(|op| op.len as usize)
        .expect("the file was written");
    let bytes = common::object_bytes(&reactor, &alloc, &url, len);
    assert!(
        String::from_utf8_lossy(&bytes).contains(&expected),
        "the parquet footer names the run"
    );
}

/// A schema that gains or loses a field is named as such (h).
#[test]
fn schema_drift_names_a_missing_or_extra_field() {
    let scratch = Scratch::new("drift-shape");
    let alloc = FakeAllocator::new();
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");

    let one = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]));
    let batch = arena_batch(&alloc, 8, 0);
    let short = RecordBatch::try_new(one, vec![batch.column(0).clone()]).expect("batch");
    let outcome = block_on(sink.write(0, Payload::table(short).expect("payload")));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("a missing field must be refused, got {outcome:?}");
    };
    assert!(msg.contains("is missing"), "{msg}");

    let scratch = Scratch::new("drift-extra");
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    sink.open(&table_source_schema()).expect("open");
    let three = Arc::new(Schema::new(vec![
        Field::new("a", DataType::Int64, false),
        Field::new("b", DataType::Int64, false),
        Field::new("c", DataType::Int64, false),
    ]));
    let batch = arena_batch(&alloc, 8, 0);
    let wide = RecordBatch::try_new(
        three,
        vec![
            batch.column(0).clone(),
            batch.column(1).clone(),
            batch.column(0).clone(),
        ],
    )
    .expect("batch");
    let outcome = block_on(sink.write(0, Payload::table(wide).expect("payload")));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("an extra field must be refused, got {outcome:?}");
    };
    assert!(msg.contains("not in the schema"), "{msg}");
}

/// A sink refuses a schema of the wrong shape and says what it accepts (d.1).
#[test]
fn a_sink_accepts_one_shape_of_payload() {
    let scratch = Scratch::new("accepts");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let parquet = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    assert_eq!(parquet.accepts().tier, TierPref::Host);
    let mut parquet = parquet;
    assert!(parquet.open(&tensor_source_schema(4)).is_err());
    parquet.open(&table_source_schema()).expect("open");
    let outcome = block_on(parquet.write(0, arena_tensor(&alloc, 4, 4, 0.0)));
    assert!(matches!(outcome, Err(AmoruError::Sink(_))), "{outcome:?}");

    let scratch = Scratch::new("accepts-ipc");
    let ipc = ArrowIpcSink::new(
        ArrowIpcSinkConfig {
            path: scratch.path().to_path_buf(),
            ..ArrowIpcSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("arrow ipc sink");
    assert_eq!(ipc.accepts().tier, TierPref::Host);
    let mut ipc = ipc;
    assert!(ipc.open(&tensor_source_schema(4)).is_err());
    ipc.open(&table_source_schema()).expect("open");
    let outcome = block_on(ipc.write(0, arena_tensor(&alloc, 4, 4, 0.0)));
    assert!(matches!(outcome, Err(AmoruError::Sink(_))), "{outcome:?}");
    let outcome = block_on(ipc.write(
        0,
        Payload::table(arena_wide_batch(&alloc, 8)).expect("payload"),
    ));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("schema drift must be refused, got {outcome:?}");
    };
    assert!(msg.contains("schema drift"), "{msg}");

    let scratch = Scratch::new("accepts-tensor");
    let tensor = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            ..TensorSinkConfig::default()
        },
        Arc::new(reactor),
        Arc::new(alloc),
    )
    .expect("tensor sink");
    assert_eq!(tensor.accepts().tier, TierPref::Host);
    assert!(!tensor.is_resumable(), "run mode does not track commits");
}

/// A batch with a nullable column and a nested one goes through the IPC sink whole; the
/// bitmap the encoder makes for it is staged rather than passed off as a payload buffer.
#[test]
fn a_nested_batch_writes_and_reads_back() {
    use amoru_kernel::arrow::ipc::reader::FileReader;

    let scratch = Scratch::new("nested");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    let mut sink = ArrowIpcSink::new(
        ArrowIpcSinkConfig {
            path: scratch.path().to_path_buf(),
            ..ArrowIpcSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("arrow ipc sink");
    sink.open(&wide_source_schema(&alloc)).expect("open");
    let batch = arena_wide_batch(&alloc, 64);
    block_on(sink.write(0, Payload::table(batch.clone()).expect("payload"))).expect("write");
    sink.finish().expect("finish");

    let bytes = written(&reactor, scratch.path(), "part-00000.arrow");
    let reader = FileReader::try_new(std::io::Cursor::new(bytes), None).expect("reader");
    let read: Vec<RecordBatch> = reader.map(|b| b.expect("batch")).collect();
    assert_eq!(read, vec![batch]);
}

/// Every dtype the aligned binary format names is spelled the same way in a safetensors
/// header, so a reader of either format sees the same tensor (e.4).
#[test]
fn every_dtype_reaches_both_formats() {
    let alloc = FakeAllocator::new();
    for dtype in DType::ALL {
        let scratch = Scratch::new("dtypes");
        let reactor = FakeReactor::new();
        let mut sink = TensorSink::new(
            TensorSinkConfig {
                path: scratch.path().to_path_buf(),
                format: TensorFormat::SafeTensors,
                one_file_per_morsel: false,
                name: "w".to_string(),
            },
            Arc::new(reactor.clone()),
            Arc::new(alloc.clone()),
        )
        .expect("tensor sink");
        sink.open(&SourceSchema::Tensor {
            dtype,
            shape: vec![-1, 2],
        })
        .expect("open");
        block_on(sink.write(0, arena_tensor_of(&alloc, 4, 2, dtype))).expect("write");
        sink.finish().expect("finish");
        let bytes = written(&reactor, scratch.path(), "w.safetensors");
        let file = safetensors::SafeTensors::deserialize(&bytes)
            .unwrap_or_else(|e| panic!("{dtype:?} produced an unreadable header: {e}"));
        let view = file.tensor("w").expect("the tensor entry");
        assert_eq!(view.shape(), &[4, 2]);
        assert_eq!(view.data().len(), 8 * dtype.item_size());
    }
}

/// A tensor of the wrong rank is refused by name, and a failed write is reported by `finish`
/// (h, SI-I3).
#[test]
fn a_tensor_sink_refuses_drift_and_reports_failure() {
    let scratch = Scratch::new("tensor-rank");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut sink = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            format: TensorFormat::Amb1,
            one_file_per_morsel: true,
            name: "w".to_string(),
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("tensor sink");
    sink.open(&SourceSchema::Tensor {
        dtype: DType::F32,
        shape: vec![-1, 2, 2],
    })
    .expect("open");
    let outcome = block_on(sink.write(0, arena_tensor(&alloc, 4, 2, 0.0)));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("a rank mismatch must be refused, got {outcome:?}");
    };
    assert!(msg.contains("rank"), "{msg}");

    let outcome = block_on(sink.write(1, arena_tensor_of(&alloc, 4, 2, DType::I64)));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("a dtype mismatch must be refused, got {outcome:?}");
    };
    assert!(msg.contains("schema drift"), "{msg}");

    // A store that refuses the write leaves the sink `Failed`, and `finish` says so.
    let scratch = Scratch::new("tensor-fail");
    let reactor = FakeReactor::new();
    let mut sink = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            format: TensorFormat::Amb1,
            one_file_per_morsel: true,
            name: "w".to_string(),
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("tensor sink");
    sink.open(&tensor_source_schema(4)).expect("open");
    let reactor = reactor.fail_next(OpKind::WriteFile, 2);
    let outcome = block_on(sink.write(0, arena_tensor(&alloc, 4, 4, 0.0)));
    assert!(matches!(outcome, Err(AmoruError::Io { .. })), "{outcome:?}");
    let _ = reactor;
    let outcome = sink.finish();
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("finish must report the failure, got {outcome:?}");
    };
    assert!(msg.contains("committed files:"), "{msg}");
    assert!(matches!(
        sink.finish(),
        Err(AmoruError::Sink(_)) // finish ran twice
    ));
}

/// A local sink commits by renaming its temporary file into place, and a resume afterwards
/// clears whatever an aborted run left (f.5, f.8).
#[test]
fn a_local_commit_renames_the_temporary_file() {
    let scratch = Scratch::new("rename");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    let mut sink = ArrowIpcSink::new(
        ArrowIpcSinkConfig {
            path: scratch.path().to_path_buf(),
            ..ArrowIpcSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("arrow ipc sink");
    sink.open(&table_source_schema()).expect("open");
    block_on(sink.write(0, arena_payload(&alloc, 32, 0))).expect("write");
    // The reactor keeps its files in memory (contracts d.15); putting the temporary file on
    // this filesystem is what lets `finish` exercise the fsync and rename of f.5.
    materialise(&reactor, scratch.path());
    let _ = std::fs::remove_file(scratch.path().join("part-00000.arrow"));
    sink.finish().expect("finish");
    assert_eq!(
        scratch.names(),
        vec!["part-00000.arrow".to_string()],
        "the temporary file was renamed into place"
    );
}

/// A Parquet sink over a local prefix lists and clears its destination on resume, and a store
/// it cannot enumerate is refused (f.8).
#[test]
fn a_parquet_resume_clears_a_local_prefix() {
    let scratch = Scratch::new("parquet-resume");
    let alloc = FakeAllocator::new();
    std::fs::write(scratch.path().join("part-00000.parquet"), b"kept").expect("write");
    std::fs::write(scratch.path().join("part-00001.parquet"), b"above").expect("write");
    std::fs::write(scratch.path().join("part-00002.parquet.tmp"), b"tmp").expect("write");
    std::fs::write(scratch.path().join("_SUCCESS"), b"").expect("write");

    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    let state = br#"{"version":1,"kind":"parquet","next_index":2,"committed":[{"name":"part-00000.parquet","seq_min":0,"seq_max":9,"rows":9,"bytes":4}]}"#;
    sink.resume(&table_source_schema(), state, Some(9))
        .expect("resume");
    assert_eq!(sink.committed_seq(), Some(9));
    assert_eq!(
        scratch.names(),
        vec!["_SUCCESS".to_string(), "part-00000.parquet".to_string()],
        "everything the checkpoint does not list is gone"
    );
    assert!(sink.stats().resumed_files_removed >= 2);
    block_on(sink.write(10, arena_payload(&alloc, 16, 0))).expect("write");
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.files.len(), 2);
    assert_eq!(summary.files[1], "part-00002.parquet");

    // A prefix that does not exist lists as empty rather than failing (f.8).
    let missing = Scratch::new("parquet-missing");
    let url = format!("{}/gone", missing.url());
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url,
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc),
    )
    .expect("parquet sink");
    let state = br#"{"version":1,"kind":"parquet","next_index":0,"committed":[]}"#;
    sink.resume(&table_source_schema(), state, None)
        .expect("resume over an empty prefix");
}

/// A checkpoint the sink cannot make sense of is refused, whatever is wrong with it (f.8).
#[test]
fn a_checkpoint_that_does_not_add_up_is_refused() {
    let scratch = Scratch::new("checkpoint-bad");
    let alloc = FakeAllocator::new();
    let make = || {
        ParquetSink::new(
            ParquetSinkConfig {
                url: scratch.url(),
                ..ParquetSinkConfig::default()
            },
            Arc::new(FakeReactor::new()),
            Arc::new(alloc.clone()),
        )
        .expect("parquet sink")
    };

    let backwards = br#"{"version":1,"kind":"parquet","next_index":1,"committed":[{"name":"p","seq_min":9,"seq_max":2,"rows":0,"bytes":0}]}"#;
    let outcome = make().resume(&table_source_schema(), backwards, Some(9));
    let Err(AmoruError::Resume(msg)) = outcome else {
        panic!("a backwards range must be refused, got {outcome:?}");
    };
    assert!(msg.contains("has range"), "{msg}");

    // A committed file with no watermark at all is above it, so it goes.
    std::fs::write(scratch.path().join("part-00000.parquet"), b"x").expect("write");
    let orphan = br#"{"version":1,"kind":"parquet","next_index":1,"committed":[{"name":"part-00000.parquet","seq_min":0,"seq_max":3,"rows":1,"bytes":1}]}"#;
    let mut sink = make();
    sink.resume(&table_source_schema(), orphan, None)
        .expect("resume");
    assert_eq!(sink.committed_seq(), None);
    assert!(!scratch.names().contains(&"part-00000.parquet".to_string()));
}

/// The parts of `ReorderBuffer` and `SinkHandle` the scheduler reads but the reorder test does
/// not reach (d.1).
#[test]
fn the_handle_and_the_buffer_answer_for_the_sink() {
    let fake = FakeSink::new();
    let buffer: ReorderBuffer<dyn Sink> =
        ReorderBuffer::new(Box::new(fake.clone()) as Box<dyn Sink>, 1 << 20);
    assert_eq!(buffer.accepts(), fake.accepts());
    assert!(!buffer.inner().requires_order());
    // A skip below the watermark is ignored rather than counted twice.
    buffer.skip(0);
    buffer.skip(0);
    assert_eq!(buffer.next_expected(), 1);
    assert_eq!(fake.skipped(), vec![0, 0]);

    let plain = SinkHandle::wrap(Box::new(FakeSink::new()), false, 0);
    assert_eq!(plain.accepts().tier, TierPref::Host);
    assert_eq!(SinkStats::default().writes, 0);
    assert_eq!(table_schema().fields().len(), 2);
}

/// A per-morsel tensor sink tracks commits and resumes like the other file sinks: the sequence
/// number is in the file name, so a file above the watermark is recognised without reading it
/// (e.4, f.7, f.8).
#[test]
fn a_per_morsel_tensor_sink_resumes() {
    let scratch = Scratch::new("tensor-resume");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    let config = || TensorSinkConfig {
        path: scratch.path().to_path_buf(),
        format: TensorFormat::Amb1,
        one_file_per_morsel: true,
        name: "w".to_string(),
    };
    let mut first = TensorSink::new(config(), Arc::new(reactor.clone()), Arc::new(alloc.clone()))
        .expect("tensor sink");
    first.open(&tensor_source_schema(4)).expect("open");
    for seq in 0..6 {
        block_on(first.write(seq, arena_tensor(&alloc, 4, 4, seq as f32))).expect("write");
    }
    let checkpoint = first.checkpoint().expect("checkpoint").expect("some");
    assert_eq!(first.committed_seq(), Some(5));
    for seq in 6..9 {
        block_on(first.write(seq, arena_tensor(&alloc, 4, 4, seq as f32))).expect("write");
    }
    drop(first);
    materialise(&reactor, scratch.path());
    std::fs::write(scratch.path().join("w-000000000009.amb1.tmp"), b"partial").expect("write");

    let mut second = TensorSink::new(config(), Arc::new(reactor.clone()), Arc::new(alloc.clone()))
        .expect("tensor sink");
    second
        .resume(&tensor_source_schema(4), &checkpoint, Some(5))
        .expect("resume");
    let names = scratch.names();
    assert_eq!(
        names.len(),
        6,
        "only the committed files are left: {names:?}"
    );
    assert!(names.iter().all(|n| n.ends_with(".amb1")));
    assert!(second.stats().resumed_files_removed >= 4);
    assert_eq!(second.committed_seq(), Some(5));

    // The scheduler declares 6 skipped and replays 7 and 8; the watermark follows both.
    second.skip(6);
    for seq in 7..9 {
        block_on(second.write(seq, arena_tensor(&alloc, 4, 4, seq as f32))).expect("write");
    }
    let summary = second.finish().expect("finish");
    assert_eq!(second.committed_seq(), Some(8));
    assert_eq!(summary.files.len(), 8, "{:?}", summary.files);
}

/// A sink URL is a prefix the destination can be reached through, or the sink says why not.
#[test]
fn a_sink_url_names_a_destination() {
    let alloc = FakeAllocator::new();
    let outcome = ParquetSink::new(
        ParquetSinkConfig {
            url: String::new(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    );
    assert!(
        matches!(
            outcome,
            Err(AmoruError::Config {
                name: "sink.url",
                ..
            })
        ),
        "an empty URL names the knob that is wrong"
    );

    // A bare path is a local directory, so it can be listed like a `file://` prefix (f.8).
    let scratch = Scratch::new("bare-path");
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.path().display().to_string(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    let state = br#"{"version":1,"kind":"parquet","next_index":0,"committed":[]}"#;
    sink.resume(&table_source_schema(), state, None)
        .expect("a bare path lists");

    // A prefix that is a file rather than a directory is an `Io` error naming it (f.8).
    let file = Scratch::new("not-a-directory");
    let path = file.path().join("regular");
    std::fs::write(&path, b"x").expect("write");
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: path.display().to_string(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc),
    )
    .expect("parquet sink");
    let outcome = sink.resume(&table_source_schema(), state, None);
    assert!(matches!(outcome, Err(AmoruError::Io { .. })), "{outcome:?}");
}

/// A run-mode sink refuses `finish` before it is open, and clears the temporary files an
/// aborted run left (e.1, h).
#[test]
fn finish_before_open_and_after_failure() {
    let scratch = Scratch::new("finish-edges");
    let alloc = FakeAllocator::new();
    let mut sink = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            ..TensorSinkConfig::default()
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    )
    .expect("tensor sink");
    assert!(matches!(sink.finish(), Err(AmoruError::Sink(_))));

    let scratch = Scratch::new("finish-tmps");
    let reactor = FakeReactor::new().fail_next(OpKind::WriteFile, 1);
    let mut sink = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            ..TensorSinkConfig::default()
        },
        Arc::new(reactor),
        Arc::new(alloc.clone()),
    )
    .expect("tensor sink");
    sink.open(&tensor_source_schema(4)).expect("open");
    std::fs::write(scratch.path().join("tensor.amb1.tmp"), b"partial").expect("write");
    assert!(block_on(sink.write(0, arena_tensor(&alloc, 4, 4, 0.0))).is_err());
    assert!(sink.finish().is_err());
    assert!(
        scratch.names().is_empty(),
        "finish cleared what the aborted run left: {:?}",
        scratch.names()
    );
}

/// A tensor name too long for the space the safetensors header reserves is refused rather
/// than truncated (f.3).
#[test]
fn a_safetensors_header_that_does_not_fit_is_refused() {
    let scratch = Scratch::new("safetensors-long");
    let alloc = FakeAllocator::new();
    let mut sink = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            format: TensorFormat::SafeTensors,
            one_file_per_morsel: false,
            name: "w".repeat(600),
        },
        Arc::new(FakeReactor::new()),
        Arc::new(alloc.clone()),
    )
    .expect("tensor sink");
    sink.open(&tensor_source_schema(4)).expect("open");
    block_on(sink.write(0, arena_tensor(&alloc, 4, 4, 0.0))).expect("write");
    let outcome = sink.finish();
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("a header that does not fit must be refused, got {outcome:?}");
    };
    assert!(msg.contains("the safetensors header is"), "{msg}");
}
