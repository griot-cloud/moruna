//! A stage's host calls (MH 4.10) over real components: a `.wasm` package whose `call` names a
//! port the job document lists for its stage, run by the same code `moruna run` runs, with a peer
//! on the host protocol's socket that serves the host ports, holds a call, lets a deadline pass,
//! cancels, and is killed with its run and resumed.
//!
//! The component (`moruna_wasm::testing::caller_component`) counts its batches, calls its port
//! with the count and the batch, and answers with what the port answered; the stub ports here
//! echo the batch, so a run's output is its input and every batch went through the port.

#![allow(clippy::result_large_err)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use moruna_kernel::{Answer, CancelToken, GuestCall, GuestPort};
use moruna_runtime::host::cli;
use moruna_runtime::host::exit;
use moruna_runtime::host::session::{HEARTBEAT, read_report_file};
use moruna_runtime::host::transport::{self, Address};
use moruna_runtime::job::{BuildOptions, JobSpec, KernelDoc, KernelLoader, LoadedKernel, build};
use moruna_wasm::testing::{caller_component, manifest, package_at};
use serde_json::{Value, json};
use support::{Scratch, one_run_at_a_time, read_back_rows, write_parquet};

const RUN_ID: &str = "0123456789abcdef0123456789abcdef";
const CHILD: &str = "MORUNA_PORTS_CHILD";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The guest handler `echo`: the batch after the count.
struct Echo;

impl GuestPort for Echo {
    fn call(&self, request: Vec<u8>, _call: &GuestCall) -> Answer {
        Answer::Ok(request[8..].to_vec())
    }
}

/// A loader with no kernels and one guest handler, `echo`: what a program embedding Moruna
/// registers.
struct Embedding;

impl KernelLoader for Embedding {
    fn load(&self, index: usize, _doc: &KernelDoc) -> moruna_kernel::Result<LoadedKernel> {
        Err(moruna_kernel::MorunaError::Plan(format!(
            "kernels[{index}]: no kernels here"
        )))
    }

    fn guest_port(&self, name: &str) -> Option<Arc<dyn GuestPort>> {
        (name == "echo").then(|| Arc::new(Echo) as Arc<dyn GuestPort>)
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// A short socket path: a Unix socket path is limited to about 100 bytes.
fn socket_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("mp-{}-{name}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// The package of the caller component, calling `port`, under `dir`.
fn caller_package(dir: &Path, port: &str, stateful: bool) -> (String, String) {
    let m = manifest("caller", stateful, true);
    let package = package_at(dir, &caller_component(&m.to_json(), port, stateful), &m);
    (
        package.path.display().to_string(),
        package.fingerprint_string(),
    )
}

/// A job over `rows` rows in `groups` row groups (one split, and so at most one morsel, each),
/// with the caller package listing `ports`.
fn document(
    scratch: &Path,
    rows: u64,
    groups: u64,
    package: (String, String),
    ports: Value,
) -> Value {
    let input = scratch.join("in.parquet");
    if !input.exists() {
        write_parquet(&input, rows, groups);
    }
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    let out = scratch.join("out");
    std::fs::create_dir_all(&out).expect("out");
    json!({
        "moruna_spec": 1,
        "run_id": RUN_ID,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": [{"kind": "package", "path": package.0, "fingerprint": package.1, "ports": ports}],
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display())},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "staging": {"dir": staging.to_string_lossy(), "limit_bytes": 1u64 << 30},
        "profiles_dir": scratch.join("profiles").to_string_lossy(),
        "report": {"file": scratch.join("report-<run_id>.json").to_string_lossy()},
    })
}

fn write_doc(scratch: &Path, doc: &Value) -> PathBuf {
    let path = scratch.join("job.json");
    std::fs::write(&path, serde_json::to_string_pretty(doc).expect("json")).expect("write");
    path
}

fn run_cli(path: &Path, loader: &dyn KernelLoader, cancel: CancelToken) -> i32 {
    let args = vec!["run".to_string(), path.to_string_lossy().into_owned()];
    cli::main_with(&args, loader, cancel, &no_env, HEARTBEAT)
}

fn report_file(scratch: &Path) -> Value {
    read_report_file(&scratch.join(format!("report-{RUN_ID}.json"))).expect("the report file")
}

/// What the stub host does with the `n`th `port_call` (from 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    /// Answer with the batch after the count.
    Echo,
    /// Answer nothing until told.
    Hold,
}

/// The peer: every message Moruna sent, with when it arrived, and the stub host ports.
struct Host {
    messages: Arc<Mutex<Vec<(Instant, Value)>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Host {
    /// Accept Moruna's connection on `listener` and serve its port calls by `policy`.
    fn serve(
        listener: transport::Listener,
        policy: impl Fn(usize, &Value) -> Reply + Send + 'static,
    ) -> Host {
        let conn = listener.accept().expect("moruna connects");
        let (reader, writer, _close) = conn.into_parts();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(writer));
        let reader = {
            let (messages, writer) = (Arc::clone(&messages), Arc::clone(&writer));
            std::thread::spawn(move || {
                let mut reader = BufReader::new(reader);
                let mut calls = 0usize;
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let message: Value = serde_json::from_str(&line).expect("a JSON line");
                    messages
                        .lock()
                        .unwrap()
                        .push((Instant::now(), message.clone()));
                    if message["type"] == "port_call" {
                        calls += 1;
                        if policy(calls, &message) == Reply::Echo {
                            let request = B64
                                .decode(message["request"].as_str().expect("a request"))
                                .expect("base64");
                            let reply = json!({
                                "type": "port_reply", "v": 1, "call": message["call"],
                                "ok": B64.encode(&request[8..]),
                            });
                            let mut w = writer.lock().unwrap();
                            let _ = writeln!(w, "{reply}");
                        }
                    }
                    if message["type"] == "exit" {
                        return;
                    }
                }
            })
        };
        Host {
            messages,
            writer,
            reader: Some(reader),
        }
    }

    fn send(&self, message: Value) {
        let mut w = self.writer.lock().unwrap();
        writeln!(w, "{message}").expect("the peer writes");
    }

    fn of_type(&self, kind: &str) -> Vec<Value> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, m)| m["type"] == kind)
            .map(|(_, m)| m.clone())
            .collect()
    }

    fn timed(&self) -> Vec<(Instant, Value)> {
        self.messages.lock().unwrap().clone()
    }

    /// Wait until `test` holds of the messages so far, or `within` passes.
    fn wait_for(&self, within: Duration, test: impl Fn(&[(Instant, Value)]) -> bool) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if test(&self.messages.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn join(mut self) -> Vec<(Instant, Value)> {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.timed()
    }
}

/// The count a `port_call` carried: its batch's number in the stage's state.
fn count_of(call: &Value) -> u64 {
    let request = B64
        .decode(call["request"].as_str().expect("a request"))
        .expect("base64");
    u64::from_le_bytes(request[..8].try_into().expect("eight bytes"))
}

fn events(messages: &[(Instant, Value)]) -> Vec<Value> {
    messages
        .iter()
        .filter(|(_, m)| m["type"] == "event")
        .map(|(_, m)| m.clone())
        .collect()
}

/// Run `doc` with a stub host on its socket answering by `policy`: the exit code and the host.
fn converse(
    scratch: &Path,
    name: &str,
    mut doc: Value,
    policy: impl Fn(usize, &Value) -> Reply + Send + 'static,
) -> (i32, Vec<(Instant, Value)>) {
    let socket = socket_path(name);
    doc["report"]["socket"] = json!(format!("unix://{}", socket.display()));
    let path = write_doc(scratch, &doc);
    let listener = transport::listen(&Address::Unix(socket.clone())).expect("listen");
    let runner = std::thread::spawn(move || run_cli(&path, &Embedding, CancelToken::new()));
    let host = Host::serve(listener, policy);
    let code = runner.join().expect("the runner");
    let messages = host.join();
    let _ = std::fs::remove_file(&socket);
    (code, messages)
}

/// PO-T1, O1's first gate (MH 4.10): a test component calls a stub host port from a job. Every
/// batch goes through the port and back, each call carries the stage and the host's name for the
/// port, and while a call waits the stage's task says so, then says it applies again.
#[test]
fn po_t1_a_component_calls_a_stub_host_port_from_a_job() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("po_t1");
    let rows = 40_000;
    let package = caller_package(scratch.path(), "stub", false);
    let doc = document(
        scratch.path(),
        rows,
        8,
        package,
        json!({"stub": {"host": "model-stub"}}),
    );
    let (code, messages) = converse(scratch.path(), "t1", doc, |_, _| Reply::Echo);
    assert_eq!(
        code,
        exit::COMPLETED,
        "{:?}",
        report_file(scratch.path())["exit"]
    );
    assert_eq!(read_back_rows(&scratch.path().join("out")), rows);
    let calls: Vec<&Value> = messages
        .iter()
        .map(|(_, m)| m)
        .filter(|m| m["type"] == "port_call")
        .collect();
    assert!(!calls.is_empty());
    for (i, call) in calls.iter().enumerate() {
        assert_eq!(call["v"], 1);
        assert_eq!(call["stage"], 1);
        assert_eq!(call["port"], "model-stub");
        assert_eq!(call["deadline_ms"], 30 * 60 * 1000);
        assert_eq!(
            count_of(call),
            i as u64 + 1,
            "one instance, counting its batches"
        );
    }
    let events = events(&messages);
    let waiting = events
        .iter()
        .position(|e| e["kind"] == "task_waiting" && e["reason"] == "port_call")
        .expect("the task waited on its port");
    assert_eq!(events[waiting]["task"], "kernel-1");
    assert_eq!(events[waiting]["port"], "stub");
    assert_eq!(events[waiting]["blocking"], json!([]));
    let resumed = events[waiting..]
        .iter()
        .find(|e| e["kind"] == "task_phase" && e["task"] == "kernel-1")
        .expect("the task worked again");
    assert!(
        ["calibrating", "applying", "draining"].contains(&resumed["phase"].as_str().unwrap()),
        "{resumed}"
    );
    let end = events
        .iter()
        .rev()
        .find(|e| e["task"] == "kernel-1")
        .unwrap();
    assert_eq!(end["kind"], "task_completed");
    assert_eq!(end["outcome"], "succeeded");
}

/// PO-T2 (MH 4.10): a guest port is answered inside the machine by the handler the embedding
/// registered, with no peer at all.
#[test]
fn po_t2_a_guest_port_is_answered_by_the_embedding() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("po_t2");
    let rows = 20_000;
    let package = caller_package(scratch.path(), "read", false);
    let doc = document(
        scratch.path(),
        rows,
        4,
        package,
        json!({"read": {"guest": "echo", "deadline_ms": 60_000}}),
    );
    let path = write_doc(scratch.path(), &doc);
    assert_eq!(
        run_cli(&path, &Embedding, CancelToken::new()),
        exit::COMPLETED
    );
    assert_eq!(read_back_rows(&scratch.path().join("out")), rows);
}

/// PO-T3 (MH 4.10): a call to a name the stage's entry does not list is refused by name, nothing
/// reaches the host, and the stage fails with the refusal.
#[test]
fn po_t3_an_unlisted_port_is_refused_by_name() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("po_t3");
    let package = caller_package(scratch.path(), "model", false);
    let doc = document(
        scratch.path(),
        20_000,
        4,
        package,
        json!({"stub": {"host": "stub"}}),
    );
    let (code, messages) = converse(scratch.path(), "t3", doc, |_, _| Reply::Echo);
    assert_eq!(code, exit::KERNEL_ERROR);
    let said = report_file(scratch.path())["exit"]["diagnostic"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        said.contains("port `model` is not listed for this stage (it lists stub)"),
        "{said}"
    );
    assert!(
        messages.iter().all(|(_, m)| m["type"] != "port_call"),
        "nothing was called"
    );
    let failed = events(&messages)
        .into_iter()
        .find(|e| e["kind"] == "task_failed")
        .expect("a task failed");
    assert_eq!(failed["task"], "kernel-1");
    assert_eq!(failed["code"], "kernel_error");
}

/// PO-T4 (MH 4.10): a call past its port's deadline fails the stage, naming the port and the
/// deadline, and the host is told the call was given up.
#[test]
fn po_t4_a_call_past_its_deadline_fails_the_stage() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("po_t4");
    let package = caller_package(scratch.path(), "stub", false);
    let doc = document(
        scratch.path(),
        20_000,
        4,
        package,
        json!({"stub": {"host": "stub", "deadline_ms": 300}}),
    );
    let (code, messages) = converse(scratch.path(), "t4", doc, |_, _| Reply::Hold);
    assert_eq!(code, exit::KERNEL_ERROR);
    let said = report_file(scratch.path())["exit"]["diagnostic"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        said.contains("port `stub`: no answer within its deadline of 300 ms"),
        "{said}"
    );
    let call = messages
        .iter()
        .find(|(_, m)| m["type"] == "port_call")
        .expect("the call")
        .1
        .clone();
    let cancel = messages
        .iter()
        .find(|(_, m)| m["type"] == "port_cancel")
        .expect("the call was given up")
        .1
        .clone();
    assert_eq!(cancel["call"], call["call"]);
    assert_eq!(cancel["reason"], "deadline");
    assert_eq!(cancel["v"], 1);
}

/// PO-T5 (MH 4.10): a cancel while a call waits ends the run promptly, as cancelled, and the
/// host is told the call was given up.
#[test]
fn po_t5_a_cancel_during_a_port_wait_ends_promptly() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("po_t5");
    let package = caller_package(scratch.path(), "stub", false);
    let mut doc = document(
        scratch.path(),
        20_000,
        4,
        package,
        json!({"stub": {"host": "stub"}}),
    );
    let socket = socket_path("t5");
    doc["report"]["socket"] = json!(format!("unix://{}", socket.display()));
    let path = write_doc(scratch.path(), &doc);
    let listener = transport::listen(&Address::Unix(socket.clone())).expect("listen");
    let runner = std::thread::spawn(move || run_cli(&path, &Embedding, CancelToken::new()));
    let host = Host::serve(listener, |_, _| Reply::Hold);
    assert!(
        host.wait_for(Duration::from_secs(30), |m| m
            .iter()
            .any(|(_, m)| m["type"] == "port_call")),
        "the stage called"
    );
    std::thread::sleep(Duration::from_millis(200));
    let cancelled = Instant::now();
    host.send(json!({"type": "cancel"}));
    let code = runner.join().expect("the runner");
    let took = cancelled.elapsed();
    let messages = host.join();
    assert_eq!(code, exit::CANCELLED);
    assert!(took < Duration::from_secs(3), "the cancel took {took:?}");
    let cancel = messages
        .iter()
        .find(|(_, m)| m["type"] == "port_cancel")
        .expect("the call was given up")
        .1
        .clone();
    assert_eq!(cancel["reason"], "cancelled");
    let end = events(&messages)
        .into_iter()
        .rev()
        .find(|e| e["task"] == "kernel-1")
        .unwrap();
    assert!(
        (end["kind"] == "task_completed" && end["outcome"] == "cancelled")
            || end["kind"] == "task_skipped",
        "{end}"
    );
    let _ = std::fs::remove_file(&socket);
}

/// The document's own refusals of `ports` (MH 4.10), each naming its field.
#[test]
fn po_t6_the_document_refuses_ports_it_cannot_serve() {
    let scratch = Scratch::new("po_t6");
    let package = caller_package(scratch.path(), "stub", false);
    let base = document(
        scratch.path(),
        1_000,
        1,
        package,
        json!({"stub": {"host": "stub"}}),
    );
    let refusal = |doc: Value| -> String {
        let job = match JobSpec::from_value(doc) {
            Ok(job) => job,
            Err(refused) => return refused.to_string(),
        };
        match build(
            &job,
            &Embedding,
            BuildOptions {
                strict: true,
                env: &no_env,
                notes: Vec::new(),
                packages: Default::default(),
                host_ports: None,
            },
        ) {
            Ok(_) => "built".to_string(),
            Err(e) => e.to_string(),
        }
    };
    let said = refusal(base.clone());
    assert!(
        said.contains("kernels[0].ports.stub.host") && said.contains("needs a peer"),
        "{said}"
    );
    let mut doc = base.clone();
    doc["kernels"][0]["ports"] = json!({"stub": {"guest": "nobody"}});
    let said = refusal(doc);
    assert!(
        said.contains("kernels[0].ports.stub.guest") && said.contains("\"nobody\""),
        "{said}"
    );
    let mut doc = base.clone();
    doc["kernels"][0]["ports"] = json!({"stub": {"guest": "echo", "host": "x"}});
    assert!(refusal(doc).contains("exactly one of `host` and `guest`"));
    let mut doc = base.clone();
    doc["kernels"][0]["ports"] = json!({"stub": {"guest": "echo", "deadline_ms": 0}});
    assert!(refusal(doc).contains("kernels[0].ports.stub.deadline_ms"));
    let mut doc = base.clone();
    doc["kernels"][0]["ports"] = json!({"no spaces": {"guest": "echo"}});
    assert!(refusal(doc).contains("is not a port name"));
    let mut doc = base.clone();
    doc["kernels"][0]["ports"] = json!({"stub": {"guest": "echo", "vsock": 5}});
    assert!(refusal(doc).contains("kernels[0]"));
    let mut doc = base.clone();
    doc["kernels"] = json!([{"kind": "std", "name": "select", "args": {"columns": ["id"]},
                             "ports": {"stub": {"guest": "echo"}}}]);
    let said = refusal(doc);
    assert!(
        said.contains("kernels[0].ports") && said.contains("only a function package"),
        "{said}"
    );
    let mut doc = base;
    doc["kernels"][0]["ports"] = json!({"stub": {"guest": "echo"}});
    assert_eq!(refusal(doc), "built");
}

/// O1's second gate: a stage that waits on its host for `wait` keeps the peer hearing a heartbeat
/// at least every 2 s, is reported waiting, keeps writing its manifests (a stateful stage whose
/// instance is busy the whole time), keeps its ceiling, and completes when the answer comes.
fn waits_and_keeps_heartbeating(name: &str, wait: Duration) {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new(name);
    let rows = 80_000;
    let package = caller_package(scratch.path(), "stub", true);
    let mut doc = document(
        scratch.path(),
        rows,
        16,
        package,
        json!({"stub": {"host": "stub", "deadline_ms": wait.as_millis() as u64 + 120_000}}),
    );
    doc["checkpoint"] = json!({"enabled": true, "interval_ms": 500, "keep": false});
    let socket = socket_path(name);
    doc["report"]["socket"] = json!(format!("unix://{}", socket.display()));
    let path = write_doc(scratch.path(), &doc);
    let listener = transport::listen(&Address::Unix(socket.clone())).expect("listen");
    let runner = std::thread::spawn(move || run_cli(&path, &Embedding, CancelToken::new()));
    // The probe's call and two more are answered; the fourth is held for `wait`.
    const HELD: usize = 4;
    let host = Host::serve(
        listener,
        |n, _| if n == HELD { Reply::Hold } else { Reply::Echo },
    );
    assert!(
        host.wait_for(Duration::from_secs(60), |m| m
            .iter()
            .filter(|(_, m)| m["type"] == "port_call")
            .count()
            >= HELD),
        "the stage reached its held call"
    );
    let held = host.of_type("port_call")[HELD - 1].clone();
    let began = Instant::now();
    std::thread::sleep(wait);
    host.send(json!({
        "type": "port_reply", "v": 1, "call": held["call"],
        "ok": B64.encode(&B64.decode(held["request"].as_str().unwrap()).unwrap()[8..]),
    }));
    let answered = Instant::now();
    let code = runner.join().expect("the runner");
    let messages = host.join();
    assert_eq!(
        code,
        exit::COMPLETED,
        "{}",
        report_file(scratch.path())["exit"]
    );
    assert_eq!(read_back_rows(&scratch.path().join("out")), rows);

    let during: Vec<&(Instant, Value)> = messages
        .iter()
        .filter(|(at, _)| *at >= began && *at <= answered)
        .collect();
    let beats: Vec<&Value> = during
        .iter()
        .map(|(_, m)| m)
        .filter(|m| m["type"] == "heartbeat")
        .collect();
    assert!(
        beats.len() as u64 >= wait.as_secs() / 2,
        "{} heartbeats in {wait:?}",
        beats.len()
    );
    let mut previous = began;
    for (at, message) in messages
        .iter()
        .filter(|(at, m)| *at >= began && *at <= answered && m["type"] == "heartbeat")
    {
        assert!(
            at.duration_since(previous) <= Duration::from_secs(2),
            "a {:?} gap before {message}",
            at.duration_since(previous)
        );
        previous = *at;
    }
    assert!(answered.duration_since(previous) <= Duration::from_secs(2));
    // Reported waiting from the controller's first tick of the wait to its end, at the same
    // ceiling.
    let settled: Vec<&&Value> = beats
        .iter()
        .filter(|m| m["t_ms"].as_u64().is_some())
        .skip(2)
        .collect();
    assert!(!settled.is_empty());
    for beat in &settled {
        assert_eq!(beat["bottleneck"], "Waiting", "{beat}");
        assert_eq!(beat["ceiling_bytes"], 1u64 << 30, "{beat}");
    }
    // Told as a wait on its port, and as working again once answered.
    let told = events(&messages);
    let waited = told
        .iter()
        .filter(|e| e["kind"] == "task_waiting" && e["port"] == "stub")
        .count();
    assert!(waited >= HELD, "{waited} waits told");
    let report = report_file(scratch.path());
    let timeline = report["report"]["bottleneck_timeline"].to_string();
    assert!(timeline.contains("Waiting"), "{timeline}");
    // Its manifests kept being written while its one instance was busy: no write failed for
    // it, or the run would have ended (10 f.12: three in a row end it).
    assert!(
        report["report"]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| !n
                .as_str()
                .unwrap()
                .contains("busy for the whole checkpoint")),
        "{report}"
    );
    let _ = std::fs::remove_file(&socket);
}

/// O1's second gate, quickly: twenty seconds, past three of the checkpoint's five-second waits
/// for a busy instance (10 f.12), which would have ended the run before MH 4.10.
#[test]
fn po_t7_a_waiting_stage_keeps_heartbeating_and_is_not_shrunk() {
    waits_and_keeps_heartbeating("po_t7", Duration::from_secs(20));
}

/// O1's second gate as written: a stage waiting ten minutes.
#[test]
#[ignore = "O1 gate at full length: ten minutes of wall time; run in the cgroup job"]
fn po_t7_a_stage_waiting_ten_minutes_keeps_heartbeating() {
    waits_and_keeps_heartbeating("po_t7_long", Duration::from_secs(600));
}

/// The child of PO-T8: run the document at `MORUNA_PORTS_CHILD` and exit; a no-op otherwise.
#[test]
fn po_t8_child() {
    let Ok(path) = std::env::var(CHILD) else {
        return;
    };
    let code = run_cli(Path::new(&path), &Embedding, CancelToken::new());
    println!("CHILD-EXIT {code}");
}

fn spawn_child(doc: &Path) -> Child {
    Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "--exact",
            "po_t8_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD, doc)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the child starts")
}

/// The counter in a manifest's state for stage 1, when it holds one.
fn state_in(manifest: &Path) -> Option<u64> {
    let value: Value = serde_json::from_slice(&std::fs::read(manifest).ok()?).ok()?;
    let row = value["kernel_states"]
        .as_array()?
        .iter()
        .find(|row| row["stage"] == 1)?
        .clone();
    let bytes = B64.decode(row["state"].as_str()?).ok()?;
    Some(u64::from_le_bytes(bytes.as_slice().try_into().ok()?))
}

/// PO-T8, O1's third gate (MH 4.10, MH 4.7): a stateful stage whose run is killed while it waits
/// on its host resumes, on a machine whose staging disk is empty, from the copy of its manifest
/// on an object-store prefix, with its state as the waiting call's batch began.
///
/// The first process answers the probe's call and two more and holds the fourth; a manifest
/// written while it waits is copied to `staging.url` with the stage's state, 3: the count before
/// the fourth batch. The process is killed with `SIGKILL`, the staging directory emptied, and the
/// same document run again: its first call carries 4, the restored count and one, where a fresh
/// stage's would carry 1, and the output is every row.
#[test]
fn po_t8_a_resumed_run_restores_a_stages_state_bytes() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("po_t8");
    let rows = 80_000;
    let package = caller_package(scratch.path(), "stub", true);
    let store = scratch.path().join("store");
    std::fs::create_dir_all(&store).expect("the store");
    let mut doc = document(
        scratch.path(),
        rows,
        16,
        package,
        json!({"stub": {"host": "stub"}}),
    );
    doc["checkpoint"] = json!({"enabled": true, "interval_ms": 500, "keep": false});
    doc["staging"]["url"] = json!(format!("file://{}", store.display()));
    doc["resume"] = json!("auto");
    let socket = socket_path("t8");
    doc["report"]["socket"] = json!(format!("unix://{}", socket.display()));
    let path = write_doc(scratch.path(), &doc);
    let copy = store.join(format!("moruna-{RUN_ID}")).join("manifest.json");

    // The first machine.
    let listener = transport::listen(&Address::Unix(socket.clone())).expect("listen");
    let mut child = spawn_child(&path);
    const HELD: usize = 4;
    let host = Host::serve(
        listener,
        |n, _| if n == HELD { Reply::Hold } else { Reply::Echo },
    );
    assert!(
        host.wait_for(Duration::from_secs(60), |m| m
            .iter()
            .filter(|(_, m)| m["type"] == "port_call")
            .count()
            >= HELD),
        "the stage reached its held call"
    );
    let held_count = count_of(&host.of_type("port_call")[HELD - 1]);
    assert_eq!(held_count, HELD as u64);
    let deadline = Instant::now() + Duration::from_secs(30);
    while state_in(&copy) != Some(held_count - 1) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        state_in(&copy),
        Some(held_count - 1),
        "a manifest written while the stage waited is on the store, with its state"
    );
    child.kill().expect("SIGKILL");
    let _ = child.wait();
    drop(host);
    let _ = std::fs::remove_file(&socket);

    // The second machine: the same mount point, empty.
    let staging = scratch.path().join("staging");
    std::fs::remove_dir_all(&staging).expect("the staging disk is gone");
    std::fs::create_dir_all(&staging).expect("a new staging disk");
    let (code, messages) = converse(scratch.path(), "t8b", doc, |_, _| Reply::Echo);
    let report = report_file(scratch.path());
    assert_eq!(code, exit::COMPLETED, "{}", report["exit"]);
    assert_eq!(report["report"]["resumed"], true);
    let first = messages
        .iter()
        .find(|(_, m)| m["type"] == "port_call")
        .expect("the resumed stage called")
        .1
        .clone();
    assert_eq!(
        count_of(&first),
        held_count,
        "the stage's state was restored: it counts on from {}",
        held_count - 1
    );
    let started = events(&messages)
        .into_iter()
        .find(|e| e["kind"] == "attempt_started")
        .unwrap();
    assert_eq!(started["resumed"], true);
    assert_eq!(read_back_rows(&scratch.path().join("out")), rows);
    assert!(
        !copy.exists(),
        "the completed run's copy is removed with its directory"
    );
}
