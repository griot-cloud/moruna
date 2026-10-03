//! peQL on both ends (MH 4.5): `PlanSource::peql` reads what peQL plans for a caller, shapes
//! and charges included; `PeqlSink` writes under a contract, by its owner only, and refreshes
//! the manifest on `finish`.

#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_datafusion::engine::{Caller, Engine, WriteMode};
use moruna_datafusion::{BudgetPool, PeqlRead, PeqlSink, PlanMemory, PlanSource, WriteMemory};

/// Room for what these plans hold, and nowhere to spill.
fn memory() -> PlanMemory {
    PlanMemory {
        pool: Arc::new(BudgetPool::new(256 << 20)),
        spill_dir: None,
    }
}
use moruna_kernel::arrow::array::{AsArray, Int64Array, RecordBatch, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int64Type, Schema};
use moruna_kernel::{Allocator, MorunaError, Payload, Sink, Source, SourceSchema, Tier};
use moruna_testkit::FakeAllocator;

/// Guests see eastern readings only, with meters hashed; fewer than five rows are withheld;
/// negative readings breach the contract, which makes the data unservable.
const READINGS: &str = r#"
contract: demo/readings
version: 1
owner: demo
binding: {parquet: readings/}
expose:
  - {name: id, type: int64}
  - {name: region, type: utf8}
  - {name: meter, type: utf8}
  - {name: kwh, type: int64}
rules:
  - {id: analytics, op: decide, expr: "ctx.purpose == 'analytics'"}
  - {id: east_for_guests, op: admit, expr: "ctx.tenant == 'demo' || row.region == 'EA'"}
  - {id: non_negative, op: assert, expr: "row.kwh >= 0", on_fail: deny}
  - {id: mask_meter, op: transform, column: meter, expr: "ctx.tenant == 'demo' ? row.meter : hash_sha256(row.meter)"}
  - {id: small, op: shape, operator: suppress, params: {k: 5}, unless: "ctx.tenant == 'demo'"}
  - id: dp
    op: shape
    operator: noise
    column: kwh
    params: {sensitivity: 10, epsilon: 1.0, budget: kwh, at: row}
    unless: "ctx.tenant == 'demo'"
"#;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("moruna-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch directory");
        Scratch(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("region", DataType::Utf8, false),
        Field::new("meter", DataType::Utf8, true),
        Field::new("kwh", DataType::Int64, true),
    ]))
}

/// Readings `from..to`: even ids in the east.
fn batch(from: i64, to: i64, kwh: impl Fn(i64) -> i64) -> RecordBatch {
    let ids: Vec<i64> = (from..to).collect();
    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(Int64Array::from(ids.clone())),
            Arc::new(StringArray::from(
                ids.iter()
                    .map(|i| if i % 2 == 0 { "EA" } else { "WA" })
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|i| format!("M{i:05}")).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                ids.iter().map(|i| kwh(*i)).collect::<Vec<_>>(),
            )),
        ],
    )
    .expect("a batch")
}

fn owner() -> Caller {
    Caller::new("ana", "demo", "analytics")
}
fn guest() -> Caller {
    Caller::new("gus", "partner", "analytics")
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

fn engine(root: &Path, rows: i64) -> Arc<Engine> {
    let engine = Engine::open(root).expect("an engine");
    engine
        .register_contract(READINGS, &schema())
        .expect("the contract compiles");
    engine
        .publish("demo/readings", moruna_datafusion::engine::store::PUBLIC)
        .expect("published");
    engine.budgets().set_limit("kwh", 2.0).expect("a limit");
    rt().block_on(engine.write(
        "demo/readings",
        vec![batch(0, rows, |i| i)],
        WriteMode::Overwrite,
    ))
    .expect("written");
    Arc::new(engine)
}

/// Every split of a source, read whole, in order.
fn drain(src: &PlanSource) -> Vec<RecordBatch> {
    let alloc = FakeAllocator::new();
    let rt = rt();
    src.plan()
        .expect("the plan")
        .iter()
        .map(
            |split| match rt.block_on(src.read(split, None, &alloc, Tier::Host)) {
                Ok(Payload::Table(b, _)) => b,
                Ok(Payload::Tensor(..)) => panic!("a table"),
                Err(e) => panic!("{e}"),
            },
        )
        .collect()
}

fn ids(batches: &[RecordBatch]) -> Vec<i64> {
    let mut out: Vec<i64> = batches
        .iter()
        .flat_map(|b| {
            let i = b.schema().index_of("id").expect("an id column");
            b.column(i).as_primitive::<Int64Type>().values().to_vec()
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_governed_view_is_what_peql_answers_and_is_charged_once() {
    let scratch = Scratch::new("peql-view");
    let engine = engine(scratch.path(), 400);
    let read = PeqlRead::Contract("demo/readings".into());

    // The guest's view: eastern rows only, meters hashed, kwh noised and paid for.
    let src = PlanSource::peql(&engine, &read, &guest(), &memory()).expect("a governed source");
    assert!(
        !src.repeatable(),
        "noise and suppression make one merged partition"
    );
    let got = drain(&src);
    let answer = rt()
        .block_on(engine.query(r#"SELECT * FROM "demo/readings""#, &guest()))
        .expect("the query");
    assert_eq!(ids(&got), ids(&answer.batches));
    assert_eq!(
        ids(&got),
        (0..400).filter(|i| i % 2 == 0).collect::<Vec<_>>()
    );
    let meters: Vec<String> = got
        .iter()
        .flat_map(|b| {
            b.column(2)
                .as_string::<i32>()
                .iter()
                .map(|m| m.unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(meters.iter().all(|m| m.len() == 64), "meters are hashed");
    // The view spent 1.0 of 2.0 when it was planned and the query the rest: nothing is left.
    assert_eq!(answer.envelope.budgets["kwh"], 0.0);
    let spent = PlanSource::peql(&engine, &read, &guest(), &memory())
        .err()
        .expect("refused");
    assert!(
        matches!(spent, MorunaError::Plan(ref m) if m.contains("budget `kwh` is exhausted")),
        "{spent}"
    );

    // The owner is exempt from every shape: all rows, several partitions, repeatable.
    let src = PlanSource::peql(&engine, &read, &owner(), &memory()).expect("the owner's source");
    let SourceSchema::Table(schema) = src.schema() else {
        panic!("a table schema");
    };
    assert_eq!(schema.fields().len(), 4);
    assert_eq!(ids(&drain(&src)), (0..400).collect::<Vec<_>>());

    // SQL over contracts, planned as `query` plans it: an aggregate the guest may not see.
    let sql = PeqlRead::Sql(
        r#"SELECT region, COUNT(*) AS n FROM "demo/readings" GROUP BY region"#.into(),
    );
    assert_eq!(sql.contracts().expect("parsed"), ["demo/readings"]);
    let src = PlanSource::peql(&engine, &sql, &owner(), &memory()).expect("an aggregate");
    let n: i64 = drain(&src)
        .iter()
        .map(|b| {
            b.column(1)
                .as_primitive::<Int64Type>()
                .values()
                .iter()
                .sum::<i64>()
        })
        .sum();
    assert_eq!(n, 400);

    // Refusals are peQL's own words.
    let marketer = Caller::new("m", "partner", "marketing");
    let denied = PlanSource::peql(&engine, &read, &marketer, &memory())
        .err()
        .expect("denied");
    assert!(denied.to_string().contains("analytics"), "{denied}");
    let unknown = PlanSource::peql(
        &engine,
        &PeqlRead::Contract("demo/none".into()),
        &owner(),
        &memory(),
    )
    .err()
    .expect("unknown");
    assert!(
        unknown.to_string().contains("no contract named"),
        "{unknown}"
    );
    assert!(PeqlRead::Sql("SELEC nothing".into()).contracts().is_err());
}

fn payload(b: RecordBatch, alloc: &FakeAllocator) -> Payload {
    let (b, _) = moruna_sources::copy_batch(&b, alloc, Tier::Host).expect("into the arena");
    Payload::table(b).expect("a payload")
}

#[test]
fn a_contract_write_in_morsels_refreshes_the_manifest_on_finish() {
    let scratch = Scratch::new("peql-sink");
    let engine = engine(scratch.path(), 10);
    let alloc = FakeAllocator::new();
    let rt = rt();

    // The run's share for the sink, two parts in flight: each part is written inside half.
    let memory = Arc::new(WriteMemory::new(2));
    memory.set_limit(32 << 20);
    assert_eq!(memory.limit(), 32 << 20);
    assert_eq!(memory.per_part(), 16 << 20);
    let mut sink = PeqlSink::new(
        Arc::clone(&engine),
        "demo/readings",
        &owner(),
        WriteMode::Append,
        Arc::clone(&memory),
    )
    .expect("the owner writes");
    assert_eq!(sink.accepts().kind, moruna_kernel::PayloadKind::Table);
    let schema = SourceSchema::Table(schema());
    sink.open(&schema).expect("opened");
    for part in 0..4 {
        let from = 10 + part * 100;
        rt.block_on(sink.write(part as u64, payload(batch(from, from + 100, |i| i), &alloc)))
            .expect("a morsel");
    }
    // Until finish, the manifest is the one written before.
    assert_eq!(
        engine.manifest("demo/readings").unwrap().unwrap().row_count,
        10
    );
    let summary = sink.finish().expect("finished");
    assert_eq!(summary.rows, 400);
    // A Parquet binding keeps no snapshots, so the write commits none.
    assert_eq!(summary.snapshot, None);
    assert!(summary.bytes > 0);
    let manifest = engine.manifest("demo/readings").unwrap().unwrap();
    assert!(manifest.valid);
    assert_eq!(manifest.row_count, 410);
    assert_eq!(summary.files.len(), manifest.files.len());

    // Overwrite replaces; data that breaches the contract lands, and the run is told. A share of
    // nothing leaves the write unbounded.
    let mut sink = PeqlSink::new(
        Arc::clone(&engine),
        "demo/readings",
        &owner(),
        WriteMode::Overwrite,
        Arc::new(WriteMemory::new(2)),
    )
    .expect("the owner writes");
    sink.open(&schema).expect("opened");
    rt.block_on(sink.write(0, payload(batch(0, 20, |i| i - 5), &alloc)))
        .expect("a morsel");
    let breached = sink.finish().expect_err("unservable");
    assert!(breached.to_string().contains("non_negative"), "{breached}");
    assert!(!engine.manifest("demo/readings").unwrap().unwrap().valid);
}

#[test]
fn only_the_owner_writes_and_the_sink_keeps_its_order() {
    let scratch = Scratch::new("peql-owner");
    let engine = engine(scratch.path(), 10);
    let refused = PeqlSink::new(
        Arc::clone(&engine),
        "demo/readings",
        &guest(),
        WriteMode::Append,
        Arc::new(WriteMemory::new(1)),
    )
    .err()
    .expect("a guest may not write");
    assert!(
        matches!(refused, MorunaError::Sink(ref m) if m.contains("writes are the owner's")),
        "{refused}"
    );

    let mut sink = PeqlSink::new(
        Arc::clone(&engine),
        "demo/readings",
        &owner(),
        WriteMode::Append,
        Arc::new(WriteMemory::new(1)),
    )
    .expect("the owner writes");
    let rt = rt();
    let alloc = FakeAllocator::new();
    let early = rt
        .block_on(sink.write(0, payload(batch(0, 1, |i| i), &alloc)))
        .expect_err("write before open");
    assert!(early.to_string().contains("before open"), "{early}");
    assert!(sink.finish().is_err(), "finish before open");
    sink.open(&SourceSchema::Table(schema())).expect("opened");
    let tensor = moruna_kernel::Payload::Tensor(
        moruna_kernel::ManagedTensor::from_buffer(
            alloc.alloc(8, Tier::Host).expect("a buffer"),
            0,
            moruna_kernel::DType::F64,
            vec![1],
        )
        .expect("a tensor"),
        Tier::Host,
    );
    let wrong = rt.block_on(sink.write(1, tensor)).expect_err("a tensor");
    assert!(wrong.to_string().contains("tensor"), "{wrong}");
    // A bad batch is peQL's refusal, named.
    let bad = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new(
            "nothing",
            DataType::Int64,
            false,
        )])),
        vec![Arc::new(Int64Array::from(vec![1]))],
    )
    .expect("a batch");
    let err = rt
        .block_on(sink.write(2, payload(bad, &alloc)))
        .expect_err("refused");
    assert!(err.to_string().starts_with("sink: peQL"), "{err}");
}
