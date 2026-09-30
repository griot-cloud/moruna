//! The Vortex sink: SI-T24, SI-T25 and SI-T26 (08 k, f.11).

mod common;

use std::sync::Arc;

use common::{CountingAlloc, Scratch, arena_payload, block_on, table_source_schema};
use moruna_kernel::arrow::array::{Array, ArrayData, ArrayRef, AsArray, make_array};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{MorunaError, Payload, Seq, Sink, SourceSchema};
use moruna_sinks::{VortexSink, VortexSinkConfig};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};
use vortex::VortexSessionDefault;
use vortex::array::VortexSessionExecute;
use vortex::array::stream::ArrayStreamExt;
use vortex::arrow::ArrowSessionExt;
use vortex::file::OpenOptionsSessionExt;
use vortex::io::runtime::BlockingRuntime;
use vortex::io::runtime::current::CurrentThreadRuntime;
use vortex::io::session::RuntimeSessionExt;
use vortex::session::VortexSession;

/// The schema of the incompressible batches: a pseudo-random `Int64` and a `Float64`.
fn random_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("k", DataType::Int64, false),
        Field::new("v", DataType::Float64, false),
    ]))
}

/// `rows` rows of values no encoding shrinks, in arena buffers, so the file sizes are the
/// payload sizes and the roll points are predictable.
fn random_batch(alloc: &FakeAllocator, rows: usize, seed: u64) -> RecordBatch {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut keys = Vec::with_capacity(rows * 8);
    let mut values = Vec::with_capacity(rows * 8);
    for _ in 0..rows {
        keys.extend_from_slice(&next().to_le_bytes());
        values.extend_from_slice(&(next() as f64 / 3.0).to_le_bytes());
    }
    let column = |bytes: &[u8], dt: DataType| -> ArrayRef {
        make_array(
            ArrayData::builder(dt)
                .len(rows)
                .add_buffer(alloc.arrow_buffer(bytes, alloc.host_tier()))
                .build()
                .expect("array data"),
        )
    };
    RecordBatch::try_new(
        random_schema(),
        vec![
            column(&keys, DataType::Int64),
            column(&values, DataType::Float64),
        ],
    )
    .expect("batch")
}

fn sink(
    url: String,
    file_bytes: u64,
    reactor: &FakeReactor,
    alloc: Arc<CountingAlloc>,
) -> VortexSink {
    VortexSink::new(
        VortexSinkConfig { url, file_bytes },
        Arc::new(reactor.clone()),
        alloc,
    )
    .expect("vortex sink")
}

/// Every batch of a Vortex file, read back with the Vortex library itself.
fn read_back(bytes: Vec<u8>) -> (SchemaRef, Vec<RecordBatch>) {
    let runtime = CurrentThreadRuntime::new();
    let session = VortexSession::default().with_handle(runtime.handle());
    let file = session
        .open_options()
        .open_buffer(bytes)
        .expect("a readable vortex file");
    let array = runtime
        .block_on(
            file.scan()
                .expect("scan")
                .into_array_stream()
                .expect("stream")
                .read_all(),
        )
        .expect("rows");
    let schema = Arc::new(
        session
            .arrow()
            .to_arrow_schema(file.dtype())
            .expect("arrow schema"),
    );
    if array.is_empty() {
        return (schema, Vec::new());
    }
    let mut ctx = session.create_execution_ctx();
    let arrow = session
        .arrow()
        .execute_arrow(array, None, &mut ctx)
        .expect("arrow");
    (schema, vec![RecordBatch::from(arrow.as_struct().clone())])
}

/// The bytes a batch's rows occupy, as the sink measures a morsel.
fn rows_bytes(batch: &RecordBatch) -> u64 {
    batch
        .columns()
        .iter()
        .map(|c| c.to_data().get_slice_memory_size().expect("size") as u64)
        .sum()
}

/// The rows of `batches` as `(k, v)` pairs, whatever the batching.
fn pairs(batches: &[RecordBatch]) -> Vec<(i64, f64)> {
    let mut out = Vec::new();
    for batch in batches {
        let k = batch
            .column(0)
            .as_primitive::<moruna_kernel::arrow::datatypes::Int64Type>();
        let v = batch
            .column(1)
            .as_primitive::<moruna_kernel::arrow::datatypes::Float64Type>();
        for i in 0..batch.num_rows() {
            out.push((k.value(i), v.value(i)));
        }
    }
    out
}

/// SI-T24 vortex_rolls_at_size_target. Files roll at the size the arena agreed to, every file is
/// at most `file_bytes`, none but the last is far below its roll point, the files are named
/// `part-{index:05}.vortex`, and they read back as the input in order. f.11, e.7, SI-I7.
#[test]
fn si_t24_vortex_rolls_at_size_target() {
    let scratch = Scratch::new("vx-t17");
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let file_bytes = 1u64 << 20;
    let mut vortex = sink(scratch.url(), file_bytes, &reactor, alloc.clone());
    vortex
        .open(&SourceSchema::Table(random_schema()))
        .expect("open");
    let rows = 4096;
    let inputs: Vec<RecordBatch> = (0..40u64)
        .map(|seq| random_batch(alloc.fake(), rows, seq))
        .collect();
    for (seq, batch) in inputs.iter().enumerate() {
        block_on(vortex.write(seq as Seq, Payload::table(batch.clone()).expect("payload")))
            .expect("write");
    }
    let summary = vortex.finish().expect("finish");
    let stats = vortex.stats();
    assert!(
        summary.files.len() > 2,
        "2.5 MiB of incompressible rows roll a 1 MiB file several times: {:?}",
        summary.files
    );
    assert_eq!(stats.files_committed as usize, summary.files.len());
    assert_eq!(stats.rolls as usize, summary.files.len());
    assert!(stats.roll_bytes > 0 && stats.roll_bytes < file_bytes);
    assert_eq!(summary.rows, 40 * rows as u64, "SI-I7");
    let morsel = rows_bytes(&inputs[0]);
    assert_eq!(stats.encode_bytes, 40 * morsel);
    assert_eq!(
        alloc.payload_copy_bytes(),
        stats.encode_bytes,
        "the encode is counted"
    );

    let mut read = Vec::new();
    let mut total_bytes = 0;
    for (i, name) in summary.files.iter().enumerate() {
        assert_eq!(name, &format!("part-{i:05}.vortex"));
        let url = format!("{}/{name}", scratch.url());
        let bytes = reactor.object(&url).expect("the file was written");
        total_bytes += bytes.len() as u64;
        assert!(
            bytes.len() as u64 <= file_bytes,
            "{name} is {} bytes, above file_bytes {file_bytes}",
            bytes.len()
        );
        if i + 1 < summary.files.len() {
            assert!(
                bytes.len() as u64 + 2 * morsel >= stats.roll_bytes,
                "{name} rolled early: {} bytes against a roll size of {}",
                bytes.len(),
                stats.roll_bytes
            );
        }
        let (schema, batches) = read_back(bytes);
        assert_eq!(schema.fields(), random_schema().fields());
        read.extend(batches);
    }
    assert_eq!(
        summary.bytes, total_bytes,
        "summary bytes are the bytes written"
    );
    assert_eq!(pairs(&read), pairs(&inputs), "every row once, in order");
    let written = reactor
        .ops()
        .iter()
        .filter(|op| op.kind == OpKind::WriteObject && op.path_or_url.ends_with(".vortex"))
        .count();
    assert_eq!(written, summary.files.len(), "one write per file");
    assert!(
        reactor
            .object(&format!("{}/_SUCCESS", scratch.url()))
            .is_some(),
        "the marker is written after the last commit"
    );
}

/// SI-T25 vortex_resume_exactly_once. A run killed in the middle of a file resumes from its
/// checkpoint: the files above the watermark and the one in progress are removed, the rest is
/// replayed, and the output holds every row exactly once. f.8, f.11, SI-I8.
#[test]
fn si_t25_vortex_resume_exactly_once() {
    let scratch = Scratch::new("vx-t18");
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let total = 60u64;
    let rows = 2048;
    let inputs: Vec<RecordBatch> = (0..total)
        .map(|seq| random_batch(alloc.fake(), rows, seq))
        .collect();
    let file_bytes = 512u64 << 10;

    let mut first = sink(scratch.url(), file_bytes, &reactor, alloc.clone());
    first
        .open(&SourceSchema::Table(random_schema()))
        .expect("open");
    let mut checkpoint = None;
    let mut watermark = None;
    for seq in 0..total {
        block_on(first.write(
            seq,
            Payload::table(inputs[seq as usize].clone()).expect("payload"),
        ))
        .expect("write");
        if seq == 35 {
            checkpoint = Some(first.checkpoint().expect("checkpoint").expect("some"));
            watermark = first.committed_seq();
        }
    }
    let checkpoint = checkpoint.expect("a checkpoint was taken");
    let watermark = watermark.expect("something was committed by sequence 35");
    let written_before = first.stats().files_committed;
    assert!(
        written_before > 2,
        "files were committed after the checkpoint too"
    );
    // Killed: no `finish`, the file in progress is never closed.
    drop(first);
    // What the store holds after the kill: every committed object.
    for op in reactor.ops() {
        if op.kind == OpKind::WriteObject
            && let Some(bytes) = reactor.object(&op.path_or_url)
            && let Some(name) = op.path_or_url.rsplit('/').next()
        {
            std::fs::write(scratch.path().join(name), bytes).expect("materialise");
        }
    }
    std::fs::write(scratch.path().join("part-00099.vortex.tmp"), b"torn").expect("tmp");

    let mut second = sink(scratch.url(), file_bytes, &reactor, alloc.clone());
    second
        .resume(
            &SourceSchema::Table(random_schema()),
            &checkpoint,
            Some(watermark),
        )
        .expect("resume");
    assert!(second.stats().resumed_files_removed > 0);
    let kept = scratch.names();
    assert!(kept.iter().all(|n| !n.ends_with(".tmp")), "{kept:?}");
    for seq in watermark + 1..total {
        block_on(second.write(
            seq,
            Payload::table(inputs[seq as usize].clone()).expect("payload"),
        ))
        .expect("write");
    }
    let summary = second.finish().expect("finish");
    assert_eq!(second.committed_seq(), Some(total - 1));
    assert_eq!(summary.rows, total * rows as u64, "SI-I7 across a resume");
    let mut read = Vec::new();
    for name in &summary.files {
        let url = format!("{}/{name}", scratch.url());
        let bytes = reactor.object(&url).expect("written");
        read.extend(read_back(bytes).1);
    }
    assert_eq!(
        pairs(&read),
        pairs(&inputs),
        "the resumed run reads back as an uninterrupted one: each row exactly once"
    );

    // A checkpoint of another sink's kind is refused.
    let mut other = sink(scratch.url(), file_bytes, &reactor, alloc);
    let parquet = br#"{"version":1,"kind":"parquet","next_index":0,"committed":[]}"#;
    assert!(matches!(
        other.resume(&SourceSchema::Table(random_schema()), parquet, None),
        Err(MorunaError::Resume(_))
    ));
}

/// SI-T26 vortex_sink_edges. An empty run writes one readable empty file with the declared
/// schema; schema drift, a tensor payload, `write` after `finish` and a second `finish` are
/// `Sink` errors; a morsel larger than a file gets a file of its own; a failed store write
/// fails the sink and `finish` names the committed files; the run id rides in each file's
/// metadata; the checkpoint is `Some` from the start. f.11, h, e.1.
#[test]
fn si_t26_vortex_sink_edges() {
    // Empty run.
    let scratch = Scratch::new("vx-t19-empty");
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut empty = sink(scratch.url(), 1 << 20, &reactor, alloc.clone());
    assert!(
        empty.checkpoint().expect("checkpoint").is_some(),
        "resumable from the start"
    );
    assert!(empty.finish().is_err(), "finish before open");
    empty.open(&table_source_schema()).expect("open");
    let summary = empty.finish().expect("finish");
    assert_eq!(summary.rows, 0);
    assert_eq!(summary.files, vec!["part-00000.vortex".to_string()]);
    let bytes = reactor
        .object(&format!("{}/part-00000.vortex", scratch.url()))
        .expect("the empty file");
    let (schema, batches) = read_back(bytes);
    assert_eq!(schema.fields().len(), 2, "the declared schema");
    assert!(batches.iter().all(|b| b.num_rows() == 0));
    assert!(empty.finish().is_err(), "finish twice");
    assert!(
        block_on(empty.write(0, arena_payload(alloc.fake(), 4, 0))).is_err(),
        "write after finish"
    );

    // Drift, a tensor, the run id, an oversized morsel.
    let scratch = Scratch::new("vx-t19-drift");
    let reactor = FakeReactor::new();
    let mut drift = sink(scratch.url(), 1 << 20, &reactor, alloc.clone())
        .with_run_id(moruna_kernel::RunId([7; 16]));
    assert!(
        drift.open(&common::tensor_source_schema(2)).is_err(),
        "a tensor schema is refused"
    );
    drift.open(&table_source_schema()).expect("open");
    assert!(drift.open(&table_source_schema()).is_err(), "a second open");
    assert_eq!(drift.accepts().kind, moruna_kernel::PayloadKind::Table);
    block_on(drift.write(0, arena_payload(alloc.fake(), 8, 0))).expect("first write");
    // Larger than the file: the open file rolls first and the morsel gets its own.
    let huge = random_batch(alloc.fake(), 80_000, 1);
    let huge_schema_payload = Payload::table(
        RecordBatch::try_new(
            common::table_schema(),
            vec![huge.column(0).clone(), huge.column(0).clone()],
        )
        .expect("batch"),
    )
    .expect("payload");
    block_on(drift.write(1, huge_schema_payload)).expect("an oversized morsel");
    assert_eq!(drift.stats().rolls, 1, "the open file rolled for it");
    let wrong = random_batch(alloc.fake(), 4, 2);
    let e = block_on(drift.write(2, Payload::table(wrong).expect("payload")))
        .expect_err("schema drift");
    assert!(e.to_string().contains("schema drift"), "{e}");
    // Drift is the caller's error, not the store's: as for Parquet, the sink stays open and
    // the scheduler terminates the run.
    let drifted = drift.finish().expect("the files written before the drift");
    assert_eq!(drifted.files.len(), 2);
    assert_eq!(drifted.rows, 8 + 80_000);

    // A first morsel larger than a file: the empty file is reopened under its own name.
    let scratch_big = Scratch::new("vx-t19-big");
    let big_reactor = FakeReactor::new();
    let mut big = sink(scratch_big.url(), 64 << 10, &big_reactor, alloc.clone());
    big.open(&SourceSchema::Table(random_schema()))
        .expect("open");
    block_on(big.write(
        0,
        Payload::table(random_batch(alloc.fake(), 20_000, 3)).expect("payload"),
    ))
    .expect("a first morsel larger than the file");
    let summary = big.finish().expect("finish");
    assert_eq!(summary.files, vec!["part-00000.vortex".to_string()]);
    assert_eq!(summary.rows, 20_000);

    // The run id is in the committed file's metadata.
    let first = reactor
        .object(&format!("{}/part-00000.vortex", scratch.url()))
        .expect("the rolled file");
    let runtime = CurrentThreadRuntime::new();
    let session = VortexSession::default().with_handle(runtime.handle());
    let opened = session
        .open_options()
        .include_metadata()
        .open_buffer(first)
        .expect("open with metadata");
    let run_id = opened
        .metadata_segment("moruna.run_id")
        .expect("the run id segment");
    assert_eq!(run_id.as_slice(), "07".repeat(16).as_bytes());

    // A store that fails the write of a rolled file.
    let scratch = Scratch::new("vx-t19-fail");
    let reactor = FakeReactor::new().fail_next(OpKind::WriteObject, 1);
    let mut failing = sink(scratch.url(), 256 << 10, &reactor, alloc.clone());
    failing
        .open(&SourceSchema::Table(random_schema()))
        .expect("open");
    let mut failed = None;
    for seq in 0..20u64 {
        if let Err(e) = block_on(failing.write(
            seq,
            Payload::table(random_batch(alloc.fake(), 2048, seq)).expect("payload"),
        )) {
            failed = Some(e);
            break;
        }
    }
    assert!(failed.is_some(), "the failed write is reported");
    let e = failing.finish().expect_err("finish reports the failure");
    assert!(e.to_string().contains("committed files"), "{e}");
    let tensor = common::arena_tensor(alloc.fake(), 2, 2, 0.0);
    let mut open = sink(scratch.url(), 1 << 20, &FakeReactor::new(), alloc);
    open.open(&table_source_schema()).expect("open");
    assert!(
        block_on(open.write(0, tensor)).is_err(),
        "a tensor payload is refused"
    );
}
