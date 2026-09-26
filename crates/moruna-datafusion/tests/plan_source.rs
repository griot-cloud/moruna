//! `PlanSource` over plain DataFusion plans (MH 4.5): one split per output partition, counted
//! rows, ranges streamed into the arena, the `repeatable()` rule.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use datafusion::datasource::MemTable;
use datafusion::prelude::{SessionConfig, SessionContext};
use moruna_datafusion::{BudgetPool, PlanMemory, PlanSource};
use moruna_kernel::arrow::array::{
    Array, AsArray, DictionaryArray, Int64Array, RecordBatch, StringArray,
};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int32Type, Int64Type, Schema};
use moruna_kernel::{Payload, RowRange, Source, SourceSchema, Split, Tier};
use moruna_testkit::FakeAllocator;

const PARTS: i64 = 3;
const PER_PART: i64 = 10_000;

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("s", DataType::Utf8, false),
    ]))
}

fn part(p: i64) -> RecordBatch {
    let xs: Vec<i64> = (p * PER_PART..(p + 1) * PER_PART).collect();
    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(Int64Array::from(xs.clone())),
            Arc::new(StringArray::from(
                xs.iter().map(|x| format!("row {x}")).collect::<Vec<_>>(),
            )),
        ],
    )
    .expect("a batch")
}

/// `t`: three partitions of ten thousand rows, each in batches of a thousand, so a range cuts
/// batches; a session of three target partitions, so a scan plans no exchange.
fn ctx(target_partitions: usize) -> SessionContext {
    let ctx = SessionContext::new_with_config(
        SessionConfig::new().with_target_partitions(target_partitions),
    );
    let partitions = (0..PARTS)
        .map(|p| {
            let b = part(p);
            (0..10).map(|i| b.slice(i * 1000, 1000)).collect::<Vec<_>>()
        })
        .collect();
    let t = MemTable::try_new(schema(), partitions).expect("a table");
    ctx.register_table("t", Arc::new(t)).expect("registered");
    ctx
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime")
}

/// Room for anything these small plans hold, and nowhere to spill.
fn memory() -> PlanMemory {
    PlanMemory {
        pool: Arc::new(BudgetPool::new(256 << 20)),
        spill_dir: None,
    }
}

fn source(ctx: &SessionContext, sql: &str) -> moruna_kernel::Result<PlanSource> {
    source_in(ctx, sql, &memory())
}

fn source_in(
    ctx: &SessionContext,
    sql: &str,
    memory: &PlanMemory,
) -> moruna_kernel::Result<PlanSource> {
    let plan = runtime()
        .block_on(async { ctx.sql(sql).await?.into_optimized_plan() })
        .expect("a plan");
    PlanSource::from_logical(plan, ctx, memory)
}

fn read(
    src: &PlanSource,
    split: &Split,
    rows: Option<RowRange>,
) -> moruna_kernel::Result<RecordBatch> {
    let alloc = FakeAllocator::new();
    let payload = runtime().block_on(src.read(split, rows, &alloc, Tier::Host))?;
    assert!(alloc.payload_copies() > 0 || payload.rows() == 0);
    match payload {
        Payload::Table(batch, tier) => {
            assert_eq!(tier, Tier::Host);
            Ok(batch)
        }
        Payload::Tensor(..) => panic!("a plan yields tables"),
    }
}

fn xs(batch: &RecordBatch) -> Vec<i64> {
    batch
        .column(0)
        .as_primitive::<Int64Type>()
        .values()
        .to_vec()
}

#[test]
fn one_split_per_partition_counted_and_repeatable() {
    let ctx = ctx(3);
    let src = source(&ctx, "SELECT x, s FROM t").expect("a source");
    assert!(src.repeatable(), "a scan of partitions is repeatable");
    let splits = src.plan().expect("the plan");
    assert_eq!(splits.len(), PARTS as usize);
    for (i, split) in splits.iter().enumerate() {
        assert_eq!(split.id as usize, i);
        assert_eq!(split.rows, PER_PART as u64);
        assert!(split.sub_splittable);
        assert!(!split.estimated);
        assert_eq!(split.column_bytes.len(), 2);
        assert_eq!(
            split.uncompressed_bytes,
            split.column_bytes.iter().sum::<u64>()
        );
        assert_eq!(split.null_counts, vec![Some(0), Some(0)]);
    }
    let SourceSchema::Table(schema) = src.schema() else {
        panic!("a table schema");
    };
    assert_eq!(schema.fields().len(), 2);

    // Ranges in order, cutting batches: every row once, in order.
    let split = &splits[1];
    let mut seen = Vec::new();
    for (start, end) in [
        (0, 1500),
        (1500, 1501),
        (1501, 7777),
        (7777, PER_PART as u64),
    ] {
        let batch = read(&src, split, Some(RowRange { start, end })).expect("a range");
        assert_eq!(batch.num_rows() as u64, end - start);
        let strings = batch.column(1).as_string::<i32>();
        assert_eq!(strings.value(0), format!("row {}", PER_PART + start as i64));
        seen.extend(xs(&batch));
    }
    assert_eq!(seen, (PER_PART..2 * PER_PART).collect::<Vec<_>>());

    // A whole split, and a range behind the stream (a re-read) after a later one.
    let whole = read(&src, &splits[2], None).expect("the whole split");
    assert_eq!(xs(&whole), (2 * PER_PART..3 * PER_PART).collect::<Vec<_>>());
    let later = read(
        &src,
        &splits[0],
        Some(RowRange {
            start: 5000,
            end: 6000,
        }),
    )
    .unwrap();
    let earlier = read(
        &src,
        &splits[0],
        Some(RowRange {
            start: 100,
            end: 200,
        }),
    )
    .unwrap();
    assert_eq!(xs(&later), (5000..6000).collect::<Vec<_>>());
    assert_eq!(xs(&earlier), (100..200).collect::<Vec<_>>());
    // And the live stream is still where it was.
    let next = read(
        &src,
        &splits[0],
        Some(RowRange {
            start: 6000,
            end: PER_PART as u64,
        }),
    )
    .unwrap();
    assert_eq!(xs(&next), (6000..PER_PART).collect::<Vec<_>>());
    // A split read to its end can be read again.
    let again = read(&src, &splits[0], Some(RowRange { start: 0, end: 10 })).unwrap();
    assert_eq!(xs(&again), (0..10).collect::<Vec<_>>());
}

#[test]
fn a_range_is_copied_compact_into_the_arena() {
    let ctx = ctx(3);
    let src = source(&ctx, "SELECT x, s FROM t").expect("a source");
    let split = &src.plan().expect("the plan")[0];
    let alloc = FakeAllocator::new();
    let payload = runtime()
        .block_on(src.read(
            split,
            Some(RowRange { start: 10, end: 20 }),
            &alloc,
            Tier::Host,
        ))
        .expect("a range");
    // Ten rows of two small columns: the copy is the range, not the thousand-row batch.
    assert_eq!(payload.rows(), 10);
    let copied = alloc.payload_copies();
    assert!(copied > 0 && copied < 1024, "{copied} bytes copied");
}

#[test]
fn an_exchange_or_a_volatile_function_is_not_repeatable() {
    // An aggregate hashes its groups across partitions: an exchange.
    let ctx = ctx(8);
    let src = source(
        &ctx,
        "SELECT x % 7 AS k, COUNT(*) AS n FROM t GROUP BY x % 7",
    )
    .unwrap();
    assert!(!src.repeatable());
    let splits = src.plan().expect("the plan");
    assert_eq!(
        splits.len(),
        1,
        "an unrepeatable plan is one merged partition"
    );
    assert_eq!(splits[0].rows, 7);
    let batch = read(&src, &splits[0], None).expect("the split");
    let n: i64 = batch
        .column(1)
        .as_primitive::<Int64Type>()
        .values()
        .iter()
        .sum();
    assert_eq!(n, PARTS * PER_PART);

    let src = source(&ctx, "SELECT x, random() AS r FROM t").expect("a source");
    assert!(!src.repeatable(), "random() is volatile");
    assert_eq!(src.plan().expect("the plan").len(), 1);
}

#[test]
fn a_physical_plan_and_an_empty_one() {
    let ctx = ctx(3);
    let rt = runtime();
    let physical = rt
        .block_on(async {
            ctx.sql("SELECT x FROM t WHERE x < 0")
                .await?
                .create_physical_plan()
                .await
        })
        .expect("a physical plan");
    let src = PlanSource::new(physical, &ctx, &memory()).expect("a source");
    let splits = src.plan().expect("the plan");
    assert!(splits.iter().all(|s| s.rows == 0));
    let empty = read(&src, &splits[0], None).expect("an empty read");
    assert_eq!(empty.num_rows(), 0);
    assert_eq!(empty.schema().field(0).name(), "x");
}

#[test]
fn failures_name_the_split() {
    let ctx = ctx(3);
    // Division by zero fails while the rows are counted.
    let err = source(&ctx, "SELECT x / (x - x) AS boom FROM t")
        .err()
        .expect("refused");
    assert!(err.to_string().contains("source split"), "{err}");

    // A split the plan does not hold.
    let src = source(&ctx, "SELECT x FROM t").expect("a source");
    let mut ghost = src.plan().expect("the plan")[0].clone();
    ghost.id = 99;
    let err = read(&src, &ghost, None).expect_err("refused");
    assert!(err.to_string().contains("no such split"), "{err}");

    // A plan whose row count changes between runs is caught, not silently truncated.
    let mut caught = false;
    for _ in 0..5 {
        let src = source(&ctx, "SELECT x FROM t WHERE random() < 0.5").expect("a source");
        let split = src.plan().expect("the plan")[0].clone();
        if let Err(err) = read(&src, &split, None) {
            let text = err.to_string();
            assert!(
                text.contains("before row") || text.contains("beyond"),
                "{text}"
            );
            caught = true;
            break;
        }
    }
    assert!(
        caught,
        "five runs of a random filter all counted the same rows"
    );
}

#[test]
fn dictionaries_are_read_as_their_values() {
    let dict: DictionaryArray<Int32Type> = vec!["a", "b", "a", "c"].into_iter().collect();
    let schema = Arc::new(Schema::new(vec![Field::new(
        "d",
        DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
        false,
    )]));
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(dict)]).expect("a batch");
    let ctx = SessionContext::new();
    ctx.register_table(
        "d",
        Arc::new(MemTable::try_new(schema, vec![vec![batch]]).expect("a table")),
    )
    .expect("registered");
    let src = source(&ctx, "SELECT d FROM d").expect("a source");
    let SourceSchema::Table(schema) = src.schema() else {
        panic!("a table schema");
    };
    assert_eq!(schema.field(0).data_type(), &DataType::Utf8);
    let split = src.plan().expect("the plan")[0].clone();
    let got = read(&src, &split, Some(RowRange { start: 1, end: 3 })).expect("a range");
    let values = got.column(0).as_string::<i32>();
    assert_eq!((values.value(0), values.value(1)), ("b", "a"));
    assert_eq!(got.column(0).len(), 2);
}

#[test]
fn a_large_partition_is_several_splits() {
    // One partition of 40 000 rows of a kilobyte: about 40 MiB, so splits of at most
    // `SPLIT_BYTES`, read in order and out of it.
    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("s", DataType::Utf8, false),
    ]));
    let batches = (0..5)
        .map(|b| {
            let xs: Vec<i64> = (b * 8000..(b + 1) * 8000).collect();
            RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(Int64Array::from(xs.clone())),
                    Arc::new(StringArray::from(
                        xs.iter().map(|x| format!("{x:01000}")).collect::<Vec<_>>(),
                    )),
                ],
            )
            .expect("a batch")
        })
        .collect();
    let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
    ctx.register_table(
        "big",
        Arc::new(MemTable::try_new(schema, vec![batches]).expect("a table")),
    )
    .expect("registered");
    let src = source(&ctx, "SELECT x, s FROM big").expect("a source");
    let splits = src.plan().expect("the plan");
    let bytes: u64 = 40_000 * 1012;
    assert_eq!(
        splits.len() as u64,
        bytes.div_ceil(moruna_datafusion::SPLIT_BYTES),
        "{splits:?}"
    );
    assert_eq!(splits.iter().map(|s| s.rows).sum::<u64>(), 40_000);
    for (i, split) in splits.iter().enumerate() {
        assert_eq!(split.id as usize, i);
        assert!(split.estimated);
        assert!(split.uncompressed_bytes <= moruna_datafusion::SPLIT_BYTES + 2048);
        assert_eq!(split.null_counts, vec![None, None]);
    }
    let first = splits[0].rows as i64;
    let second = read(&src, &splits[1], Some(RowRange { start: 10, end: 20 })).unwrap();
    assert_eq!(xs(&second), (first + 10..first + 20).collect::<Vec<_>>());
    let head = read(&src, &splits[0], Some(RowRange { start: 0, end: 5 })).unwrap();
    assert_eq!(xs(&head), (0..5).collect::<Vec<_>>());
    let mut all = Vec::new();
    for split in &splits {
        all.extend(xs(&read(&src, split, None).expect("a split")));
    }
    assert_eq!(all, (0..40_000).collect::<Vec<_>>());
}

#[test]
fn a_file_scan_partition_reads_its_own_files() {
    // Four Parquet files, four partitions. DataFusion's partitions steal files from one another
    // when they run together; a split is counted with its siblings and may be read alone, so
    // each must read the files it was given and no others.
    let dir = std::env::temp_dir().join(format!("moruna-plan-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory");
    let rt = runtime();
    let writer = ctx(3);
    for p in 0..4i64 {
        let batch = part(p % PARTS);
        let df = writer.read_batch(batch).expect("a frame");
        rt.block_on(df.write_parquet(
            dir.join(format!("f{p}.parquet")).to_str().expect("a path"),
            datafusion::dataframe::DataFrameWriteOptions::new().with_single_file_output(true),
            None,
        ))
        .expect("written");
    }
    let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(4));
    rt.block_on(ctx.register_parquet("files", dir.to_str().expect("a path"), Default::default()))
        .expect("registered");
    let src = source(&ctx, "SELECT x FROM files").expect("a source");
    assert!(src.repeatable());
    let splits = src.plan().expect("the plan");
    assert_eq!(splits.len(), 4);
    assert!(
        splits.iter().all(|s| s.rows == PER_PART as u64),
        "{splits:?}"
    );
    // The last split alone, then the first: each is one file's rows.
    for index in [3, 0] {
        let got = read(&src, &splits[index], None).expect("a split");
        assert_eq!(got.num_rows() as i64, PER_PART);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_source_drops_inside_a_runtime() {
    let ctx = ctx(3);
    let src = source(&ctx, "SELECT x FROM t").expect("a source");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .build()
        .expect("a runtime");
    rt.block_on(async move { drop(src) });
}

/// `w`: 160,000 rows of 256 bytes of text, about 40 MiB, in one partition.
fn wide() -> SessionContext {
    let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
    let batches = (0..160)
        .map(|b| {
            let xs: Vec<i64> = (b * 1000..(b + 1) * 1000).collect();
            RecordBatch::try_new(
                schema(),
                vec![
                    Arc::new(Int64Array::from(xs.clone())),
                    Arc::new(StringArray::from(
                        xs.iter().map(|x| format!("{x:0>256}")).collect::<Vec<_>>(),
                    )),
                ],
            )
            .expect("a batch")
        })
        .collect();
    let t = MemTable::try_new(schema(), vec![batches]).expect("a table");
    ctx.register_table("w", Arc::new(t)).expect("registered");
    ctx
}

/// MH 4.5: a plan's operators hold their state in the run's pool, not in an unbounded one. A
/// sort larger than the pool spills to the staging directory and completes, holding no more
/// than the pool at any moment; with nowhere to spill it is refused for the budget rather than
/// allowed past it.
#[test]
fn a_plan_larger_than_its_pool_spills_or_is_refused() {
    const POOL: u64 = 16 << 20;
    let ctx = wide();
    let sql = "SELECT x, s FROM w ORDER BY s DESC";
    let spill = std::env::temp_dir().join(format!("moruna-spill-{}", std::process::id()));
    std::fs::create_dir_all(&spill).expect("a spill directory");
    let pool = Arc::new(BudgetPool::new(POOL));
    let memory = PlanMemory {
        pool: Arc::clone(&pool),
        spill_dir: Some(spill.clone()),
    };
    let src = source_in(&ctx, sql, &memory).expect("a source");
    let splits = src.plan().expect("the plan");
    let total: u64 = splits.iter().map(|s| s.rows).sum();
    assert_eq!(total, 160_000);
    let first = read(&src, &splits[0], Some(RowRange { start: 0, end: 1 })).expect("a read");
    assert_eq!(xs(&first), vec![159_999], "sorted descending by text");
    assert!(pool.peak() > 0, "the sort reserved from the run's pool");
    assert!(
        pool.peak() <= POOL,
        "the pool held {} of {POOL}",
        pool.peak()
    );

    let nowhere = PlanMemory {
        pool: Arc::new(BudgetPool::new(POOL)),
        spill_dir: None,
    };
    let refused = source_in(&ctx, sql, &nowhere)
        .err()
        .expect("a sort that must spill with nowhere to spill");
    assert!(
        refused.to_string().contains("budget for its plan")
            || refused.to_string().contains("isabled"),
        "{refused}"
    );
    let _ = std::fs::remove_dir_all(&spill);
}
