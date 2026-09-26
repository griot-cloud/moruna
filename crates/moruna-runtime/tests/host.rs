//! The hosted-engine gates (MH H1, H2, H8) over real components: a job document on disk or on a Unix socket, run to its exit code
//! by the same code `moruna run` and `moruna serve` run, with a peer that listens, stops
//! listening, cancels and asks for a checkpoint (H1, H2, H8).
//!
//! The kernels are the Rust ones of `support`, named in the document as Python kernels are and
//! handed out by a test loader; what is under test is the document, the session and the
//! protocol, and the Python loader is `moruna-py`'s.

#![allow(clippy::result_large_err)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::{CancelToken, Kernel};
use moruna_runtime::host::cli;
use moruna_runtime::host::exit;
use moruna_runtime::host::session::{self, HEARTBEAT, read_report_file};
use moruna_runtime::host::transport::{self, Address};
use moruna_runtime::job::{KernelDoc, KernelLoader, LoadedKernel};
use moruna_runtime::{JobSpec, RunSpec, Runtime, SinkSpec, SourceSpec};
use serde_json::{Value, json};
use support::{
    Doubler, FailOnce, Scratch, Sleeper, one_run_at_a_time, read_back_rows, write_parquet,
};

const RUN_ID: &str = "00112233445566778899aabbccddeeff";

/// Hands out the support kernels by the callable a document names.
struct TestKernels;

impl KernelLoader for TestKernels {
    fn load(&self, index: usize, doc: &KernelDoc) -> moruna_kernel::Result<LoadedKernel> {
        let kernel: Arc<dyn Kernel> = match doc.callable.as_deref() {
            Some("double") => Arc::new(Doubler::new()),
            Some("sleep") => Arc::new(Sleeper::new(700)),
            Some("fail") => Arc::new(FailOnce::new(1)),
            other => {
                return Err(moruna_kernel::MorunaError::Plan(format!(
                    "kernels[{index}]: no test kernel `{other:?}`"
                )));
            }
        };
        Ok(LoadedKernel {
            kernel,
            #[cfg(feature = "python")]
            python: None,
        })
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// A document over a Parquet file the test writes, with one kernel, a 1 GiB budget and two
/// CPUs, everything under the scratch directory.
fn document(scratch: &Path, rows: u64, groups: u64, kernel: Option<&str>) -> Value {
    let input = scratch.join("in.parquet");
    write_parquet(&input, rows, groups);
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    let out = scratch.join("out");
    std::fs::create_dir_all(&out).expect("out");
    let kernels: Vec<Value> = kernel
        .map(|k| vec![json!({"kind": "python", "module": "tests", "callable": k})])
        .unwrap_or_default();
    json!({
        "moruna_spec": 1,
        "run_id": RUN_ID,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": kernels,
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display()),
                 "options": {"row_group_bytes": 16 << 20, "file_bytes": 64 << 20}},
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

fn run_cli(args: &[&str], env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    cli::main_with(&args, &TestKernels, CancelToken::new(), env, HEARTBEAT)
}

fn report_file(scratch: &Path) -> Value {
    read_report_file(&scratch.join(format!("report-{RUN_ID}.json"))).expect("the report file")
}

/// What two runs of one job must agree on: everything but timing and memory figures.
fn projection(report: &Value) -> Value {
    let stages: Vec<Value> = report["stages"]
        .as_array()
        .expect("stages")
        .iter()
        .map(|s| {
            json!([
                s["stage"],
                s["rows_in"],
                s["rows_out"],
                s["errors"],
                s["skipped"]
            ])
        })
        .collect();
    json!({
        "exit": report["exit"],
        "resumed": report["resumed"],
        "limits": [report["limits"]["memory_ceiling"], report["limits"]["cpu_quota"], report["limits"]["source"]],
        "io_paths": report["io_paths"],
        "gil_serialised": report["gil_serialised"],
        "sizer_used": report["sizer_used"],
        "stages": stages,
    })
}

/// HO-T5 file_run_equals_library_run (H1): the same job as a document and as a `RunSpec` built
/// by hand, which is what the library path built before this feature, produce the same report
/// modulo timing and the same output.
#[test]
fn ho_t5_file_run_equals_library_run() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ho_t5");
    let rows = 20_000;
    let doc = document(scratch.path(), rows, 4, Some("double"));
    let path = write_doc(scratch.path(), &doc);
    assert_eq!(
        run_cli(&["run", &path.to_string_lossy()], &no_env),
        exit::COMPLETED
    );
    let from_file = report_file(scratch.path());
    assert_eq!(from_file["exit"]["code"], 0);
    assert_eq!(from_file["run_id"], RUN_ID);
    let job = JobSpec::from_value(doc.clone()).expect("the document");
    assert_eq!(from_file["spec_digest"], job.digest());
    assert_eq!(read_back_rows(&scratch.path().join("out")), rows);

    // The same run through the library's own construction.
    let out2 = scratch.path().join("out2");
    std::fs::create_dir_all(&out2).expect("out2");
    let input = scratch.path().join("in.parquet");
    let sink_url = format!("file://{}", out2.display());
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            moruna_sources::ParquetSource::new(
                moruna_sources::ParquetSourceConfig {
                    urls: vec![format!("file://{}", input.display())],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|s| Arc::new(s) as Arc<dyn moruna_kernel::Source>)
        })),
        vec![Arc::new(Doubler::new()) as Arc<dyn Kernel>],
        SinkSpec::Build(Box::new(move |ctx| {
            moruna_sinks::ParquetSink::new(
                moruna_sinks::ParquetSinkConfig {
                    url: sink_url,
                    row_group_bytes: 16 << 20,
                    file_bytes: 64 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|s| Box::new(s.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
        })),
    );
    spec.budget = Some(1 << 30);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.staging_limit = Some(1 << 30);
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    let direct = Runtime::run(spec, CancelToken::new()).expect("the library run");
    let direct = serde_json::to_value(&direct).expect("json");
    assert_eq!(projection(&from_file["report"]), projection(&direct));
    assert_eq!(read_back_rows(&out2), rows);
}

/// Read every message until `exit`, with the instant each arrived.
fn drain(reader: &mut dyn BufRead) -> Vec<(Instant, Value)> {
    let mut messages = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return messages,
            Ok(_) => {
                let value: Value = serde_json::from_str(&line).expect("a JSON line");
                let last = value["type"] == "exit";
                messages.push((Instant::now(), value));
                if last {
                    return messages;
                }
            }
        }
    }
}

fn read_one(reader: &mut dyn BufRead) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("a line");
    serde_json::from_str(&line).expect("a JSON line")
}

fn socket_path(scratch: &Path) -> PathBuf {
    // A Unix socket path is limited to about 100 bytes, which a temp directory may exceed.
    let short = std::env::temp_dir().join(format!(
        "mh-{}-{}.sock",
        std::process::id(),
        scratch
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.len())
            .unwrap_or(0)
            + scratch.to_string_lossy().len()
    ));
    let _ = std::fs::remove_file(&short);
    short
}

/// HO-T6 heartbeat_cadence (H2): over a Unix socket the peer hears `hello` with the document's
/// digest, a heartbeat at least every 2 s for as long as the run lasts, the full report and
/// then `exit`; the same report is on disk.
#[test]
fn ho_t6_heartbeat_cadence_and_report_delivery() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ho_t6");
    let socket = socket_path(scratch.path());
    let mut doc = document(scratch.path(), 16_000, 8, Some("sleep"));
    doc["report"]["socket"] = json!(format!("unix://{}", socket.display()));
    let digest = JobSpec::from_value(doc.clone()).expect("doc").digest();
    let path = write_doc(scratch.path(), &doc);

    let listener = transport::listen(&Address::Unix(socket.clone())).expect("listen");
    let started = Instant::now();
    let runner = std::thread::spawn(move || run_cli(&["run", &path.to_string_lossy()], &no_env));
    let conn = listener.accept().expect("moruna connects");
    let (reader, _writer, _close) = conn.into_parts();
    let mut reader = BufReader::new(reader);
    let messages = drain(&mut reader);
    let code = runner.join().expect("the runner");
    assert_eq!(code, exit::COMPLETED);

    let types: Vec<&str> = messages
        .iter()
        .map(|(_, m)| m["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(types.first(), Some(&"hello"), "{types:?}");
    assert_eq!(messages[0].1["spec_digest"], digest);
    assert_eq!(messages[0].1["limits"]["memory_ceiling"], 1u64 << 30);
    assert_eq!(&types[types.len() - 2..], &["report", "exit"], "{types:?}");
    let beats = types.iter().filter(|t| **t == "heartbeat").count();
    assert!(
        beats >= 2,
        "{beats} heartbeats over {:?}",
        started.elapsed()
    );
    let mut previous = started;
    for (at, message) in &messages {
        let gap = at.duration_since(previous);
        assert!(
            gap <= Duration::from_secs(2),
            "a {gap:?} gap before {message}"
        );
        previous = *at;
    }
    let last_beat = messages
        .iter()
        .rev()
        .find(|(_, m)| m["type"] == "heartbeat")
        .map(|(_, m)| m.clone())
        .expect("a heartbeat");
    for field in [
        "t_ms",
        "committed_seq",
        "rows_out",
        "bytes_out",
        "active_workers",
        "ceiling_bytes",
        "anon_bytes",
        "bottleneck",
        "staging_bytes",
    ] {
        assert!(
            last_beat.get(field).is_some(),
            "the heartbeat has no {field}"
        );
    }
    assert_eq!(last_beat["ceiling_bytes"], 1u64 << 30);
    let exit_msg = &messages[messages.len() - 1].1;
    assert_eq!(exit_msg["code"], 0);
    let delivered = &messages[messages.len() - 2].1["report"];
    let on_disk = report_file(scratch.path());
    assert_eq!(delivered["run_id"], on_disk["report"]["run_id"]);
    assert_eq!(delivered["run_id"], RUN_ID);
    assert_eq!(delivered["exit"], json!("Completed"));
    let _ = std::fs::remove_file(&socket);
}

/// HO-T7 peer_gone (H2): the peer reads `hello` and one heartbeat and goes away. The run
/// carries on to completion, and the report, with a note saying when the peer went, is on
/// disk.
#[test]
fn ho_t7_peer_gone_the_run_continues() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ho_t7");
    let socket = socket_path(scratch.path());
    let mut doc = document(scratch.path(), 16_000, 8, Some("sleep"));
    doc["report"]["socket"] = json!(format!("unix://{}", socket.display()));
    let path = write_doc(scratch.path(), &doc);

    let listener = transport::listen(&Address::Unix(socket.clone())).expect("listen");
    let runner = std::thread::spawn(move || run_cli(&["run", &path.to_string_lossy()], &no_env));
    {
        let conn = listener.accept().expect("moruna connects");
        let (reader, writer, close) = conn.into_parts();
        let mut reader = BufReader::new(reader);
        assert_eq!(read_one(&mut reader)["type"], "hello");
        assert_eq!(read_one(&mut reader)["type"], "heartbeat");
        close();
        drop(writer);
    }
    assert_eq!(runner.join().expect("the runner"), exit::COMPLETED);
    let on_disk = report_file(scratch.path());
    assert_eq!(on_disk["exit"]["code"], 0);
    assert_eq!(on_disk["report"]["exit"], json!("Completed"));
    assert_eq!(read_back_rows(&scratch.path().join("out")), 16_000);
    let notes = on_disk["host_notes"].as_array().expect("host notes");
    assert!(
        notes.iter().any(|n| n
            .as_str()
            .is_some_and(|n| n.starts_with("the peer stopped receiving"))),
        "{notes:?}"
    );
    let _ = std::fs::remove_file(&socket);
}

/// HO-T8 checkpoint_and_cancel: `checkpoint` writes the manifest now (the interval is a
/// minute, so nothing else would), and `cancel` ends the run with 130 and a report that says so.
#[test]
fn ho_t8_checkpoint_then_cancel() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ho_t8");
    let socket = socket_path(scratch.path());
    let mut doc = document(scratch.path(), 32_000, 16, Some("sleep"));
    doc["report"]["socket"] = json!(format!("unix://{}", socket.display()));
    doc["checkpoint"] = json!({"enabled": true, "interval_ms": 60_000, "keep": true});
    doc["budget"]["cpu"] = json!(1.0);
    let path = write_doc(scratch.path(), &doc);
    let manifest = scratch
        .path()
        .join("staging")
        .join(format!("moruna-{RUN_ID}"))
        .join("manifest.json");

    let listener = transport::listen(&Address::Unix(socket.clone())).expect("listen");
    let runner = std::thread::spawn(move || run_cli(&["run", &path.to_string_lossy()], &no_env));
    let conn = listener.accept().expect("moruna connects");
    let (reader, mut writer, _close) = conn.into_parts();
    let mut reader = BufReader::new(reader);
    assert_eq!(read_one(&mut reader)["type"], "hello");
    // Two heartbeats in, the probe is over and the scheduler is running.
    assert_eq!(read_one(&mut reader)["type"], "heartbeat");
    assert_eq!(read_one(&mut reader)["type"], "heartbeat");
    assert!(!manifest.exists(), "no manifest before it is asked for");
    writer
        .write_all(b"{\"type\":\"checkpoint\"}\n")
        .expect("checkpoint");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !manifest.exists() && Instant::now() < deadline {
        let _ = read_one(&mut reader);
    }
    assert!(manifest.exists(), "the manifest was written on request");
    writer
        .write_all(b"{\"type\":\"cancel\"}\n")
        .expect("cancel");
    let messages = drain(&mut reader);
    assert_eq!(runner.join().expect("the runner"), exit::CANCELLED);
    let exit_msg = &messages.last().expect("exit").1;
    assert_eq!(exit_msg["type"], "exit");
    assert_eq!(exit_msg["code"], 130);
    let on_disk = report_file(scratch.path());
    assert_eq!(on_disk["exit"]["code"], 130);
    assert_eq!(on_disk["report"]["exit"], json!("Cancelled"));
    let _ = std::fs::remove_file(&socket);
}

fn connect_when_ready(socket: &Path) -> transport::Conn {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match transport::connect(&Address::Unix(socket.to_path_buf())) {
            Ok(conn) => return conn,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("moruna serve never listened: {e}"),
        }
    }
}

/// HO-T10 serve (MH 4.2): the peer connects, hears `hello` with no digest yet, sends the
/// document, and the run goes exactly as `moruna run` would; a second `spec` is refused and
/// said so in the report.
#[test]
fn ho_t10_serve_runs_one_spec() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ho_t10");
    let socket = socket_path(scratch.path());
    let doc = document(scratch.path(), 16_000, 4, Some("sleep"));
    let listen = format!("unix://{}", socket.display());
    let server = std::thread::spawn(move || run_cli(&["serve", "--listen", &listen], &no_env));
    let conn = connect_when_ready(&socket);
    let (reader, mut writer, _close) = conn.into_parts();
    let mut reader = BufReader::new(reader);
    let hello = read_one(&mut reader);
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["spec_digest"], Value::Null);
    writer
        .write_all(b"{\"type\":\"checkpoint\"}\nnot json\n")
        .expect("noise first");
    let message = json!({"type": "spec", "spec": doc});
    writer
        .write_all(format!("{message}\n{message}\n").as_bytes())
        .expect("the spec, twice");
    let messages = drain(&mut reader);
    assert_eq!(server.join().expect("the server"), exit::COMPLETED);
    assert_eq!(messages.last().expect("exit").1["code"], 0);
    let on_disk = report_file(scratch.path());
    let notes: Vec<String> = on_disk["host_notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|n| n.as_str().map(str::to_string))
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n == "a second spec was refused: one job per process"),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("checkpoint before a spec arrived")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("an unreadable message was ignored")),
        "{notes:?}"
    );
    assert!(
        !socket.exists(),
        "the listener's socket is gone once the peer is in"
    );
}

/// HO-T11 strict_refusal (H8): a document that leaves a field to its environment variable is
/// refused under `--strict` with the field and the variable named, exit 2, and the report file
/// says so; without `--strict` the same document runs and the note says where the value came
/// from. `moruna serve` is strict by default.
#[test]
fn ho_t11_strict_refusal() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ho_t11");
    let mut doc = document(scratch.path(), 4_000, 2, None);
    doc["budget"] = json!({"cpu": 2.0});
    let path = write_doc(scratch.path(), &doc);
    let env = |name: &str| (name == "MORUNA_BUDGET").then(|| "1GiB".to_string());
    assert_eq!(
        run_cli(&["run", "--strict", &path.to_string_lossy()], &env),
        exit::SPEC_REFUSED
    );
    let refused = report_file(scratch.path());
    assert_eq!(refused["exit"]["code"], 2);
    assert_eq!(refused["report"], Value::Null);
    let diagnostic = refused["exit"]["diagnostic"]
        .as_str()
        .expect("a diagnostic");
    assert!(
        diagnostic.ends_with(
            "spec refused: budget.memory_bytes: strict mode resolves no field from the \
             environment, and this document would resolve budget.memory_bytes from \
             MORUNA_BUDGET; set the field in the document or unset the variable"
        ),
        "{diagnostic}"
    );

    // Lenient: it runs, and says where the budget came from. The variable is only visible to
    // the refusal check here; discovery reads the process environment, where it is not set.
    assert_eq!(
        run_cli(&["run", &path.to_string_lossy()], &env),
        exit::COMPLETED
    );
    let ran = report_file(scratch.path());
    let notes = ran["report"]["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|n| n == "budget.memory_bytes resolved from MORUNA_BUDGET"),
        "{notes:?}"
    );

    // Serve is strict unless told otherwise.
    let socket = socket_path(scratch.path());
    let listen = format!("unix://{}", socket.display());
    let server = std::thread::spawn(move || {
        let args: Vec<String> = ["serve", "--listen", &listen]
            .iter()
            .map(|s| s.to_string())
            .collect();
        cli::main_with(&args, &TestKernels, CancelToken::new(), &env, HEARTBEAT)
    });
    let conn = connect_when_ready(&socket);
    let (reader, mut writer, _close) = conn.into_parts();
    let mut reader = BufReader::new(reader);
    assert_eq!(read_one(&mut reader)["type"], "hello");
    writer
        .write_all(format!("{}\n", json!({"type": "spec", "spec": doc})).as_bytes())
        .expect("spec");
    let messages = drain(&mut reader);
    assert_eq!(server.join().expect("server"), exit::SPEC_REFUSED);
    let exit_msg = &messages.last().expect("exit").1;
    assert_eq!(exit_msg["code"], 2);
    assert!(
        exit_msg["diagnostic"]
            .as_str()
            .is_some_and(|d| d.contains("MORUNA_BUDGET"))
    );
}

/// HO-T12 exit_codes (MH 4.2): 2 for a document that is not one, with the report file where it
/// said; 4 for a kernel error under `terminate`; 5 for a resume that cannot be; 130 for a
/// cancel before any spec. Code 3 is the mapping of a budget refusal, which discovery's floor
/// keeps out of reach of a test process; `exit_code`'s own test covers it.
#[test]
fn ho_t12_exit_codes() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ho_t12");

    // 2: a document with an unknown field, whose report file is still written where it says.
    let mut doc = document(scratch.path(), 4_000, 2, None);
    doc["budgett"] = json!({});
    let path = write_doc(scratch.path(), &doc);
    assert_eq!(
        run_cli(&["run", &path.to_string_lossy()], &no_env),
        exit::SPEC_REFUSED
    );
    let refused = report_file(scratch.path());
    assert_eq!(
        refused["exit"]["diagnostic"],
        "spec refused: budgett: not a field of moruna_spec 1"
    );
    std::fs::write(&path, "{").expect("write");
    assert_eq!(
        run_cli(&["run", &path.to_string_lossy()], &no_env),
        exit::SPEC_REFUSED
    );

    // 4: the kernel fails on its first morsel and the policy is `terminate`.
    let doc = document(scratch.path(), 4_000, 2, Some("fail"));
    let path = write_doc(scratch.path(), &doc);
    assert_eq!(
        run_cli(&["run", &path.to_string_lossy()], &no_env),
        exit::KERNEL_ERROR
    );
    let failed = report_file(scratch.path());
    assert_eq!(failed["exit"]["code"], 4);
    assert!(
        failed["report"].is_object(),
        "a terminated run still reports"
    );

    // 5: a manifest that is not there.
    let mut doc = document(scratch.path(), 4_000, 2, None);
    doc["resume"] = json!(scratch.path().join("nope.json").to_string_lossy());
    let path = write_doc(scratch.path(), &doc);
    assert_eq!(
        run_cli(&["run", &path.to_string_lossy()], &no_env),
        exit::RESUME_REFUSED
    );
    // A kernel that no loader has is a refused document.
    let doc = document(scratch.path(), 4_000, 2, Some("unknown"));
    let path = write_doc(scratch.path(), &doc);
    assert_eq!(
        run_cli(&["run", &path.to_string_lossy()], &no_env),
        exit::SPEC_REFUSED
    );

    // 130: cancelled before a spec arrived.
    let socket = socket_path(scratch.path());
    let listen = format!("unix://{}", socket.display());
    let server = std::thread::spawn(move || run_cli(&["serve", "--listen", &listen], &no_env));
    let conn = connect_when_ready(&socket);
    let (reader, mut writer, _close) = conn.into_parts();
    let mut reader = BufReader::new(reader);
    assert_eq!(read_one(&mut reader)["type"], "hello");
    writer
        .write_all(b"{\"type\":\"cancel\"}\n")
        .expect("cancel");
    assert_eq!(read_one(&mut reader)["code"], 130);
    assert_eq!(server.join().expect("server"), exit::CANCELLED);

    // 2: a peer that leaves before sending a spec, and one whose spec is not a document.
    let listen = format!("unix://{}", socket.display());
    let server = std::thread::spawn(move || run_cli(&["serve", "--listen", &listen], &no_env));
    drop(connect_when_ready(&socket));
    assert_eq!(server.join().expect("server"), exit::SPEC_REFUSED);
    let listen = format!("unix://{}", socket.display());
    let server = std::thread::spawn(move || run_cli(&["serve", "--listen", &listen], &no_env));
    let conn = connect_when_ready(&socket);
    let (reader, mut writer, _close) = conn.into_parts();
    let mut reader = BufReader::new(reader);
    let _ = read_one(&mut reader);
    writer
        .write_all(b"{\"type\":\"spec\",\"spec\":{\"moruna_spec\":7}}\n")
        .expect("spec");
    let exit_msg = read_one(&mut reader);
    assert_eq!(exit_msg["code"], 2);
    assert_eq!(server.join().expect("server"), exit::SPEC_REFUSED);

    // A socket that cannot be listened on is refused before anything else.
    assert_eq!(
        run_cli(
            &["serve", "--listen", "/no/such/moruna/dir/m.sock"],
            &no_env
        ),
        exit::SPEC_REFUSED
    );
}

/// HO-T13 report_file_placement (MH 4.1): absent `report.file` puts the report in the staging
/// directory under the run's id; a `report.socket` nobody listens on is a note, not a failure.
#[test]
fn ho_t13_report_file_placement() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ho_t13");
    let mut doc = document(scratch.path(), 4_000, 2, None);
    doc["report"] =
        json!({"socket": format!("unix://{}", scratch.path().join("nobody.sock").display())});
    let path = write_doc(scratch.path(), &doc);
    let outcome = session::run_file(
        &path,
        &TestKernels,
        &session::HostOptions {
            strict: true,
            env: &no_env,
            cancel: CancelToken::new(),
            heartbeat: HEARTBEAT,
        },
    );
    assert_eq!(outcome.code, exit::COMPLETED, "{:?}", outcome.diagnostic);
    let expected = scratch
        .path()
        .join("staging")
        .join(format!("moruna-{RUN_ID}.report.json"));
    assert_eq!(outcome.report_file.as_deref(), Some(expected.as_path()));
    let on_disk = read_report_file(&expected).expect("the default report file");
    assert_eq!(on_disk["moruna_report"], 1);
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("report.socket unix://")),
        "{:?}",
        outcome.notes
    );
    assert_eq!(
        cli::summary(&outcome),
        format!("moruna: exit 0; report: {}", expected.display())
    );
}
