use std::sync::{Arc, Mutex};
use std::time::Instant;

use moruna_kernel::{MorunaError, StageCounters, StageEvent};
use serde_json::{Value, json};

use super::*;
use moruna_job::{Edge, Job, KindDoc, Task};

/// The plan of a linear chain of tasks named `ids`: the first the source, the last the sink.
fn refs(ids: &[&str]) -> Plan {
    let entry = || KindDoc::new("any", serde_json::Map::new());
    let last = ids.len() - 1;
    let tasks = ids
        .iter()
        .enumerate()
        .map(|(at, id)| match at {
            0 => Task::source(*id, entry()),
            at if at == last => Task::sink(*id, entry()),
            _ => Task::kernel(*id, entry()),
        })
        .collect();
    let edges = ids.windows(2).map(|w| Edge::between(w[0], w[1])).collect();
    Plan::of(&Job::new(tasks, edges), None)
}

/// A lifecycle whose events are kept, as the wire would write them (kind and fields).
fn lifecycle(ids: &[&str]) -> (Arc<Lifecycle>, Arc<Mutex<Vec<Value>>>) {
    let told: Arc<Mutex<Vec<Value>>> = Arc::default();
    let kept = told.clone();
    let lifecycle = Lifecycle::new(
        refs(ids),
        Instant::now(),
        Box::new(move |t_us, event| {
            let mut value = serde_json::to_value(&event).expect("an event is JSON");
            value["t_us"] = json!(t_us);
            kept.lock().unwrap().push(value);
        }),
    );
    (lifecycle, told)
}

/// `kind task` for each event, or `kind task phase|outcome|reason`, compactly.
fn said(told: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
    told.lock()
        .unwrap()
        .iter()
        .map(|e| {
            let kind = e["kind"].as_str().unwrap();
            let task = e["task"].as_str().unwrap_or("-");
            let detail = ["phase", "outcome", "reason", "code"]
                .iter()
                .find_map(|k| e.get(*k).and_then(Value::as_str))
                .map(|d| format!(" {d}"))
                .unwrap_or_default();
            format!("{kind} {task}{detail}")
        })
        .collect()
}

fn counters(rows: u64) -> StageCounters {
    StageCounters {
        morsels: 2,
        rows_in: rows,
        rows_out: rows,
        bytes_in: rows * 8,
        bytes_out: rows * 8,
        skipped: 0,
        errors: 0,
    }
}

/// The source, one kernel and the sink of a run that probes, then runs, then completes: every
/// task is registered and waits, each task the probe starts is ready and then calibrating,
/// `Running` moves the calibrating tasks to their own phases, each queue's close completes its
/// producer and drains its consumer, and the sink commits and completes.
#[test]
fn a_completed_run_tells_each_task_once_in_order() {
    let (lc, told) = lifecycle(&["src", "k1", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::SourceReading { source: 0 });
    lc.on(&StageEvent::Fed { queue: 0 });
    lc.on(&StageEvent::StageStarted { stage: 1 });
    lc.on(&StageEvent::Fed { queue: 1 });
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::SinkWriting { sink: 0 });
    lc.on(&StageEvent::QueueClosed {
        queue: 0,
        counters: counters(10),
    });
    lc.on(&StageEvent::QueueClosed {
        queue: 1,
        counters: counters(10),
    });
    lc.on(&StageEvent::SinkCommitting { sink: 0 });
    lc.on(&StageEvent::SinkFinished {
        sink: 0,
        counters: counters(10),
    });
    lc.end(Ending::Completed);
    assert_eq!(
        said(&told),
        vec![
            "attempt_started -",
            "task_registered src",
            "task_registered k1",
            "task_registered out",
            "task_waiting src engine_preparing",
            "task_waiting k1 awaiting_input",
            "task_waiting out awaiting_input",
            "task_ready src",
            "task_started src calibrating",
            "task_ready k1",
            "task_started k1 calibrating",
            "task_ready out",
            "task_phase src reading",
            "task_phase k1 applying",
            "task_started out writing",
            "task_completed src succeeded",
            "task_phase k1 draining",
            "task_completed k1 succeeded",
            "task_phase out draining",
            "task_phase out committing",
            "task_completed out succeeded",
        ]
    );
    let told = told.lock().unwrap();
    let registered: Vec<&Value> = told
        .iter()
        .filter(|e| e["kind"] == "task_registered")
        .collect();
    assert_eq!(registered[0]["element"], "source");
    assert_eq!(registered[0]["upstream"], json!([]));
    assert_eq!(registered[1]["element"], "kernel");
    assert_eq!(registered[1]["stage"], 1);
    assert_eq!(registered[1]["upstream"], json!(["src"]));
    assert_eq!(registered[2]["element"], "sink");
    assert_eq!(registered[2]["stage"], 2);
    let waits = told
        .iter()
        .find(|e| e["task"] == "k1" && e["kind"] == "task_waiting");
    assert_eq!(waits.unwrap()["blocking"], json!(["src"]));
    let done = told
        .iter()
        .find(|e| e["task"] == "out" && e["kind"] == "task_completed")
        .unwrap();
    assert_eq!(done["counters"]["rows_out"], 10);
    assert!(done["duration_us"].is_u64());
    let mut last = 0;
    for e in told.iter() {
        let t = e["t_us"].as_u64().unwrap();
        assert!(t >= last, "time runs forward: {e}");
        last = t;
    }
}

/// Empty input: nothing is read, nothing is fed, every queue closes and the sink commits with
/// no write. The source and the kernel complete without having started (no duration), the sink
/// is ready and starts at its commit.
#[test]
fn empty_input_completes_what_never_had_input() {
    let (lc, told) = lifecycle(&["src", "k1", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::QueueClosed {
        queue: 0,
        counters: StageCounters::default(),
    });
    lc.on(&StageEvent::QueueClosed {
        queue: 1,
        counters: StageCounters::default(),
    });
    lc.on(&StageEvent::SinkCommitting { sink: 0 });
    lc.on(&StageEvent::SinkFinished {
        sink: 0,
        counters: StageCounters::default(),
    });
    lc.end(Ending::Completed);
    let s = said(&told);
    assert_eq!(
        &s[7..],
        &[
            "task_ready src",
            "task_completed src succeeded",
            "task_completed k1 succeeded",
            "task_ready out",
            "task_started out committing",
            "task_completed out succeeded",
        ]
    );
    let told = told.lock().unwrap();
    for task in ["src", "k1"] {
        let done = told
            .iter()
            .find(|e| e["task"] == task && e["kind"] == "task_completed")
            .unwrap();
        assert_eq!(done["duration_us"], Value::Null, "{task} never started");
        assert_eq!(done["counters"]["morsels"], 0);
    }
}

/// A kernel fails: it fails (its code, its stage and morsel, nothing the kernel said), the
/// source still reading is stopped by it, the sink that never started is skipped for it.
#[test]
fn a_kernel_failure_fails_its_task_and_ends_the_others_by_it() {
    let (lc, told) = lifecycle(&["src", "k1", "k2", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::SourceReading { source: 0 });
    lc.on(&StageEvent::Fed { queue: 0 });
    lc.on(&StageEvent::StageStarted { stage: 1 });
    lc.end(Ending::Failed(&MorunaError::Kernel {
        stage: 1,
        seq: 4,
        msg: "value 'secret-123' is not a number".into(),
        features: None,
    }));
    let s = said(&told);
    assert_eq!(
        &s[s.len() - 4..],
        &[
            "task_failed k1 kernel_error",
            "task_completed src stopped",
            "task_skipped k2 failed_task",
            "task_skipped out failed_task",
        ]
    );
    let told = told.lock().unwrap();
    let failed = told.iter().find(|e| e["kind"] == "task_failed").unwrap();
    assert_eq!(failed["scope"], "task");
    assert_eq!(failed["retryable"], false);
    assert_eq!(failed["message"], "kernel stage 1 failed on morsel 4");
    assert!(!failed.to_string().contains("secret"), "{failed}");
    let stopped = told.iter().find(|e| e["kind"] == "task_completed").unwrap();
    assert_eq!(stopped["cause"], "k1");
    assert_eq!(stopped["counters"], Value::Null);
    let skipped = told.iter().find(|e| e["kind"] == "task_skipped").unwrap();
    assert_eq!(skipped["blocking"], "k1");
}

/// A failure no task owns fails the attempt; every task ends by it.
#[test]
fn a_failure_no_task_owns_fails_the_attempt() {
    let (lc, told) = lifecycle(&["src", "out"]);
    lc.begin(false);
    lc.end(Ending::Failed(&MorunaError::Config {
        name: "budget.memory_bytes",
        msg: "below the floor".into(),
    }));
    let s = said(&told);
    assert_eq!(
        &s[s.len() - 3..],
        &[
            "task_failed - config_refused",
            "task_skipped src attempt_failed",
            "task_skipped out attempt_failed",
        ]
    );
    let told = told.lock().unwrap();
    let failed = told.iter().find(|e| e["kind"] == "task_failed").unwrap();
    assert_eq!(failed["scope"], "attempt");
    assert_eq!(failed["task"], Value::Null);
}

/// A sink failure is the sink's; a source failure the source's.
#[test]
fn the_ends_own_their_failures() {
    for (error, owner, code) in [
        (
            MorunaError::Sink("disk full at /tenant/x".into()),
            "out",
            "sink_error",
        ),
        (
            MorunaError::Source {
                split: 3,
                msg: "row 'x'".into(),
            },
            "src",
            "source_error",
        ),
    ] {
        let (lc, told) = lifecycle(&["src", "k1", "out"]);
        lc.begin(false);
        lc.end(Ending::Failed(&error));
        let told = told.lock().unwrap();
        let failed = told.iter().find(|e| e["kind"] == "task_failed").unwrap();
        assert_eq!(failed["task"], owner);
        assert_eq!(failed["code"], code);
        assert_eq!(failed["retryable"], true);
        assert!(!failed["message"].as_str().unwrap().contains('\''));
    }
}

/// A cancel: acknowledged when it reaches the run, then what was running is cancelled, what
/// never started is skipped for it, and the cancel's end comes last.
#[test]
fn a_cancel_is_acknowledged_then_ends_every_task() {
    let (lc, told) = lifecycle(&["src", "k1", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::SourceReading { source: 0 });
    lc.cancel_acknowledged();
    lc.cancel_acknowledged();
    lc.end(Ending::Cancelled);
    let s = said(&told);
    assert_eq!(
        &s[s.len() - 5..],
        &[
            "cancel_acknowledged -",
            "task_completed src cancelled",
            "task_skipped k1 cancelled",
            "task_skipped out cancelled",
            "cancel_ended -",
        ]
    );
    assert_eq!(
        told.lock().unwrap().last().unwrap()["disposition"],
        "cancelled"
    );
}

/// A crash (PY-I11): every task still running fails `internal` with what the panic said, one
/// that ended stays ended, one that never started is skipped because the attempt failed, and a
/// cancel the run heard ends `failed`, never `cancelled`.
#[test]
fn a_crash_fails_what_runs_and_is_never_a_cancel() {
    let (lc, told) = lifecycle(&["src", "k1", "k2", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::SourceReading { source: 0 });
    lc.on(&StageEvent::StageStarted { stage: 1 });
    lc.on(&StageEvent::QueueClosed {
        queue: 0,
        counters: counters(10),
    });
    lc.cancel_acknowledged();
    lc.end(Ending::Crashed("index out of bounds"));
    let s = said(&told);
    assert_eq!(
        &s[s.len() - 4..],
        &[
            "task_failed k1 internal",
            "task_skipped k2 attempt_failed",
            "task_skipped out attempt_failed",
            "cancel_ended -",
        ]
    );
    let told = told.lock().unwrap();
    let failed = told.iter().find(|e| e["kind"] == "task_failed").unwrap();
    assert_eq!(failed["message"], "index out of bounds");
    assert_eq!(failed["scope"], "task");
    assert_eq!(failed["retryable"], false);
    assert_eq!(told.last().unwrap()["disposition"], "failed");
}

/// A crash when no task is running is the attempt's: one `internal` failure naming no task.
#[test]
fn a_crash_with_nothing_running_fails_the_attempt() {
    let (lc, told) = lifecycle(&["src", "out"]);
    lc.begin(false);
    lc.end(Ending::Crashed("the plan broke"));
    lc.end(Ending::Crashed("a second crash"));
    let s = said(&told);
    assert_eq!(
        &s[s.len() - 3..],
        &[
            "task_failed - internal",
            "task_skipped src attempt_failed",
            "task_skipped out attempt_failed",
        ]
    );
    assert_eq!(told.lock().unwrap()[s.len() - 3]["scope"], "attempt");
}

/// A cancel that arrives as the run completes ends `completed`: the run was not stopped by it.
#[test]
fn a_cancel_the_run_outran_says_so() {
    let (lc, told) = lifecycle(&["src", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::QueueClosed {
        queue: 0,
        counters: StageCounters::default(),
    });
    lc.on(&StageEvent::SinkCommitting { sink: 0 });
    lc.on(&StageEvent::SinkFinished {
        sink: 0,
        counters: StageCounters::default(),
    });
    lc.cancel_acknowledged();
    lc.end(Ending::Completed);
    assert_eq!(
        told.lock().unwrap().last().unwrap()["disposition"],
        "completed"
    );
}

/// An ended task is never told of again, whatever arrives late.
#[test]
fn an_ended_task_stays_ended() {
    let (lc, told) = lifecycle(&["src", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::QueueClosed {
        queue: 0,
        counters: StageCounters::default(),
    });
    let before = told.lock().unwrap().len();
    lc.on(&StageEvent::SourceReading { source: 0 });
    lc.on(&StageEvent::QueueClosed {
        queue: 0,
        counters: StageCounters::default(),
    });
    assert_eq!(told.lock().unwrap().len(), before);
}

/// Groups: the members the document gives an element are in its registration.
#[test]
fn a_group_is_registered_with_its_members() {
    let told: Arc<Mutex<Vec<Value>>> = Arc::default();
    let kept = told.clone();
    let mut tasks = refs(&["src", "out"]);
    tasks.nodes[0].task.members = vec!["orders".into(), "customers".into()];
    let lc = Lifecycle::new(
        tasks,
        Instant::now(),
        Box::new(move |_, e| kept.lock().unwrap().push(serde_json::to_value(&e).unwrap())),
    );
    lc.begin(true);
    let told = told.lock().unwrap();
    assert_eq!(told[0]["resumed"], true);
    assert_eq!(told[1]["members"], json!(["orders", "customers"]));
    assert_eq!(told[2]["members"], json!([]));
}

/// PY-I12, the interleaving behind HO-T16's missing `task_ready`: a producer pushes its first
/// morsel and only then tells `Fed` on its own thread, so the consumer, on another thread, can
/// take the morsel and tell its start first. Each consumer is told ready and then started, the
/// late `Fed` tells nothing, and nothing is told twice. The source read while the chain
/// calibrates, before `Running`, is told ready before it is started the same way.
#[test]
fn a_start_that_outruns_its_feed_is_told_ready_first() {
    let (lc, told) = lifecycle(&["src", "k1", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::SourceReading { source: 0 });
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::StageStarted { stage: 1 });
    lc.on(&StageEvent::Fed { queue: 0 });
    lc.on(&StageEvent::SinkWriting { sink: 0 });
    lc.on(&StageEvent::Fed { queue: 1 });
    lc.on(&StageEvent::Fed { queue: 1 });
    assert_eq!(
        &said(&told)[7..],
        &[
            "task_ready src",
            "task_started src calibrating",
            "task_phase src reading",
            "task_ready k1",
            "task_started k1 applying",
            "task_ready out",
            "task_started out writing",
        ]
    );
}

/// A sink that commits with nothing written starts at its commit, and was ready for it first.
#[test]
fn a_sink_that_starts_at_its_commit_is_ready_first() {
    let (lc, told) = lifecycle(&["src", "out"]);
    lc.begin(false);
    lc.on(&StageEvent::Running);
    lc.on(&StageEvent::QueueClosed {
        queue: 0,
        counters: StageCounters::default(),
    });
    lc.on(&StageEvent::SinkCommitting { sink: 0 });
    assert_eq!(
        &said(&told)[5..],
        &[
            "task_ready src",
            "task_completed src succeeded",
            "task_ready out",
            "task_started out committing",
        ]
    );
}
