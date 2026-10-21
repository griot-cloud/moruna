use std::sync::Mutex;

use super::*;

fn pieces(assembler: &mut Assembler, chunks: &[&[u8]]) -> Vec<Piece> {
    let mut out = Vec::new();
    let now = Instant::now();
    for (i, chunk) in chunks.iter().enumerate() {
        assembler.push(chunk, i as u64, now, &mut out);
    }
    out
}

fn texts(pieces: &[Piece]) -> Vec<&str> {
    pieces.iter().map(|p| p.text.as_str()).collect()
}

/// Lines are records without their newline, however the bytes were cut on the way.
#[test]
fn lines_are_kept_whole_across_reads() {
    let mut a = Assembler::new(false);
    let out = pieces(&mut a, &[b"one\ntw", b"o\n", b"\nthree"]);
    assert_eq!(texts(&out), ["one", "two", ""]);
    assert!(
        out.iter()
            .all(|p| !p.partial && !p.continued && p.truncated == 0)
    );
    let mut rest = Vec::new();
    a.eof(&mut rest);
    assert_eq!(texts(&rest), ["three"]);
    assert!(!rest[0].partial);
}

/// A character split between two reads is never split between two records.
#[test]
fn a_utf8_character_is_never_split() {
    let euro = "€".as_bytes();
    let mut a = Assembler::new(false);
    let out = pieces(&mut a, &[&[b'a', euro[0]], &euro[1..2], &[euro[2], b'\n']]);
    assert_eq!(texts(&out), ["a€"]);
    // A long line of three-byte characters is cut on character boundaries.
    let line: String = "€".repeat(RECORD_MAX);
    let mut a = Assembler::new(false);
    let out = pieces(&mut a, &[line.as_bytes(), b"\n"]);
    assert!(out.len() > 1);
    assert_eq!(
        out.iter().map(|p| p.text.as_str()).collect::<String>(),
        line
    );
    assert!(out[..out.len() - 1].iter().all(|p| p.partial));
    assert!(!out[out.len() - 1].partial);
    assert!(!out[0].continued && out[1..].iter().all(|p| p.continued));
    assert!(out.iter().all(|p| p.text.len() <= RECORD_MAX));
}

/// Past `LINE_MAX` a line is cut, and the last record of it says by how much.
#[test]
fn a_line_past_its_limit_is_cut_and_counted() {
    let mut a = Assembler::new(false);
    let line = vec![b'x'; LINE_MAX + 1000];
    let out = pieces(&mut a, &[&line, b"\nnext\n"]);
    let kept: usize = out[..out.len() - 1].iter().map(|p| p.text.len()).sum();
    assert_eq!(kept, LINE_MAX);
    let last = &out[out.len() - 2];
    assert_eq!(last.truncated, 1000);
    assert!(!last.partial);
    assert_eq!(out[out.len() - 1].text, "next");
    assert!(!out[out.len() - 1].continued);
}

/// A traceback on standard error is one record, a chained one included; the line after it is
/// its own.
#[test]
fn a_traceback_is_one_record() {
    let mut a = Assembler::new(true);
    let text = "before\n\
                Traceback (most recent call last):\n  File \"k.py\", line 3, in f\n    raise ValueError('x')\n\
                ValueError: x\n\
                \n\
                During handling of the above exception, another exception occurred:\n\
                \n\
                Traceback (most recent call last):\n  File \"k.py\", line 5, in f\n\
                KeyError: 'y'\n\
                after\n";
    let out = pieces(&mut a, &[text.as_bytes()]);
    assert_eq!(out.len(), 3, "{out:?}");
    assert_eq!(out[0].text, "before");
    assert!(out[1].text.starts_with("Traceback"));
    assert!(out[1].text.contains("ValueError: x"));
    assert!(out[1].text.contains("During handling"));
    assert!(out[1].text.ends_with("KeyError: 'y'"));
    assert_eq!(out[2].text, "after");
    // Standard output groups nothing.
    let mut b = Assembler::new(false);
    assert_eq!(pieces(&mut b, &[text.as_bytes()]).len(), 12);
}

/// An unfinished line goes out as a partial record after it has waited, and the rest of the
/// line continues it; a traceback that stopped coming goes out as it is.
#[test]
fn what_waits_too_long_goes_out() {
    let mut a = Assembler::new(true);
    let t0 = Instant::now();
    let mut out = Vec::new();
    a.push(b"progress 50%", 1, t0, &mut out);
    a.tick(t0 + PARTIAL_AFTER / 2, &mut out);
    assert!(out.is_empty());
    a.tick(t0 + PARTIAL_AFTER, &mut out);
    assert_eq!(texts(&out), ["progress 50%"]);
    assert!(out[0].partial && !out[0].continued);
    out.clear();
    a.push(b" 100%\n", 2, t0 + PARTIAL_AFTER, &mut out);
    assert_eq!(texts(&out), [" 100%"]);
    assert!(!out[0].partial && out[0].continued);
    out.clear();
    a.push(
        b"Traceback (most recent call last):\n  File \"k\"\n",
        3,
        t0,
        &mut out,
    );
    assert!(out.is_empty());
    a.tick(t0 + PARTIAL_AFTER, &mut out);
    assert_eq!(out.len(), 1);
    assert!(out[0].text.ends_with("File \"k\""));
}

struct Told {
    events: Arc<Mutex<Vec<Event>>>,
    notes: Arc<Mutex<Vec<String>>>,
}

fn told() -> (Told, Tell) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let notes = Arc::new(Mutex::new(Vec::new()));
    let (e, n) = (Arc::clone(&events), Arc::clone(&notes));
    (
        Told { events, notes },
        Tell {
            event: Box::new(move |_, event| e.lock().unwrap().push(event)),
            note: Box::new(move |note| n.lock().unwrap().push(note)),
        },
    )
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "moruna-logs-{tag}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

const START: &str = "0123456789abcdef0123456789abcdef";

fn last_reach(events: &[Event]) -> Vec<LogReach> {
    match events.last() {
        Some(Event::Logs { producers }) => producers.clone(),
        other => panic!("the last event is not the store's reach: {other:?}"),
    }
}

/// Concurrent stages writing interleaved standard output and error: each record is its
/// stage's, numbered with no gap per producer, and the last reach counts them all, drained.
#[test]
fn interleaved_output_of_concurrent_stages_is_attributed() {
    let dir = scratch("interleaved");
    let (told, tell) = told();
    let capture = Capture::start(
        attempt_dir(&dir, 1, START),
        1,
        START.into(),
        vec!["src".into(), "k1".into(), "k2".into(), "snk".into()],
        Instant::now(),
        tell,
    );
    let sink = capture.sink();
    let writers: Vec<_> = [1u16, 2u16]
        .into_iter()
        .map(|stage| {
            let sink = Arc::clone(&sink);
            std::thread::spawn(move || {
                let _working = moruna_kernel::output::enter(&sink, stage);
                for i in 0..200 {
                    let line = format!("stage {stage} line {i}\n");
                    // Half a line on each call: the other stage writes between the halves.
                    let (a, b) = line.as_bytes().split_at(line.len() / 2);
                    assert!(moruna_kernel::output::write(Stream::Stdout, a));
                    assert!(moruna_kernel::output::write(Stream::Stdout, b));
                    assert!(moruna_kernel::output::write(
                        Stream::Stderr,
                        format!("err {stage} {i}\n").as_bytes()
                    ));
                }
                moruna_kernel::output::log(Level::Warning, &format!("stage {stage} done"));
            })
        })
        .collect();
    sink.bytes(None, Stream::Stderr, b"the engine's own line\n");
    for w in writers {
        w.join().unwrap();
    }
    assert!(capture.finish(FINISH_WAIT));
    for (stage, task) in [(1u16, "k1"), (2u16, "k2")] {
        let records = read_producer(&attempt_dir(&dir, 1, START).join(task)).unwrap();
        assert_eq!(records.len(), 401);
        for (i, r) in records.iter().enumerate() {
            assert_eq!(r["seq"], (i + 1) as u64);
            assert_eq!(r["task"], task);
            assert_eq!(r["type"], "event");
            assert_eq!(r["kind"], "log");
            assert_eq!(r["v"], EVENTS_VERSION);
            assert_eq!(r["id"], format!("{START}/{task}/{}", i + 1));
        }
        let out: Vec<&str> = records
            .iter()
            .filter(|r| r["stream"] == "stdout")
            .map(|r| r["text"].as_str().unwrap())
            .collect();
        assert_eq!(out.len(), 200);
        for (i, line) in out.iter().enumerate() {
            assert_eq!(*line, format!("stage {stage} line {i}"));
        }
        assert!(
            records
                .iter()
                .filter(|r| r["stream"] == "stderr")
                .all(|r| r["level"].is_null())
        );
        let last = records.last().unwrap();
        assert_eq!(last["stream"], "structured");
        assert_eq!(last["level"], "warning");
    }
    let engine = read_producer(&attempt_dir(&dir, 1, START).join(ENGINE)).unwrap();
    assert_eq!(engine.len(), 1);
    assert!(engine[0]["task"].is_null());
    let reach = last_reach(&told.events.lock().unwrap());
    assert_eq!(reach.len(), 5);
    assert!(reach.iter().all(|r| r.drained && r.dropped == 0));
    let k1 = reach
        .iter()
        .find(|r| r.task.as_deref() == Some("k1"))
        .unwrap();
    assert_eq!(k1.records, 401);
    assert!(told.notes.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A flood: memory stays bounded, what is past the rate is dropped and counted, the store says
/// where, and the reach is told while the flood goes on.
#[test]
fn a_flood_is_bounded_and_counted() {
    let dir = scratch("flood");
    let (told, tell) = told();
    let capture = Capture::start(
        attempt_dir(&dir, 1, START),
        1,
        START.into(),
        vec!["src".into(), "k1".into(), "snk".into()],
        Instant::now(),
        tell,
    );
    let sink = capture.sink();
    let line = vec![b'z'; 1000];
    let started = Instant::now();
    let mut peak = 0usize;
    while started.elapsed() < LOGS_EVERY + Duration::from_millis(700) {
        for _ in 0..200 {
            sink.bytes(Some(1), Stream::Stdout, &line);
            sink.bytes(Some(1), Stream::Stdout, b"\n");
        }
        peak = peak.max(capture.shared.queued.load(Ordering::Acquire));
    }
    assert!(peak <= QUEUE_BYTES, "{peak} bytes queued");
    let during = told.events.lock().unwrap().len();
    assert!(during >= 1, "the reach was not told during the flood");
    assert!(capture.finish(FINISH_WAIT));
    let reach = last_reach(&told.events.lock().unwrap());
    let k1 = reach
        .iter()
        .find(|r| r.task.as_deref() == Some("k1"))
        .unwrap();
    assert!(k1.dropped > 0, "{k1:?}");
    assert!(k1.records > 0);
    let records = read_producer(&attempt_dir(&dir, 1, START).join("k1")).unwrap();
    assert_eq!(records.len() as u64, k1.records);
    let marks: Vec<&serde_json::Value> = records
        .iter()
        .filter(|r| r["stream"] == "platform")
        .collect();
    assert!(!marks.is_empty());
    assert!(
        marks[0]["text"]
            .as_str()
            .unwrap()
            .contains("of this output were dropped here")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Output written after its stage's work ended, by a writer still holding the sink, is kept
/// under the same task and attempt before the last reach is told.
#[test]
fn output_after_its_task_ended_is_drained() {
    let dir = scratch("late");
    let (told, tell) = told();
    let capture = Capture::start(
        attempt_dir(&dir, 2, START),
        2,
        START.into(),
        vec!["src".into(), "k1".into(), "snk".into()],
        Instant::now(),
        tell,
    );
    let sink = capture.sink();
    let hold = sink.hold();
    let late = {
        let sink = Arc::clone(&sink);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            sink.bytes(Some(1), Stream::Stderr, b"written late\n");
            drop(hold);
        })
    };
    assert!(capture.finish(FINISH_WAIT));
    late.join().unwrap();
    let records = read_producer(&attempt_dir(&dir, 2, START).join("k1")).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["text"], "written late");
    assert_eq!(records[0]["attempt"], 2);
    let reach = last_reach(&told.events.lock().unwrap());
    assert!(reach.iter().all(|r| r.drained));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A store that cannot be made is a note, and what was written is dropped and counted.
#[test]
fn a_store_that_cannot_be_written_is_told() {
    let file = scratch("blocked");
    std::fs::write(&file, b"a file where the store would be").unwrap();
    let (told, tell) = told();
    let capture = Capture::start(
        file.join("store"),
        1,
        START.into(),
        vec!["src".into(), "snk".into()],
        Instant::now(),
        tell,
    );
    capture.sink().bytes(Some(0), Stream::Stdout, b"lost\n");
    assert!(capture.finish(FINISH_WAIT));
    assert_eq!(told.notes.lock().unwrap().len(), 1);
    let reach = last_reach(&told.events.lock().unwrap());
    let src = reach
        .iter()
        .find(|r| r.task.as_deref() == Some("src"))
        .unwrap();
    assert_eq!((src.records, src.dropped, src.drained), (0, 1, true));
    let _ = std::fs::remove_file(&file);
}

/// The process's own standard output and error, while captured, are the engine's output in the
/// store; given back, they go where they went. Run in a child process: a test's process is not
/// a run's alone.
#[cfg(unix)]
#[test]
fn the_process_own_output_is_the_engines() {
    const CHILD: &str = "MORUNA_LOGS_TEST_CHILD";
    if let Some(dir) = std::env::var_os(CHILD) {
        let dir = PathBuf::from(dir);
        let (_told, tell) = told();
        let capture = Capture::start(
            dir.clone(),
            1,
            START.into(),
            vec!["src".into(), "snk".into()],
            Instant::now(),
            tell,
        );
        let streams = ProcessStreams::redirect(capture.sink()).expect("redirected");
        println!("printed by a kernel with no stage");
        eprintln!("an engine diagnostic");
        streams.restore();
        assert!(capture.finish(FINISH_WAIT));
        println!("after the run");
        return;
    }
    let dir = scratch("process");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "host::logs::tests::the_process_own_output_is_the_engines",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &dir)
        .output()
        .expect("the child runs");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains("printed by a kernel"), "{stdout}");
    assert!(!stderr.contains("an engine diagnostic"), "{stderr}");
    assert!(stdout.contains("after the run"), "{stdout}");
    let records = read_producer(&dir.join(ENGINE)).unwrap();
    let lines: Vec<(&str, &str)> = records
        .iter()
        .map(|r| (r["stream"].as_str().unwrap(), r["text"].as_str().unwrap()))
        .collect();
    assert!(
        lines.contains(&("stdout", "printed by a kernel with no stage")),
        "{lines:?}"
    );
    assert!(
        lines.contains(&("stderr", "an engine diagnostic")),
        "{lines:?}"
    );
    assert!(records.iter().all(|r| r["task"].is_null()));
    let _ = std::fs::remove_dir_all(&dir);
}
