//! Kinds their embedder serves (SDD 18 e.4, KD-I11), over real components: an embedder declares
//! its kinds in the declaration format, serves their ports, and Moruna runs every task of them by
//! handing each batch to the port. The ports here are written in the test, as an embedder writes
//! them: each reads the request with `moruna_job::ServedCall` and Arrow, and answers with an Arrow
//! IPC stream, or with what a broken or hostile embedder would answer.
//!
//! KD-T12 declare and list, KD-T13 runs across a fan-out and a fan-in, KD-T14 every way a port
//! fails a task, KD-T15 the type grammar has one reader, KD-T16 a cancel stops the calls,
//! KD-T17 a run held at a known batch, killed and resumed, gives the same output exactly once,
//! KD-T18 the port served over the host protocol by `moruna run --kinds`.

#![allow(clippy::result_large_err)]

mod support;

use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use moruna_job::{
    Declaration, Edge, Fact, Flow, Isolation, Job, KindDoc, Needs, PortDoc, Role, Served,
    ServedCall, Task,
};
use moruna_kernel::arrow::array::{Array, ArrayRef, Int64Array};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::ipc::reader::StreamReader;
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, Answer, CancelToken, GuestCall, GuestPort, InitCtx, Kernel, MorunaError, Payload,
    RunPorts, StagePorts, StageWaits, Tier,
};
use moruna_runtime::job::{BuildOptions, build};
use moruna_runtime::kinds::conformance::{self, Harness};
use moruna_runtime::kinds::{Context, Embedding, Entry, Registry, RunnableKind};
use moruna_runtime::{Components, RunId, Runtime};
use moruna_testkit::FakeAllocator;
use serde_json::{Value, json};
use support::{Scratch, one_run_at_a_time, planned, write_parquet};

// ------------------------------------------------------------------------------------------
// The embedder: its declarations and its ports.
// ------------------------------------------------------------------------------------------

/// What a test port answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answering {
    /// The batch and `scaled`, its `value` times the task's `factor`.
    Scale,
    /// The batch and `input`, the input port it came in on.
    Tag,
    /// The batch and `scaled`, as two record batches of half the rows each.
    Halves,
    /// The port's own refusal.
    Refuse,
    /// Bytes that are no Arrow IPC stream.
    Garbage,
    /// The batch as it came, which is not what a kind that adds `scaled` declares.
    Unchanged,
    /// A stream of no record batch.
    Empty,
    /// Nothing, until the call is given up.
    Hold,
    /// A panic.
    Panic,
}

/// A port an embedder serves inside the machine, counting its calls.
struct TestPort {
    answering: Answering,
    calls: Arc<AtomicUsize>,
    /// Set by a test before it cancels a run: calls after it are counted in `late`.
    after: Arc<AtomicBool>,
    late: Arc<AtomicUsize>,
}

impl TestPort {
    fn new(answering: Answering) -> TestPort {
        TestPort {
            answering,
            calls: Arc::default(),
            after: Arc::default(),
            late: Arc::default(),
        }
    }
}

/// The batch a request carries, read as an embedder reads it.
fn requested(request: &[u8]) -> (ServedCall, RecordBatch) {
    let (call, stream) = ServedCall::decode(request).expect("a served call");
    let mut reader = StreamReader::try_new(stream, None).expect("an Arrow IPC stream");
    let batch = reader.next().expect("one batch").expect("a batch");
    assert!(reader.next().is_none(), "a request carries one batch");
    (call, batch)
}

/// `batch` with a column `name` of `values` appended.
fn with_column(batch: &RecordBatch, name: &str, values: Vec<i64>) -> RecordBatch {
    let mut fields: Vec<Field> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    fields.push(Field::new(name, DataType::Int64, false));
    let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
    columns.push(Arc::new(Int64Array::from(values)));
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).expect("a batch")
}

fn ipc(batches: &[RecordBatch], schema: &Schema) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = StreamWriter::try_new(&mut out, schema).expect("a writer");
    for batch in batches {
        writer.write(batch).expect("written");
    }
    writer.finish().expect("finished");
    drop(writer);
    out
}

fn scaled(batch: &RecordBatch, factor: i64) -> RecordBatch {
    let values = batch
        .column_by_name("value")
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>().cloned())
        .expect("an Int64 `value`");
    with_column(
        batch,
        "scaled",
        values.iter().map(|v| v.unwrap_or(0) * factor).collect(),
    )
}

/// What an embedder answers a request with, `answering` as it is told.
fn answer(answering: Answering, request: &[u8], call: Option<&GuestCall>) -> Answer {
    let (served, batch) = requested(request);
    let factor = served
        .settings
        .get("factor")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    match answering {
        Answering::Scale => {
            let out = scaled(&batch, factor);
            Answer::Ok(ipc(std::slice::from_ref(&out), &out.schema()))
        }
        Answering::Halves => {
            let out = scaled(&batch, factor);
            let half = out.num_rows() / 2;
            let parts = [out.slice(0, half), out.slice(half, out.num_rows() - half)];
            Answer::Ok(ipc(&parts, &out.schema()))
        }
        Answering::Tag => {
            let out = with_column(
                &batch,
                "input",
                vec![i64::from(served.input); batch.num_rows()],
            );
            Answer::Ok(ipc(std::slice::from_ref(&out), &out.schema()))
        }
        Answering::Refuse => Answer::Error("the model is over its budget".into()),
        Answering::Garbage => Answer::Ok(b"this is not an Arrow IPC stream".to_vec()),
        Answering::Unchanged => Answer::Ok(ipc(std::slice::from_ref(&batch), &batch.schema())),
        Answering::Empty => Answer::Ok(ipc(&[], &batch.schema())),
        Answering::Hold => {
            let call = call.expect("a guest call");
            while !call.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            Answer::Error("given up".into())
        }
        Answering::Panic => panic!("the embedder's port fell over"),
    }
}

impl GuestPort for TestPort {
    fn call(&self, request: Vec<u8>, call: &GuestCall) -> Answer {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.after.load(Ordering::SeqCst) {
            self.late.fetch_add(1, Ordering::SeqCst);
        }
        answer(self.answering, &request, Some(call))
    }
}

/// The embedder: its guest ports by name.
struct Embedder(BTreeMap<String, Arc<dyn GuestPort>>);

impl Embedding for Embedder {
    fn guest_port(&self, name: &str) -> Option<Arc<dyn GuestPort>> {
        self.0.get(name).cloned()
    }
}

/// A served kind `name` on `port`: its settings an optional integer `factor` of at least 1.
fn declared(name: &str, port: PortDoc, input: Option<Value>, output: Option<Value>) -> Declaration {
    Declaration {
        name: name.into(),
        role: Role::Kernel,
        summary: format!("{name}, which the embedder serves"),
        settings: json!({
            "$schema": moruna_job::SCHEMA_DIALECT,
            "type": "object",
            "additionalProperties": false,
            "properties": {"factor": {"type": "integer", "minimum": 1}},
            "examples": [{"factor": 2}, {}]
        }),
        inputs: Flow::Batches,
        outputs: Flow::Batches,
        needs: Needs::a_port(),
        deterministic: Fact::Yes,
        preserves_columns: Fact::Yes,
        isolation: Isolation::Embedder,
        served: Some(Served {
            port,
            input_schema: input,
            output_schema: output,
        }),
    }
}

/// `scale`: takes `value`, adds `scaled`.
fn scale(name: &str, port: PortDoc) -> Declaration {
    declared(
        name,
        port,
        Some(json!({"value": "int64"})),
        Some(json!({"adds": {"scaled": "int64"}})),
    )
}

/// The standard kinds, `declarations` and the ports that serve them.
fn registry(declarations: Vec<Declaration>, ports: Vec<(&str, Arc<dyn GuestPort>)>) -> Registry {
    let mut kinds = Registry::standard().with_embedding(Arc::new(Embedder(
        ports.into_iter().map(|(n, p)| (n.to_string(), p)).collect(),
    )));
    for declaration in declarations {
        kinds.declare(declaration).expect("declared");
    }
    kinds
}

// ------------------------------------------------------------------------------------------
// Jobs.
// ------------------------------------------------------------------------------------------

fn entry(value: Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn opts() -> BuildOptions<'static> {
    BuildOptions {
        strict: true,
        env: &|_| None,
        notes: Vec::new(),
        packages: Default::default(),
        host_ports: None,
    }
}

fn url(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn configured(mut job: Job, scratch: &Path, cpu: f64) -> Job {
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).expect("the staging directory");
    job.budget.memory_bytes = Some(1 << 30);
    job.budget.cpu = Some(cpu);
    job.staging.dir = Some(staging.display().to_string());
    job.staging.limit_bytes = Some(1 << 30);
    job.profiles_dir = Some(scratch.join("profiles").display().to_string());
    job
}

/// Parquet over `rows` rows in `groups` row groups, through the kernel `kernel`, to Parquet.
fn one_kernel(scratch: &Path, rows: u64, groups: u64, kernel: Value) -> Job {
    let input = scratch.join("in.parquet");
    write_parquet(&input, rows, groups);
    let out = scratch.join("out");
    std::fs::create_dir_all(&out).expect("out");
    configured(
        Job::new(
            vec![
                Task::source("in", entry(json!({"kind": "parquet", "url": url(&input)}))),
                Task::kernel("served", entry(kernel)),
                Task::sink("out", entry(json!({"kind": "parquet", "url": url(&out)}))),
            ],
            vec![
                Edge::between("in", "served"),
                Edge::between("served", "out"),
            ],
        ),
        scratch,
        4.0,
    )
}

fn run(job: &Job, kinds: &Registry) -> Result<moruna_runtime::RunReport, MorunaError> {
    let built = build(job, &planned(job), kinds, opts())?;
    Runtime::run(built.spec, CancelToken::new()).map_err(|e| e.into_parts().error)
}

/// Every row of every Parquet file under `dir`: its `id`, `value` and the named third column.
fn rows(dir: &Path, third: &str) -> BTreeMap<(i64, i64, i64), u32> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir)
        .expect("the output directory")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        let file = std::fs::File::open(&path).expect("an output file");
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("a reader")
            .build()
            .expect("batches");
        for batch in reader {
            let batch = batch.expect("a batch");
            let column = |name: &str| -> Int64Array {
                batch
                    .column_by_name(name)
                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>().cloned())
                    .unwrap_or_else(|| panic!("an Int64 column `{name}`"))
            };
            let (ids, values, thirds) = (column("id"), column("value"), column(third));
            for i in 0..batch.num_rows() {
                *out.entry((ids.value(i), values.value(i), thirds.value(i)))
                    .or_insert(0) += 1;
            }
        }
    }
    out
}

/// What `write_parquet` wrote, each row once, with its third column made by `third`.
fn expected(rows: u64, third: impl Fn(i64, i64) -> i64) -> BTreeMap<(i64, i64, i64), u32> {
    (0..rows as i64)
        .map(|id| ((id, id * 2, third(id, id * 2)), 1))
        .collect()
}

// ------------------------------------------------------------------------------------------
// KD-T12 declare and list.
// ------------------------------------------------------------------------------------------

/// KD-T12 declare_and_list (KD-I11, e.3): an embedder's served kinds are declared beside the
/// standard ones and listed with them; one that collides with a built-in kind, a declaration
/// that is not a served kind's, a served kind registered with an adapter of its own and a
/// settings schema the validator does not read are each refused naming why; the served kind
/// passes the runnable-kind suite; `--kinds` gives the command line the same.
#[test]
fn kd_t12_declare_and_list() {
    let kinds = registry(
        vec![scale("scale", PortDoc::guest("scale"))],
        vec![("scale", Arc::new(TestPort::new(Answering::Scale)))],
    );
    let listing = kinds.listing();
    let names: Vec<&str> = listing.kernels.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["image", "package", "scale", "std"]);
    let listed = listing.kernels.iter().find(|d| d.name == "scale").unwrap();
    assert_eq!(listed, &scale("scale", PortDoc::guest("scale")));
    // The listing an embedder checks with is the catalogue of the same kinds.
    let catalogue = moruna_job::Catalogue::from_listing(listing).expect("a catalogue");
    assert!(
        catalogue
            .get(Role::Kernel, "scale")
            .unwrap()
            .served
            .is_some()
    );

    let mut more = kinds.clone();
    for builtin in ["std", "package", "image"] {
        let refused = more
            .declare(scale(builtin, PortDoc::guest("scale")))
            .expect_err("a built-in's name");
        assert_eq!(
            refused.to_string(),
            format!("config kinds: a kernel kind `{builtin}` is registered already")
        );
    }
    assert!(more.declare(scale("scale", PortDoc::guest("x"))).is_err());
    let mut plain = scale("plain", PortDoc::guest("scale"));
    plain.served = None;
    plain.isolation = Isolation::Builtin;
    plain.needs = Needs::nothing();
    assert!(
        more.declare(plain)
            .expect_err("not served")
            .to_string()
            .contains("`plain` is not served by a port")
    );
    let mut odd = scale("odd", PortDoc::guest("scale"));
    odd.settings["properties"]["factor"]["pattern"] = json!("[0-9]+");
    let refused = more
        .declare(odd)
        .expect_err("outside the subset")
        .to_string();
    assert!(
        refused.contains("`odd`'s settings schema cannot be checked against")
            && refused.contains("pattern"),
        "{refused}"
    );
    // A served kind is declared, never registered with an adapter of its own.
    let adapter = moruna_runtime::kinds::served::Served::new(scale("own", PortDoc::guest("scale")))
        .expect("an adapter");
    let refused = more
        .register_kernel(Arc::new(adapter))
        .expect_err("served")
        .to_string();
    assert!(refused.contains("a served kind is declared"), "{refused}");
    // The registry the program built is unchanged by what a clone of it declared.
    assert_eq!(kinds.listing().kernels.len(), 4);

    // KD-C1 to KD-C6 over its examples, in a context whose embedder serves its port.
    let suite = moruna_runtime::kinds::served::Served::new(scale("scale", PortDoc::guest("scale")))
        .expect("an adapter");
    let passed = conformance::kernel(
        &suite,
        &Harness::new().context(|| {
            Context::new(Arc::new(Embedder(
                [(
                    "scale".to_string(),
                    Arc::new(TestPort::new(Answering::Scale)) as Arc<dyn GuestPort>,
                )]
                .into_iter()
                .collect(),
            )))
        }),
    )
    .unwrap_or_else(|v| panic!("{v}"));
    assert!(passed.cases > 0);

    // The command line's `--kinds`: a file of declarations, read and declared for the command.
    let scratch = Scratch::new("kd_t12");
    let file = scratch.path().join("kinds.json");
    std::fs::write(
        &file,
        serde_json::to_string(&[scale("scale", PortDoc::host("scale"))]).unwrap(),
    )
    .unwrap();
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (files, rest) = moruna_runtime::host::cli::split_kinds(&args(&[
        "kinds",
        "--kinds",
        &file.display().to_string(),
    ]))
    .expect("split");
    assert_eq!(rest, ["kinds"]);
    let extended =
        moruna_runtime::host::cli::with_declared(&Registry::standard(), &files).expect("declared");
    assert!(extended.kernel("kernels[0]", "scale").is_ok());
    assert!(Registry::standard().kernel("kernels[0]", "scale").is_err());
    for (line, says) in [
        (args(&["--version", "--kinds", "f"]), "not by `--version`"),
        (args(&["run", "--vm", "j", "--kinds", "f"]), "drop --kinds"),
        (args(&["kinds", "--kinds"]), "--kinds needs a file"),
    ] {
        let refused = moruna_runtime::host::cli::split_kinds(&line).expect_err("refused");
        assert!(refused.contains(says), "{refused}");
    }
    std::fs::write(&file, "[{\"name\": 1}]").unwrap();
    let Err(refused) = moruna_runtime::host::cli::with_declared(&Registry::standard(), &files)
    else {
        panic!("not declarations, and declared");
    };
    assert!(
        refused.contains("not a JSON array of declarations"),
        "{refused}"
    );
    std::fs::write(
        &file,
        serde_json::to_string(&[scale("std", PortDoc::host("scale"))]).unwrap(),
    )
    .unwrap();
    let Err(refused) = moruna_runtime::host::cli::with_declared(&Registry::standard(), &files)
    else {
        panic!("a built-in's name, and declared");
    };
    assert!(refused.contains("registered already"), "{refused}");
}

// ------------------------------------------------------------------------------------------
// KD-T13 runs.
// ------------------------------------------------------------------------------------------

/// KD-T13 a_served_kind_runs: the rows out are the port's transform of the rows in, through a
/// fan-out to two sinks, a fan-in of two sources into one task, a reply of several batches, and
/// two served kinds in a row; every batch went through the port.
#[test]
fn kd_t13_a_served_kind_runs() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("kd_t13");
    let port = Arc::new(TestPort::new(Answering::Scale));
    let tag = Arc::new(TestPort::new(Answering::Tag));
    let halves = Arc::new(TestPort::new(Answering::Halves));
    let kinds = registry(
        vec![
            scale("scale", PortDoc::guest("scale")),
            scale("halves", PortDoc::guest("halves")),
            declared(
                "tag",
                PortDoc::guest("tag"),
                None,
                Some(json!({"adds": {"input": "int64"}})),
            ),
        ],
        vec![
            ("scale", port.clone() as Arc<dyn GuestPort>),
            ("tag", tag.clone() as Arc<dyn GuestPort>),
            ("halves", halves.clone() as Arc<dyn GuestPort>),
        ],
    );

    // A fan-out: one served task to two sinks.
    let rows_in = 30_000u64;
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, rows_in, 6);
    let (a, b) = (scratch.path().join("a"), scratch.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let job = configured(
        Job::new(
            vec![
                Task::source("in", entry(json!({"kind": "parquet", "url": url(&input)}))),
                Task::kernel("scale", entry(json!({"kind": "scale", "factor": 3}))),
                Task::sink("a", entry(json!({"kind": "parquet", "url": url(&a)}))),
                Task::sink("b", entry(json!({"kind": "parquet", "url": url(&b)}))),
            ],
            vec![
                Edge::between("in", "scale"),
                Edge::between("scale", "a"),
                Edge::between("scale", "b"),
            ],
        ),
        scratch.path(),
        4.0,
    );
    assert_eq!(
        moruna_job::check(
            &job,
            &moruna_job::Catalogue::from_listing(kinds.listing()).unwrap()
        ),
        Ok(())
    );
    run(&job, &kinds).expect("the fan-out runs");
    let want = expected(rows_in, |_, value| value * 3);
    assert_eq!(rows(&a, "scaled"), want);
    assert_eq!(rows(&b, "scaled"), want);
    assert!(
        port.calls.load(Ordering::SeqCst) >= 6,
        "every batch was a call"
    );

    // A fan-in: two sources into one task of two inputs, each batch told its port.
    let other = scratch.path().join("other.parquet");
    write_parquet(&other, 7_000, 2);
    let joined = scratch.path().join("joined");
    std::fs::create_dir_all(&joined).unwrap();
    let job = configured(
        Job::new(
            vec![
                Task::source(
                    "left",
                    entry(json!({"kind": "parquet", "url": url(&input)})),
                ),
                Task::source(
                    "right",
                    entry(json!({"kind": "parquet", "url": url(&other)})),
                ),
                Task::kernel_with_inputs("tag", entry(json!({"kind": "tag"})), &["l", "r"]),
                Task::sink(
                    "out",
                    entry(json!({"kind": "parquet", "url": url(&joined)})),
                ),
            ],
            vec![
                Edge::new("left", "out", "tag", "l"),
                Edge::new("right", "out", "tag", "r"),
                Edge::between("tag", "out"),
            ],
        ),
        scratch.path(),
        4.0,
    );
    run(&job, &kinds).expect("the fan-in runs");
    let mut want = expected(rows_in, |_, _| 0);
    for (key, n) in expected(7_000, |_, _| 1) {
        *want.entry(key).or_insert(0) += n;
    }
    assert_eq!(rows(&joined, "input"), want);

    // A reply of two batches is the task's output for its batch, and two served kinds in a row
    // each see what the one before gave.
    let chained = scratch.path().join("chained");
    std::fs::create_dir_all(&chained).unwrap();
    let job = configured(
        Job::new(
            vec![
                Task::source("in", entry(json!({"kind": "parquet", "url": url(&other)}))),
                Task::kernel("halves", entry(json!({"kind": "halves", "factor": 5}))),
                Task::kernel("tag", entry(json!({"kind": "tag"}))),
                Task::sink(
                    "out",
                    entry(json!({"kind": "parquet", "url": url(&chained)})),
                ),
            ],
            vec![
                Edge::between("in", "halves"),
                Edge::between("halves", "tag"),
                Edge::between("tag", "out"),
            ],
        ),
        scratch.path(),
        4.0,
    );
    run(&job, &kinds).expect("the chain runs");
    assert_eq!(rows(&chained, "scaled"), expected(7_000, |_, v| v * 5));
    assert_eq!(rows(&chained, "input"), expected(7_000, |_, _| 0));
}

// ------------------------------------------------------------------------------------------
// KD-T14 failures.
// ------------------------------------------------------------------------------------------

/// A failed run's error, as its text.
fn failed(answering: Answering, port: PortDoc, rows: u64) -> String {
    let scratch = Scratch::new("kd_t14");
    let name = port.place().unwrap().name().to_string();
    let kinds = registry(
        vec![scale("model", port)],
        vec![(
            &name,
            Arc::new(TestPort::new(answering)) as Arc<dyn GuestPort>,
        )],
    );
    let job = one_kernel(
        scratch.path(),
        rows,
        2,
        json!({"kind": "model", "factor": 2}),
    );
    match run(&job, &kinds) {
        Ok(_) => panic!("{answering:?}: the run completed"),
        Err(error) => {
            assert!(
                matches!(error, MorunaError::Kernel { .. }),
                "{answering:?}: {error:?}"
            );
            error.to_string()
        }
    }
}

/// KD-T14 a_port_fails_the_task (KD-I11): every way a port fails fails the task, naming the kind
/// and the port: its own refusal, with what it said; bytes that are no stream; a stream of no
/// batch; a reply that is not what the kind declares it gives; a deadline passed; a handler that
/// panicked. A port that is not there refuses the document before anything runs.
#[test]
fn kd_t14_a_port_fails_the_task() {
    let _serial = one_run_at_a_time();
    let guest = || PortDoc::guest("model");
    for (answering, port, says) in [
        (
            Answering::Refuse,
            guest(),
            "`model` (port `model`): the port refused the batch: the model is over its budget",
        ),
        (
            Answering::Garbage,
            guest(),
            "`model` (port `model`): the reply is not an Arrow IPC stream Moruna reads",
        ),
        (
            Answering::Empty,
            guest(),
            "`model` (port `model`): the reply holds no record batch",
        ),
        (
            Answering::Unchanged,
            guest(),
            "`model` (port `model`): the reply is not what `model` declares it gives: column \
             `scaled`: declared int64, not produced",
        ),
        (
            Answering::Hold,
            guest().with_deadline_ms(300),
            "`model`: port `model`: no answer within its deadline of 300 ms",
        ),
        (
            Answering::Panic,
            guest(),
            "`model`: port `model`: the guest handler panicked: the embedder's port fell over",
        ),
    ] {
        let said = failed(answering, port, 4_000);
        assert!(said.contains(says), "{answering:?}: {said}");
    }

    // A port that is not there: no handler of the name, no peer for a host port.
    let scratch = Scratch::new("kd_t14_missing");
    let job = one_kernel(scratch.path(), 100, 1, json!({"kind": "model"}));
    for (port, says) in [
        (
            PortDoc::guest("absent"),
            "tasks[1].kind: `model` is served by the guest port `absent`, which cannot be \
             called: no guest handler \"absent\" is registered by the program running this \
             document",
        ),
        (
            PortDoc::host("remote"),
            "tasks[1].kind: `model` is served by the host port `remote`, which cannot be called: \
             a host port needs a peer to serve it",
        ),
    ] {
        let kinds = registry(vec![scale("model", port)], Vec::new());
        let refused = match build(&job, &planned(&job), &kinds, opts()) {
            Ok(_) => panic!("built"),
            Err(e) => e.to_string(),
        };
        assert!(refused.contains(says), "{refused}");
    }

    // Settings the kind's schema refuses are refused at the entry, before any call.
    let kinds = registry(
        vec![scale("model", PortDoc::guest("model"))],
        vec![(
            "model",
            Arc::new(TestPort::new(Answering::Scale)) as Arc<dyn GuestPort>,
        )],
    );
    let bad = one_kernel(
        scratch.path(),
        100,
        1,
        json!({"kind": "model", "factor": 0}),
    );
    let refused = match build(&bad, &planned(&bad), &kinds, opts()) {
        Ok(_) => panic!("built"),
        Err(e) => e.to_string(),
    };
    assert!(
        refused.contains("tasks[1].settings.factor: `model` settings: `factor` must be at least 1"),
        "{refused}"
    );
    // Input that does not fit the declared input_schema is refused when the run is planned.
    let wants = registry(
        vec![declared(
            "model",
            PortDoc::guest("model"),
            Some(json!({"text": "string"})),
            None,
        )],
        vec![(
            "model",
            Arc::new(TestPort::new(Answering::Scale)) as Arc<dyn GuestPort>,
        )],
    );
    let job = one_kernel(scratch.path(), 100, 1, json!({"kind": "model"}));
    let built = build(&job, &planned(&job), &wants, opts()).expect("builds");
    let refused = Runtime::run(built.spec, CancelToken::new())
        .expect_err("refused")
        .to_string();
    assert!(
        refused.contains(
            "`model` takes what its input_schema declares, and its input 0 does not fit: column \
             `text`: declared string, not produced"
        ),
        "{refused}"
    );
}

/// The kernel of a task of `declaration`, its ports bound to a run that never ends, answered by
/// `port`, and an instance of it over `alloc`: the harness of the tests that hand a kernel
/// replies a run would take a long time to make.
fn kernel_of(
    declaration: Declaration,
    port: Arc<dyn GuestPort>,
    alloc: Arc<dyn Allocator>,
) -> (Arc<dyn Kernel>, Box<dyn moruna_kernel::KernelState>) {
    let name = declaration
        .served
        .as_ref()
        .unwrap()
        .port
        .place()
        .unwrap()
        .name()
        .to_string();
    let kind = moruna_runtime::kinds::served::Served::new(declaration).expect("an adapter");
    let mut cx = Context::new(Arc::new(Embedder([(name, port)].into_iter().collect())));
    let doc = entry(json!({"kind": "model", "factor": 2}));
    let stage = kind
        .stage(&Entry::new("kernels[0]", Some(0), &doc), &mut cx)
        .expect("a stage");
    let calls = stage.ports.expect("a served stage calls its port");
    let ports = StagePorts::new(1, calls.routes);
    (calls.bind)(Arc::clone(&ports));
    ports.bind_run(RunPorts {
        cancel: CancelToken::new(),
        waits: StageWaits::new(None),
    });
    let state = stage
        .kernel
        .init(&InitCtx {
            instance: 0,
            device: None,
            alloc,
        })
        .expect("an instance");
    (stage.kernel, state)
}

/// A port that answers every call with what the test last put in it.
struct Swapped(Arc<Mutex<Vec<u8>>>);

impl GuestPort for Swapped {
    fn call(&self, _request: Vec<u8>, _call: &GuestCall) -> Answer {
        Answer::Ok(self.0.lock().unwrap().clone())
    }
}

/// A port that answers every call with the same bytes.
struct Fixed(Vec<u8>);

impl GuestPort for Fixed {
    fn call(&self, _request: Vec<u8>, _call: &GuestCall) -> Answer {
        Answer::Ok(self.0.clone())
    }
}

fn input_batch(rows: i64) -> RecordBatch {
    RecordBatch::try_new(
        support::schema(),
        vec![
            Arc::new(Int64Array::from((0..rows).collect::<Vec<_>>())) as ArrayRef,
            Arc::new(Int64Array::from(
                (0..rows).map(|v| v * 2).collect::<Vec<_>>(),
            )) as ArrayRef,
        ],
    )
    .expect("a batch")
}

/// KD-T14, the reply as an attacker writes it: every corruption and truncation of valid replies
/// fails the call with an error, never a panic; a compressed reply whose buffer says it inflates
/// to a terabyte, and an honest one that inflates past the ceiling the stage's arena allows, are
/// refused before the memory is taken (contracts CT-I13, e.8).
#[test]
fn kd_t14_a_hostile_reply_is_refused() {
    let declaration = || declared("model", PortDoc::guest("model"), None, None);
    let apply = |bytes: Vec<u8>, alloc: Arc<dyn Allocator>| {
        let (kernel, mut state) = kernel_of(declaration(), Arc::new(Fixed(bytes)), alloc);
        let input = Payload::Table(input_batch(16), Tier::Host);
        kernel.apply(state.as_mut(), input)
    };
    // One kernel for the sweep, its port answering whatever the case is.
    let reply = Arc::new(Mutex::new(Vec::new()));
    let (kernel, state) = kernel_of(
        declaration(),
        Arc::new(Swapped(Arc::clone(&reply))),
        Arc::new(FakeAllocator::new()),
    );
    let state = Mutex::new(state);
    let sweep = |bytes: Vec<u8>| {
        *reply.lock().unwrap() = bytes;
        let input = Payload::Table(input_batch(16), Tier::Host);
        kernel.apply(state.lock().unwrap().as_mut(), input)
    };
    // Every fifth case of the sweep: the decode under it is swept whole by contracts CT-T22,
    // and what this proves is that nothing the served kernel does after it panics.
    let mut panicked = Vec::new();
    let refused = std::cell::Cell::new(0usize);
    for (label, valid) in moruna_testkit::ipc::streams() {
        assert!(
            sweep(valid.clone()).is_ok(),
            "{label}: a valid reply is taken"
        );
        let cases = moruna_testkit::ipc::corruptions(&valid, 0x00ca_6e14 ^ valid.len() as u64, 40)
            .step_by(5);
        let counted = |bytes: Vec<u8>| {
            let applied = sweep(bytes);
            if applied.is_err() {
                refused.set(refused.get() + 1);
            }
            applied
        };
        for case in moruna_testkit::ipc::panics(cases, counted) {
            panicked.push(format!("{label}, {case}"));
        }
    }
    assert!(
        panicked.is_empty(),
        "{}",
        moruna_testkit::ipc::tally(&panicked)
    );
    let refused = refused.get();
    assert!(refused > 0, "corrupted replies are refused");

    use moruna_kernel::arrow::ipc::CompressionType;
    use moruna_testkit::ipc::crafted::{says, stream, zstd_frame};
    let one = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, true)]));
    let lying = stream(
        &one,
        CompressionType::ZSTD,
        4,
        &[(4, 0)],
        &[Vec::new(), says(1 << 40, &zstd_frame(&[0u8; 32]))],
    );
    let said = apply(lying, Arc::new(FakeAllocator::new()))
        .expect_err("a lying length")
        .to_string();
    assert!(
        said.contains("`model` (port `model`): the reply is not an Arrow IPC stream Moruna reads"),
        "{said}"
    );
    // Eight MiB of zeros compresses to almost nothing; the stage's arena has four, so half of
    // that is the ceiling.
    let zeros = RecordBatch::try_new(
        Arc::clone(&one),
        vec![Arc::new(Int64Array::from(vec![0i64; 1 << 20])) as ArrayRef],
    )
    .unwrap();
    let bomb = moruna_testkit::ipc::compressed_stream(&[zeros], CompressionType::ZSTD, Some(3));
    assert!(bomb.len() < 64 << 10, "{} bytes", bomb.len());
    let small = Arc::new(FakeAllocator::new().with_limit(Tier::Host, 4 << 20));
    let said = apply(bomb.clone(), small.clone())
        .expect_err("past the ceiling")
        .to_string();
    assert!(
        said.contains("this reader may inflate for one batch"),
        "{said}"
    );
    assert_eq!(small.in_use(Tier::Host), 0, "nothing was taken for it");
    assert!(
        apply(bomb, Arc::new(FakeAllocator::new())).is_ok(),
        "under a roomy arena the same reply is taken"
    );
}

// ------------------------------------------------------------------------------------------
// KD-T15 the converter of the type grammar.
// ------------------------------------------------------------------------------------------

/// Every type the grammar has: each leaf, every unit with and without a zone, decimals, and
/// lists and large lists of each, with the default item name and another, and a nested one.
fn every_type() -> Vec<moruna_types::TypeName> {
    use moruna_types::{TimeUnit as Unit, TypeName as T};
    let mut out = vec![
        T::Null,
        T::Bool,
        T::Int8,
        T::Int16,
        T::Int32,
        T::Int64,
        T::UInt8,
        T::UInt16,
        T::UInt32,
        T::UInt64,
        T::HalfFloat,
        T::Float,
        T::Double,
        T::String,
        T::LargeString,
        T::StringView,
        T::Binary,
        T::LargeBinary,
        T::BinaryView,
        T::Date32,
        T::Date64,
        T::Decimal128(10, 2),
        T::Decimal128(38, -3),
    ];
    for unit in Unit::ALL {
        out.push(T::Timestamp(unit, None));
        out.push(T::Timestamp(unit, Some("UTC".into())));
    }
    let leaves = out.clone();
    for leaf in leaves {
        out.push(T::List("item".into(), Box::new(leaf.clone())));
        out.push(T::LargeList("element".into(), Box::new(leaf)));
    }
    out.push(T::List(
        "item".into(),
        Box::new(T::LargeList("v".into(), Box::new(T::Double))),
    ));
    out
}

/// KD-T15 the_grammar_has_one_reader (SDD 22 JB-I16): the grammar is read once, by
/// `moruna_types::parse`, and `moruna_kernel::declare` converts what it reads. For every type
/// the grammar has, the Arrow type it converts to is named by it again and spelled as it is
/// spelled; two descriptions convert to one Arrow type exactly when they are one description;
/// `parse_type` is the grammar read and converted. A list of non-nullable items is named as the
/// list of nullable ones, and a type outside the grammar is named by nothing and spelled as Arrow
/// prints it. A package's and a served kind's schemas are the same rules, converted.
#[test]
fn kd_t15_the_grammar_has_one_reader() {
    use moruna_kernel::declare::{
        ColumnDecl, SchemaDecl, TypeDecl, data_type, named, parse_type, type_name,
    };
    let every = every_type();
    for ty in &every {
        let dt = data_type(ty);
        assert_eq!(named(&dt).as_ref(), Some(ty), "{ty}");
        assert_eq!(type_name(&dt), ty.to_string());
        assert_eq!(
            parse_type(&ty.to_string()).expect("read"),
            TypeDecl::Exact(dt)
        );
    }
    for a in &every {
        for b in &every {
            assert_eq!(data_type(a) == data_type(b), a == b, "{a} and {b}");
        }
    }
    let strict = DataType::List(Arc::new(Field::new("item", DataType::Int64, false)));
    assert_eq!(type_name(&strict), "list<item: int64>");
    let outside = DataType::Time32(moruna_kernel::arrow::datatypes::TimeUnit::Second);
    assert_eq!(named(&outside), None);
    assert_eq!(type_name(&outside), format!("{outside}"));
    assert_eq!(parse_type(" any ").expect("any"), TypeDecl::Any);
    assert_eq!(
        parse_type("integer").unwrap_err().to_string(),
        "plan: unknown type `integer` in a schema declaration"
    );

    let spelled = json!({"adds": {"c": "float64"}, "drops": ["a"], "changes": {"b": "list<utf8>"}});
    let rules = moruna_job::SchemaRules::parse(&spelled, "s", true).expect("rules");
    let moruna_job::SchemaRules::Relative { adds, changes, .. } = rules else {
        panic!("relative")
    };
    let decl = moruna_package::manifest::schema_decl(&spelled, "s", true).expect("a declaration");
    let converted = |columns: Vec<moruna_job::rules::Column>| -> Vec<ColumnDecl> {
        columns
            .into_iter()
            .map(|c| ColumnDecl {
                ty: TypeDecl::from(&c.ty),
                name: c.name,
                nullable: c.nullable,
            })
            .collect()
    };
    assert_eq!(
        decl,
        SchemaDecl::Relative {
            adds: converted(adds),
            drops: vec!["a".into()],
            changes: converted(changes),
        }
    );
}

// ------------------------------------------------------------------------------------------
// KD-T16 cancel.
// ------------------------------------------------------------------------------------------

/// KD-T16 a_cancel_stops_the_calls (MH 4.10): a run cancelled while its served task waits on the
/// port ends promptly as cancelled, the waiting call is given up (the handler is told), and no
/// call is made after the cancel.
#[test]
fn kd_t16_a_cancel_stops_the_calls() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("kd_t16");
    let port = Arc::new(TestPort::new(Answering::Hold));
    let kinds = registry(
        vec![scale("model", PortDoc::guest("model"))],
        vec![("model", port.clone() as Arc<dyn GuestPort>)],
    );
    let mut job = one_kernel(
        scratch.path(),
        20_000,
        8,
        json!({"kind": "model", "factor": 2}),
    );
    // One worker: the one call that waits is every call there is.
    job.budget.cpu = Some(1.0);
    let built = build(&job, &planned(&job), &kinds, opts()).expect("builds");
    let cancel = CancelToken::new();
    let runner = {
        let cancel = cancel.clone();
        std::thread::spawn(move || Runtime::run(built.spec, cancel))
    };
    let started = Instant::now();
    while port.calls.load(Ordering::SeqCst) == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the task called"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(100));
    port.after.store(true, Ordering::SeqCst);
    let cancelled = Instant::now();
    cancel.cancel();
    let ended = runner.join().expect("the runner");
    let took = cancelled.elapsed();
    let error = ended.expect_err("cancelled").into_parts().error;
    assert!(matches!(error, MorunaError::Cancelled), "{error:?}");
    assert!(took < Duration::from_secs(3), "the cancel took {took:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        port.late.load(Ordering::SeqCst),
        0,
        "a call after the cancel"
    );
}

// ------------------------------------------------------------------------------------------
// KD-T17 kill and resume.
// ------------------------------------------------------------------------------------------

const CHILD: &str = "MORUNA_SERVED_RESUME_CHILD";
const ROWS: u64 = 60_000;
const GROUPS: u64 = 30;
const RUN_HEX: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";

/// What one child is told: where its job reads and writes, the row whose batch its port holds
/// (none for a run that holds nothing), and where its port records the calls it answers.
struct ChildJob {
    input: PathBuf,
    staging: PathBuf,
    outs: [PathBuf; 2],
    hold: Option<i64>,
    calls: PathBuf,
}

impl ChildJob {
    fn encode(&self) -> String {
        [
            self.input.display().to_string(),
            self.staging.display().to_string(),
            self.outs[0].display().to_string(),
            self.outs[1].display().to_string(),
            self.hold
                .map_or_else(|| "none".to_string(), |id| id.to_string()),
            self.calls.display().to_string(),
        ]
        .join(";")
    }

    fn decode(text: &str) -> ChildJob {
        let parts: Vec<&str> = text.split(';').collect();
        ChildJob {
            input: parts[0].into(),
            staging: parts[1].into(),
            outs: [parts[2].into(), parts[3].into()],
            hold: parts[4].parse().ok(),
            calls: parts[5].into(),
        }
    }

    /// The file the port touches when it holds its batch.
    fn holding(&self) -> PathBuf {
        self.staging.with_extension("holding")
    }

    /// The run's manifest (MH 4.7): `staging.dir/moruna-<run id>/manifest.json`.
    fn manifest(&self) -> PathBuf {
        self.staging
            .join(format!("moruna-{RUN_HEX}"))
            .join("manifest.json")
    }

    /// The job: Parquet through a served `scale`, fanned out to two Parquet sinks, resumed from
    /// its manifest when there is one.
    fn job(&self) -> Job {
        let sink = |out: &Path| entry(json!({"kind": "parquet", "url": url(out)}));
        let mut job = Job::new(
            vec![
                Task::source(
                    "in",
                    entry(json!({"kind": "parquet", "url": url(&self.input)})),
                ),
                Task::kernel("scale", entry(json!({"kind": "scale", "factor": 7}))),
                Task::sink("a", sink(&self.outs[0])),
                Task::sink("b", sink(&self.outs[1])),
            ],
            vec![
                Edge::between("in", "scale"),
                Edge::between("scale", "a"),
                Edge::between("scale", "b"),
            ],
        );
        job.id = Some(RUN_HEX.into());
        job.budget.memory_bytes = Some(512 << 20);
        job.budget.cpu = Some(2.0);
        job.staging.dir = Some(self.staging.display().to_string());
        job.staging.limit_bytes = Some(256 << 20);
        job.staging.durable = Some(true);
        job.checkpoint.interval_ms = 500;
        job.resume = Some("auto".into());
        job.profiles_dir = Some(self.staging.join("profiles").display().to_string());
        job
    }
}

/// The child's port: it scales each batch as an embedder would, writes the first and last id of
/// every batch it is asked to answer to `calls`, and, given a row to hold, holds the batch that
/// carries it: it touches the holding file and answers nothing until the process is killed.
struct Recording {
    hold: Option<i64>,
    holding: PathBuf,
    calls: Mutex<std::fs::File>,
}

impl GuestPort for Recording {
    fn call(&self, request: Vec<u8>, _call: &GuestCall) -> Answer {
        let (_, batch) = requested(&request);
        let ids = batch
            .column_by_name("id")
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>().cloned())
            .expect("an Int64 `id`");
        let (first, last) = (ids.value(0), ids.value(ids.len() - 1));
        {
            let mut calls = self.calls.lock().unwrap();
            writeln!(calls, "{first} {last}").expect("the call is recorded");
            calls.flush().expect("the record is written");
        }
        if self.hold.is_some_and(|row| first <= row && row <= last) {
            std::fs::write(&self.holding, format!("{first} {last}")).expect("the holding file");
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        answer(Answering::Scale, &request, None)
    }
}

/// The child's whole life: one job, to its end or to the parent's SIGKILL. A no-op when the
/// variable is unset.
#[test]
fn served_resume_child() {
    let Ok(text) = std::env::var(CHILD) else {
        return;
    };
    let child = ChildJob::decode(&text);
    let calls = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&child.calls)
        .expect("the calls file");
    let port = Recording {
        hold: child.hold,
        holding: child.holding(),
        calls: Mutex::new(calls),
    };
    let kinds = registry(
        vec![scale("scale", PortDoc::guest("scale"))],
        vec![("scale", Arc::new(port) as Arc<dyn GuestPort>)],
    );
    let job = child.job();
    let mut built = build(&job, &planned(&job), &kinds, opts()).expect("builds");
    // A document's Parquet sink closes a file every 64 MiB at least (preamble section 5), and a
    // sink commits what its closed files hold: these close one every 256 KiB, so a manifest
    // written while the port holds its batch says what each sink committed.
    built.spec.sinks = child.outs.iter().map(|out| small_files(out)).collect();
    let components = Components {
        run_id: Some(RunId::from_hex(RUN_HEX).unwrap()),
        ..Components::default()
    };
    match Runtime::run_with(built.spec, CancelToken::new(), components) {
        Ok(report) => println!("CHILD-OK resumed={}", report.resumed),
        Err(error) => panic!("CHILD-FAILED {error}"),
    }
}

/// A Parquet sink at `dir` that closes a file every 256 KiB.
fn small_files(dir: &Path) -> moruna_runtime::SinkSpec {
    let url = url(dir);
    moruna_runtime::SinkSpec::Build(Box::new(move |ctx| {
        moruna_sinks::ParquetSink::new(
            moruna_sinks::ParquetSinkConfig {
                url: url.clone(),
                row_group_bytes: 64 << 10,
                file_bytes: 256 << 10,
                ..Default::default()
            },
            ctx.reactor.clone(),
            ctx.alloc.clone(),
        )
        .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
    }))
}

fn spawn(child: &ChildJob) -> Child {
    Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "--exact",
            "served_resume_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD, child.encode())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("a child process")
}

fn finish(mut child: Child, what: &str) -> String {
    let start = Instant::now();
    let mut done = false;
    while start.elapsed() < Duration::from_secs(180) {
        if !matches!(child.try_wait(), Ok(None)) {
            done = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !done {
        let _ = child.kill();
    }
    let out = child.wait_with_output().expect("the child's output");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        done && out.status.success() && stdout.contains("CHILD-OK"),
        "{what} failed ({:?}, finished in time: {done}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// The batches a child's port was asked to answer, as `(first id, last id)`.
fn calls_in(path: &Path) -> Vec<(i64, i64)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let (first, last) = line.split_once(' ').expect("a recorded call");
            (first.parse().expect("an id"), last.parse().expect("an id"))
        })
        .collect()
}

/// A manifest written at or after `since` (wall clock, nanoseconds) in which every sink has
/// committed something: what a resume has to resume from.
fn committed_since(manifest: &Path, since: u128) -> bool {
    let Some(value) = std::fs::read(manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return false;
    };
    let written = value["written_ns"].as_u64().map_or(0, u128::from);
    let committed = value["committed"].as_array().cloned().unwrap_or_default();
    written >= since && committed.len() == 2 && committed.iter().all(|c| c.is_u64())
}

fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos()
}

/// KD-T17 a_held_run_killed_resumes (MH 4.7, KA5's per-edge resume, KD-I11): a run of a served
/// kind fanned out to two sinks is held by its port at a known batch, the one that carries a
/// chosen row, at a quarter, half and three quarters of the input. Only once the port has said
/// it holds that batch, and a manifest written after that says both sinks committed rows, is the
/// process killed with SIGKILL; a hold that is never reached, a held run that ends, and a held
/// run that writes no such manifest each fail the test saying which. A fresh process resumes it
/// from the manifest, holding nothing: it reports it resumed, its port is asked again for the
/// held batch and for fewer rows than the input (what both sinks committed is not asked for
/// again), and each sink then holds exactly what an uninterrupted run's does, every row once,
/// each the port's transform. Asking the port again for uncommitted batches is exactly once in
/// the sinks.
#[cfg(unix)]
#[test]
fn kd_t17_a_held_run_killed_resumes() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let scratch = Scratch::new("kd_t17");
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, ROWS, GROUPS);
    let child = |name: &str, hold: Option<i64>, attempt: &str| {
        let make = |part: &str| {
            let dir = scratch.path().join(format!("{name}-{part}"));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        };
        ChildJob {
            input: input.clone(),
            staging: make("staging"),
            outs: [make("a"), make("b")],
            hold,
            calls: scratch.path().join(format!("{name}-{attempt}.calls")),
        }
    };
    let whole = child("whole", None, "only");
    finish(spawn(&whole), "the uninterrupted run");
    let want = expected(ROWS, |_, v| v * 7);
    assert_eq!(rows(&whole.outs[0], "scaled"), want);
    assert_eq!(rows(&whole.outs[1], "scaled"), want);

    for quarter in 1..=3i64 {
        let row = ROWS as i64 * quarter / 4 + 17;
        let held = child(&format!("point{quarter}"), Some(row), "held");
        let mut running = spawn(&held);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut held_at: Option<u128> = None;
        let mut ready = false;
        while Instant::now() < deadline && matches!(running.try_wait(), Ok(None)) {
            if held_at.is_none() && held.holding().exists() {
                held_at = Some(now_ns());
            }
            if let Some(since) = held_at
                && committed_since(&held.manifest(), since)
            {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let ended = !matches!(running.try_wait(), Ok(None));
        let _ = running.kill();
        let out = running.wait_with_output().expect("the held child");
        assert!(
            !ended,
            "row {row}: the held run ended before it could be killed ({:?}):\n{}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            held_at.is_some(),
            "row {row}: the run never reached the batch its port holds"
        );
        assert!(
            ready,
            "row {row}: held, and no manifest written after the hold says both sinks committed; \
             the last manifest: {}",
            std::fs::read_to_string(held.manifest())
                .map(|text| text.chars().take(2_000).collect::<String>())
                .unwrap_or_else(|e| format!("none ({e})"))
        );
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(out.status.signal(), Some(9), "row {row}: killed by SIGKILL");
        }
        let held_batch = calls_in(&held.calls)
            .into_iter()
            .find(|&(first, last)| first <= row && row <= last)
            .expect("the held batch was asked for");

        let resumed = ChildJob {
            hold: None,
            calls: scratch.path().join(format!("point{quarter}-resumed.calls")),
            ..held
        };
        let stdout = finish(
            spawn(&resumed),
            &format!("the resume after holding row {row}"),
        );
        assert!(
            stdout.contains("CHILD-OK resumed=true"),
            "row {row}: the run resumed from its manifest:\n{stdout}"
        );
        let again = calls_in(&resumed.calls);
        assert!(
            again
                .iter()
                .any(|&(first, last)| first <= row && row <= last),
            "row {row}: the port was asked again for the held batch {held_batch:?}"
        );
        let asked: i64 = again.iter().map(|(first, last)| last - first + 1).sum();
        assert!(
            asked < ROWS as i64,
            "row {row}: the resumed run asked the port for {asked} rows, and what both sinks \
             committed is not asked for again"
        );
        for sink in &resumed.outs {
            assert_eq!(
                rows(sink, "scaled"),
                want,
                "row {row}: {} holds every row once, each the port's transform",
                sink.display()
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// KD-T18 over the host protocol.
// ------------------------------------------------------------------------------------------

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A host on the protocol's socket that serves `scale` as an embedder does, or says it serves no
/// such port: every `port_call` answered, every message kept.
fn host(
    listener: moruna_runtime::host::transport::Listener,
    serves: bool,
) -> std::thread::JoinHandle<Vec<Value>> {
    std::thread::spawn(move || {
        let conn = listener.accept().expect("moruna connects");
        let (reader, mut writer, _close) = conn.into_parts();
        let mut reader = BufReader::new(reader);
        let mut seen = Vec::new();
        loop {
            let message = match moruna_runtime::host::protocol::read_json_message(&mut reader) {
                Ok(None) => return seen,
                Ok(Some(message)) => message,
                Err(error) => panic!("actual host protocol message: {error}"),
            };
            seen.push(message.clone());
            if message["type"] == "port_call" {
                let reply = if serves && message["port"] == "scale" {
                    let request = B64
                        .decode(message["request"].as_str().expect("a request"))
                        .expect("base64");
                    let Answer::Ok(bytes) = answer(Answering::Scale, &request, None) else {
                        unreachable!()
                    };
                    json!({"type": "port_reply", "v": 1, "call": message["call"], "ok": B64.encode(bytes)})
                } else {
                    json!({"type": "port_reply", "v": 1, "call": message["call"],
                           "error": format!("no port {} is served here", message["port"])})
                };
                let _ = writeln!(writer, "{reply}");
            }
            if message["type"] == "exit" {
                return seen;
            }
        }
    })
}

/// KD-T18 served_over_the_host (MH 4.3, 4.10): `moruna run --kinds` with a file of the
/// embedder's declarations runs a document naming its kind, each batch a `port_call` to the
/// host, each answer the batch's output; a host that does not serve the port fails the task
/// naming the kind, the port and what the host said.
#[cfg(unix)]
#[test]
fn kd_t18_served_over_the_host() {
    use moruna_runtime::host::exit;
    use moruna_runtime::host::transport::{self, Address};
    let _serial = one_run_at_a_time();
    for serves in [true, false] {
        let scratch = Scratch::new("kd_t18");
        let kinds = scratch.path().join("kinds.json");
        std::fs::write(
            &kinds,
            serde_json::to_string(&[scale("scale", PortDoc::host("scale"))]).unwrap(),
        )
        .unwrap();
        let input = scratch.path().join("in.parquet");
        write_parquet(&input, 9_000, 3);
        let out = scratch.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let socket = std::env::temp_dir().join(format!("ms-{}-{serves}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let doc = json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": url(&input)},
            "kernels": [{"kind": "scale", "factor": 4}],
            "sink": {"kind": "parquet", "url": url(&out)},
            "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
            "staging": {"dir": scratch.path().join("staging").to_string_lossy(), "limit_bytes": 1u64 << 30},
            "profiles_dir": scratch.path().join("profiles").to_string_lossy(),
            "report": {"socket": format!("unix://{}", socket.display()),
                       "file": scratch.path().join("report.json").to_string_lossy()},
        });
        let path = scratch.path().join("job.json");
        std::fs::write(&path, doc.to_string()).unwrap();
        let listener = transport::listen(&Address::Unix(socket.clone())).expect("listen");
        let args: Vec<String> = [
            "run",
            &path.display().to_string(),
            "--kinds",
            &kinds.display().to_string(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let runner = std::thread::spawn(move || {
            moruna_runtime::host::cli::main_with(
                &args,
                &Registry::standard(),
                CancelToken::new(),
                &|_| None,
                moruna_runtime::host::session::HEARTBEAT,
            )
        });
        let seen = host(listener, serves).join().expect("the host");
        let code = runner.join().expect("the runner");
        let _ = std::fs::remove_file(&socket);
        let calls: Vec<&Value> = seen.iter().filter(|m| m["type"] == "port_call").collect();
        assert!(!calls.is_empty(), "the task called the host");
        assert!(calls.iter().all(|c| c["port"] == "scale"));
        if serves {
            assert_eq!(code, exit::COMPLETED);
            assert_eq!(rows(&out, "scaled"), expected(9_000, |_, v| v * 4));
        } else {
            assert_eq!(code, exit::KERNEL_ERROR);
            let report = moruna_runtime::host::session::read_report_file(
                &scratch.path().join("report.json"),
            )
            .expect("the report file");
            let said = report["exit"]["diagnostic"].as_str().unwrap_or_default();
            assert!(
                said.contains(
                    "`scale` (port `scale`): the port refused the batch: no port \"scale\" is \
                     served here"
                ),
                "{said}"
            );
        }
    }
}

/// The served kind's request carries the kind, the task's settings and the input port, and its
/// batch reads back as the batch the task was given.
#[test]
fn a_request_is_what_the_port_is_told() {
    let told: Arc<Mutex<Vec<(ServedCall, RecordBatch)>>> = Arc::default();
    struct Records(Arc<Mutex<Vec<(ServedCall, RecordBatch)>>>);
    impl GuestPort for Records {
        fn call(&self, request: Vec<u8>, _call: &GuestCall) -> Answer {
            let (call, batch) = requested(&request);
            let reply = ipc(std::slice::from_ref(&batch), &batch.schema());
            self.0.lock().unwrap().push((call, batch));
            Answer::Ok(reply)
        }
    }
    let (kernel, mut state) = kernel_of(
        declared("model", PortDoc::guest("model"), None, None),
        Arc::new(Records(Arc::clone(&told))),
        Arc::new(FakeAllocator::new()),
    );
    let batch = input_batch(10);
    let out = kernel
        .apply_port(state.as_mut(), 3, Payload::Table(batch.clone(), Tier::Host))
        .expect("answered");
    let Payload::Table(out, _) = out else {
        panic!("a table")
    };
    assert_eq!(out, batch);
    let told = told.lock().unwrap();
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].0.kind, "model");
    assert_eq!(told[0].0.input, 3);
    assert_eq!(
        told[0].0.settings,
        *json!({"factor": 2}).as_object().unwrap()
    );
    assert_eq!(told[0].1, batch);
    assert_eq!(kernel.max_inputs(), u16::MAX);
    assert!(!kernel.declared().is_checkable());
}
