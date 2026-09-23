//! Commit tracking and resume: SI-T12, SI-T13, SI-T14 (08 k). S17, G-I12.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{
    Scratch, arena_batch, arena_payload, block_on, materialise, table_source_schema,
    tensor_source_schema, written,
};
use moruna_kernel::arrow::ipc::reader::FileReader;
use moruna_kernel::arrow::ipc::root_as_footer;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{MorunaError, Seq, Sink, SourceSchema};
use moruna_sinks::{
    ArrowIpcSink, ArrowIpcSinkConfig, ParquetSink, ParquetSinkConfig, ReorderBuffer, TensorFormat,
    TensorSink, TensorSinkConfig,
};
use moruna_testkit::{FakeAllocator, FakeReactor};

const ROWS: usize = 32;

fn ipc_sink(
    scratch: &Scratch,
    reactor: &FakeReactor,
    alloc: &FakeAllocator,
    file_bytes: u64,
) -> ArrowIpcSink {
    ArrowIpcSink::new(
        ArrowIpcSinkConfig {
            path: scratch.path().to_path_buf(),
            file_bytes,
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("arrow ipc sink")
}

/// The highest sequence number such that every lower one has been written or skipped.
fn contiguous(seen: &BTreeSet<Seq>) -> Option<Seq> {
    let mut next = 0;
    while seen.contains(&next) {
        next += 1;
    }
    next.checked_sub(1)
}

/// A deterministic shuffle, so a failure is reproducible.
fn shuffled(n: u64, seed: u64) -> Vec<Seq> {
    let mut order: Vec<Seq> = (0..n).collect();
    let mut state = seed | 1;
    for i in (1..order.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        order.swap(i, (state % (i as u64 + 1)) as usize);
    }
    order
}

/// SI-T12. A thousand sequences, delivered in order and out of order, with three declared
/// skipped: `committed_seq` never names a sequence whose file is not committed, never goes
/// backwards, and ends exactly at the last one; every file's footer range brackets its rows.
/// SI-I8.
#[test]
fn si_t12_committed_seq_exact() {
    for ordered in [true, false] {
        let scratch = Scratch::new(if ordered {
            "t12-ordered"
        } else {
            "t12-unordered"
        });
        let reactor = FakeReactor::new();
        let alloc = FakeAllocator::new();
        // About fifty records to a file, which is what makes the watermark move in steps.
        let mut sink = ipc_sink(&scratch, &reactor, &alloc, 50 * 5 * 4096);
        sink.open(&table_source_schema()).expect("open");

        let skips = [7u64, 23, 24];
        let mut seen: BTreeSet<Seq> = BTreeSet::new();
        for seq in skips {
            sink.skip(seq);
            seen.insert(seq);
        }
        assert_eq!(
            sink.committed_seq(),
            None,
            "a skip above the watermark does not move it on its own"
        );

        let order: Vec<Seq> = if ordered {
            (0..1_000).filter(|s| !skips.contains(s)).collect()
        } else {
            shuffled(1_000, 0x2545F4914F6CDD1D)
                .into_iter()
                .filter(|s| !skips.contains(s))
                .collect()
        };

        let mut last = None;
        for seq in order {
            block_on(sink.write(seq, arena_payload(&alloc, ROWS, seq as i64))).expect("write");
            seen.insert(seq);
            let watermark = sink.committed_seq();
            match (last, watermark) {
                (Some(before), Some(now)) => assert!(now >= before, "the watermark went back"),
                (Some(_), None) => panic!("the watermark went back to none"),
                _ => {}
            }
            if let Some(now) = watermark {
                let bound = contiguous(&seen).expect("something has been written");
                assert!(
                    now <= bound,
                    "committed_seq {now} overstates: only {bound} is contiguous"
                );
            }
            last = watermark;
        }

        let summary = sink.finish().expect("finish");
        assert_eq!(
            sink.committed_seq(),
            Some(999),
            "every sequence is committed or skipped once the last file is in"
        );
        assert!(summary.files.len() > 1, "{:?}", summary.files);

        // Each file's footer names the range of sequence numbers whose rows it holds (e.3).
        let mut rows_seen = 0u64;
        for name in &summary.files {
            let bytes = written(&reactor, scratch.path(), name);
            let (seq_min, seq_max) = footer_range(&bytes).expect("a sequence range");
            assert!(seq_min <= seq_max);
            let reader = FileReader::try_new(std::io::Cursor::new(bytes), None).expect("reader");
            let records: Vec<RecordBatch> = reader.map(|b| b.expect("batch")).collect();
            let rows: usize = records.iter().map(|b| b.num_rows()).sum();
            assert_eq!(rows, records.len() * ROWS, "{name} rows do not match");
            assert!(
                (seq_max - seq_min + 1) as usize >= records.len(),
                "{name} declares {seq_min}..{seq_max} for {} records",
                records.len()
            );
            rows_seen += rows as u64;
        }
        assert_eq!(summary.rows, rows_seen);
    }
}

fn footer_range(bytes: &[u8]) -> Option<(u64, u64)> {
    let n = bytes.len();
    let footer_len =
        u32::from_le_bytes([bytes[n - 10], bytes[n - 9], bytes[n - 8], bytes[n - 7]]) as usize;
    let footer = &bytes[n - 10 - footer_len..n - 10];
    let parsed = root_as_footer(footer).ok()?;
    let metadata = parsed.custom_metadata()?;
    let mut min = None;
    let mut max = None;
    for i in 0..metadata.len() {
        let kv = metadata.get(i);
        match (kv.key(), kv.value()) {
            (Some("moruna.seq_min"), Some(v)) => min = v.parse().ok(),
            (Some("moruna.seq_max"), Some(v)) => max = v.parse().ok(),
            _ => {}
        }
    }
    Some((min?, max?))
}

/// SI-T13. A sink resumed from a checkpoint removes every file above the watermark and every
/// temporary file, continues the file numbering, and the run's output reads back the same as an
/// uninterrupted one. A checkpoint that disagrees with the watermark is refused. f.8, e.5.
#[test]
fn si_t13_resume_removes_uncommitted() {
    let scratch = Scratch::new("t13");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();
    let total = 200u64;
    let inputs: Vec<RecordBatch> = (0..total)
        .map(|seq| arena_batch(&alloc, ROWS, seq as i64))
        .collect();

    let mut first = ipc_sink(&scratch, &reactor, &alloc, 20 * 5 * 4096);
    first.open(&table_source_schema()).expect("open");
    let mut checkpoint = None;
    let mut watermark = None;
    for seq in 0..total {
        block_on(first.write(
            seq,
            moruna_kernel::Payload::table(inputs[seq as usize].clone()).expect("payload"),
        ))
        .expect("write");
        if seq == 120 {
            checkpoint = Some(first.checkpoint().expect("checkpoint").expect("some"));
            watermark = first.committed_seq();
        }
    }
    let checkpoint = checkpoint.expect("a checkpoint was taken");
    let watermark = watermark.expect("something was committed by sequence 120");
    // The sink is dropped without `finish`, the way a killed process leaves it.
    drop(first);

    materialise(&reactor, scratch.path());
    let before = scratch.names();
    assert!(
        before.iter().any(|n| n.ends_with(".tmp")),
        "the interrupted run left a temporary file: {before:?}"
    );

    let mut second = ipc_sink(&scratch, &reactor, &alloc, 20 * 5 * 4096);
    second
        .resume(&table_source_schema(), &checkpoint, Some(watermark))
        .expect("resume");
    let kept: Vec<String> = serde_kept(&checkpoint);
    let after = scratch.names();
    for name in &after {
        assert!(
            !name.ends_with(".tmp"),
            "resume left a temporary file: {name}"
        );
        assert!(
            kept.contains(name),
            "resume kept {name}, which the checkpoint does not list"
        );
    }
    assert!(second.stats().resumed_files_removed > 0);

    // Everything above the watermark is replayed, in the order the scheduler re-reads it.
    for seq in watermark + 1..total {
        block_on(second.write(
            seq,
            moruna_kernel::Payload::table(inputs[seq as usize].clone()).expect("payload"),
        ))
        .expect("write");
    }
    let summary = second.finish().expect("finish");
    assert_eq!(second.committed_seq(), Some(total - 1));
    assert!(
        summary.files.iter().any(|n| n == &kept[0]),
        "the committed files of the first run are still in the summary"
    );
    // The file that was open when the checkpoint was taken is not in it, so the numbering
    // continues past it and a resumed run never reuses a name (e.5).
    let next_index = checkpoint_next_index(&checkpoint);
    assert!(
        next_index > kept.len() as u32,
        "the open file's index was already taken: {next_index} against {} committed",
        kept.len()
    );
    assert_eq!(
        summary.files[kept.len()],
        format!("part-{next_index:05}.arrow"),
        "the numbering continues at {next_index}: {:?}",
        summary.files
    );

    let mut read: Vec<RecordBatch> = Vec::new();
    for name in &summary.files {
        let bytes = written(&reactor, scratch.path(), name);
        let reader = FileReader::try_new(std::io::Cursor::new(bytes), None).expect("reader");
        for batch in reader {
            read.push(batch.expect("batch"));
        }
    }
    assert_eq!(
        read.len(),
        inputs.len(),
        "every morsel is in the output once"
    );
    assert_eq!(
        read, inputs,
        "the resumed run reads back as an uninterrupted one"
    );

    // A checkpoint whose file range straddles the watermark means the checkpoint and the store
    // disagree; the sink refuses rather than guessing (f.8).
    let scratch = Scratch::new("t13-straddle");
    let mut sink = ipc_sink(&scratch, &FakeReactor::new(), &alloc, 1 << 20);
    let straddling = br#"{"version":1,"kind":"ipc","next_index":1,"committed":[{"name":"part-00000.arrow","seq_min":10,"seq_max":20,"rows":1,"bytes":1}]}"#;
    let outcome = sink.resume(&table_source_schema(), straddling, Some(15));
    let Err(MorunaError::Resume(msg)) = outcome else {
        panic!("a straddling file must be refused, got {outcome:?}");
    };
    assert!(msg.contains("straddles the watermark"), "{msg}");

    // So does a checkpoint of another kind, or of a version this build does not write.
    let wrong_kind = br#"{"version":1,"kind":"parquet","next_index":0,"committed":[]}"#;
    assert!(matches!(
        sink.resume(&table_source_schema(), wrong_kind, None),
        Err(MorunaError::Resume(_))
    ));
    let wrong_version = br#"{"version":2,"kind":"ipc","next_index":0,"committed":[]}"#;
    assert!(matches!(
        sink.resume(&table_source_schema(), wrong_version, None),
        Err(MorunaError::Resume(_))
    ));
    assert!(matches!(
        sink.resume(&table_source_schema(), b"not json", None),
        Err(MorunaError::Resume(_))
    ));
}

/// The index the next rolled file takes, as the checkpoint records it (e.5).
fn checkpoint_next_index(checkpoint: &[u8]) -> u32 {
    let text = std::str::from_utf8(checkpoint).expect("utf8");
    let at = text.find("\"next_index\":").expect("next_index") + "\"next_index\":".len();
    let rest = &text[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().expect("a number")
}

/// The names the checkpoint document lists as committed.
fn serde_kept(checkpoint: &[u8]) -> Vec<String> {
    let text = std::str::from_utf8(checkpoint).expect("utf8");
    let mut out = Vec::new();
    for piece in text.split("\"name\":\"").skip(1) {
        if let Some(end) = piece.find('"') {
            out.push(piece[..end].to_string());
        }
    }
    out
}

/// SI-T14. A sink says at startup whether it can be resumed: a resumable one answers `Some`
/// from the moment it opens, with an empty file list, and one that cannot answers `None` and
/// refuses `resume` in as many words. d.1, SC f.11.
#[test]
fn si_t14_unresumable_says_so() {
    let scratch = Scratch::new("t14");
    let reactor = FakeReactor::new();
    let alloc = FakeAllocator::new();

    // Not resumable: the run-mode tensor sinks leave the contract's defaults in place.
    for (format, per_morsel) in [
        (TensorFormat::Amb1, false),
        (TensorFormat::SafeTensors, false),
        (TensorFormat::SafeTensors, true),
    ] {
        let scratch = Scratch::new("t14-unresumable");
        let mut sink = TensorSink::new(
            TensorSinkConfig {
                path: scratch.path().to_path_buf(),
                format,
                one_file_per_morsel: per_morsel,
                name: "weights".to_string(),
            },
            Arc::new(reactor.clone()),
            Arc::new(alloc.clone()),
        )
        .expect("tensor sink");
        let schema: SourceSchema = tensor_source_schema(4);
        assert_eq!(
            sink.checkpoint().expect("checkpoint"),
            None,
            "a sink that cannot resume says so before it opens, not at resume time"
        );
        sink.open(&schema).expect("open");
        assert_eq!(sink.checkpoint().expect("checkpoint"), None);
        assert_eq!(sink.committed_seq(), None);
        sink.skip(3);
        assert_eq!(sink.committed_seq(), None);
        let outcome = sink.resume(&schema, b"", None);
        let Err(MorunaError::Resume(msg)) = outcome else {
            panic!("resume must be refused, got {outcome:?}");
        };
        assert_eq!(msg, "sink does not support resume");
    }

    // Resumable: `Some` at startup, with an empty committed list (SC f.11).
    let per_morsel = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            format: TensorFormat::Amb1,
            one_file_per_morsel: true,
            name: "weights".to_string(),
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("tensor sink");
    let parquet = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    let ipc = ipc_sink(&scratch, &reactor, &alloc, 1 << 20);
    for (name, state) in [
        ("mrb1 per morsel", per_morsel.checkpoint()),
        ("parquet", parquet.checkpoint()),
        ("ipc", ipc.checkpoint()),
    ] {
        let state = state.expect("checkpoint").unwrap_or_else(|| {
            panic!("{name} is resumable and must answer Some before its first write")
        });
        let text = String::from_utf8(state).expect("utf8");
        assert!(text.contains("\"committed\":[]"), "{name}: {text}");
        assert!(text.contains("\"version\":1"), "{name}: {text}");
    }
    assert_eq!(per_morsel.committed_seq(), None);
    assert_eq!(parquet.committed_seq(), None);
    assert_eq!(ipc.committed_seq(), None);

    // A reorder buffer over a resumable sink answers for it, and adds nothing of its own.
    let inner = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor.clone()),
        Arc::new(alloc.clone()),
    )
    .expect("parquet sink");
    let buffer: ReorderBuffer<dyn Sink> =
        ReorderBuffer::new(Box::new(inner) as Box<dyn Sink>, 1 << 20);
    let state = buffer
        .checkpoint()
        .expect("checkpoint")
        .expect("the buffer delegates to a resumable sink");
    assert!(String::from_utf8(state).expect("utf8").contains("parquet"));
    assert_eq!(buffer.committed_seq(), None);
    buffer.skip(0);
    assert_eq!(
        buffer.committed_seq(),
        Some(0),
        "a skip reaches the inner sink's watermark"
    );
}
