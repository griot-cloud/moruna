//! peQL on both ends (MH 4.5): `PlanSource::peql` reads what peQL plans for a caller, shapes
//! and charges included, leaving out rows that break a row rule (or refusing them) and saying
//! so in its read report; `PeqlSink` writes under a contract, by its owner only, refreshes the
//! manifest on `finish`, deals with rows that break a row rule as the write asks (refused,
//! excluded and kept apart, or committed), and reports what peQL said about the rows it wrote.

#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use moruna_datafusion::engine::violation::VIOLATIONS_COLUMN;
use moruna_datafusion::engine::{Caller, Engine, OnReadViolation, OnWriteViolation, WriteMode};
use moruna_datafusion::{
    BudgetPool, OnViolation, PeqlRead, PeqlSink, PlanMemory, PlanSource, WriteMemory,
};

/// Room for what these plans hold, and nowhere to spill.
fn memory() -> PlanMemory {
    PlanMemory {
        pool: Arc::new(BudgetPool::new(256 << 20)),
        spill_dir: None,
    }
}
use moruna_kernel::arrow::array::{Array, AsArray, Int64Array, RecordBatch, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int64Type, Schema};
use moruna_kernel::{Allocator, MorunaError, Payload, Sink, Source, SourceSchema, Tier};
use moruna_testkit::FakeAllocator;

/// Guests see eastern readings only, with meters hashed; fewer than five rows are withheld;
/// a negative reading breaks a row rule, so a read leaves it out unless asked to refuse.
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

const EXCLUDE: OnReadViolation = OnReadViolation::Exclude;

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
        OnWriteViolation::Refuse,
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
    let src =
        PlanSource::peql(&engine, &read, &guest(), EXCLUDE, &memory()).expect("a governed source");
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
    // What peQL decided for the contract: its rules, and no row left out for breaking one.
    let said = src.read_report().expect("peQL's report of the read");
    assert_eq!(said.engine, "peql");
    assert_eq!(said.report["contracts"][0]["contract"], "demo/readings");
    assert_eq!(
        said.report["contracts"][0]["excluded"],
        serde_json::json!({})
    );
    assert_eq!(
        said.report["contracts"][0]["decisions"],
        serde_json::json!(["analytics"])
    );
    // The view spent 1.0 of 2.0 when it was planned and the query the rest: nothing is left.
    assert_eq!(answer.envelope.budgets["kwh"], 0.0);
    let spent = PlanSource::peql(&engine, &read, &guest(), EXCLUDE, &memory())
        .err()
        .expect("refused");
    assert!(
        matches!(spent, MorunaError::Plan(ref m) if m.contains("budget `kwh` is exhausted")),
        "{spent}"
    );

    // The owner is exempt from every shape: all rows, several partitions, repeatable.
    let src =
        PlanSource::peql(&engine, &read, &owner(), EXCLUDE, &memory()).expect("the owner's source");
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
    let src = PlanSource::peql(&engine, &sql, &owner(), EXCLUDE, &memory()).expect("an aggregate");
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
    let denied = PlanSource::peql(&engine, &read, &marketer, EXCLUDE, &memory())
        .err()
        .expect("denied");
    assert!(denied.to_string().contains("analytics"), "{denied}");
    let unknown = PlanSource::peql(
        &engine,
        &PeqlRead::Contract("demo/none".into()),
        &owner(),
        EXCLUDE,
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
        OnViolation::default(),
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
    assert!(summary.bytes > 0);
    let manifest = engine.manifest("demo/readings").unwrap().unwrap();
    assert!(manifest.valid);
    assert_eq!(manifest.row_count, 410);
    // The files are the ones this write added, not the table's.
    assert!(!summary.files.is_empty());
    assert!(summary.files.len() < manifest.files.len());
    for file in &summary.files {
        assert!(
            manifest.files.iter().any(|f| &f.path == file),
            "{file} is in the table"
        );
    }
    // peQL's report of the write, as peQL serialises it: a verdict over the 400 rows written,
    // not the table's 410, with the hash of the files they landed in. A Parquet binding keeps
    // no snapshots, so the write commits none.
    let write = summary.write.as_ref().expect("peQL's report of the write");
    assert_eq!(write.engine, "peql");
    let said = &write.report;
    assert_eq!(said["rows_written"], 400);
    assert_eq!(said["verdict"]["valid"], true, "{said}");
    assert_eq!(said["verdict"]["row_count"], 400);
    assert_eq!(said["verdict"]["breached"], serde_json::json!([]));
    assert!(said["verdict"]["stats"].is_object(), "{said}");
    assert!(
        !said["verdict"]["data_hash"]
            .as_str()
            .expect("a hash")
            .is_empty(),
        "{said}"
    );
    let paths: Vec<&str> = said["files"]
        .as_array()
        .expect("the files written")
        .iter()
        .map(|f| f["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(paths, summary.files);
    assert!(said.get("snapshot").is_none(), "{said}");
    // A write that breaks no rule says so whatever it would have done with rows that did.
    assert_eq!(said["on_violation"], "refuse");
    assert_eq!(said["violations"], serde_json::json!({}));
    assert_eq!(said["rows_excluded"], 0);
    assert!(write.excluded.is_none());
}

/// A write of `morsels` through a sink that deals with rows breaking a rule as `on_violation`
/// says; what `finish` returned.
fn write_through(
    engine: &Arc<Engine>,
    on_violation: OnViolation,
    morsels: Vec<RecordBatch>,
) -> moruna_kernel::Result<moruna_kernel::SinkSummary> {
    let alloc = FakeAllocator::new();
    let rt = rt();
    let mut sink = PeqlSink::new(
        Arc::clone(engine),
        "demo/readings",
        &owner(),
        WriteMode::Append,
        on_violation,
        Arc::new(WriteMemory::new(2)),
    )?;
    sink.open(&SourceSchema::Table(schema()))?;
    for (seq, morsel) in morsels.into_iter().enumerate() {
        rt.block_on(sink.write(seq as u64, payload(morsel, &alloc)))?;
    }
    sink.finish()
}

fn row_count(engine: &Engine) -> i64 {
    engine
        .manifest("demo/readings")
        .expect("a manifest")
        .expect("written")
        .row_count
}

/// The default: a write any of whose rows breaks a row rule commits nothing, and the run's
/// error names each rule and how many rows broke it.
#[test]
fn a_refusing_write_commits_nothing_and_names_each_rule() {
    let scratch = Scratch::new("peql-refuse");
    let engine = engine(scratch.path(), 10);
    let refused = write_through(
        &engine,
        OnViolation::Refuse,
        vec![batch(10, 30, |i| i), batch(30, 50, |i| i - 35)],
    )
    .expect_err("refused");
    assert_eq!(
        refused.to_string(),
        "sink: peQL: `demo/readings` was not committed, because rows break its rules and this \
         write refuses them: non_negative (5 rows)"
    );
    assert_eq!(row_count(&engine), 10, "nothing was committed");
    assert!(engine.manifest("demo/readings").unwrap().unwrap().valid);
}

/// `exclude`: the rows that pass are committed, the others are kept apart with the rules each
/// broke, and the report counts them per rule beside where they are.
#[test]
fn an_excluding_write_commits_what_passes_and_keeps_the_rest_with_reasons() {
    let scratch = Scratch::new("peql-exclude");
    let engine = engine(scratch.path(), 10);
    let kept = scratch.path().join("run").join("excluded");
    let summary = write_through(
        &engine,
        OnViolation::Exclude(kept.clone()),
        vec![batch(10, 30, |i| i), batch(30, 50, |i| i - 35)],
    )
    .expect("committed");
    assert_eq!(summary.rows, 35);
    assert_eq!(row_count(&engine), 45);
    assert!(engine.manifest("demo/readings").unwrap().unwrap().valid);
    let write = summary.write.expect("peQL's report of the write");
    let said = &write.report;
    assert_eq!(said["on_violation"], "exclude");
    assert_eq!(said["rows_written"], 35);
    assert_eq!(said["rows_excluded"], 5);
    assert_eq!(said["violations"], serde_json::json!({"non_negative": 5}));
    assert_eq!(said["verdict"]["valid"], true, "{said}");

    // Only the morsel that had such rows left a file, named by its sequence number.
    let excluded = write.excluded.expect("the rows kept apart");
    assert_eq!(excluded.url, kept.display().to_string());
    assert_eq!(excluded.rows, 5);
    let file = kept.join("excluded-00000000000000000001.parquet");
    assert_eq!(excluded.files, [file.display().to_string()]);
    let rows: Vec<RecordBatch> =
        ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&file).expect("the file"))
            .expect("Parquet")
            .build()
            .expect("a reader")
            .collect::<std::result::Result<_, _>>()
            .expect("its rows");
    assert_eq!(ids(&rows), (30..35).collect::<Vec<_>>());
    for b in &rows {
        let reasons = b
            .column_by_name(VIOLATIONS_COLUMN)
            .expect("the rules each row broke")
            .as_list::<i32>();
        for i in 0..reasons.len() {
            let rules = reasons.value(i);
            let rules = rules.as_string::<i32>();
            assert_eq!(rules.iter().collect::<Vec<_>>(), [Some("non_negative")]);
        }
    }

    // Kept in a directory of the write's own: one that holds anything is refused before a row
    // is written.
    let again = write_through(
        &engine,
        OnViolation::Exclude(kept.clone()),
        vec![batch(50, 60, |i| i)],
    )
    .expect_err("the directory holds the last write's rows");
    assert!(again.to_string().contains("already holds files"), "{again}");
    assert_eq!(row_count(&engine), 45);
}

/// `warn`: every row is committed, and peQL's verdict and counts say which rules rows broke.
#[test]
fn a_warning_write_commits_every_row_and_says_which_broke_a_rule() {
    let scratch = Scratch::new("peql-warn");
    let engine = engine(scratch.path(), 10);
    let summary = write_through(&engine, OnViolation::Warn, vec![batch(10, 30, |i| i - 15)])
        .expect("committed");
    assert_eq!(summary.rows, 20);
    assert_eq!(row_count(&engine), 30);
    let write = summary.write.expect("peQL's report of the write");
    let said = &write.report;
    assert_eq!(said["on_violation"], "warn");
    assert_eq!(said["rows_excluded"], 0);
    assert_eq!(said["violations"], serde_json::json!({"non_negative": 5}));
    assert_eq!(said["verdict"]["valid"], false, "{said}");
    assert_eq!(
        said["verdict"]["breached"],
        serde_json::json!(["non_negative"])
    );
    assert!(write.excluded.is_none());
    assert!(!engine.manifest("demo/readings").unwrap().unwrap().valid);
}

/// Data holding rows that break a row rule: by default a read leaves them out and its report
/// counts them per rule; asked to refuse, the read is refused naming the rule and the count.
#[test]
fn a_read_leaves_out_rows_that_break_a_rule_unless_asked_to_refuse() {
    let scratch = Scratch::new("peql-read-violation");
    let engine = engine(scratch.path(), 10);
    write_through(&engine, OnViolation::Warn, vec![batch(10, 30, |i| i - 15)]).expect("committed");
    let read = PeqlRead::Contract("demo/readings".into());

    let src = PlanSource::peql(&engine, &read, &owner(), EXCLUDE, &memory()).expect("a source");
    let ids = ids(&drain(&src));
    assert_eq!(ids.len(), 25);
    assert!((10..15).all(|i| !ids.contains(&i)), "{ids:?}");
    let report = src.read_report().expect("peQL's report of the read");
    assert_eq!(report.engine, "peql");
    let contracts = report.report["contracts"].as_array().expect("contracts");
    assert_eq!(contracts.len(), 1);
    assert_eq!(contracts[0]["contract"], "demo/readings");
    assert_eq!(
        contracts[0]["excluded"],
        serde_json::json!({"non_negative": 5})
    );

    let refused = PlanSource::peql(&engine, &read, &owner(), OnReadViolation::Refuse, &memory())
        .err()
        .expect("refused");
    assert!(
        matches!(refused, MorunaError::Plan(ref m) if m.contains("non_negative (5 rows)")),
        "{refused}"
    );
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
        OnViolation::Refuse,
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
        OnViolation::Refuse,
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
