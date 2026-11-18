//! `moruna serve --listen channel:<slot>`, node mode (system design 3A, 3B), on the host: a
//! Unix socket pair stands in for the kernel's channel, each message framed by its length, at
//! most a channel's 64 KiB, so the engine's side is the very code it runs on a node above the
//! channel ([`transport::over_messages`]). The test is the node's `api`: it sends the document
//! with the job's secrets beside it and reads what the engine says until it exits.

#![allow(clippy::result_large_err)]

mod support;

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use moruna_kernel::CancelToken;
use moruna_runtime::host::exit;
use moruna_runtime::host::node::Node;
use moruna_runtime::host::protocol;
use moruna_runtime::host::session::{self, HEARTBEAT};
use moruna_runtime::host::transport::{
    self, Address, CHANNEL_MESSAGE_BYTES, Conn, Messages, over_messages,
};
use moruna_runtime::{SinkSpec, SourceSpec};
use serde_json::{Value, json};
use support::{Scratch, one_run_at_a_time, read_back_rows, write_parquet};

/// What the job's secret is; it must appear in nothing the engine says.
const SECRET: &str = "wJalrXUtnFEMI-K7MDENG-bPxRfiCY-NODE-SECRET";

/// One end of a socket pair as a channel's: each message a `u32` length and its bytes, and an
/// empty frame, which no message is, for the end: what a channel's peer sees when this end
/// closes. (On macOS a socket pair's `shutdown` does not always wake the other end's blocked
/// read, so the end is said in the stream, then the sockets are shut down.)
struct Framed {
    reading: Mutex<UnixStream>,
    writing: Mutex<UnixStream>,
    closer: UnixStream,
    /// The largest message sent from this end.
    largest: AtomicUsize,
    /// Whether this end was closed.
    closed: AtomicBool,
}

impl Framed {
    fn pair() -> (Arc<Framed>, Arc<Framed>) {
        let (a, b) = UnixStream::pair().expect("a socket pair");
        let end = |stream: UnixStream| {
            Arc::new(Framed {
                reading: Mutex::new(stream.try_clone().expect("clone")),
                writing: Mutex::new(stream.try_clone().expect("clone")),
                closer: stream,
                largest: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
            })
        };
        (end(a), end(b))
    }
}

impl Messages for Framed {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        assert!(
            !bytes.is_empty() && bytes.len() <= CHANNEL_MESSAGE_BYTES,
            "a channel's message is 1 to 64 KiB, not {}",
            bytes.len()
        );
        self.largest.fetch_max(bytes.len(), Ordering::Relaxed);
        let mut writing = self.writing.lock().unwrap();
        writing.write_all(&(bytes.len() as u32).to_le_bytes())?;
        writing.write_all(bytes)
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut reading = self.reading.lock().unwrap();
        let mut length = [0u8; 4];
        match reading.read_exact(&mut length) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
            Err(e) => return Err(e),
        }
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 {
            return Ok(0);
        }
        reading.read_exact(&mut buf[..length])?;
        Ok(length)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Ok(mut writing) = self.writing.lock() {
            let _ = writing.write_all(&0u32.to_le_bytes());
        }
        let _ = self.closer.shutdown(std::net::Shutdown::Both);
    }
}

/// The standard kinds; `test.rows`, a Parquet source over `input`; and `test.out`, a Parquet
/// sink into `out`: their files are the test's, so the document names no `file://` path.
fn kinds(input: &Path, out: &Path) -> moruna_runtime::kinds::Registry {
    let mut kinds = moruna_runtime::kinds::Registry::standard();
    let url = format!("file://{}", input.display());
    let rows = support::Handed::new("test.rows", move || {
        let url = url.clone();
        SourceSpec::Build(Box::new(move |ctx| {
            moruna_sources::ParquetSource::new(
                moruna_sources::ParquetSourceConfig {
                    urls: vec![url.clone()],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|source| Arc::new(source) as Arc<dyn moruna_kernel::Source>)
        }))
    });
    let url = format!("file://{}", out.display());
    let sink = support::Handed::new("test.out", move || {
        let url = url.clone();
        SinkSpec::Build(Box::new(move |ctx| {
            moruna_sinks::ParquetSink::new(
                moruna_sinks::ParquetSinkConfig {
                    url: url.clone(),
                    row_group_bytes: 2 << 20,
                    file_bytes: 8 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
        }))
    });
    if let Err(e) = kinds.register_source(Arc::new(rows)) {
        panic!("{e}");
    }
    if let Err(e) = kinds.register_sink(Arc::new(sink)) {
        panic!("{e}");
    }
    kinds
}

/// A document a node takes: no `file://` path, no host socket, no `resume`, no run ID, no
/// `report.socket`; the object store's key a reference to the secret `aws.key`.
fn document(scratch: &Path) -> Value {
    json!({
        "moruna_spec": 1,
        "source": {"kind": "test.rows"},
        "kernels": [{"kind": "std", "name": "select", "args": {"columns": ["id", "value"]}}],
        "sink": {"kind": "test.out"},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "object_store": {"s3": {"endpoint": "https://s3.example.com", "region": "r",
                                "access_key_id": {"secret": "aws.id"},
                                "secret_access_key": {"secret": "aws.key"}}},
        "report": {"file": scratch.join("report-<run_id>.json").to_string_lossy(),
                   "logs": scratch.join("logs-<run_id>").to_string_lossy()},
    })
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// What one served job comes to: the engine's outcome, every message it sent, and the bytes
/// they came in.
struct Served {
    outcome: session::Outcome,
    messages: Vec<Value>,
    bytes: Vec<u8>,
    largest: usize,
}

/// Serves `spec` (the `spec` message's members beside its `type`) in node mode on the channel
/// `channel:engine`, as the node's `api` would send it, and reads until the engine has gone.
fn serve(scratch: &Scratch, spec: Value) -> Served {
    serve_bounded(scratch, spec, None)
}

/// [`serve`] on a node whose jobs may have at most `budget_max` bytes.
fn serve_bounded(scratch: &Scratch, spec: Value, budget_max: Option<u64>) -> Served {
    let node = Node {
        channel: "channel:engine".to_string(),
        budget_max,
        volumes: None,
    };
    serve_on(scratch, spec, Some(node))
}

/// Serves `spec` as [`serve`] does, in node mode on `node`, or off a node with `None`.
fn serve_on(scratch: &Scratch, spec: Value, node: Option<Node>) -> Served {
    let input = scratch.path().join("in.parquet");
    let out = scratch.path().join("out");
    serve_kinds(spec, node, kinds(&input, &out))
}

/// [`serve_on`] with the kinds the engine has.
fn serve_kinds(spec: Value, node: Option<Node>, kinds: moruna_runtime::kinds::Registry) -> Served {
    let (engine, api) = Framed::pair();
    let peer = std::thread::spawn({
        let api = api.clone();
        move || {
            let Conn {
                mut reader,
                mut writer,
                ..
            } = over_messages(api);
            let mut message = spec;
            message["type"] = json!("spec");
            writer
                .write_all(format!("{message}\n").as_bytes())
                .expect("the spec is sent");
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).expect("read to the end");
            bytes
        }
    });
    let options = session::HostOptions {
        cost: None,
        strict: true,
        env: &no_env,
        cancel: CancelToken::new(),
        heartbeat: HEARTBEAT,
        packages: moruna_runtime::packaged::PackageHost::default(),
        capture_process: false,
        events: Default::default(),
        args: Vec::new(),
        pulled_images: None,
    };
    let outcome = match &node {
        Some(node) => session::serve_node(over_messages(engine.clone()), node, &kinds, &options),
        None => session::serve_conn(over_messages(engine.clone()), &kinds, &options),
    };
    // The engine has returned: its end is closed, and the peer reads to the end at once. A
    // peer still reading after a while is a failure that says how the engine closed, not a hang.
    let waited = std::time::Instant::now();
    while !peer.is_finished() {
        assert!(
            waited.elapsed() < std::time::Duration::from_secs(30),
            "the peer never read to the end: the engine closed its end: {}, the outcome {:?}",
            engine.closed.load(Ordering::Relaxed),
            (outcome.code, &outcome.diagnostic),
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let bytes = peer.join().expect("the peer");
    let messages = protocol::json_messages(&bytes).expect("every message reads");
    Served {
        outcome,
        messages,
        bytes,
        largest: engine.largest.load(Ordering::Relaxed),
    }
}

/// A function that declares nothing about its memory and gives back what it is given: what a
/// registered function is to the engine before anything is measured of it.
struct Silent;

impl moruna_kernel::Kernel for Silent {
    fn fingerprint(&self) -> moruna_kernel::Fingerprint {
        moruna_kernel::Fingerprint::compute("moruna-runtime::tests::node::Silent", b"v1")
    }
    fn kind(&self) -> moruna_kernel::KernelKind {
        moruna_kernel::KernelKind::Stateless
    }
    fn accepts(&self) -> moruna_kernel::PayloadSpec {
        moruna_kernel::PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: moruna_kernel::TierPref::Host,
        }
    }
    fn output_schema(
        &self,
        input: &moruna_kernel::SourceSchema,
    ) -> moruna_kernel::Result<moruna_kernel::SourceSchema> {
        Ok(input.clone())
    }
    fn init(
        &self,
        _ctx: &moruna_kernel::InitCtx,
    ) -> moruna_kernel::Result<Box<dyn moruna_kernel::KernelState>> {
        Ok(Box::new(moruna_kernel::NoState))
    }
    fn apply(
        &self,
        _state: &mut dyn moruna_kernel::KernelState,
        input: moruna_kernel::Payload,
    ) -> moruna_kernel::Result<moruna_kernel::Payload> {
        Ok(input)
    }
}

/// The `function` kind on the host, for the test: a function task, `{"function": NAME}`, runs
/// [`Silent`]. On a node the kind runs the registered file; here only the engine's own part is
/// under test: what it does with the facts that came with the job for the function's name.
struct HostFunction;

impl moruna_runtime::kinds::RunnableKind for HostFunction {
    /// The node's own declaration: the test's kind stands in for it, declared the same.
    fn declaration(&self) -> moruna_job::Declaration {
        moruna_runtime::kinds::RunnableKind::declaration(&moruna_runtime::kinds::function::Function)
    }

    fn stage(
        &self,
        entry: &moruna_runtime::kinds::Entry<'_>,
        _cx: &mut moruna_runtime::kinds::Context,
    ) -> moruna_kernel::Result<moruna_runtime::kinds::Stage> {
        let settings: moruna_runtime::kinds::function::Settings = entry.settings()?;
        assert!(!settings.function.is_empty());
        Ok(moruna_runtime::kinds::Stage::of(Arc::new(Silent)))
    }
}

/// The share of its allowance a run left above the arena, from the report's own account of how
/// it sized the arena (`arena sized at A bytes: S of the B byte allowance`).
fn room_share(notes: &[Value]) -> f64 {
    let note = notes
        .iter()
        .filter_map(Value::as_str)
        .find(|note| note.starts_with("arena sized at "))
        .unwrap_or_else(|| panic!("the run says how it sized its arena: {notes:?}"));
    let number_after = |marker: &str| -> f64 {
        let rest = &note[note.find(marker).expect("the note's wording") + marker.len()..];
        rest.split(' ')
            .next()
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("a byte count after `{marker}` in: {note}")) as f64
    };
    1.0 - number_after("arena sized at ") / number_after(" of the ")
}

/// System design 3A, 3E (6.3g): on a node the measured memory of a job's functions comes with
/// the job, in the `spec` message beside its secrets, by each function's registered name. The
/// engine sizes its arena from it, where it sized every registered function from a fixed 4.0
/// before; and its report returns the facts on the function's task.
#[test]
fn a_function_s_measured_memory_comes_with_the_job_and_sizes_the_engine() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("node_measured");
    write_parquet(&scratch.path().join("in.parquet"), 200_000, 4);
    let document = |function: &str| {
        json!({
            "moruna_spec": 1,
            "source": {"kind": "test.rows"},
            "kernels": [{"kind": "function", "function": function}],
            "sink": {"kind": "test.out"},
            "budget": {"memory_bytes": 2u64 << 30, "cpu": 2.0},
            "report": {"file": scratch.path().join("report-<run_id>.json").to_string_lossy(),
                       "logs": scratch.path().join("logs-<run_id>").to_string_lossy()},
        })
    };
    // What the node's registry holds of two functions: one measured to hold twelve bytes of its
    // own per byte of input and a mebibyte whatever its batch, one to hold nothing.
    let hungry = json!({"fixed_bytes": 1048576.0, "per_input_byte": 12.0,
                        "output_per_input_byte": 1.0, "spread": 0.0, "units": 21,
                        "input_bytes_min": 16000, "input_bytes_max": 16000000});
    let lean = json!({"fixed_bytes": 0.0, "per_input_byte": 0.0, "spread": 0.0, "units": 21,
                      "input_bytes_min": 16000, "input_bytes_max": 16000000});
    let ran = |run: &str, function: &str, measured: Option<Value>, by: Option<Value>| {
        let out = scratch.path().join(format!("out-{run}"));
        std::fs::create_dir_all(&out).expect("out");
        let mut kinds = kinds(&scratch.path().join("in.parquet"), &out);
        if let Err(e) = kinds.replace_kernel(Arc::new(HostFunction)) {
            panic!("the test's function kind stands in for the node's: {e}");
        }
        let mut message = json!({"spec": document(function)});
        if let Some(measured) = measured {
            message["measured"] = measured;
        }
        if let Some(by) = by {
            message["measured_by"] = by;
        }
        let node = Node {
            channel: "channel:engine".to_string(),
            budget_max: None,
            volumes: None,
        };
        let served = serve_kinds(message, Some(node), kinds);
        assert_eq!(
            served.outcome.code,
            exit::COMPLETED,
            "{:?}",
            served.outcome.diagnostic
        );
        assert_eq!(read_back_rows(&out), 200_000);
        let report = served
            .messages
            .iter()
            .find(|m| m["type"] == "report")
            .expect("a report")["report"]
            .clone();
        let notes = report["notes"].as_array().expect("notes").clone();
        let task = report["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|t| t["kind"] == "function")
            .expect("the function's task")
            .clone();
        (room_share(&notes), notes, task)
    };
    let said = |notes: &[Value], what: &str| {
        notes
            .iter()
            .filter_map(Value::as_str)
            .any(|note| note.contains(what))
    };

    // Nothing came with the job: the function starts where one nothing is known of starts, two
    // thirds of the allowance above the arena.
    let (unknown, notes, task) = ran("unknown", "hungry", None, None);
    assert!((unknown - 2.0 / 3.0).abs() < 0.02, "{unknown}");
    assert!(!said(&notes, "came with the job") && task.get("measured").is_none());

    // The facts came with the job, by the function's name, beside others' it does not use.
    let both = json!({"hungry": hungry.clone(), "lean": lean.clone(), "other": lean.clone()});
    let (room, notes, task) = ran("hungry", "hungry", Some(both.clone()), None);
    assert!(
        room > 0.8,
        "sized for twelve bytes a byte in flight: {room}"
    );
    assert!(said(
        &notes,
        "the measured memory of the function of stage 1 came with the job"
    ));
    assert!(
        said(
            &notes,
            "stage 1: its function's memory was measured, not assumed"
        ) && said(&notes, "12.00 bytes a byte of input"),
        "{notes:?}"
    );
    // The report returns them on the function's task.
    assert_eq!(task["measured"], hungry);

    let (room, _, task) = ran("lean", "lean", Some(both.clone()), None);
    assert!(
        room < 0.3,
        "a function measured to hold nothing leaves the arena the room: {room}"
    );
    assert_eq!(task["measured"]["per_input_byte"], 0.0);

    // What is not a measurement sizes nothing, and the run says so.
    let mut wrong = hungry.clone();
    wrong["per_input_byte"] = json!(-1.0);
    let (room, notes, task) = ran("wrong", "hungry", Some(json!({"hungry": wrong})), None);
    assert!((room - unknown).abs() < 0.02, "{room}");
    assert!(
        said(&notes, "for the function `hungry` is not a measurement"),
        "{notes:?}"
    );
    assert!(task.get("measured").is_none());

    // The node's own readings of the function, as `api` says whose they are (phase 3): said as
    // the node's, and sized as before on a host (their accounts and planes are the node's).
    let (_, notes, task) = ran(
        "by-node",
        "hungry",
        Some(both),
        Some(json!({"hungry": "node", "lean": "registration"})),
    );
    assert!(
        said(
            &notes,
            "the measured memory of the function of stage 1 (the node's own readings) came with \
             the job"
        ),
        "{notes:?}"
    );
    assert_eq!(task["measured"], hungry);
}

fn exit_of(served: &Served) -> &Value {
    let last = served.messages.last().expect("a message");
    assert_eq!(last["type"], "exit", "the engine's last word is its exit");
    last
}

fn secrets() -> Value {
    json!({"aws.id": "AKIA-NODE-ID", "aws.key": SECRET, "unused": "x"})
}

/// A document with references to the job's secrets runs in node mode: the engine fills in the
/// run's ID and the channel as `report.socket`, resolves the references, and nothing it says
/// carries a secret.
#[test]
fn a_job_on_a_channel_runs_in_node_mode_with_its_secrets() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("node_runs");
    write_parquet(&scratch.path().join("in.parquet"), 4_000, 2);
    std::fs::create_dir_all(scratch.path().join("out")).expect("out");
    let served = serve(
        &scratch,
        json!({"spec": document(scratch.path()), "secrets": secrets()}),
    );
    assert_eq!(
        served.outcome.code,
        exit::COMPLETED,
        "{:?}",
        served.outcome.diagnostic
    );
    assert_eq!(served.messages[0]["type"], "hello");
    assert_eq!(exit_of(&served)["code"], 0);
    assert_eq!(read_back_rows(&scratch.path().join("out")), 4_000);
    // The run's ID the engine made, in the report it sent and the file it wrote.
    let report = served
        .messages
        .iter()
        .find(|m| m["type"] == "report")
        .expect("a report");
    let run_id = report["report"]["run_id"].as_str().expect("a run ID");
    assert_eq!(run_id.len(), 32, "{run_id}");
    assert!(
        scratch
            .path()
            .join(format!("report-{run_id}.json"))
            .exists()
    );
    assert!(
        served
            .outcome
            .notes
            .iter()
            .all(|note| !note.contains("report.socket")),
        "a node's channel is the peer, and no note says otherwise: {:?}",
        served.outcome.notes
    );
    // No secret in what crossed the channel, nor in the report file.
    let said = String::from_utf8_lossy(&served.bytes);
    assert!(!said.contains(SECRET) && !said.contains("AKIA-NODE-ID"));
    let file = std::fs::read_to_string(scratch.path().join(format!("report-{run_id}.json")))
        .expect("the report file");
    assert!(!file.contains(SECRET));
    assert!(served.largest > 0 && served.largest <= CHANNEL_MESSAGE_BYTES);
}

/// A job of the standard `rows` source and `count` sink needs nothing of a node but its budget:
/// it runs in node mode with no file, volume or object store, and its report counts the rows.
#[test]
fn a_rows_to_count_job_runs_in_node_mode_with_nothing_attached() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("node_rows_count");
    let served = serve_bounded(
        &scratch,
        json!({"spec": {
            "moruna_spec": 1,
            "source": {"kind": "rows", "rows": 3_000_000},
            "sink": {"kind": "count"},
            "budget": {"memory_bytes": 1u64 << 30},
            "report": {"file": scratch.path().join("report-<run_id>.json").to_string_lossy(),
                       "logs": scratch.path().join("logs-<run_id>").to_string_lossy()},
        }}),
        Some(1 << 30),
    );
    assert_eq!(
        served.outcome.code,
        exit::COMPLETED,
        "{:?}",
        served.outcome.diagnostic
    );
    assert_eq!(exit_of(&served)["code"], 0);
    let report = served
        .messages
        .iter()
        .find(|m| m["type"] == "report")
        .expect("a report");
    let sinks = report["report"]["sinks"].as_array().expect("its sinks");
    assert_eq!(sinks.len(), 1);
    assert_eq!(sinks[0]["rows"], 3_000_000);
    assert_eq!(sinks[0]["files"], json!([]));
}

/// What node mode refuses is the run's refusal, exit 2, by its field, before anything runs.
#[test]
fn what_a_node_refuses_exits_2_by_its_field() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("node_refuses");
    type Change<'a> = &'a dyn Fn(&mut Value);
    let cases: [(Change<'_>, Value, &str); 5] = [
        (
            &|d| d["source"] = json!({"kind": "parquet", "url": "file:///data/in/"}),
            secrets(),
            "source.url",
        ),
        (
            &|d| d["report"]["socket"] = json!("unix:///run/moruna.sock"),
            secrets(),
            "report.socket",
        ),
        (&|d| d["resume"] = json!("auto"), secrets(), "resume"),
        (
            &|d| d["object_store"]["s3"]["endpoint"] = json!("vsock://2:5000"),
            secrets(),
            "object_store.s3.endpoint",
        ),
        // A reference to a secret the job does not come with.
        (
            &|_| {},
            json!({"aws.id": "AKIA-NODE-ID"}),
            "object_store.s3.secret_access_key",
        ),
    ];
    for (change, secrets, field) in cases {
        let mut doc = document(scratch.path());
        change(&mut doc);
        let served = serve(&scratch, json!({"spec": doc, "secrets": secrets}));
        assert_eq!(served.outcome.code, exit::SPEC_REFUSED, "{field}");
        let diagnostic = exit_of(&served)["diagnostic"]
            .as_str()
            .expect("a diagnostic")
            .to_string();
        assert!(
            diagnostic.contains(&format!(": {field}: ")),
            "{field}: {diagnostic}"
        );
        assert!(!String::from_utf8_lossy(&served.bytes).contains(SECRET));
    }
}

/// Off the Moruna kernel there is no channel to serve on: `moruna serve --listen channel:SLOT`
/// is refused, exit 2, saying so.
#[test]
fn off_the_kernel_a_channel_is_refused() {
    let address = Address::parse("channel:engine").expect("a channel's address");
    assert_eq!(address.to_string(), "channel:engine");
    assert_eq!(
        transport::connect(&address).err().map(|e| e.kind()),
        Some(io::ErrorKind::Unsupported)
    );
    let input = Path::new("/nonexistent/in.parquet");
    let outcome = session::serve(
        &address,
        &kinds(input, input),
        &session::HostOptions {
            cost: None,
            strict: true,
            env: &no_env,
            cancel: CancelToken::new(),
            heartbeat: HEARTBEAT,
            packages: moruna_runtime::packaged::PackageHost::default(),
            capture_process: false,
            events: Default::default(),
            args: Vec::new(),
            pulled_images: None,
        },
    );
    assert_eq!(outcome.code, exit::SPEC_REFUSED);
    let diagnostic = outcome.diagnostic.unwrap_or_default();
    assert!(
        diagnostic.contains("listen") && diagnostic.contains("not on it"),
        "{diagnostic}"
    );
}

/// `moruna check-job` is the check a node runs for `validate`: with the names of the job's
/// secrets (never their values) and the node's channel, it refuses what the run would, every
/// refusal by its field, the kinds' first, then the secrets', then node mode's.
#[test]
fn check_job_refuses_what_a_node_would() {
    let scratch = Scratch::new("node_check");
    let mut doc = document(scratch.path());
    doc["sink"] = json!({"kind": "parquet", "url": "file:///data/out/"});
    doc["resume"] = json!("auto");
    let path = scratch.path().join("job.json");
    std::fs::write(&path, doc.to_string()).expect("the document");
    let kinds = kinds(Path::new("/in.parquet"), Path::new("/out"));
    let check = |secrets: &[&str], node: Option<&str>| {
        let node = node.map(Node::on);
        let node = node.as_ref();
        let mut said = Vec::new();
        let code =
            moruna_runtime::host::cli::check_job_with(&path, &kinds, secrets, node, &mut said);
        let said = String::from_utf8(said).expect("text");
        let fields: Vec<String> = said
            .lines()
            .map(|line| line.split(": ").next().unwrap_or("").to_string())
            .collect();
        (code, fields)
    };
    assert_eq!(
        check(&["aws.id"], Some("channel:engine")),
        (
            exit::SPEC_REFUSED,
            vec![
                "object_store.s3.secret_access_key".to_string(),
                "sink.url".to_string(),
                "resume".to_string(),
            ]
        )
    );
    // Without the node's rules, only the secret not given.
    assert_eq!(
        check(&["aws.id"], None),
        (
            exit::SPEC_REFUSED,
            vec!["object_store.s3.secret_access_key".to_string()]
        )
    );
    assert_eq!(
        check(&["aws.id", "aws.key"], None),
        (exit::COMPLETED, vec![])
    );
    // Through the command line, the same.
    let argv: Vec<String> = [
        "check-job",
        &path.display().to_string(),
        "--secret",
        "aws.id",
        "--secret",
        "aws.key",
        "--node",
        "channel:engine",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    let code =
        moruna_runtime::host::cli::main_with(&argv, &kinds, CancelToken::new(), &no_env, HEARTBEAT);
    assert_eq!(code, exit::SPEC_REFUSED, "sink.url and resume");
}

/// A node bounds a job's budget: more than the node allows is refused by its field before
/// anything runs, and a document that names none runs with all of it.
#[test]
fn a_nodes_budget_is_the_most_a_job_may_have_and_its_default() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("node_budget");
    write_parquet(&scratch.path().join("in.parquet"), 4_000, 2);
    std::fs::create_dir_all(scratch.path().join("out")).expect("out");
    let max = 1u64 << 30;
    let mut over = document(scratch.path());
    over["budget"]["memory_bytes"] = json!(max + 4096);
    let served = serve_bounded(
        &scratch,
        json!({"spec": over, "secrets": secrets()}),
        Some(max),
    );
    assert_eq!(served.outcome.code, exit::SPEC_REFUSED);
    let diagnostic = exit_of(&served)["diagnostic"].as_str().unwrap_or_default();
    assert!(
        diagnostic.contains(": budget.memory_bytes: "),
        "{diagnostic}"
    );
    let mut none = document(scratch.path());
    none["budget"] = json!({"cpu": 2.0});
    let served = serve_bounded(
        &scratch,
        json!({"spec": none, "secrets": secrets()}),
        Some(max),
    );
    assert_eq!(
        served.outcome.code,
        exit::COMPLETED,
        "{:?}",
        served.outcome.diagnostic
    );
    let report = served
        .messages
        .iter()
        .find(|m| m["type"] == "report")
        .expect("a report");
    // The engine ran within the arena's share of its whole budget, never the whole.
    let arena = moruna_runtime::host::node::sizes(max, false).arena;
    assert_eq!(report["report"]["host"]["memory_limit_bytes"], json!(arena));
}

/// A job with no budget gets the node's job bound (system design 3A, item 3.2c): the check
/// answers the bound and the job runs exactly as one whose budget is the bound, its report
/// saying the bound as its budget (`budget.memory_bytes`) and the arena's share of it as its
/// memory limit (1.3c's 326 MiB of a 2 GiB node's bound). A document's own budget is kept, and
/// one larger than the bound is refused by its field.
#[test]
fn a_job_with_no_budget_runs_with_the_nodes_job_bound() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("node_default_budget");
    write_parquet(&scratch.path().join("in.parquet"), 4_000, 2);
    std::fs::create_dir_all(scratch.path().join("out")).expect("out");
    let bound = 590_229_504u64; // a 2 GiB node's job bound, as 1.3c measured it
    let node = Node {
        channel: "channel:engine".to_string(),
        budget_max: Some(bound),
        volumes: None,
    };
    let kinds = kinds(Path::new("/in.parquet"), Path::new("/out"));
    let checked = |document: Value| {
        let (engine, api) = Framed::pair();
        let line = format!(
            "{}\n",
            json!({"type": "check", "spec": document, "secrets": ["aws.id", "aws.key"]})
        );
        let peer = std::thread::spawn(move || {
            let Conn {
                mut reader,
                mut writer,
                ..
            } = over_messages(api);
            writer
                .write_all(line.as_bytes())
                .expect("the check is sent");
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).expect("read to the end");
            bytes
        });
        let code =
            moruna_runtime::host::cli::check_conn(over_messages(engine), &kinds, Some(&node));
        let text = String::from_utf8(peer.join().expect("the peer")).expect("text");
        let answer: Value = serde_json::from_str(text.trim_end()).expect("JSON");
        (code, answer["memory_bytes"].clone())
    };
    let ran = |document: Value| {
        let served = serve_bounded(
            &scratch,
            json!({"spec": document, "secrets": secrets()}),
            Some(bound),
        );
        assert_eq!(
            served.outcome.code,
            exit::COMPLETED,
            "{:?}",
            served.outcome.diagnostic
        );
        let report = served
            .messages
            .iter()
            .find(|m| m["type"] == "report")
            .expect("a report");
        let report = &report["report"];
        (
            report["budget"]["memory_bytes"].clone(),
            report["host"]["memory_limit_bytes"].clone(),
        )
    };
    let with_budget = |memory: Option<u64>| {
        let mut document = document(scratch.path());
        document["budget"] = match memory {
            Some(bytes) => json!({"memory_bytes": bytes, "cpu": 2.0}),
            None => json!({"cpu": 2.0}),
        };
        document
    };
    let arena = |whole: u64| json!(moruna_runtime::host::node::sizes(whole, false).arena);
    // No budget: the bound, checked and run as a budget of the bound is.
    assert_eq!(checked(with_budget(None)), (exit::COMPLETED, json!(bound)));
    assert_eq!(
        checked(with_budget(Some(bound))),
        (exit::COMPLETED, json!(bound))
    );
    let none = ran(with_budget(None));
    assert_eq!(none, (json!(bound), arena(bound)));
    assert_eq!(none, ran(with_budget(Some(bound))));
    // 1.3c's figures: 326 MiB of arena and 108 MiB of scratch beside the engine's own 128 MiB.
    let sizes = moruna_runtime::host::node::sizes(bound, false);
    assert_eq!((sizes.arena >> 20, sizes.scratch >> 20), (326, 108));
    // A document's own budget is kept.
    let own = 512u64 << 20;
    assert_eq!(
        checked(with_budget(Some(own))),
        (exit::COMPLETED, json!(own))
    );
    assert_eq!(ran(with_budget(Some(own))), (json!(own), arena(own)));
    // One larger than the bound is refused by its field.
    let (code, _) = checked(with_budget(Some(bound + 4096)));
    assert_eq!(code, exit::SPEC_REFUSED);
}

/// The check a node runs for `validate`, over a channel (`moruna check-job --listen`): one
/// `check` line in, the document and its secrets' names; one `checked` line out, every refusal
/// by field and the budget the job would run with; exit 0 or 2.
#[test]
fn a_check_on_a_channel_answers_its_refusals_and_the_budget() {
    let scratch = Scratch::new("node_check_on");
    let kinds = kinds(Path::new("/in.parquet"), Path::new("/out"));
    let max = 1u64 << 30;
    let node = Node {
        channel: "channel:engine".to_string(),
        budget_max: Some(max),
        volumes: None,
    };
    let check = |line: String| {
        let (engine, api) = Framed::pair();
        let peer = std::thread::spawn(move || {
            let Conn {
                mut reader,
                mut writer,
                ..
            } = over_messages(api);
            writer
                .write_all(line.as_bytes())
                .expect("the check is sent");
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).expect("read to the end");
            bytes
        });
        let code =
            moruna_runtime::host::cli::check_conn(over_messages(engine), &kinds, Some(&node));
        let bytes = peer.join().expect("the peer");
        let text = String::from_utf8(bytes).expect("text");
        assert_eq!(text.lines().count(), 1, "one answer: {text}");
        let answer: Value = serde_json::from_str(text.trim_end()).expect("JSON");
        (code, answer)
    };
    let request = |document: Value, secrets: Value| {
        format!(
            "{}\n",
            json!({"type": "check", "spec": document, "secrets": secrets})
        )
    };
    // Sound, with every secret named: ok, and the document's own budget.
    let (code, answer) = check(request(
        document(scratch.path()),
        json!(["aws.id", "aws.key"]),
    ));
    assert_eq!(code, exit::COMPLETED);
    assert_eq!(
        answer,
        json!({"type": "checked", "ok": true, "refusals": [], "memory_bytes": max,
               "hosts": ["s3.example.com"], "volumes": [], "functions": []})
    );
    // A document's volumes and hosts, as the check answers them for node-admission to keep.
    let mut disks = document(scratch.path());
    disks["source"] = json!({"kind": "parquet", "url": "disk://vol-in/taxi/"});
    disks["sink"] = json!({"kind": "parquet", "url": "disk://vol-out/rows/"});
    disks["object_store"]["azure"] = json!({"account": "acct"});
    let (code, answer) = check(request(disks, json!(["aws.id", "aws.key"])));
    assert_eq!(code, exit::COMPLETED, "{answer}");
    assert_eq!(
        answer["hosts"],
        json!(["acct.blob.core.windows.net", "s3.example.com"])
    );
    assert_eq!(
        answer["volumes"],
        json!([{"serial": "vol-in", "write": false}, {"serial": "vol-out", "write": true}])
    );
    // No budget: all the node allows.
    let mut none = document(scratch.path());
    none["budget"] = json!({"cpu": 2.0});
    let (code, answer) = check(request(none, json!(["aws.id", "aws.key"])));
    assert_eq!(
        (code, &answer["memory_bytes"]),
        (exit::COMPLETED, &json!(max))
    );
    // Too much budget, a secret not named and a `file://` sink: every refusal, by field.
    let mut bad = document(scratch.path());
    bad["budget"]["memory_bytes"] = json!(max * 2);
    bad["sink"] = json!({"kind": "parquet", "url": "file:///out/"});
    let (code, answer) = check(request(bad, json!(["aws.id"])));
    assert_eq!(code, exit::SPEC_REFUSED);
    assert_eq!(answer["ok"], json!(false));
    let fields: Vec<&str> = answer["refusals"]
        .as_array()
        .expect("refusals")
        .iter()
        .map(|r| r["field"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        fields,
        [
            "object_store.s3.secret_access_key",
            "sink.url",
            "budget.memory_bytes"
        ]
    );
    assert_eq!(answer["memory_bytes"], json!(max * 2));
    // A version 2 document, as base64.
    let job = moruna_job::Job::from_v1(
        &moruna_job::JobSpec::from_value(document(scratch.path())).expect("a document"),
    );
    let encoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(job.to_flatbuffer().expect("encodes"))
    };
    let (code, answer) = check(request(json!(encoded), json!(["aws.id", "aws.key"])));
    assert_eq!((code, &answer["ok"]), (exit::COMPLETED, &json!(true)));
    // What is not a check, and a document that is not one, are refused by `spec`.
    for line in [
        "{\"type\":\"spec\",\"spec\":{}}\n".to_string(),
        "not json\n".to_string(),
        request(json!({"moruna_spec": 1}), json!([])),
        request(json!("!!"), json!([])),
    ] {
        let (code, answer) = check(line.clone());
        assert_eq!(code, exit::SPEC_REFUSED, "{line}");
        assert_eq!(answer["ok"], json!(false));
        assert_eq!(answer["memory_bytes"], Value::Null);
        assert_eq!(
            (&answer["hosts"], &answer["volumes"], &answer["functions"]),
            (&json!([]), &json!([]), &json!([]))
        );
    }
    let (code, answer) = check(request(document(scratch.path()), json!(["-bad"])));
    assert_eq!(
        (code, &answer["refusals"][0]["field"]),
        (exit::SPEC_REFUSED, &json!("secrets"))
    );
}

/// `moruna run` reads its document from a file and is given no secrets: a reference in it is
/// refused by its field, as a reference to a secret not given is anywhere.
#[test]
fn a_reference_in_a_files_document_is_refused() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("node_file_refs");
    let path = scratch.path().join("job.json");
    std::fs::write(&path, document(scratch.path()).to_string()).expect("the document");
    let outcome = session::run_file(
        &path,
        None,
        &kinds(Path::new("/in.parquet"), Path::new("/out")),
        &session::HostOptions {
            cost: None,
            strict: true,
            env: &no_env,
            cancel: CancelToken::new(),
            heartbeat: HEARTBEAT,
            packages: moruna_runtime::packaged::PackageHost::default(),
            capture_process: false,
            events: Default::default(),
            args: Vec::new(),
            pulled_images: None,
        },
    );
    assert_eq!(outcome.code, exit::SPEC_REFUSED);
    let diagnostic = outcome.diagnostic.unwrap_or_default();
    assert!(
        diagnostic.contains(": object_store.s3.access_key_id: ")
            && diagnostic.contains("no secret named `aws.id`"),
        "{diagnostic}"
    );
}

/// An S3 API on this machine's loopback that refuses every request with `403 Forbidden` and says
/// back the key ID and the secret key it was given, as a careless server might: its port.
fn forbidding_store() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("its address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(1) => head.push(byte[0]),
                    _ => break,
                }
            }
            let body = format!(
                "<Error><Code>InvalidAccessKeyId</Code><AWSAccessKeyId>AKIA-NODE-ID</AWSAccessKeyId>\
                 <SignatureProvided>{SECRET}</SignatureProvided></Error>"
            );
            let _ = write!(
                stream,
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

/// Every file under `dir`, read as text.
fn files_under(dir: &Path) -> Vec<(std::path::PathBuf, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("a directory").flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            found.push((path, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    found
}

/// A store that refuses the job's credentials (item 3.5): the run fails with the store's error
/// naming the secrets the credentials came from, and no credential's value is in anything the
/// engine says (its events, its report, its exit) or writes (its report file, its logs), though
/// the store said them back.
#[test]
fn a_store_that_refuses_the_credentials_names_the_secrets_never_their_values() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("store_refuses");
    let port = forbidding_store();
    let document = json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": "s3://taxi/yellow_tripdata_2024-01.parquet"},
        "sink": {"kind": "count"},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "object_store": {"s3": {"endpoint": format!("http://127.0.0.1:{port}"),
                                "region": "us-east-1",
                                "access_key_id": {"secret": "aws.id"},
                                "secret_access_key": {"secret": "aws.key"}},
                         "allow_http": true},
        "report": {"file": scratch.path().join("report-<run_id>.json").to_string_lossy(),
                   "logs": scratch.path().join("logs-<run_id>").to_string_lossy()},
    });
    let served = serve_on(
        &scratch,
        json!({"spec": document, "secrets": secrets()}),
        None,
    );
    assert_ne!(served.outcome.code, exit::COMPLETED);
    assert_ne!(
        served.outcome.code,
        exit::SPEC_REFUSED,
        "the document is sound"
    );
    let diagnostic = exit_of(&served)["diagnostic"]
        .as_str()
        .expect("a diagnostic")
        .to_string();
    assert!(
        diagnostic.contains("the store refused the credentials this job gave it")
            && diagnostic.contains("secret `aws.id` (object_store.s3.access_key_id)")
            && diagnostic.contains("secret `aws.key` (object_store.s3.secret_access_key)"),
        "{diagnostic}"
    );
    let said = String::from_utf8_lossy(&served.bytes);
    assert!(
        !said.contains(SECRET) && !said.contains("AKIA-NODE-ID"),
        "a credential crossed the channel: {said}"
    );
    assert!(
        said.contains("<secret `aws.key`>"),
        "what the store said back was read again: {said}"
    );
    let written = files_under(scratch.path());
    assert!(!written.is_empty(), "the run wrote its report and logs");
    for (path, text) in written {
        assert!(
            !text.contains(SECRET) && !text.contains("AKIA-NODE-ID"),
            "a credential in {}",
            path.display()
        );
    }
}
