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
    let input = scratch.path().join("in.parquet");
    let out = scratch.path().join("out");
    let kinds = kinds(&input, &out);
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
    let node = Node {
        channel: "channel:engine".to_string(),
        budget_max,
        volumes: None,
    };
    let outcome = session::serve_node(
        over_messages(engine.clone()),
        &node,
        &kinds,
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
