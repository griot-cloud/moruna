//! The Vortex source's SO tests, SO-T23 to SO-T29 (07 k, e.7, e.8).
//!
//! The files are written here with the Vortex library's own writer and default strategy
//! (8,192-row zones, cascading compression), in the shape the Parquet fixtures have: a known
//! row count, a null ratio, and a ledger the tests assert against rather than the reader under
//! test. Each goes to a scratch directory unique to this process.

// The contract error is large by design (contracts d.14); a builder closure returns it.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, AsArray, Float64Array, Int64Array, RecordBatch, StringArray,
};
use moruna_kernel::arrow::datatypes::{DataType, Field, Float64Type, Int64Type, Schema};
use moruna_kernel::buffer::arena_tier_of;
use moruna_kernel::{
    Allocator, MorunaError, ObjectMetadata, Payload, Reactor, RowRange, Source, SourceSchema,
    Split, Tier,
};
use moruna_sources::{VortexSource, VortexSourceConfig};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};
use vortex::VortexSessionDefault;
use vortex::arrow::ArrowSessionExt;
use vortex::file::WriteOptionsSessionExt;
use vortex::io::runtime::BlockingRuntime;
use vortex::io::runtime::current::CurrentThreadRuntime;
use vortex::io::session::RuntimeSessionExt;
use vortex::session::VortexSession;

use crate::support::{
    CountingAllocator, NoObjectStore, block_on, err, scratch, seed, seed_truncated,
};

const ZONE: u64 = 8192;

/// What a generated Vortex file holds.
struct VxLedger {
    path: PathBuf,
    rows: u64,
    null_every: u64,
}

impl VxLedger {
    fn id_at(&self, row: u64) -> i64 {
        row as i64
    }

    fn x_at(&self, row: u64) -> Option<i64> {
        if self.null_every > 0 && row.is_multiple_of(self.null_every) {
            None
        } else {
            Some(row as i64 * 3)
        }
    }

    fn r_at(&self, row: u64) -> i64 {
        let mut z = row.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as i64
    }

    fn f_at(&self, row: u64) -> f64 {
        row as f64 * 0.25
    }

    fn s_at(&self, row: u64) -> String {
        format!("s{row}")
    }

    fn nulls_in(&self, rows: std::ops::Range<u64>) -> u64 {
        rows.filter(|r| self.x_at(*r).is_none()).count() as u64
    }
}

/// Columns `id` (sequential, which Vortex encodes as a sequence), `x` (nullable), `r`
/// (pseudo-random, which no encoding shrinks, so it stays canonical), `f` and `s` (a string).
fn write_vortex(dir: &Path, name: &str, rows: u64, null_every: u64) -> VxLedger {
    let path = dir.join(format!("{name}.vortex"));
    let ledger = VxLedger {
        path: path.clone(),
        rows,
        null_every,
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("x", DataType::Int64, true),
        Field::new("r", DataType::Int64, false),
        Field::new("f", DataType::Float64, false),
        Field::new("s", DataType::Utf8, false),
    ]));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values(
            (0..rows).map(|r| ledger.id_at(r)),
        )),
        Arc::new(Int64Array::from_iter((0..rows).map(|r| ledger.x_at(r)))),
        Arc::new(Int64Array::from_iter_values(
            (0..rows).map(|r| ledger.r_at(r)),
        )),
        Arc::new(Float64Array::from_iter_values(
            (0..rows).map(|r| ledger.f_at(r)),
        )),
        Arc::new(StringArray::from_iter_values(
            (0..rows).map(|r| ledger.s_at(r)),
        )),
    ];
    let batch = RecordBatch::try_new(schema.clone(), columns).expect("the fixture batch");
    write_batch(&path, batch);
    ledger
}

fn write_batch(path: &Path, batch: RecordBatch) {
    let runtime = CurrentThreadRuntime::new();
    let session = VortexSession::default().with_handle(runtime.handle());
    let schema = batch.schema();
    let array = session
        .arrow()
        .from_arrow_record_batch(batch, &schema)
        .expect("to vortex");
    let mut bytes: Vec<u8> = Vec::new();
    runtime
        .block_on(
            session
                .write_options()
                .write(&mut bytes, array.to_array_stream()),
        )
        .expect("the vortex writer");
    std::fs::write(path, bytes).expect("the fixture file");
}

fn source_over(
    ledger: &VxLedger,
    reactor: FakeReactor,
    columns: Option<&[&str]>,
    split_bytes: u64,
) -> VortexSource {
    VortexSource::new(
        VortexSourceConfig {
            urls: vec![ledger.path.display().to_string()],
            columns: columns.map(|c| c.iter().map(|s| s.to_string()).collect()),
            split_bytes: Some(split_bytes),
        },
        Arc::new(reactor) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the vortex source")
}

fn table(payload: &Payload) -> &RecordBatch {
    match payload {
        Payload::Table(batch, _) => batch,
        Payload::Tensor(_, _) => panic!("expected a table payload"),
    }
}

/// Every value of `batch`, whichever of the columns it holds, against the ledger from file row
/// `first`.
fn check_rows(batch: &RecordBatch, ledger: &VxLedger, first: u64) {
    for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
        for i in 0..batch.num_rows() {
            let row = first + i as u64;
            match field.name().as_str() {
                "id" => assert_eq!(
                    column.as_primitive::<Int64Type>().value(i),
                    ledger.id_at(row)
                ),
                "x" => {
                    let x = column.as_primitive::<Int64Type>();
                    let got = x.is_valid(i).then(|| x.value(i));
                    assert_eq!(got, ledger.x_at(row), "x at row {row}");
                }
                "r" => assert_eq!(
                    column.as_primitive::<Int64Type>().value(i),
                    ledger.r_at(row)
                ),
                "f" => assert_eq!(
                    column.as_primitive::<Float64Type>().value(i),
                    ledger.f_at(row)
                ),
                "s" => assert_eq!(column.as_string::<i32>().value(i), ledger.s_at(row)),
                other => panic!("an unexpected column {other}"),
            }
        }
    }
}

/// The file row a split starts at: the sum of the rows of the splits before it.
fn starts(plan: &[Split]) -> Vec<u64> {
    let mut at = 0;
    plan.iter()
        .map(|s| {
            let start = at;
            at += s.rows;
            start
        })
        .collect()
}

fn rng(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^ (z >> 31)
}

// SO-T23 vortex_plan_granularity. The plan is cut at zone boundaries, each split a run of whole
// zones of about `split_bytes`; rows and null counts are the zone maps' own, a fixed-width
// column's bytes are exact, and only a projection with a variable-width column is estimated.
// e.7, SO-I2, SO-I8.
#[test]
fn so_t23_vortex_plan_granularity() {
    let dir = scratch();
    let ledger = write_vortex(&dir, "t17", 100_000, 7);
    let target = 1 << 20;
    let source = source_over(
        &ledger,
        seed(FakeReactor::new(), &ledger.path),
        None,
        target,
    );
    let plan = source.plan().expect("the plan");
    assert!(
        plan.len() > 2,
        "a 4 MiB file at 1 MiB splits: {}",
        plan.len()
    );
    assert_eq!(plan.iter().map(|s| s.rows).sum::<u64>(), ledger.rows);
    let first_rows = starts(&plan);
    for (split, start) in plan.iter().zip(&first_rows) {
        assert_eq!(start % ZONE, 0, "split {} starts inside a zone", split.id);
        let last = start + split.rows == ledger.rows;
        if !last {
            assert_eq!(
                split.rows % ZONE,
                0,
                "split {} is not whole zones",
                split.id
            );
            assert!(
                split.uncompressed_bytes >= target,
                "split {} is short",
                split.id
            );
            // One zone less would have been under the target: the run stops as soon as it is
            // reached.
            let per_row = split.uncompressed_bytes as f64 / split.rows as f64;
            assert!(
                (split.uncompressed_bytes as f64 - per_row * ZONE as f64) < target as f64,
                "split {} runs past the target",
                split.id
            );
        }
        assert!(split.sub_splittable);
        assert!(split.estimated, "a string column is estimated");
        assert_eq!(split.column_bytes.len(), 5);
        assert_eq!(split.column_bytes[0], split.rows * 8, "id is exact");
        assert_eq!(
            split.column_bytes[1],
            split.rows * 8 + split.rows.div_ceil(8),
            "x is exact, with its validity"
        );
        assert_eq!(
            split.uncompressed_bytes,
            split.column_bytes.iter().sum::<u64>()
        );
        assert_eq!(split.null_counts[0], Some(0));
        assert_eq!(
            split.null_counts[1],
            Some(ledger.nulls_in(*start..start + split.rows)),
            "x's nulls, from the zone maps"
        );
    }
    // SO-I8: the plan is repeatable.
    let again = source.plan().expect("again");
    assert_eq!(
        again.iter().map(|s| (s.id, s.rows)).collect::<Vec<_>>(),
        plan.iter().map(|s| (s.id, s.rows)).collect::<Vec<_>>()
    );
    let stats = source.stats();
    assert_eq!(stats.splits, plan.len() as u64);
    assert!(stats.footer_reads >= 2, "the footer and the zone maps");
    assert_eq!(
        stats.bytes_planned,
        plan.iter().map(|s| s.uncompressed_bytes).sum::<u64>()
    );

    // A fixed-width projection is exact throughout.
    let fixed = source_over(
        &ledger,
        seed(FakeReactor::new(), &ledger.path),
        Some(&["r", "id"]),
        target,
    );
    for split in fixed.plan().expect("plan") {
        assert!(!split.estimated);
        assert_eq!(split.uncompressed_bytes, split.rows * 16);
    }
    // A split size above the file is one split.
    let whole = source_over(
        &ledger,
        seed(FakeReactor::new(), &ledger.path),
        None,
        u64::MAX,
    );
    assert_eq!(whole.plan().expect("plan").len(), 1);
}

// SO-T24 vortex_projection. A projection by name keeps the file's column order, the schema
// and every read carry exactly those columns, and the split metadata is over them only. d.1.
#[test]
fn so_t24_vortex_projection() {
    let dir = scratch();
    let ledger = write_vortex(&dir, "t18", 20_000, 5);
    let source = source_over(
        &ledger,
        seed(FakeReactor::new(), &ledger.path),
        Some(&["s", "x"]),
        1 << 18,
    );
    let SourceSchema::Table(schema) = source.schema() else {
        panic!("a table schema");
    };
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, vec!["x", "s"], "file order, not projection order");
    assert_eq!(
        schema.field(1).data_type(),
        &DataType::Utf8,
        "offsets, not views"
    );
    let alloc = FakeAllocator::new();
    let plan = source.plan().expect("plan");
    for (split, first) in plan.iter().zip(starts(&plan)) {
        assert_eq!(split.column_bytes.len(), 2);
        let payload = block_on(source.read(split, None, &alloc, Tier::Host)).expect("read");
        let batch = table(&payload);
        assert_eq!(batch.schema().fields(), schema.fields());
        check_rows(batch, &ledger, first);
    }
}

// SO-T25 vortex_random_ranges. Fifty random row ranges read the generator's values;
// consecutive ranges concatenate to the split; an empty range is an empty payload; the read
// lands in the requested tier and the other host tier is the arena's own refusal. SO-I4,
// SO-I3, SO-I6.
#[test]
fn so_t25_vortex_random_ranges() {
    let dir = scratch();
    let ledger = write_vortex(&dir, "t19", 60_000, 11);
    let source = source_over(
        &ledger,
        seed(FakeReactor::new(), &ledger.path),
        None,
        1 << 20,
    );
    let plan = source.plan().expect("plan");
    let first_rows = starts(&plan);
    // `FakeAllocator` does not refuse the tier it is not pinned for, so the other host tier
    // is given a budget of zero, which is the `Alloc` the arena raises for it (as SO-T3).
    let alloc = FakeAllocator::new().with_limit(Tier::PinnedHost, 0);
    let mut seed_value = 17u64;
    for _ in 0..50 {
        let i = (rng(&mut seed_value) % plan.len() as u64) as usize;
        let split = &plan[i];
        let a = rng(&mut seed_value) % split.rows;
        let b = a + 1 + rng(&mut seed_value) % (split.rows - a);
        let payload = block_on(source.read(
            split,
            Some(RowRange { start: a, end: b }),
            &alloc,
            Tier::Host,
        ))
        .expect("a random range");
        assert_eq!(payload.rows(), b - a);
        assert_eq!(payload.tier(), Tier::Host);
        check_rows(table(&payload), &ledger, first_rows[i] + a);
    }
    // Consecutive ranges are the split, in order.
    let split = &plan[1];
    let mut rows = 0;
    for (a, b) in [(0, 10), (10, 25), (25, split.rows)] {
        let payload = block_on(source.read(
            split,
            Some(RowRange { start: a, end: b }),
            &alloc,
            Tier::Host,
        ))
        .expect("range");
        check_rows(table(&payload), &ledger, first_rows[1] + a);
        rows += payload.rows();
    }
    assert_eq!(rows, split.rows);
    // Two reads of one range are equal, in either order (SO-I6).
    let one = block_on(source.read(
        split,
        Some(RowRange { start: 5, end: 900 }),
        &alloc,
        Tier::Host,
    ))
    .expect("one");
    let two = block_on(source.read(
        split,
        Some(RowRange { start: 5, end: 900 }),
        &alloc,
        Tier::Host,
    ))
    .expect("two");
    assert_eq!(table(&one), table(&two));
    let empty = block_on(source.read(
        split,
        Some(RowRange { start: 3, end: 3 }),
        &alloc,
        Tier::Host,
    ))
    .expect("an empty range");
    assert_eq!(empty.rows(), 0);
    let outside = block_on(source.read(
        split,
        Some(RowRange {
            start: 0,
            end: split.rows + 1,
        }),
        &alloc,
        Tier::Host,
    ));
    assert!(matches!(outside, Err(MorunaError::Source { .. })));
    // The wrong host tier is the arena's refusal, passed through.
    let refused = block_on(source.read(split, None, &alloc, Tier::PinnedHost)).unwrap_err();
    assert!(
        matches!(
            refused,
            MorunaError::Alloc {
                tier: Tier::PinnedHost,
                ..
            }
        ),
        "{refused}"
    );
    let pinned = FakeAllocator::new().pinned(true).with_limit(Tier::Host, 0);
    let payload = block_on(source.read(split, None, &pinned, Tier::PinnedHost)).expect("pinned");
    assert_eq!(payload.tier(), Tier::PinnedHost);
    check_rows(table(&payload), &ledger, first_rows[1]);
    // An unplanned split is refused (SO-I1).
    let mut stranger = split.clone();
    stranger.id = 999;
    assert!(block_on(source.read(&stranger, None, &alloc, Tier::Host)).is_err());
}

// SO-T26 vortex_canonical_lands_in_place. A column in a canonical encoding is decoded as a view
// over the arena buffer the reactor landed its segment in: no decode copy, no payload copy
// announced, and its values pointer lies inside the arena. A compressed column is decoded and
// copied once. Every buffer of every column is arena memory. e.8, SO-I5, SO-I3.
#[test]
fn so_t26_vortex_canonical_lands_in_place() {
    let dir = scratch();
    let ledger = write_vortex(&dir, "t20", 30_000, 0);
    let canonical = source_over(
        &ledger,
        seed(FakeReactor::new(), &ledger.path),
        Some(&["r"]),
        u64::MAX,
    );
    let split = canonical.plan().expect("plan").remove(0);
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let payload = block_on(canonical.read(&split, None, &alloc, Tier::Host)).expect("read");
    let batch = table(&payload);
    check_rows(batch, &ledger, 0);
    let values = batch.column(0).to_data().buffers()[0].clone();
    assert!(
        alloc.contains(values.as_ptr()),
        "the values are in the arena"
    );
    assert_eq!(
        arena_tier_of(&values),
        Some(Tier::Host),
        "the reactor's own buffer"
    );
    let stats = canonical.stats();
    assert_eq!(stats.decode_bytes, 0, "nothing was copied");
    assert!(stats.zero_copy_bytes >= ledger.rows * 8, "{stats:?}");
    assert_eq!(alloc.payload_copies(), 0, "no payload copy was announced");

    // A compressed column is decoded, then copied into the arena once.
    let compressed = source_over(
        &ledger,
        seed(FakeReactor::new(), &ledger.path),
        Some(&["id", "s"]),
        u64::MAX,
    );
    let split = compressed.plan().expect("plan").remove(0);
    let payload = block_on(compressed.read(&split, None, &alloc, Tier::Host)).expect("read");
    let batch = table(&payload);
    check_rows(batch, &ledger, 0);
    for column in batch.columns() {
        let data = column.to_data();
        for buffer in data.buffers() {
            assert!(
                alloc.contains(buffer.as_ptr()),
                "every buffer is arena memory"
            );
        }
    }
    let stats = compressed.stats();
    assert!(stats.decode_bytes >= ledger.rows * 8, "{stats:?}");
    assert_eq!(alloc.payload_copies(), 1, "one decode copy per read");
    assert_eq!(alloc.payload_copy_bytes(), stats.decode_bytes);
}

// SO-T27 vortex_truncated_file. A file that comes back shorter than its footer says is a
// `Source` error naming the split, and the buffers of the failed read go back to the arena; a
// failed reactor read likewise, and the retry reads; a file truncated on disk does not plan,
// with a `Source` error naming it. h, SO-I6.
#[test]
fn so_t27_vortex_truncated_file() {
    let dir = scratch();
    let ledger = write_vortex(&dir, "t21", 30_000, 3);
    let len = std::fs::metadata(&ledger.path).expect("size").len() as usize;
    let short = source_over(
        &ledger,
        seed_truncated(FakeReactor::new(), &ledger.path, len / 2),
        None,
        u64::MAX,
    );
    let split = short.plan().expect("plan").remove(0);
    let alloc = FakeAllocator::new();
    let baseline = alloc.in_use(Tier::Host);
    let error = block_on(short.read(&split, None, &alloc, Tier::Host)).unwrap_err();
    assert!(
        matches!(error, MorunaError::Source { split: id, .. } if id == split.id),
        "{error}"
    );
    assert!(error.to_string().contains("short"), "{error}");
    assert_eq!(
        alloc.in_use(Tier::Host),
        baseline,
        "the failed read's buffers are released"
    );

    let reactor = seed(FakeReactor::new(), &ledger.path).fail_next(OpKind::ReadFile, 1);
    let observer = reactor.clone();
    let failing = source_over(&ledger, reactor, None, u64::MAX);
    let split = failing.plan().expect("plan").remove(0);
    let error = block_on(failing.read(&split, None, &alloc, Tier::Host)).unwrap_err();
    assert!(error.to_string().contains("reading bytes"), "{error}");
    let good = block_on(failing.read(&split, None, &alloc, Tier::Host)).expect("the retry");
    check_rows(table(&good), &ledger, 0);
    // An allocation failure comes back before the reactor is asked for anything.
    let before = observer.ops().len();
    let refusing = FakeAllocator::new().fail_next(1);
    let error = block_on(failing.read(&split, None, &refusing, Tier::Host)).unwrap_err();
    assert!(matches!(error, MorunaError::Alloc { .. }), "{error}");
    assert_eq!(observer.ops().len(), before);

    // Truncated on disk: the footer is gone.
    let bytes = std::fs::read(&ledger.path).expect("bytes");
    let cut = dir.join("cut.vortex");
    std::fs::write(&cut, &bytes[..bytes.len() / 3]).expect("cut");
    let error = VortexSource::new(
        VortexSourceConfig {
            urls: vec![cut.display().to_string()],
            ..VortexSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .err()
    .expect("a truncated file does not plan");
    assert!(matches!(error, MorunaError::Source { .. }), "{error}");
    assert!(error.to_string().contains("cut.vortex"), "{error}");
}

// SO-T28 vortex_missing_column. A projection naming a column the file lacks is a `Plan` error
// naming the column and the file; so is an empty configuration, a directory with no Vortex
// file, a file that is not a table, and two files that disagree on a projected type. h.
#[test]
fn so_t28_vortex_missing_column() {
    let dir = scratch();
    let ledger = write_vortex(&dir, "t22", 1_000, 0);
    let build = |urls: Vec<String>, columns: Option<Vec<String>>| {
        VortexSource::new(
            VortexSourceConfig {
                urls,
                columns,
                split_bytes: None,
            },
            Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
            Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
        )
    };
    let url = ledger.path.display().to_string();
    let error = err(build(vec![url.clone()], Some(vec!["nope".into()])));
    assert!(matches!(error, MorunaError::Plan(_)), "{error}");
    assert!(
        error.to_string().contains("nope") && error.to_string().contains("t22"),
        "{error}"
    );
    assert!(matches!(err(build(Vec::new(), None)), MorunaError::Plan(_)));
    let empty = scratch();
    std::fs::write(empty.join("notes.txt"), b"not vortex").expect("notes");
    assert!(matches!(
        err(build(vec![empty.display().to_string()], None)),
        MorunaError::Plan(_)
    ));
    assert!(matches!(
        err(build(
            vec![dir.join("absent.vortex").display().to_string()],
            None
        )),
        MorunaError::Plan(_)
    ));

    // Two files under one prefix whose `id` types differ.
    let mixed = scratch();
    write_vortex(&mixed, "a", 100, 0);
    let other = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, false)]));
    write_batch(
        &mixed.join("b.vortex"),
        RecordBatch::try_new(
            other,
            vec![Arc::new(StringArray::from_iter_values(["x", "y"])) as ArrayRef],
        )
        .expect("batch"),
    );
    let error = err(build(
        vec![mixed.display().to_string()],
        Some(vec!["id".into()]),
    ));
    assert!(
        error.to_string().contains("a.vortex") && error.to_string().contains("b.vortex"),
        "{error}"
    );

    // A file of one column that is not a struct is not a table.
    let scalar = dir.join("scalar.vortex");
    let runtime = CurrentThreadRuntime::new();
    let session = VortexSession::default().with_handle(runtime.handle());
    let array = session
        .arrow()
        .from_arrow_array(Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef, false)
        .expect("array");
    let mut bytes: Vec<u8> = Vec::new();
    runtime
        .block_on(
            session
                .write_options()
                .write(&mut bytes, array.to_array_stream()),
        )
        .expect("write");
    std::fs::write(&scalar, bytes).expect("scalar");
    let error = err(build(vec![scalar.display().to_string()], None));
    assert!(error.to_string().contains("not a table"), "{error}");
}

// SO-T29 vortex_local_and_object. Local paths and `file://` URLs read through `read_file`
// only and never touch `ObjectMetadata`; an object prefix is listed and read through
// `read_object` into the arena, and without an allocator says what it needs; an empty file is
// one split of zero rows. d.1, e.7, SO-I7.
#[test]
fn so_t29_vortex_local_and_object() {
    let dir = scratch();
    let ledger = write_vortex(&dir, "t23", 10_000, 4);
    let reactor = seed(FakeReactor::new(), &ledger.path);
    let observer = reactor.clone();
    let local = VortexSource::new(
        VortexSourceConfig {
            urls: vec![format!("file://{}", ledger.path.display())],
            ..VortexSourceConfig::default()
        },
        Arc::new(reactor) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("a file:// url");
    let alloc = FakeAllocator::new();
    for split in local.plan().expect("plan") {
        block_on(local.read(&split, None, &alloc, Tier::Host)).expect("read");
    }
    assert!(
        observer.ops().iter().all(|op| op.kind == OpKind::ReadFile),
        "a local file is read with read_file only"
    );

    let bytes = std::fs::read(&ledger.path).expect("bytes");
    let store = FakeReactor::new()
        .with_file("s3://bucket/vx/a.vortex", bytes.clone())
        .with_file("s3://bucket/vx/b.vortex", bytes.clone())
        .with_file("s3://bucket/vx/notes.txt", b"not vortex".to_vec());
    let shared: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let objects = VortexSource::with_allocator(
        VortexSourceConfig {
            urls: vec!["s3://bucket/vx/".into()],
            columns: Some(vec!["id".into(), "s".into()]),
            split_bytes: Some(1 << 30),
        },
        Arc::new(store.clone()) as Arc<dyn Reactor>,
        Arc::new(store.clone()) as Arc<dyn ObjectMetadata>,
        shared.clone(),
    )
    .expect("a source over objects");
    let plan = objects.plan().expect("plan");
    assert_eq!(plan.len(), 2, "one split per object at this size");
    for split in &plan {
        let payload =
            block_on(objects.read(split, None, shared.as_ref(), Tier::Host)).expect("read");
        check_rows(table(&payload), &ledger, 0);
    }
    let kinds: Vec<OpKind> = store.ops().iter().map(|op| op.kind).collect();
    assert!(kinds.contains(&OpKind::ListPrefix));
    assert!(kinds.contains(&OpKind::ReadObject));
    assert!(!kinds.contains(&OpKind::ReadFile), "{kinds:?}");
    let without = VortexSource::new(
        VortexSourceConfig {
            urls: vec!["s3://bucket/vx/a.vortex".into()],
            ..VortexSourceConfig::default()
        },
        Arc::new(store.clone()) as Arc<dyn Reactor>,
        Arc::new(store) as Arc<dyn ObjectMetadata>,
    );
    assert!(err(without).to_string().contains("with_allocator"));

    // An empty file.
    let zero = write_vortex(&dir, "t23-empty", 0, 0);
    let source = source_over(&zero, seed(FakeReactor::new(), &zero.path), None, 1 << 20);
    let plan = source.plan().expect("plan");
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].rows, 0);
    let payload = block_on(source.read(&plan[0], None, &alloc, Tier::Host)).expect("read");
    assert_eq!(payload.rows(), 0);
    assert_eq!(table(&payload).num_columns(), 5);
}
