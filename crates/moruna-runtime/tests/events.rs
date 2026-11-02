//! PY-T36 and the headers and ids of PY-T32 and PY-T33 on real runs (SDD 12 PY-I13, MH 4.3
//! version 7): where a run's events go, what they carry, and how they are read.
//!
//! The kernels are the Rust ones of `support`, named in the document as Python kernels are.

#![allow(clippy::result_large_err)]

mod support;

use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use moruna_kernel::{CancelToken, Kernel};
use moruna_record::{Decoded, decode as decode_record};
use moruna_runtime::host::events::{EVENTS_FILE, EventOptions, EventRing, JsonLines};
use moruna_runtime::host::protocol::{self, Message};
use moruna_runtime::host::session::{self, HEARTBEAT, HostOptions, Mode, Wire};
use moruna_runtime::host::{exit, logs};
use serde_json::{Value, json};
use support::{Doubler, Scratch, one_run_at_a_time, write_parquet};

const RUN_ID: &str = "00112233445566778899aabbccddeeff";

fn test_kernel(index: usize, callable: &str) -> moruna_kernel::Result<Arc<dyn Kernel>> {
    Ok(match callable {
        "double" => Arc::new(Doubler::new()),
        other => {
            return Err(moruna_kernel::MorunaError::Plan(format!(
                "kernels[{index}]: no test kernel `{other:?}`"
            )));
        }
    })
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// A document over a Parquet file, one kernel, its output store at `scratch/logs`.
fn document(scratch: &Path) -> Value {
    let input = scratch.join("in.parquet");
    write_parquet(&input, 4_000, 2);
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    let out = scratch.join("out");
    std::fs::create_dir_all(&out).expect("out");
    json!({
        "moruna_spec": 1,
        "run_id": RUN_ID,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": [{"kind": "python", "module": "tests", "callable": "double"}],
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display()),
                 "options": {"row_group_bytes": 16 << 20, "file_bytes": 64 << 20}},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "staging": {"dir": staging.to_string_lossy(), "limit_bytes": 1u64 << 30},
        "profiles_dir": scratch.join("profiles").to_string_lossy(),
        "report": {
            "file": scratch.join("report-<run_id>.json").to_string_lossy(),
            "logs": scratch.join("logs").to_string_lossy(),
        },
    })
}

fn write_doc(scratch: &Path, doc: &Value) -> PathBuf {
    let path = scratch.join("job.json");
    std::fs::write(&path, serde_json::to_string_pretty(doc).expect("json")).expect("write");
    path
}

fn options(events: EventOptions) -> HostOptions<'static> {
    HostOptions {
        cost: None,
        strict: false,
        env: &no_env,
        cancel: CancelToken::new(),
        heartbeat: HEARTBEAT,
        packages: Default::default(),
        capture_process: false,
        events,
    }
}

/// The attempt's directory in the output store: the one `a1-<start>` there.
fn attempt_dir(scratch: &Path) -> PathBuf {
    let logs = scratch.join("logs");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&logs)
        .expect("the output store")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("a1-"))
        })
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    found.remove(0)
}

/// Every record in `bytes`, whole and as the bytes it was written in.
fn records(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let Decoded::Record(_, used) = decode_record(&bytes[at..]) else {
            panic!("not a record at byte {at}");
        };
        out.push(bytes[at..at + used].to_vec());
        at += used;
    }
    out
}

fn kinds_of(events: &[moruna_event::EventMsg]) -> Vec<&'static str> {
    events.iter().map(|e| e.body.kind().name()).collect()
}

/// What a peer on a connection hears.
#[derive(Clone, Default)]
struct Heard(Arc<Mutex<Vec<u8>>>);

impl Write for Heard {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The properties of a run's events that PY-T32 and PY-T33 state, on its records.
fn check_records(all: &[Vec<u8>]) {
    let mut ids = std::collections::HashSet::new();
    let mut last_t = 0;
    for (i, bytes) in all.iter().enumerate() {
        let Decoded::Record(record, _) = decode_record(bytes) else {
            panic!("not a record");
        };
        let header = |key: &str| {
            record
                .headers
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v)
        };
        assert_eq!(record.seq, i as u64 + 1, "seq counts from 1 with no gap");
        assert!(ids.insert(record.msg_id.to_string()), "{}", record.msg_id);
        let start = moruna_event::decode(bytes).expect("an event").start;
        assert_eq!(record.msg_id, format!("1-{start}-{}", record.seq));
        assert_eq!(header("run"), Some(RUN_ID));
        assert_eq!(header("attempt"), Some("1"));
        assert!(
            header("plan").is_some(),
            "a planned run's events carry plan"
        );
        let event = moruna_event::decode(bytes).expect("an event");
        let kind = event.body.kind();
        assert_eq!(record.subject, kind.subject());
        assert!(
            event.ts_ns > 1_000_000_000_000_000_000,
            "ts is unix nanoseconds"
        );
        assert!(event.t_us >= last_t, "time runs forward");
        last_t = event.t_us;
        match kind.name() {
            "task_registered" | "task_waiting" | "task_ready" | "task_started" | "task_phase"
            | "task_completed" | "task_skipped" => {
                assert!(header("task").is_some(), "{kind:?}");
                assert!(header("step").is_some(), "{kind:?}");
            }
            "step_started" | "step_ended" => assert!(header("step").is_some()),
            "attempt_started" | "logs" => assert!(header("task").is_none()),
            _ => {}
        }
    }
}

/// PY-T36, PY-T32, PY-T33 on a real run with no peer: the events are in the output store, a
/// ring the embedder gave has the same records, and `--events-json`'s stream has them as JSON.
#[test]
fn py_t36_a_run_with_no_peer_keeps_its_events() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t36_file");
    let path = write_doc(scratch.path(), &document(scratch.path()));
    let ring = EventRing::new(1 << 20).expect("a ring");
    let printed = Heard::default();
    let json: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(printed.clone())));
    let events = EventOptions {
        ring: Some(ring.clone()),
        json: Some(json),
    };
    let outcome = session::run_file(
        &path,
        None,
        &support::kinds_with(test_kernel),
        &options(events),
    );
    assert_eq!(outcome.code, exit::COMPLETED, "{:?}", outcome.diagnostic);
    assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);

    // The file in the attempt's directory.
    let file = attempt_dir(scratch.path()).join(EVENTS_FILE);
    let bytes = std::fs::read(&file).expect("the events file");
    let in_file = records(&bytes);
    assert!(in_file.len() > 10, "{}", in_file.len());
    check_records(&in_file);
    let events = moruna_event::read_records(&bytes).expect("events");
    let kinds = kinds_of(&events);
    assert_eq!(kinds[0], "attempt_started");
    assert!(kinds.contains(&"step_started") && kinds.contains(&"step_ended"));
    assert_eq!(*kinds.last().unwrap(), "logs");
    assert!(events.iter().all(|e| e.run.as_deref() == Some(RUN_ID)));

    // The ring has the records the file has, byte for byte.
    let mut drained = Vec::new();
    let n = ring.drain(|r| drained.push(r.to_vec())).expect("a drain");
    assert_eq!(n, in_file.len());
    assert_eq!(drained, in_file);
    assert_eq!(ring.dropped(), 0);
    assert_eq!(
        ring.drain(|_| panic!("a record is delivered once"))
            .unwrap(),
        0
    );

    // The JSON stream has the same events with the names of version 6 and `v` 7.
    let text = String::from_utf8(printed.0.lock().unwrap().clone()).expect("UTF-8");
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect();
    assert_eq!(lines.len(), in_file.len());
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(line["type"], "event");
        assert_eq!(line["v"], 7);
        assert_eq!(line["attempt"], 1);
        assert_eq!(line["seq"], i as u64 + 1);
        assert_eq!(line["kind"], kinds[i]);
        assert_eq!(line["start"].as_str().unwrap().len(), 32);
    }

    // The run's output records are the same record, in segments of records.
    let segments: Vec<_> = std::fs::read_dir(attempt_dir(scratch.path()).join("_engine"))
        .map(|d| d.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    for segment in segments {
        let name = segment.file_name().to_string_lossy().into_owned();
        assert!(name.ends_with(".mrec"), "{name}");
    }
}

/// With a peer the events are frames on the connection and no file is written.
#[test]
fn py_t36_a_run_with_a_peer_tells_it_frames() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t36_peer");
    let doc = document(scratch.path());
    let heard = Heard::default();
    let wire = Wire::new(Some(Box::new(heard.clone())), Instant::now());
    let outcome = session::run_job(
        moruna_job::Job::from_v1(&moruna_job::JobSpec::from_value(doc).expect("a document")),
        &support::kinds_with(test_kernel),
        &options(EventOptions::default()),
        &wire,
        None,
        Mode::Run,
    );
    assert_eq!(outcome.code, exit::COMPLETED, "{:?}", outcome.diagnostic);
    let bytes = heard.0.lock().unwrap().clone();
    // Every event is a frame: the mark, then a record, then the next message.
    let mut reader = BufReader::new(std::io::Cursor::new(bytes.clone()));
    let mut frames = Vec::new();
    let mut lines = 0;
    while let Some(message) = protocol::read_message(&mut reader, 1 << 20).expect("a message") {
        match message {
            Message::Event(record) => frames.push(record),
            Message::Line(line) => {
                assert!(line.starts_with('{'), "{line}");
                lines += 1;
            }
        }
    }
    assert!(lines >= 2, "the report and the exit are lines");
    assert!(frames.len() > 10);
    check_records(&frames);
    // No events file: the peer is the place.
    let logs = scratch.path().join("logs");
    let mut files = Vec::new();
    for entry in walk(&logs) {
        files.push(entry);
    }
    assert!(
        files.iter().all(|f| f.file_name().unwrap() != EVENTS_FILE),
        "{files:?}"
    );
    // Read as JSON the stream is lines and event lines, in the order sent.
    let all = protocol::json_messages(&bytes).expect("JSON messages");
    assert_eq!(all[0]["type"], "event");
    assert_eq!(
        all.iter().filter(|m| m["type"] == "event").count(),
        frames.len()
    );
    assert_eq!(all.last().unwrap()["type"], "exit");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// A ring with no room refuses the record and counts it; the run does not wait and says so.
#[test]
fn py_t36_a_ring_without_room_refuses_and_the_run_says_so() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t36_small");
    let path = write_doc(scratch.path(), &document(scratch.path()));
    let ring = EventRing::new(64).expect("a ring");
    let events = EventOptions {
        ring: Some(ring.clone()),
        json: None,
    };
    let outcome = session::run_file(
        &path,
        None,
        &support::kinds_with(test_kernel),
        &options(events),
    );
    assert_eq!(outcome.code, exit::COMPLETED, "{:?}", outcome.diagnostic);
    assert!(ring.dropped() > 0);
    assert!(
        outcome.notes.iter().any(|n| n.contains("had no room")),
        "{:?}",
        outcome.notes
    );
    // The file still has every event: the ring is a copy, not the place.
    let bytes = std::fs::read(attempt_dir(scratch.path()).join(EVENTS_FILE)).expect("the file");
    check_records(&records(&bytes));
    // What the ring kept is whole records.
    let mut kept = 0;
    ring.drain(|r| {
        moruna_event::decode(r).expect("a whole event");
        kept += 1;
    })
    .expect("a drain");
    assert!(kept < records(&bytes).len());
}

/// The output store's segments are files of records, and `read_producer` reads them back.
#[test]
fn py_t36_output_records_are_in_segments_of_records() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t36_logs");
    let path = write_doc(scratch.path(), &document(scratch.path()));
    let outcome = session::run_file(
        &path,
        None,
        &support::kinds_with(test_kernel),
        &options(EventOptions::default()),
    );
    assert_eq!(outcome.code, exit::COMPLETED);
    // The engine's own producer wrote whatever the process printed; a producer that wrote
    // nothing has no directory, and the reader says so with no records.
    assert!(
        logs::read_producer(&attempt_dir(scratch.path()).join("no-such-task"))
            .expect("none")
            .is_empty()
    );
}

/// `JsonLines` writes lines as they are and an event's frame as the line its record renders,
/// however the bytes are cut on the way, and refuses a frame that is not an event.
#[test]
fn py_t36_json_lines_renders_a_relayed_stream() {
    let mut encoder = moruna_event::Encoder::new();
    let stamp = moruna_event::Stamp {
        run: Some(RUN_ID),
        plan: None,
        attempt: 1,
        start: "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
    };
    let mut stream = b"{\"type\":\"hello\"}\n".to_vec();
    let mut expected = String::from("{\"type\":\"hello\"}\n");
    for seq in 1..=3u64 {
        let event = moruna_event::Event::CancelAcknowledged;
        let record = encoder
            .encode(&stamp, seq, 0, seq, &event)
            .expect("a record")
            .to_vec();
        stream.push(moruna_event::frame::FRAME);
        stream.extend_from_slice(&record);
        expected.push_str(&moruna_event::decode(&record).expect("an event").json_line());
    }
    stream.extend_from_slice(b"{\"type\":\"exit\",\"code\":0}\n");
    expected.push_str("{\"type\":\"exit\",\"code\":0}\n");
    for chunk in [1usize, 2, 5, 7, 64, stream.len()] {
        let mut out = Vec::new();
        {
            let mut rendered = JsonLines::new(&mut out);
            for piece in stream.chunks(chunk) {
                rendered.write_all(piece).expect("rendered");
            }
            rendered.flush().expect("flushed");
        }
        assert_eq!(String::from_utf8(out).expect("UTF-8"), expected, "{chunk}");
    }
    // A frame whose bytes are not an event stops the stream.
    let mut bad = vec![moruna_event::frame::FRAME];
    bad.extend_from_slice(&40u32.to_le_bytes());
    bad.extend_from_slice(&[7u8; 40]);
    let mut rendered = JsonLines::new(Vec::new());
    assert!(rendered.write_all(&bad).is_err());
    // A length no record has is refused before anything is waited for.
    let mut rendered = JsonLines::new(Vec::new());
    let mut huge = vec![moruna_event::frame::FRAME];
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(rendered.write_all(&huge).is_err());
}

/// The protocol's reader cuts lines and frames apart, and refuses a frame that claims a length
/// no record has.
#[test]
fn py_t36_the_reader_of_the_protocol_cuts_frames_and_lines() {
    let mut encoder = moruna_event::Encoder::new();
    let stamp = moruna_event::Stamp {
        run: None,
        plan: None,
        attempt: 2,
        start: "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
    };
    // A record whose bytes contain newlines is one message.
    let event = moruna_event::Event::TaskFailed {
        task: None,
        step: None,
        code: moruna_event::FailureCode::IoError,
        message: "a\nb\nc".to_string(),
        scope: moruna_event::FailureScope::Attempt,
        retryable: true,
    };
    let record = encoder
        .encode(&stamp, 1, 0, 1, &event)
        .expect("a record")
        .to_vec();
    let mut bytes = b"{\"type\":\"hello\"}\n".to_vec();
    moruna_event::frame::write_frame(&mut bytes, &mut Vec::new(), &record).expect("a frame");
    bytes.extend_from_slice(b"{\"type\":\"exit\",\"code\":0}\n");
    let mut reader = BufReader::new(std::io::Cursor::new(bytes));
    let mut seen = Vec::new();
    while let Some(m) = protocol::read_message(&mut reader, 1 << 10).expect("a message") {
        seen.push(m);
    }
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0], Message::Line("{\"type\":\"hello\"}\n".into()));
    assert_eq!(seen[1], Message::Event(record));
    let mut huge = vec![moruna_event::frame::FRAME];
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut reader = BufReader::new(std::io::Cursor::new(huge));
    assert!(protocol::read_message(&mut reader, 1 << 10).is_err());
    let mut cut = vec![moruna_event::frame::FRAME];
    cut.extend_from_slice(&100u32.to_le_bytes());
    cut.extend_from_slice(&[0u8; 10]);
    let mut reader = BufReader::new(std::io::Cursor::new(cut));
    assert!(protocol::read_message(&mut reader, 1 << 10).is_err());
}

/// Two processes run one attempt (a host that ran the document twice): every event of each has
/// the counts from 1, so `seq` repeats, and no `msg_id` does (PY-T33).
#[test]
fn py_t33_two_processes_of_one_attempt_tell_disjoint_ids() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t33_twice");
    let path = write_doc(scratch.path(), &document(scratch.path()));
    for _ in 0..2 {
        let outcome = session::run_file(
            &path,
            None,
            &support::kinds_with(test_kernel),
            &options(EventOptions::default()),
        );
        assert_eq!(outcome.code, exit::COMPLETED, "{:?}", outcome.diagnostic);
    }
    let mut ids = std::collections::HashSet::new();
    let mut starts = std::collections::HashSet::new();
    let mut total = 0;
    for entry in std::fs::read_dir(scratch.path().join("logs")).expect("the store") {
        let dir = entry.expect("an entry").path();
        let bytes = std::fs::read(dir.join(EVENTS_FILE)).expect("the events file");
        for event in moruna_event::read_records(&bytes).expect("events") {
            assert_eq!(event.attempt, 1);
            starts.insert(event.start.clone());
            ids.insert(event.msg_id);
            total += 1;
        }
    }
    assert_eq!(starts.len(), 2);
    assert_eq!(ids.len(), total, "a msg_id was told twice");
}
