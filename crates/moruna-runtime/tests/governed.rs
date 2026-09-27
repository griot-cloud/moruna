//! H6 (MH 3, E8.5): a governed plan is a source, a governed write is a sink. Job documents with
//! `"kind": "datafusion"` sources and `"kind": "peql"` sinks, run for real over a peQL engine
//! opened on a disk, with data larger than the run's memory budget, at 256 MiB and at 1 GiB.
//!
//! A run's budget counts what its process already holds (12 f.1), so each run here is its own
//! process, as it is in a guest (`support::apart`): the parent writes the input, checks what the
//! runs left on the disk, and checks that each whole process stayed under its ceiling, by the
//! run's report and by the operating system (F8.9).

#![allow(clippy::result_large_err)]

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use moruna_datafusion::engine::{Caller, Engine};
use moruna_kernel::CancelToken;
use moruna_kernel::arrow::array::{AsArray, Int64Array, RecordBatch, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int64Type, Schema};
use moruna_runtime::Runtime;
use moruna_runtime::job::{BuildOptions, JobSpec, NoKernels, build};
use support::apart::{self, BUDGETS};
use support::{Scratch, one_run_at_a_time};

/// Rows of a kilobyte of incompressible text each: over the budget as Parquet and decoded.
const ROWS: i64 = 400_000;
const NOTE_BYTES: usize = 1000;

/// Guests read the eastern half; the whole result is withheld below five rows; meters are
/// hashed for them. Only the owner writes.
const BIG: &str = r#"
contract: demo/big
version: 1
owner: demo
binding: {parquet: big/}
expose:
  - {name: id, type: int64}
  - {name: region, type: utf8}
  - {name: meter, type: utf8}
  - {name: note, type: utf8}
rules:
  - {id: analytics, op: decide, expr: "ctx.purpose == 'analytics'"}
  - {id: east_for_guests, op: admit, expr: "ctx.tenant == 'demo' || row.region == 'EA'"}
  - {id: ids_positive, op: assert, expr: "row.id >= 0", on_fail: deny}
  - {id: mask_meter, op: transform, column: meter, expr: "ctx.tenant == 'demo' ? row.meter : hash_sha256(row.meter)"}
  - {id: small, op: shape, operator: suppress, params: {k: 5}, unless: "ctx.tenant == 'demo'"}
"#;

/// Where the guest keeps what it may read of it: its own contract.
const KEPT: &str = r#"
contract: partner/kept
version: 1
owner: partner
binding: {parquet: kept/}
expose:
  - {name: id, type: int64}
  - {name: region, type: utf8}
  - {name: meter, type: utf8}
  - {name: note, type: utf8}
rules:
  - {id: east_only, op: assert, expr: "row.region == 'EA'", on_fail: deny}
"#;

/// Where the owner copies it.
const COPY: &str = r#"
contract: demo/copy
version: 1
owner: demo
binding: {parquet: copy/}
expose:
  - {name: id, type: int64}
  - {name: region, type: utf8}
  - {name: meter, type: utf8}
  - {name: note, type: utf8}
rules:
  - {id: ids_positive, op: assert, expr: "row.id >= 0", on_fail: deny}
"#;

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("region", DataType::Utf8, false),
        Field::new("meter", DataType::Utf8, false),
        Field::new("note", DataType::Utf8, false),
    ]))
}

/// A kilobyte of text no codec shrinks much, the same for the same id.
fn note(id: i64) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state = (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = String::with_capacity(NOTE_BYTES);
    while out.len() < NOTE_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        for shift in (0..60).step_by(6) {
            if out.len() < NOTE_BYTES {
                out.push(ALPHABET[((state >> shift) & 63) as usize] as char);
            }
        }
    }
    out
}

fn batch(from: i64, to: i64) -> RecordBatch {
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
                ids.iter().map(|i| format!("M{i:07}")).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|i| note(*i)).collect::<Vec<_>>(),
            )),
        ],
    )
    .expect("a batch")
}

/// The input: one Parquet file, written a row group (about 4 MB) at a time.
fn input(path: &Path) {
    let props = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(4_000))
        .build();
    let file = std::fs::File::create(path).expect("the input file");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, schema(), Some(props)).expect("a writer");
    for from in (0..ROWS).step_by(4_000) {
        writer
            .write(&batch(from, (from + 4_000).min(ROWS)))
            .expect("a batch");
    }
    writer.close().expect("closed");
}

/// The disk: both contracts registered, nothing written.
fn disk(root: &Path) {
    let engine = Engine::open(root).expect("an engine");
    engine.register_contract(BIG, &schema()).expect("compiles");
    engine.register_contract(COPY, &schema()).expect("compiles");
    engine.register_contract(KEPT, &schema()).expect("compiles");
    engine
        .publish("demo/big", moruna_datafusion::engine::store::PUBLIC)
        .expect("published");
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

fn caller(id: &str, tenant: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "tenant": tenant, "purpose": "analytics"})
}

/// Run `source` into `sink` in a process of its own, at `budget`. The report, or the error.
fn run_apart(
    scratch: &Path,
    source: serde_json::Value,
    sink: serde_json::Value,
    budget: u64,
) -> serde_json::Value {
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    apart::run(
        "child_run",
        &serde_json::json!({
            "moruna_spec": 1,
            "source": source,
            "sink": sink,
            "budget": {"memory_bytes": budget, "cpu": 2.0},
            "staging": {"dir": staging, "limit_bytes": 1u64 << 30},
            "profiles_dir": scratch.join("profiles"),
        }),
    )
}

/// The child: one run, from the job its parent left, its outcome written back. Run by the
/// parent only.
#[test]
#[ignore = "run by h6 in a process of its own"]
fn child_run() {
    let Some(job) = apart::job() else {
        return;
    };
    let job = JobSpec::from_value(job).expect("the document parses");
    let env = |_: &str| None;
    let outcome = match build(
        &job,
        &NoKernels,
        BuildOptions {
            strict: true,
            env: &env,
            notes: Vec::new(),
        },
    ) {
        Err(e) => serde_json::json!({"error": e.to_string()}),
        Ok(built) => match Runtime::run(built.spec, CancelToken::new()) {
            Ok(report) => serde_json::json!({"report": report}),
            Err(e) => serde_json::json!({"error": e.to_string()}),
        },
    };
    apart::answer(outcome);
}

/// `id -> meter` for every row, each note checked whole.
fn rows(batches: &[RecordBatch]) -> BTreeMap<i64, String> {
    let mut out = BTreeMap::new();
    for b in batches {
        let s = b.schema();
        let text = |name: &str| {
            moruna_kernel::arrow::compute::cast(
                b.column(s.index_of(name).expect("a column")),
                &DataType::Utf8,
            )
            .expect("text")
        };
        let id = b
            .column(s.index_of("id").unwrap())
            .as_primitive::<Int64Type>();
        let (meter, note) = (text("meter"), text("note"));
        let (meter, note) = (meter.as_string::<i32>(), note.as_string::<i32>());
        for r in 0..b.num_rows() {
            assert_eq!(
                note.value(r),
                self::note(id.value(r)),
                "a note arrived whole"
            );
            out.insert(id.value(r), meter.value(r).to_string());
        }
    }
    out
}

fn bytes_under(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .expect("a directory")
        .map(|e| e.expect("an entry").metadata().expect("metadata").len())
        .sum()
}

/// H6. A write larger than the budget (a Parquet file into a contract) lands with a valid
/// manifest; the guest's gated, shaped view of it, larger than the budget, streams through a
/// run inside the budget and lands exactly as `Engine::query` answers it; the owner copies the
/// contract into another with SQL; a guest's write is refused before anything is read. Every
/// run, at 256 MiB and at 1 GiB, keeps its whole process under its ceiling.
#[test]
fn h6_a_governed_plan_is_a_source_and_a_contract_write_is_a_sink() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("h6_governed");
    let root = scratch.path().join("disk");
    disk(&root);
    let file = scratch.path().join("in.parquet");
    input(&file);
    assert!(
        std::fs::metadata(&file).expect("the file").len() > BUDGETS[0],
        "the input file is larger than the smaller budget"
    );
    let rt = rt();

    for budget in BUDGETS {
        // The write: a file larger than the budget, under the contract, by its owner.
        let wrote = run_apart(
            scratch.path(),
            serde_json::json!({"kind": "parquet", "url": file}),
            serde_json::json!({"kind": "peql", "root": root, "contract": "demo/big",
                               "caller": caller("ana", "demo"), "mode": "overwrite"}),
            budget,
        );
        apart::within(&wrote, "the write", budget);
        let engine = Engine::open(&root).expect("the engine");
        let manifest = engine.manifest("demo/big").expect("read").expect("written");
        assert!(manifest.valid, "{:?}", manifest.breached);
        assert_eq!(manifest.row_count, ROWS);
        assert!(
            bytes_under(&root.join("big")) > BUDGETS[0],
            "the contract's files outweigh the smaller budget"
        );

        // The guest's view, kept under the guest's own contract.
        let viewed = run_apart(
            scratch.path(),
            serde_json::json!({"kind": "datafusion", "root": root, "contract": "demo/big",
                               "caller": caller("gus", "partner")}),
            serde_json::json!({"kind": "peql", "root": root, "contract": "partner/kept",
                               "caller": caller("gus", "partner"), "mode": "overwrite"}),
            budget,
        );
        apart::within(&viewed, "the view", budget);
        let engine = Engine::open(&root).expect("the engine again");
        let kept = engine
            .manifest("partner/kept")
            .expect("read")
            .expect("written");
        assert!(kept.valid, "only eastern rows arrived: {:?}", kept.breached);
        let guest = Caller::new("gus", "partner", "analytics");
        let moved = rows(
            &rt.block_on(engine.query(r#"SELECT * FROM "partner/kept""#, &guest))
                .expect("the kept rows")
                .batches,
        );
        assert_eq!(moved.len() as i64, ROWS / 2, "the eastern half");
        assert!(
            moved.values().all(|meter| meter.len() == 64),
            "meters hashed"
        );
        let answer = rt
            .block_on(engine.query(r#"SELECT * FROM "demo/big""#, &guest))
            .expect("the query");
        assert_eq!(
            moved,
            rows(&answer.batches),
            "the run delivered what the query answers"
        );
        drop(answer);

        // The owner's copy through SQL: another write larger than the budget.
        let copied = run_apart(
            scratch.path(),
            serde_json::json!({"kind": "datafusion", "root": root,
                               "sql": r#"SELECT id, region, meter, note FROM "demo/big""#,
                               "caller": caller("ana", "demo")}),
            serde_json::json!({"kind": "peql", "root": root, "contract": "demo/copy",
                               "caller": caller("ana", "demo"), "mode": "overwrite"}),
            budget,
        );
        apart::within(&copied, "the copy", budget);
        let engine = Engine::open(&root).expect("the engine again");
        let manifest = engine
            .manifest("demo/copy")
            .expect("read")
            .expect("written");
        assert!(manifest.valid, "{:?}", manifest.breached);
        assert_eq!(manifest.row_count, ROWS);
        let sums = rt
            .block_on(engine.query(
                r#"SELECT COUNT(*) AS n, SUM(id) AS s FROM "demo/copy""#,
                &Caller::new("ana", "demo", "analytics"),
            ))
            .expect("the copy answers");
        let b = &sums.batches[0];
        assert_eq!(b.column(0).as_primitive::<Int64Type>().value(0), ROWS);
        assert_eq!(
            b.column(1).as_primitive::<Int64Type>().value(0),
            ROWS * (ROWS - 1) / 2
        );
    }

    // A guest may read the contract, and may not write under it.
    let engine = Engine::open(&root).expect("the engine again");
    let before = engine.manifest("demo/big").expect("read").expect("written");
    let refused = run_apart(
        scratch.path(),
        serde_json::json!({"kind": "parquet", "url": file}),
        serde_json::json!({"kind": "peql", "root": root, "contract": "demo/big",
                           "caller": caller("gus", "partner")}),
        BUDGETS[0],
    );
    let error = refused["error"]
        .as_str()
        .expect("a guest's write is refused");
    assert!(error.contains("writes are the owner's"), "{error}");
    let after = Engine::open(&root)
        .expect("the engine again")
        .manifest("demo/big")
        .expect("read")
        .expect("still written");
    assert_eq!(after.row_count, ROWS, "nothing landed");
    assert_eq!(after.written_at, before.written_at);
}

/// The runs of H6 whose source cannot be re-read, into a Parquet sink, at 256 MiB and at 1 GiB:
/// the guest's gated view (its whole-result suppression makes it unrepeatable, so its queued
/// morsels are staged, which is where an arena buffer could not be found, F8.5's finding), and
/// an aggregation whose state is larger than the budget, whose operators hold their state in
/// the run's pool and spill to the staging directory past it (MH 4.5). An aggregation's output
/// order depends on how it spilled, so it is not repeatable either (F8.9). Every run keeps its
/// whole process under its ceiling.
#[test]
fn h6_an_unrepeatable_plan_into_parquet_inside_the_budget() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("h6_parquet");
    let root = scratch.path().join("disk");
    disk(&root);
    let file = scratch.path().join("in.parquet");
    input(&file);
    for budget in BUDGETS {
        let wrote = run_apart(
            scratch.path(),
            serde_json::json!({"kind": "parquet", "url": file}),
            serde_json::json!({"kind": "peql", "root": root, "contract": "demo/big",
                               "caller": caller("ana", "demo"), "mode": "overwrite"}),
            budget,
        );
        apart::within(&wrote, "the write", budget);

        let viewed_to = scratch.path().join(format!("view-out-{budget}"));
        let viewed = run_apart(
            scratch.path(),
            serde_json::json!({"kind": "datafusion", "root": root, "contract": "demo/big",
                               "caller": caller("gus", "partner")}),
            serde_json::json!({"kind": "parquet", "url": viewed_to}),
            budget,
        );
        apart::within(&viewed, "the view into Parquet", budget);
        operators_within(&viewed, "the view into Parquet");
        assert_eq!(support::read_back_rows(&viewed_to), (ROWS / 2) as u64);

        // Every id is its own group, and each group keeps a kilobyte note: 400 MB of state.
        let grouped_to = scratch.path().join(format!("grouped-out-{budget}"));
        let grouped = run_apart(
            scratch.path(),
            serde_json::json!({"kind": "datafusion", "root": root,
                               "sql": r#"SELECT id, MAX(note) AS note, COUNT(*) AS n
                                         FROM "demo/big" GROUP BY id"#,
                               "caller": caller("ana", "demo")}),
            serde_json::json!({"kind": "parquet", "url": grouped_to}),
            budget,
        );
        apart::within(&grouped, "the aggregation", budget);
        let refused = operators_within(&grouped, "the aggregation");
        assert!(
            refused > 0,
            "400 MB of groups spilled from a pool a fraction of that size"
        );
        assert_eq!(support::read_back_rows(&grouped_to), ROWS as u64);
    }
}

/// The plan's operators held no more than their pool, and how often the pool refused them.
fn operators_within(outcome: &serde_json::Value, what: &str) -> u64 {
    let notes = outcome["report"]["notes"].as_array().expect("notes");
    let note = notes
        .iter()
        .filter_map(|n| n.as_str())
        .find(|n| n.starts_with("the source's plan operators held at most"))
        .unwrap_or_else(|| panic!("{what}: no note on the plan's operators: {notes:?}"));
    let figures: Vec<u64> = note.split(' ').filter_map(|w| w.parse().ok()).collect();
    let [held, capacity, refused] = figures[..] else {
        panic!("{what}: {note}");
    };
    eprintln!("{what}: {note}");
    assert!(capacity > 0, "{what}: the plan had a pool: {note}");
    assert!(held <= capacity, "{what}: {note}");
    refused
}

/// Another name for `demo/big`'s files.
const ALIAS: &str = r#"
contract: demo/alias
version: 1
owner: demo
binding: {parquet: ./big}
expose:
  - {name: id, type: int64}
  - {name: region, type: utf8}
  - {name: meter, type: utf8}
  - {name: note, type: utf8}
"#;

fn refusal(source: serde_json::Value, sink: serde_json::Value) -> String {
    let job = JobSpec::from_value(serde_json::json!({
        "moruna_spec": 1, "source": source, "sink": sink,
    }))
    .expect("parses");
    let env = |_: &str| None;
    build(
        &job,
        &NoKernels,
        BuildOptions {
            strict: false,
            env: &env,
            notes: Vec::new(),
        },
    )
    .err()
    .expect("refused")
    .to_string()
}

/// The sink equals source rule compares where the data is, as each contract's binding resolves,
/// and not the contracts' names: a run whose sink writes the files its source reads is refused
/// before anything is read, whether the two name one contract, two contracts bound to the same
/// files, or a contract and a Parquet source over its directory.
#[test]
fn a_sink_over_its_own_source_is_refused() {
    let scratch = Scratch::new("h6_same");
    let root = scratch.path().join("disk");
    disk(&root);
    Engine::open(&root)
        .expect("the engine")
        .register_contract(ALIAS, &schema())
        .expect("compiles");
    let owner = caller("ana", "demo");
    let same = "the sink writes where the source reads";

    let joined = refusal(
        serde_json::json!({"kind": "datafusion", "root": root,
                           "sql": r#"SELECT * FROM "demo/big" JOIN "demo/copy" USING (id)"#,
                           "caller": owner}),
        serde_json::json!({"kind": "peql", "root": root, "contract": "demo/copy",
                           "caller": owner}),
    );
    assert!(joined.contains(same), "{joined}");

    let aliased = refusal(
        serde_json::json!({"kind": "datafusion", "root": root, "contract": "demo/big",
                           "caller": owner}),
        serde_json::json!({"kind": "peql", "root": root, "contract": "demo/alias",
                           "caller": owner}),
    );
    assert!(
        aliased.contains(same),
        "two names, one set of files: {aliased}"
    );

    let by_path = refusal(
        serde_json::json!({"kind": "parquet", "url": root.join("big").join("part.parquet")}),
        serde_json::json!({"kind": "peql", "root": root, "contract": "demo/alias",
                           "caller": owner}),
    );
    assert!(by_path.contains(same), "{by_path}");

    // A contract peQL does not know is refused by the field that names it.
    let unknown = refusal(
        serde_json::json!({"kind": "datafusion", "root": root, "contract": "demo/none",
                           "caller": owner}),
        serde_json::json!({"kind": "peql", "root": root, "contract": "demo/copy",
                           "caller": owner}),
    );
    assert!(unknown.contains("source.contract"), "{unknown}");
    assert!(!root.join("big").exists(), "nothing was written");
}
