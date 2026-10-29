//! The Arrow IPC source's SO tests, SO-T35 to SO-T37, SO-T39 to SO-T41 (07 k, e.9, e.10); SO-T34
//! is the round trip in `moruna-runtime`'s `tests/end_to_end.rs` (with the sink's compression,
//! SO-T42), SO-T38 the file pyarrow wrote and SO-T43 the compressed one, both in
//! `python/tests/test_ipc.py`.
//!
//! The files are written here with arrow's own `FileWriter`, a writer that is none of this
//! project's, in the shape the Parquet fixtures have: a known set of blocks and a ledger the
//! tests assert against rather than the reader under test.

// The contract error is large by design (contracts d.14); a builder closure returns it.
#![allow(clippy::result_large_err)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moruna_kernel::arrow::array::{
    Array, ArrayRef, AsArray, Float64Array, Int64Array, RecordBatch, StringArray,
    StringDictionaryBuilder,
};
use moruna_kernel::arrow::datatypes::{DataType, Field, Float64Type, Int32Type, Int64Type, Schema};
use moruna_kernel::arrow::ipc::writer::{FileWriter, IpcWriteOptions, StreamWriter};
use moruna_kernel::arrow::ipc::{CompressionType, MetadataVersion};
use moruna_kernel::buffer::arena_tier_of;
use moruna_kernel::{MorunaError, Payload, Reactor, RowRange, Source, SourceSchema, Split, Tier};
use moruna_sources::{ArrowIpcSource, ArrowIpcSourceConfig};
use moruna_testkit::ipc::Rng;
use moruna_testkit::{FakeAllocator, FakeReactor};

use crate::support::{CountingAllocator, block_on, err, scratch, seed};

/// What a generated IPC file holds: one entry per record batch block, the rows it has.
struct IpcLedger {
    path: PathBuf,
    blocks: Vec<u64>,
}

impl IpcLedger {
    fn id_at(&self, row: u64) -> i64 {
        row as i64
    }

    fn name_at(&self, row: u64) -> Option<String> {
        (!row.is_multiple_of(5)).then(|| format!("name-{row}"))
    }

    fn x_at(&self, row: u64) -> f64 {
        row as f64 * 0.5
    }

    fn nulls_in(&self, rows: std::ops::Range<u64>) -> u64 {
        rows.filter(|r| self.name_at(*r).is_none()).count() as u64
    }

    /// Where block `i` starts in the file's rows.
    fn start_of(&self, block: usize) -> u64 {
        self.blocks[..block].iter().sum()
    }
}

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
        Field::new("x", DataType::Float64, false),
    ]))
}

/// A batch of `ledger`'s rows `[start, start + rows)`.
fn batch_of(ledger: &IpcLedger, start: u64, rows: u64) -> RecordBatch {
    let range = start..start + rows;
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values(
            range.clone().map(|r| ledger.id_at(r)),
        )),
        Arc::new(StringArray::from_iter(
            range.clone().map(|r| ledger.name_at(r)),
        )),
        Arc::new(Float64Array::from_iter_values(
            range.map(|r| ledger.x_at(r)),
        )),
    ];
    RecordBatch::try_new(schema(), columns).expect("the fixture batch")
}

/// A file of one block per entry of `blocks`, rows numbered on from `first`.
fn write_ipc(dir: &Path, name: &str, first: u64, blocks: &[u64]) -> IpcLedger {
    let ledger = IpcLedger {
        path: dir.join(name),
        blocks: blocks.to_vec(),
    };
    let file = std::fs::File::create(&ledger.path).expect("the fixture file");
    let mut writer = FileWriter::try_new(file, &schema()).expect("a writer");
    let mut start = first;
    for rows in blocks {
        writer
            .write(&batch_of(&ledger, start, *rows))
            .expect("a block");
        start += rows;
    }
    writer.finish().expect("the footer");
    ledger
}

fn source_over(
    urls: &[&Path],
    reactor: FakeReactor,
    columns: Option<&[&str]>,
    split_rows: Option<u64>,
) -> moruna_kernel::Result<ArrowIpcSource> {
    ArrowIpcSource::new(
        ArrowIpcSourceConfig {
            urls: urls.iter().map(|p| p.display().to_string()).collect(),
            columns: columns.map(|c| c.iter().map(|s| (*s).to_string()).collect()),
            split_rows,
        },
        Arc::new(reactor) as Arc<dyn Reactor>,
    )
}

fn batch_of_payload(payload: &Payload) -> &RecordBatch {
    let Payload::Table(batch, _) = payload else {
        panic!("an Arrow IPC read gave a tensor");
    };
    batch
}

/// The ledger's rows `range` as ids, for comparison with a payload.
fn ids(payload: &Payload) -> Vec<i64> {
    let batch = batch_of_payload(payload);
    let column = batch
        .column_by_name("id")
        .expect("the id column")
        .as_primitive::<Int64Type>();
    (0..column.len()).map(|i| column.value(i)).collect()
}

/// The payload's rows against the ledger's, column by column.
fn assert_rows(ledger: &IpcLedger, payload: &Payload, start: u64, rows: u64) {
    let batch = batch_of_payload(payload);
    assert_eq!(batch.num_rows() as u64, rows);
    let id = batch
        .column_by_name("id")
        .expect("id")
        .as_primitive::<Int64Type>();
    let name = batch
        .column_by_name("name")
        .expect("name")
        .as_string::<i32>();
    let x = batch
        .column_by_name("x")
        .expect("x")
        .as_primitive::<Float64Type>();
    for i in 0..rows {
        let row = start + i;
        let at = i as usize;
        assert_eq!(id.value(at), ledger.id_at(row), "row {row}");
        assert_eq!(
            (!name.is_null(at)).then(|| name.value(at).to_string()),
            ledger.name_at(row),
            "row {row}"
        );
        assert_eq!(x.value(at), ledger.x_at(row), "row {row}");
    }
}

const BLOCKS: [u64; 7] = [100, 0, 333, 17, 1000, 1, 250];

// SO-T35 ipc_splits. SO-I2, SO-I4, SO-I6, e.9.
#[test]
fn so_t35_ipc_splits() {
    let dir = scratch();
    let ledger = write_ipc(&dir, "t35.arrow", 0, &BLOCKS);
    let reactor = seed(FakeReactor::new(), &ledger.path);
    let alloc = FakeAllocator::new();

    // One split per block, in file order, each with the block's rows, and the nulls the block's
    // own header counts.
    let source = source_over(&[&ledger.path], reactor.clone(), None, None).expect("the plan");
    let splits = source.plan().expect("the plan");
    assert_eq!(splits.len(), BLOCKS.len(), "one split per block");
    for (i, split) in splits.iter().enumerate() {
        assert_eq!(split.id, i as u32);
        assert_eq!(split.rows, BLOCKS[i], "block {i}");
        assert!(split.sub_splittable);
        assert!(split.uncompressed_bytes > 0);
        assert_eq!(split.column_bytes.len(), 3);
        assert_eq!(
            split.column_bytes.iter().sum::<u64>(),
            split.uncompressed_bytes
        );
        let start = ledger.start_of(i);
        assert_eq!(
            split.null_counts,
            vec![
                Some(0),
                Some(ledger.nulls_in(start..start + BLOCKS[i])),
                Some(0)
            ],
            "block {i}"
        );
        let payload = block_on(source.read(split, None, &alloc, Tier::Host)).expect("a block");
        assert_rows(&ledger, &payload, start, BLOCKS[i]);
    }
    let again = source.plan().expect("again");
    assert_eq!(
        again.iter().map(|s| (s.id, s.rows)).collect::<Vec<_>>(),
        splits.iter().map(|s| (s.id, s.rows)).collect::<Vec<_>>(),
        "SO-I8"
    );
    let SourceSchema::Table(declared) = source.schema() else {
        panic!("a table");
    };
    assert_eq!(declared.fields(), schema().fields());

    // Grouped toward 400 rows: whole blocks, [100, 0, 333], [17, 1000], [1, 250].
    let source = source_over(&[&ledger.path], reactor.clone(), None, Some(400)).expect("the plan");
    let grouped = source.plan().expect("the plan");
    assert_eq!(
        grouped.iter().map(|s| s.rows).collect::<Vec<_>>(),
        [433, 1017, 251]
    );
    for split in &grouped {
        let start: u64 = grouped[..split.id as usize].iter().map(|s| s.rows).sum();
        let whole = block_on(source.read(split, None, &alloc, Tier::Host)).expect("a split");
        assert_rows(&ledger, &whole, start, split.rows);
        // Ranges across block edges concatenate to the split (SO-I4) and read the same twice
        // (SO-I6).
        let mut joined = Vec::new();
        let mut at = 0;
        for width in [1u64, 99, 100, 7, 300, 1000] {
            let end = (at + width).min(split.rows);
            if at == end {
                continue;
            }
            let range = RowRange { start: at, end };
            let part =
                block_on(source.read(split, Some(range), &alloc, Tier::Host)).expect("a range");
            let again =
                block_on(source.read(split, Some(range), &alloc, Tier::Host)).expect("again");
            assert_eq!(batch_of_payload(&part), batch_of_payload(&again));
            assert_rows(&ledger, &part, start + at, end - at);
            joined.extend(ids(&part));
            at = end;
        }
        assert_eq!(joined, ids(&whole), "ranges concatenate to the split");
    }
    let bad = RowRange {
        start: 0,
        end: grouped[0].rows + 1,
    };
    assert!(matches!(
        err(block_on(source.read(
            &grouped[0],
            Some(bad),
            &alloc,
            Tier::Host
        ))),
        MorunaError::Source { .. }
    ));
    let stranger = Split {
        id: 99,
        ..grouped[0].clone()
    };
    assert!(matches!(
        err(block_on(source.read(&stranger, None, &alloc, Tier::Host))),
        MorunaError::Source { split: 99, .. }
    ));

    // A projection keeps the file's order, whatever order it names the columns in.
    let source =
        source_over(&[&ledger.path], reactor, Some(&["x", "id", "x"]), None).expect("the plan");
    let SourceSchema::Table(declared) = source.schema() else {
        panic!("a table");
    };
    let names: Vec<_> = declared
        .fields()
        .iter()
        .map(|f| f.name().as_str())
        .collect();
    assert_eq!(names, ["id", "x"]);
    let splits = source.plan().expect("the plan");
    assert_eq!(splits[2].null_counts, vec![Some(0), Some(0)]);
    let payload = block_on(source.read(&splits[2], None, &alloc, Tier::Host)).expect("a block");
    assert_eq!(
        batch_of_payload(&payload).schema().fields(),
        declared.fields()
    );
    assert_eq!(ids(&payload), (100..433).collect::<Vec<i64>>());
}

// SO-T35 continued: a read is the reactor's landing, the arena's and in place (SO-I3, SO-I5).
#[test]
fn so_t35_ipc_read_lands_in_the_arena_and_copies_nothing() {
    let dir = scratch();
    let ledger = write_ipc(&dir, "t35b.arrow", 0, &[500, 500]);
    let reactor = seed(FakeReactor::new(), &ledger.path);
    let source = source_over(&[&ledger.path], reactor, None, None).expect("the plan");
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let splits = source.plan().expect("the plan");
    let baseline = alloc.fake().in_use(Tier::Host);
    let payload = block_on(source.read(&splits[1], None, &alloc, Tier::Host)).expect("a block");
    assert_rows(&ledger, &payload, 500, 500);
    for column in batch_of_payload(&payload).columns() {
        let data = column.to_data();
        for buffer in data.buffers() {
            assert_eq!(arena_tier_of(buffer), Some(Tier::Host));
        }
    }
    let stats = source.stats();
    assert_eq!(stats.reads, 1);
    assert_eq!(
        stats.decode_bytes, 0,
        "a block that is a split is decoded in place"
    );
    assert!(stats.zero_copy_bytes > 0);
    assert_eq!(alloc.payload_copies(), 0);
    drop(payload);
    assert_eq!(
        alloc.fake().in_use(Tier::Host),
        baseline,
        "the arena got its bytes back"
    );

    // Blocks joined into one split take the decode copy once.
    let reactor = seed(FakeReactor::new(), &ledger.path);
    let source = source_over(&[&ledger.path], reactor, None, Some(1000)).expect("the plan");
    let splits = source.plan().expect("the plan");
    assert_eq!(splits.len(), 1);
    let payload = block_on(source.read(&splits[0], None, &alloc, Tier::Host)).expect("a split");
    assert_rows(&ledger, &payload, 0, 1000);
    assert!(source.stats().decode_bytes > 0);
    assert_eq!(alloc.payload_copies(), 1);
    for column in batch_of_payload(&payload).columns() {
        for buffer in column.to_data().buffers() {
            assert_eq!(arena_tier_of(buffer), Some(Tier::Host));
        }
    }
}

// SO-T35 continued: directories, names and what a plan refuses by configuration (h).
#[test]
fn so_t35_ipc_directories_and_plan_errors() {
    let dir = scratch();
    // `b` is listed after `a`, whatever order the directory returns them in; `c.txt` is not an
    // Arrow file.
    let b = write_ipc(&dir, "b.arrow", 100, &[10, 20]);
    let a = write_ipc(&dir, "a.arrow", 0, &[100]);
    std::fs::write(dir.join("c.txt"), b"not arrow").expect("a text file");
    let reactor = seed(seed(FakeReactor::new(), &a.path), &b.path);
    let source = source_over(&[&dir], reactor.clone(), None, None).expect("the plan");
    let splits = source.plan().expect("the plan");
    assert_eq!(
        splits.iter().map(|s| s.rows).collect::<Vec<_>>(),
        [100, 10, 20]
    );
    assert_eq!(splits.iter().map(|s| s.id).collect::<Vec<_>>(), [0, 1, 2]);
    let alloc = FakeAllocator::new();
    let last = block_on(source.read(&splits[2], None, &alloc, Tier::Host)).expect("a block");
    assert_rows(&b, &last, 110, 20);
    // Grouping never crosses a file.
    let source = source_over(&[&dir], reactor.clone(), None, Some(1_000)).expect("the plan");
    assert_eq!(source.plan().expect("the plan").len(), 2);
    // Two urls are read in the order given.
    let source = source_over(&[&b.path, &a.path], reactor.clone(), None, None).expect("the plan");
    assert_eq!(
        source
            .plan()
            .expect("the plan")
            .iter()
            .map(|s| s.rows)
            .collect::<Vec<_>>(),
        [10, 20, 100]
    );

    let named = |e: MorunaError, parts: &[&str]| {
        let text = e.to_string();
        assert!(matches!(e, MorunaError::Plan(_)), "{text}");
        for part in parts {
            assert!(text.contains(part), "{text} lacks {part}");
        }
    };
    // Files with another schema: both are named.
    let other = {
        let path = dir.join("other").join("o.arrow");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        let file = std::fs::File::create(&path).expect("a file");
        let narrow = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let mut writer = FileWriter::try_new(file, &narrow).expect("a writer");
        let batch = RecordBatch::try_new(
            narrow,
            vec![Arc::new(Int64Array::from_iter_values(0..3)) as ArrayRef],
        )
        .expect("a batch");
        writer.write(&batch).expect("a block");
        writer.finish().expect("the footer");
        path
    };
    let reactor = seed(reactor, &other);
    let e = err(source_over(&[&a.path, &other], reactor.clone(), None, None));
    named(
        e,
        &[
            &a.path.display().to_string(),
            &other.display().to_string(),
            "different schemas",
        ],
    );
    // A column the file lacks, named with the file.
    let e = err(source_over(
        &[&a.path],
        reactor.clone(),
        Some(&["id", "nope"]),
        None,
    ));
    named(e, &["\"nope\"", &a.path.display().to_string()]);
    // No url, a directory with nothing in it, a url that is not local, a missing path, a split of
    // no rows.
    named(
        err(source_over(&[], reactor.clone(), None, None)),
        &["at least one url"],
    );
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).expect("a directory");
    named(
        err(source_over(&[&empty], reactor.clone(), None, None)),
        &["no `*.arrow` file"],
    );
    let e = err(ArrowIpcSource::new(
        ArrowIpcSourceConfig {
            urls: vec!["s3://bucket/a.arrow".into()],
            ..Default::default()
        },
        Arc::new(reactor.clone()) as Arc<dyn Reactor>,
    ));
    named(e, &["s3://bucket/a.arrow", "local"]);
    assert!(matches!(
        err(source_over(
            &[&dir.join("missing.arrow")],
            reactor.clone(),
            None,
            None
        )),
        MorunaError::Plan(_)
    ));
    named(
        err(source_over(&[&a.path], reactor.clone(), None, Some(0))),
        &["split_rows"],
    );

    // A column of dictionary type is refused, naming the file.
    let dictionary = {
        let path = dir.join("dictionary.arrow");
        let mut builder = StringDictionaryBuilder::<Int32Type>::new();
        builder.append_value("a");
        builder.append_value("b");
        let column: ArrayRef = Arc::new(builder.finish());
        let schema = Arc::new(Schema::new(vec![Field::new(
            "d",
            column.data_type().clone(),
            false,
        )]));
        let file = std::fs::File::create(&path).expect("a file");
        let mut writer = FileWriter::try_new(file, &schema).expect("a writer");
        writer
            .write(&RecordBatch::try_new(schema, vec![column]).expect("a batch"))
            .expect("a block");
        writer.finish().expect("the footer");
        path
    };
    let e = err(source_over(
        &[&dictionary],
        seed(FakeReactor::new(), &dictionary),
        None,
        None,
    ));
    named(e, &[&dictionary.display().to_string(), "dictionary"]);

    // A file with no block is one split of no rows, and reads as an empty payload (SO-I7).
    let none = write_ipc(&dir, "none.arrow", 0, &[]);
    let source = source_over(
        &[&none.path],
        seed(FakeReactor::new(), &none.path),
        None,
        None,
    )
    .expect("the plan");
    let splits = source.plan().expect("the plan");
    assert_eq!(splits.len(), 1);
    assert_eq!(splits[0].rows, 0);
    let payload = block_on(source.read(&splits[0], None, &alloc, Tier::Host)).expect("empty");
    assert_eq!(batch_of_payload(&payload).num_rows(), 0);
    assert_eq!(
        batch_of_payload(&payload).schema().fields(),
        schema().fields()
    );
}

// SO-T36 ipc_malformed_never_panics. SO-I10, e.9, e.10, contracts CT-I13.

/// How the cases ended, so the test can say its cases reached every place a file can be refused.
static REFUSED_AT_PLAN: AtomicUsize = AtomicUsize::new(0);
static REFUSED_AT_READ: AtomicUsize = AtomicUsize::new(0);
static READ_WHOLE: AtomicUsize = AtomicUsize::new(0);

/// What one malformed file did: planned and read (`Ok`) or refused (`Err`), or panicked.
fn exercise(path: &Path, on_disk: &[u8], planned_on: &[u8], label: &str) -> Result<(), String> {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        std::fs::write(path, planned_on).expect("the file to plan");
        let reactor = FakeReactor::new().with_file(&path.display().to_string(), on_disk.to_vec());
        let source = match source_over(&[path], reactor, None, None) {
            Ok(source) => source,
            Err(e) => {
                let text = e.to_string();
                assert!(
                    matches!(e, MorunaError::Source { .. } | MorunaError::Plan(_)),
                    "{label}: a plan failed with {text}, not Source or Plan"
                );
                assert!(
                    text.contains(&path.display().to_string()),
                    "{label}: the plan error does not name the file: {text}"
                );
                REFUSED_AT_PLAN.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        std::fs::write(path, on_disk).expect("the file to read");
        let SourceSchema::Table(schema) = source.schema() else {
            panic!("an Arrow IPC source declares a table");
        };
        let alloc = FakeAllocator::new();
        let baseline = alloc.in_use(Tier::Host);
        for split in source.plan().expect("a planned source plans") {
            let read = block_on(source.read(&split, None, &alloc, Tier::Host));
            match read {
                Ok(Payload::Table(batch, _)) => {
                    assert_eq!(batch.num_rows() as u64, split.rows, "{label}: split rows");
                    assert_eq!(batch.schema().fields(), schema.fields(), "{label}: columns");
                    READ_WHOLE.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Payload::Tensor(_, _)) => panic!("{label}: an Arrow IPC read gave a tensor"),
                Err(MorunaError::Source { split: id, msg }) => {
                    REFUSED_AT_READ.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(id, split.id, "{label}: the error names the split");
                    assert!(
                        msg.contains(&path.display().to_string()),
                        "{label}: the read error does not name the file: {msg}"
                    );
                }
                Err(other) => panic!("{label}: a read failed with {other}, not Source"),
            }
        }
        assert_eq!(
            alloc.in_use(Tier::Host),
            baseline,
            "{label}: the arena is whole"
        );
    }));
    outcome.map_err(|panic| {
        let what = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_default();
        format!("{label}: {what}")
    })
}

/// The byte of the file at which the footer starts, and the block message offsets, from a
/// file read with arrow's own reader.
fn landmarks(bytes: &[u8]) -> (usize, Vec<usize>) {
    let len = bytes.len();
    let footer_len = u32::from_le_bytes([
        bytes[len - 10],
        bytes[len - 9],
        bytes[len - 8],
        bytes[len - 7],
    ]) as usize;
    let footer = &bytes[len - 10 - footer_len..len - 10];
    let parsed = moruna_kernel::arrow::ipc::root_as_footer(footer).expect("the fixture footer");
    let blocks = parsed
        .recordBatches()
        .expect("blocks")
        .iter()
        .map(|b| b.offset() as usize)
        .collect();
    (len - 10 - footer_len, blocks)
}

#[test]
fn so_t36_ipc_malformed_never_panics() {
    let dir = scratch();
    let ledger = write_ipc(&dir, "t36.arrow", 0, &[40, 7, 25]);
    let full = std::fs::read(&ledger.path).expect("the file");
    let len = full.len();
    let (footer_start, blocks) = landmarks(&full);
    let path = dir.join("case.arrow");
    let mut rng = Rng::new(0x5036_0000_0000_0036);
    let mut failures = Vec::new();

    // Every truncation, cut before the plan and cut after it.
    for cut in 0..len {
        let short = &full[..cut];
        failures.extend(
            exercise(
                &path,
                short,
                short,
                &format!("cut to {cut} before the plan"),
            )
            .err(),
        );
        failures
            .extend(exercise(&path, short, &full, &format!("cut to {cut} after the plan")).err());
    }

    // Every byte of the footer, its trailer and each block's message set to 0x00 and to 0xFF;
    // the message of a block is its first 200 bytes at most.
    let mut spots: Vec<usize> = (footer_start..len).collect();
    for block in &blocks {
        spots.extend(*block..block + 200.min(len - block));
    }
    for at in spots {
        for value in [0x00u8, 0xFF, 0x7F, 0x01] {
            let mut bytes = full.clone();
            bytes[at] = value;
            failures
                .extend(exercise(&path, &bytes, &bytes, &format!("byte {at}={value:#04x}")).err());
        }
    }

    // Random corruptions of one to four bytes anywhere in the file, and of the file after the
    // plan.
    for case in 0..600 {
        let mut bytes = full.clone();
        let mut label = format!("random {case}:");
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(len);
            let value = rng.next_u64() as u8;
            bytes[at] = value;
            label.push_str(&format!(" {at}={value:#04x}"));
        }
        failures.extend(exercise(&path, &bytes, &bytes, &label).err());
        failures.extend(exercise(&path, &bytes, &full, &format!("{label} after the plan")).err());
    }
    assert!(
        failures.is_empty(),
        "{} malformed files panicked; the first: {:?}",
        failures.len(),
        &failures[..failures.len().min(10)]
    );
    for (name, count) in [
        ("refused at the plan", &REFUSED_AT_PLAN),
        ("refused at a read", &REFUSED_AT_READ),
        ("read whole", &READ_WHOLE),
    ] {
        assert!(count.load(Ordering::Relaxed) > 20, "{name}: too few cases");
    }

    // The three named faults say what they are and where, to the file and the block.
    let source_error = |bytes: &[u8], planned_on: &[u8]| -> String {
        std::fs::write(&path, planned_on).expect("the file to plan");
        let reactor = FakeReactor::new().with_file(&path.display().to_string(), bytes.to_vec());
        let alloc = FakeAllocator::new();
        match source_over(&[&path], reactor, None, None) {
            Err(e) => e.to_string(),
            Ok(source) => source
                .plan()
                .expect("the plan")
                .iter()
                .find_map(|s| block_on(source.read(s, None, &alloc, Tier::Host)).err())
                .map(|e| e.to_string())
                .expect("a fault"),
        }
    };
    let file = path.display().to_string();
    // A truncated file: the end is gone, the file says it is cut short.
    let text = source_error(&full[..len - 20], &full[..len - 20]);
    assert!(text.contains(&file) && text.contains("ARROW1"), "{text}");
    // A corrupted footer: the length of the footer is wrong.
    let mut bytes = full.clone();
    bytes[len - 10..len - 6].copy_from_slice(&u32::MAX.to_le_bytes());
    let text = source_error(&bytes, &bytes);
    assert!(text.contains(&file) && text.contains("footer"), "{text}");
    // A corrupted block: the second block's message is not a message.
    let mut bytes = full.clone();
    bytes[blocks[1]..blocks[1] + 8].fill(0);
    let text = source_error(&bytes, &bytes);
    assert!(text.contains(&file) && text.contains("block 1"), "{text}");
    // A corrupted body, found when the block is read, not when it is planned: the check refuses
    // a record batch whose buffers lie outside its body, by block.
    let mut bytes = full.clone();
    let (start, end) = (blocks[2] + 8, blocks[2] + 200.min(len - blocks[2]));
    let mut found = None;
    for at in start..end {
        let mut tried = bytes.clone();
        tried[at] = 0xFF;
        let text = {
            std::fs::write(&path, &full).expect("the file to plan");
            let reactor = FakeReactor::new().with_file(&path.display().to_string(), tried);
            let alloc = FakeAllocator::new();
            let source =
                source_over(&[&path], reactor, None, None).expect("planned on the whole file");
            let splits = source.plan().expect("the plan");
            block_on(source.read(&splits[2], None, &alloc, Tier::Host))
                .err()
                .map(|e| e.to_string())
        };
        if let Some(text) = text
            && text.contains("block 2")
        {
            found = Some(text);
            break;
        }
    }
    bytes.clear();
    let text = found
        .expect("a byte of block 2's message whose corruption is a read error naming the block");
    assert!(text.contains(&file), "{text}");
}

// SO-T37 ipc_stream_refused. e.9.
#[test]
fn so_t37_ipc_stream_refused() {
    let dir = scratch();
    let ledger = write_ipc(&dir, "ledger.arrow", 0, &[10, 10]);
    let path = dir.join("stream.arrow");
    let mut bytes = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut bytes, &schema()).expect("a stream writer");
        writer.write(&batch_of(&ledger, 0, 10)).expect("a batch");
        writer.finish().expect("the end of the stream");
    }
    std::fs::write(&path, &bytes).expect("the stream");
    let e = err(source_over(
        &[&path],
        FakeReactor::new().with_file(&path.display().to_string(), bytes),
        None,
        None,
    ));
    let text = e.to_string();
    assert!(matches!(e, MorunaError::Plan(_)), "{text}");
    assert!(text.contains("stream"), "{text}");
    assert!(text.contains("file format"), "{text}");
    assert!(text.contains(&path.display().to_string()), "{text}");

    // A directory holding one stream and one file refuses on the stream, naming it.
    let mixed = scratch();
    std::fs::copy(&path, mixed.join("a.arrow")).expect("a copy");
    std::fs::copy(&ledger.path, mixed.join("b.arrow")).expect("a copy");
    let reactor = FakeReactor::new();
    let e = err(source_over(&[&mixed], reactor, None, None));
    assert!(
        e.to_string().contains("a.arrow") && e.to_string().contains("stream"),
        "{e}"
    );

    // Not Arrow at all is said as that.
    let text = dir.join("text.arrow");
    std::fs::write(&text, b"just some text that is not arrow at all").expect("a text file");
    let e = err(source_over(&[&text], FakeReactor::new(), None, None));
    assert!(e.to_string().contains("ARROW1"), "{e}");
}

// SO-T40 ipc_compressed_files. e.9, e.10, contracts e.8, KA10.

/// As `write_ipc`, with every body buffer compressed by `codec`: arrow's writer, none of this
/// project's.
fn write_ipc_compressed(
    dir: &Path,
    name: &str,
    first: u64,
    blocks: &[u64],
    codec: CompressionType,
) -> IpcLedger {
    let ledger = IpcLedger {
        path: dir.join(name),
        blocks: blocks.to_vec(),
    };
    let file = std::fs::File::create(&ledger.path).expect("the fixture file");
    let options = IpcWriteOptions::try_new(8, false, MetadataVersion::V5)
        .and_then(|o| o.try_with_compression(Some(codec)))
        .expect("write options");
    let mut writer = FileWriter::try_new_with_options(file, &schema(), options).expect("a writer");
    let mut start = first;
    for rows in blocks {
        writer
            .write(&batch_of(&ledger, start, *rows))
            .expect("a block");
        start += rows;
    }
    writer.finish().expect("the footer");
    ledger
}

const CODECS: [(&str, CompressionType); 2] = [
    ("zstd", CompressionType::ZSTD),
    ("lz4", CompressionType::LZ4_FRAME),
];

#[test]
fn so_t40_ipc_compressed_files_are_read_in_the_arena() {
    for (name, codec) in CODECS {
        let dir = scratch();
        let ledger = write_ipc_compressed(&dir, "t40.arrow", 0, &BLOCKS, codec);
        let source = source_over(
            &[&ledger.path],
            seed(FakeReactor::new(), &ledger.path),
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let splits = source.plan().expect("the plan");
        assert_eq!(splits.len(), BLOCKS.len(), "{name}");
        let alloc = CountingAllocator::new(FakeAllocator::new());
        let baseline = alloc.fake().in_use(Tier::Host);
        let mut start = 0;
        for (split, rows) in splits.iter().zip(BLOCKS) {
            assert_eq!(split.rows, rows, "{name}");
            let payload = block_on(source.read(split, None, &alloc, Tier::Host))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_rows(&ledger, &payload, start, rows);
            // The inflated body is an arena buffer the batch is decoded over: nothing is copied
            // into the arena once more.
            for column in batch_of_payload(&payload).columns() {
                for buffer in column.to_data().buffers() {
                    if rows == 0 {
                        // A block of no rows is the decoder's own empty arrays.
                        continue;
                    }
                    assert_eq!(
                        arena_tier_of(buffer),
                        Some(Tier::Host),
                        "{name}: a buffer of {} bytes in {:?} was not read in the arena ({rows} rows)",
                        buffer.len(),
                        column.data_type()
                    );
                }
            }
            start += rows;
        }
        assert_eq!(source.stats().decode_bytes, 0, "{name}: decoded in place");
        assert_eq!(alloc.payload_copies(), 0, "{name}");
        assert_eq!(alloc.fake().in_use(Tier::Host), baseline, "{name}");

        // Several blocks to a split, a projection, and a row range inside one block.
        let source = source_over(
            &[&ledger.path],
            seed(FakeReactor::new(), &ledger.path),
            Some(&["x", "id"]),
            Some(400),
        )
        .expect("the plan");
        let splits = source.plan().expect("the plan");
        let payload = block_on(source.read(&splits[0], None, &alloc, Tier::Host)).expect("a split");
        assert_eq!(batch_of_payload(&payload).num_columns(), 2, "{name}");
        assert_eq!(
            ids(&payload),
            (0..splits[0].rows as i64).collect::<Vec<_>>()
        );
        let range = RowRange { start: 7, end: 30 };
        let payload =
            block_on(source.read(&splits[0], Some(range), &alloc, Tier::Host)).expect("a range");
        assert_eq!(ids(&payload), (7..30).collect::<Vec<i64>>(), "{name}");
    }
}

#[test]
fn so_t40_ipc_compressed_files_of_every_type() {
    let every = moruna_testkit::ipc::batches()
        .into_iter()
        .find(|(label, _, _)| *label == "every_type")
        .expect("the every_type batch")
        .1
        .remove(0);
    for (name, codec) in CODECS {
        let dir = scratch();
        let path = dir.join("every.arrow");
        let options = IpcWriteOptions::try_new(8, false, MetadataVersion::V5)
            .and_then(|o| o.try_with_compression(Some(codec)))
            .expect("write options");
        let mut writer = FileWriter::try_new_with_options(
            std::fs::File::create(&path).expect("the file"),
            &every.schema(),
            options,
        )
        .expect("a writer");
        writer.write(&every).expect("a block");
        writer.write(&every.slice(1, 2)).expect("a block");
        writer.finish().expect("the footer");

        let source = source_over(&[&path], seed(FakeReactor::new(), &path), None, None)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let alloc = FakeAllocator::new();
        let splits = source.plan().expect("the plan");
        assert_eq!(splits.len(), 2, "{name}");
        let first = block_on(source.read(&splits[0], None, &alloc, Tier::Host))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(batch_of_payload(&first), &every, "{name}");
        let second = block_on(source.read(&splits[1], None, &alloc, Tier::Host))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(batch_of_payload(&second), &every.slice(1, 2), "{name}");
    }
}

#[test]
fn so_t40_ipc_compressed_block_over_the_budget_is_refused_by_name() {
    for (name, codec) in CODECS {
        let dir = scratch();
        let ledger = write_ipc_compressed(&dir, "t40c.arrow", 0, &[5_000], codec);
        let source = source_over(
            &[&ledger.path],
            seed(FakeReactor::new(), &ledger.path),
            None,
            None,
        )
        .expect("the plan");
        let splits = source.plan().expect("the plan");
        // The arena has room for the landed block and a little more; half of what is left is
        // the ceiling, and the block inflates to more.
        let alloc = FakeAllocator::new().with_limit(Tier::Host, 256 << 10);
        let refused = err(block_on(source.read(&splits[0], None, &alloc, Tier::Host)));
        let text = refused.to_string();
        assert!(
            matches!(refused, MorunaError::Source { .. }),
            "{name}: {text}"
        );
        assert!(
            text.contains("block 0") && text.contains("bytes this reader may inflate"),
            "{name}: {text}"
        );
        assert_eq!(alloc.in_use(Tier::Host), 0, "{name}");
    }
}

// SO-T41 ipc_compressed_malformed_never_panics. SO-I10, contracts CT-I13, e.8.
#[test]
fn so_t41_ipc_compressed_malformed_never_panics() {
    for (name, codec) in CODECS {
        let dir = scratch();
        let ledger = write_ipc_compressed(&dir, "t41.arrow", 0, &[40, 7, 25], codec);
        let full = std::fs::read(&ledger.path).expect("the file");
        let len = full.len();
        let (footer_start, blocks) = landmarks(&full);
        let path = dir.join("case.arrow");
        let mut rng = Rng::new(0x5041_0000_0000_0041 ^ len as u64);
        let mut failures = Vec::new();

        // Every truncation, before the plan and after it.
        for cut in (0..len).step_by(3) {
            let short = &full[..cut];
            failures.extend(
                exercise(
                    &path,
                    short,
                    short,
                    &format!("{name}: cut to {cut} before the plan"),
                )
                .err(),
            );
            failures.extend(
                exercise(
                    &path,
                    short,
                    &full,
                    &format!("{name}: cut to {cut} after the plan"),
                )
                .err(),
            );
        }
        // The footer, and the bodies' first bytes, where each buffer's length is.
        let mut spots: Vec<usize> = (footer_start..len).collect();
        for block in &blocks {
            spots.extend(*block..block + 140.min(len - block));
        }
        for at in spots {
            for value in [0x00u8, 0xFF, 0x7F, 0x01] {
                let mut bytes = full.clone();
                bytes[at] = value;
                failures.extend(
                    exercise(
                        &path,
                        &bytes,
                        &bytes,
                        &format!("{name}: byte {at}={value:#04x}"),
                    )
                    .err(),
                );
            }
        }
        // Random edits anywhere, which are mostly inside compressed frames.
        for case in 0..600 {
            let mut bytes = full.clone();
            let mut label = format!("{name}: random {case}:");
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(len);
                let value = rng.next_u64() as u8;
                bytes[at] = value;
                label.push_str(&format!(" {at}={value:#04x}"));
            }
            failures.extend(exercise(&path, &bytes, &bytes, &label).err());
            failures
                .extend(exercise(&path, &bytes, &full, &format!("{label} after the plan")).err());
        }
        assert!(
            failures.is_empty(),
            "{} malformed compressed files panicked; the first: {:?}",
            failures.len(),
            &failures[..failures.len().min(10)]
        );
    }
}
