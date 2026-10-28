//! PY-I11 (12 c, MH 4.3 `event`): every task a run registers gets exactly one end, and a task
//! that started ends completed, failed with its cause, or cancelled, never skipped, whatever ends
//! the run: a cancel at any point, a kernel failure at any point under each error policy, a
//! source failure, a run dropped without a cancel, and a crash. Properties over many points of a
//! real run, each checked on the events the session wrote to its peer. A run let go of ends its
//! tasks cancelled (PY-T26); a run that a panic of Moruna's own unwound ends them failed,
//! `internal`, with what the panic said, and the panic still leaves the session (PY-T27).
//!
//! The session is `moruna run`'s own (`session::run_job`), over a Parquet source, the support
//! kernels and a Parquet sink, with the peer an in-memory writer that also cancels the run when a
//! chosen event is written, so "a cancel at point n" is the same point on every machine.

#![allow(clippy::result_large_err)]

mod support;

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use moruna_kernel::{
    Allocator, BoxFuture, CancelToken, Kernel, Payload, PayloadSpec, Result, RowRange, Seq, Sink,
    SinkSummary, Source, SourceSchema, Split, StageEvent,
};
use moruna_runtime::host::exit;
use moruna_runtime::host::lifecycle::{Lifecycle, task_refs};
use moruna_runtime::host::session::{self, HEARTBEAT, HostOptions, Mode, Outcome, Wire};
use moruna_runtime::job::{BuildOptions, KindDoc, build};
use moruna_runtime::kinds::Registry;
use moruna_runtime::{Components, JobSpec, Runtime, SinkSpec, SourceSpec};
use moruna_testkit::FakeSink;
use serde_json::{Value, json};
use support::{FailOnce, Scratch, Sleeper, one_run_at_a_time, write_parquet};

/// The support kernels by the callable a document names: `pass`, a sleeper of a few
/// milliseconds, or `fail:<n>`, which fails its `n`-th apply.
fn test_kernel(index: usize, callable: &str) -> Result<Arc<dyn Kernel>> {
    Ok(match callable.split_once(':') {
        None if callable == "pass" => Arc::new(Sleeper::new(3)),
        Some(("fail", n)) => Arc::new(FailOnce::new(n.parse().map_err(|_| {
            moruna_kernel::MorunaError::Plan(format!("kernels[{index}]: `{callable}`"))
        })?)),
        _ => {
            return Err(moruna_kernel::MorunaError::Plan(format!(
                "kernels[{index}]: no test kernel `{callable}`"
            )));
        }
    })
}

/// The standard kinds, the support kernels as `python`, and a `python` source whose plan panics
/// on the facade's own thread, after the run's tasks are registered, so the run unwinds out of
/// the session (PY-T27).
fn test_kinds() -> Registry {
    let mut kinds = support::kinds_with(test_kernel);
    let panics = support::Handed::new("python", || SourceSpec::Built(Arc::new(PlanPanics)));
    if let Err(e) = kinds.register_source(Arc::new(panics)) {
        panic!("{e}");
    }
    kinds
}

/// A test-local `Source` (12 k): its `plan` panics.
struct PlanPanics;

impl Source for PlanPanics {
    fn schema(&self) -> SourceSchema {
        SourceSchema::Table(support::schema())
    }

    fn plan(&self) -> Result<Vec<Split>> {
        panic!("the source's plan broke")
    }

    fn read<'a>(
        &'a self,
        _split: &'a Split,
        _rows: Option<RowRange>,
        _alloc: &'a dyn Allocator,
        _tier: moruna_kernel::Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        panic!("a source whose plan broke is never read")
    }

    fn repeatable(&self) -> bool {
        true
    }
}

/// The kinds of a run that crashes on the facade's thread while its tasks run (PY-T27): `fail`
/// fails its third apply, so the error policy ends the run, and `pass` is a sleeper; the sink,
/// `"kind": "python"`, is a resumable fake whose checkpoint panics once that kernel has failed.
/// The scheduler writes the last manifest of a terminated run on the facade's thread (SC f.12),
/// so the panic is on that thread, after the tasks started and before the run's end is told.
fn crashes(fail: support::FailOnce) -> Registry {
    let kernel = fail.shared();
    let mut kinds = support::kinds_with(move |index, callable| -> Result<Arc<dyn Kernel>> {
        Ok(match callable {
            "fail" => Arc::new(kernel.shared()),
            "pass" => Arc::new(Sleeper::new(3)),
            other => {
                return Err(moruna_kernel::MorunaError::Plan(format!(
                    "kernels[{index}]: no test kernel {other:?}"
                )));
            }
        })
    });
    let sink = support::Handed::new("python", move || {
        SinkSpec::Built(Box::new(CheckpointBreaks {
            inner: FakeSink::new().resumable(true),
            fail: fail.shared(),
        }))
    });
    if let Err(e) = kinds.register_sink(Arc::new(sink)) {
        panic!("{e}");
    }
    kinds
}

/// What the sink's checkpoint says as it panics.
const CHECKPOINT_BROKE: &str = "the sink's checkpoint broke";

/// A test-local `Sink` (12 k): a resumable fake whose `checkpoint` panics once `fail` has
/// failed. Before the run starts, the facade and the scheduler ask it whether it checkpoints,
/// and it answers as the fake does.
struct CheckpointBreaks {
    inner: FakeSink,
    fail: support::FailOnce,
}

impl Sink for CheckpointBreaks {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        self.inner.open(schema)
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        self.inner.write(seq, payload)
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        self.inner.finish()
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.inner.committed_seq()
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        if self.fail.failed() {
            panic!("{CHECKPOINT_BROKE}");
        }
        self.inner.checkpoint()
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// `{"kind": "python"}`: an object the caller holds.
fn python_entry() -> KindDoc {
    KindDoc::new("python", serde_json::Map::new())
}

/// Rows and row groups of the input: one morsel a row group, so a run has `GROUPS` applies a
/// kernel stage.
const ROWS: u64 = 8_000;
const GROUPS: u64 = 8;

/// A document over a Parquet file of `GROUPS` row groups, through `kernels`, into Parquet.
fn document(scratch: &Scratch, kernels: &[&str], policy: Value) -> JobSpec {
    let dir = scratch.path();
    let input = dir.join("in.parquet");
    if !input.exists() {
        write_parquet(&input, ROWS, GROUPS);
    }
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    let out = dir.join(format!("out-{}", unique()));
    std::fs::create_dir_all(&out).expect("out");
    let kernels: Vec<Value> = kernels
        .iter()
        .map(|k| json!({"kind": "python", "module": "tests", "callable": k}))
        .collect();
    JobSpec::from_value(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": kernels,
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display())},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "staging": {"dir": staging.to_string_lossy(), "limit_bytes": 1u64 << 30},
        "profiles_dir": dir.join("profiles").to_string_lossy(),
        "error_policy": policy,
        "report": {"file": dir.join("report-<run_id>.json").to_string_lossy()},
    }))
    .expect("the document parses")
}

fn unique() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// How the run is cancelled at its chosen event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cancel {
    /// The surface's token, as a signal handler sets it.
    Token,
    /// The peer's `cancel` message, read by the session's peer thread.
    Message,
}

/// The peer: every line the session writes, and a cancel when the `cancel_at`-th event is
/// written (never, when it is zero), by the token or by a `cancel` message on the session's
/// inbound half.
struct Peer {
    text: Arc<Mutex<Vec<u8>>>,
    events: Arc<AtomicUsize>,
    cancel_at: usize,
    cancel: CancelToken,
    inbound: Option<std::io::PipeWriter>,
}

impl Write for Peer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let lines = bytes.iter().filter(|b| **b == b'\n').count();
        let is_event = bytes.windows(15).any(|w| w == b"\"type\":\"event\"");
        self.text
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        if is_event && lines > 0 {
            let seen = self.events.fetch_add(1, Ordering::SeqCst) + 1;
            if seen == self.cancel_at {
                match self.inbound.as_mut() {
                    Some(inbound) => inbound.write_all(b"{\"type\":\"cancel\"}\n")?,
                    None => self.cancel.cancel(),
                }
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run `job` as `moruna run` does, cancelling by `how` when the `cancel_at`-th event is written.
/// The outcome and every event, in order.
fn run_cancelled(job: JobSpec, cancel_at: usize, how: Cancel) -> (Outcome, Vec<Value>) {
    let cancel = CancelToken::new();
    let text = Arc::new(Mutex::new(Vec::new()));
    let (reader, inbound) = match how {
        Cancel::Token => (None, None),
        Cancel::Message => {
            let (reader, writer) = std::io::pipe().expect("a pipe");
            let reader: Box<dyn std::io::BufRead + Send> =
                Box::new(std::io::BufReader::new(reader));
            (Some(reader), Some(writer))
        }
    };
    let peer = Peer {
        text: text.clone(),
        events: Arc::new(AtomicUsize::new(0)),
        cancel_at,
        cancel: cancel.clone(),
        inbound,
    };
    let wire = Wire::new(Some(Box::new(peer)), Instant::now());
    let opts = HostOptions {
        strict: false,
        env: &no_env,
        cancel,
        heartbeat: HEARTBEAT,
        packages: Default::default(),
        capture_process: false,
    };
    let outcome = session::run_job(job, &test_kinds(), &opts, &wire, reader, Mode::Run);
    (outcome, events_in(&text))
}

/// Run `job` as `moruna run` does, uncancelled.
fn run(job: JobSpec, cancel_at: usize) -> (Outcome, Vec<Value>) {
    run_cancelled(job, cancel_at, Cancel::Token)
}

/// The events among the lines the peer read.
fn events_in(text: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
    let text = text.lock().unwrap_or_else(|e| e.into_inner()).clone();
    String::from_utf8(text)
        .expect("the peer reads UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a JSON line"))
        .filter(|m| m["type"] == "event")
        .collect()
}

/// The property, on one run's events: every registered task ends exactly once; a task that
/// started ends completed or failed, never skipped; nothing is told of a task after its end;
/// and a failed task carries its cause.
fn one_end_each(events: &[Value], what: &str) {
    let registered: Vec<String> = events
        .iter()
        .filter(|e| e["kind"] == "task_registered")
        .map(|e| e["task"].as_str().expect("a task id").to_string())
        .collect();
    assert!(!registered.is_empty(), "{what}: no task was registered");
    for task in &registered {
        let told: Vec<(usize, &Value)> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| e["task"] == task.as_str())
            .collect();
        let ends: Vec<&(usize, &Value)> = told
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e["kind"].as_str(),
                    Some("task_completed" | "task_failed" | "task_skipped")
                )
            })
            .collect();
        assert_eq!(
            ends.len(),
            1,
            "{what}: task {task} ends exactly once: {ends:?}\nevents: {events:#?}"
        );
        let (at, end) = ends[0];
        let started = told.iter().any(|(_, e)| e["kind"] == "task_started");
        if started {
            assert_ne!(
                end["kind"], "task_skipped",
                "{what}: task {task} started and was told skipped: {end}"
            );
        }
        if end["kind"] == "task_failed" {
            assert!(
                end["message"].as_str().is_some_and(|m| !m.is_empty()),
                "{what}: task {task} failed with no cause: {end}"
            );
        }
        let after: Vec<&Value> = told
            .iter()
            .filter(|(i, _)| i > at)
            .map(|(_, e)| *e)
            .collect();
        assert!(
            after.is_empty(),
            "{what}: task {task} was told of after its end: {after:?}"
        );
    }
}

/// PY-T23 every_cancel_point: the run is cancelled when its first event is written, when its
/// second is, and so on to its last, by the token and by the peer's `cancel` message; every run
/// exits completed or cancelled and satisfies the property, and a cancel the run acknowledged
/// has exactly one `cancel_ended`.
#[test]
fn py_t23_every_cancel_point() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t23");
    let kernels = ["pass", "pass"];
    let (whole, events) = run(document(&scratch, &kernels, json!("terminate")), 0);
    assert_eq!(whole.code, exit::COMPLETED, "{:?}", whole.diagnostic);
    one_end_each(&events, "the uncancelled run");
    for at in 1..=events.len() {
        for how in [Cancel::Token, Cancel::Message] {
            let what = format!("cancelled by {how:?} at event {at} of {}", events.len());
            let job = document(&scratch, &kernels, json!("terminate"));
            let (outcome, told) = run_cancelled(job, at, how);
            assert!(
                matches!(outcome.code, exit::CANCELLED | exit::COMPLETED),
                "{what}: exit {} {:?}",
                outcome.code,
                outcome.diagnostic
            );
            one_end_each(&told, &what);
            let kinds: Vec<&str> = told.iter().filter_map(|e| e["kind"].as_str()).collect();
            let acknowledged = kinds
                .iter()
                .filter(|k| **k == "cancel_acknowledged")
                .count();
            let cancel_ended = kinds.iter().filter(|k| **k == "cancel_ended").count();
            assert!(acknowledged <= 1, "{what}: {kinds:?}");
            assert_eq!(
                cancel_ended, acknowledged,
                "{what}: a cancel the run heard has its one end: {kinds:?}"
            );
        }
    }
}

/// PY-T24 every_failure_point: a kernel fails its first apply, its second, and so on to its
/// last, under `terminate`, `skip` and `budget(2)`, as the first of two stages and as the
/// second; every run satisfies the property, and exactly one failure is told when the policy
/// ends the run.
#[test]
fn py_t24_every_failure_point() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t24");
    for policy in [json!("terminate"), json!("skip"), json!({"budget": 2})] {
        for n in 1..=GROUPS {
            let failing = format!("fail:{n}");
            for kernels in [[failing.as_str(), "pass"], ["pass", failing.as_str()]] {
                let what = format!("{kernels:?} under {policy}");
                let (outcome, told) = run(document(&scratch, &kernels, policy.clone()), 0);
                one_end_each(&told, &what);
                if outcome.code == exit::KERNEL_ERROR {
                    let failed: Vec<&Value> =
                        told.iter().filter(|e| e["kind"] == "task_failed").collect();
                    assert_eq!(failed.len(), 1, "{what}: one failure is told: {failed:?}");
                }
            }
        }
    }
}

/// PY-T25 a_source_failure: a source that cannot be read ends the run, and every task ends
/// once.
#[test]
fn py_t25_a_source_failure() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t25");
    let mut job = document(&scratch, &["pass"], json!("terminate"));
    // A file whose column chunks hold garbage where its pages should be: the footer reads, the
    // pages do not.
    let broken = scratch.path().join("broken.parquet");
    let mut bytes = std::fs::read(scratch.path().join("in.parquet")).expect("the input");
    let from = bytes.len() / 8;
    for byte in &mut bytes[from..from * 3] {
        *byte = 0xab;
    }
    std::fs::write(&broken, bytes).expect("the broken file");
    job.source = JobSpec::from_value(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": format!("file://{}", broken.display())},
        "sink": {"kind": "parquet", "url": "/unused"},
    }))
    .expect("parses")
    .source;
    let (outcome, told) = run(job, 0);
    assert_ne!(
        outcome.code,
        exit::COMPLETED,
        "a broken file does not complete"
    );
    one_end_each(&told, "a broken source");
}

/// Run `job` as `moruna run` does, with `kinds`, catching the panic that leaves the session:
/// what the panic said, and every event the peer read.
fn run_crashing(job: JobSpec, kinds: &Registry) -> (Option<String>, Vec<Value>) {
    let text = Arc::new(Mutex::new(Vec::new()));
    let peer = Peer {
        text: text.clone(),
        events: Arc::new(AtomicUsize::new(0)),
        cancel_at: 0,
        cancel: CancelToken::new(),
        inbound: None,
    };
    let wire = Wire::new(Some(Box::new(peer)), Instant::now());
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = HostOptions {
            strict: false,
            env: &no_env,
            cancel: CancelToken::new(),
            heartbeat: HEARTBEAT,
            packages: Default::default(),
            capture_process: false,
        };
        session::run_job(job, kinds, &opts, &wire, None, Mode::Run)
    }));
    let said = match unwound {
        Ok(outcome) => panic!("the run did not unwind: exit {}", outcome.code),
        Err(panic) => panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string())),
    };
    (said, events_in(&text))
}

/// Nothing of a crash is told as a cancel: no task completes `cancelled`, none is skipped as
/// cancelled, and no cancel is acknowledged or ended.
fn nothing_cancelled(events: &[Value], what: &str) {
    for e in events {
        assert_ne!(e["outcome"], "cancelled", "{what}: {e}");
        assert_ne!(e["reason"], "cancelled", "{what}: {e}");
        assert!(
            !matches!(
                e["kind"].as_str(),
                Some("cancel_acknowledged" | "cancel_ended")
            ),
            "{what}: {e}"
        );
    }
}

/// PY-T27 a_crash_is_a_failure, on the facade's thread while the tasks run: the kernel fails
/// its third apply, the error policy ends the run, and the scheduler, writing the terminated
/// run's last manifest on the facade's thread, calls the sink's checkpoint, which panics. Every
/// task that started and had not ended fails `internal` with what the panic said, never
/// cancelled; a task that never started is skipped because the attempt failed; and the panic
/// still leaves the session, as it was.
#[test]
fn py_t27_a_crash_on_the_facades_thread_fails_every_started_task() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t27_facade");
    let mut job = document(&scratch, &["fail", "pass"], json!("terminate"));
    job.sink = python_entry();
    job.checkpoint.interval_ms = 60_000;
    let (said, events) = run_crashing(job, &crashes(support::FailOnce::new(3)));
    assert_eq!(
        said.as_deref(),
        Some(CHECKPOINT_BROKE),
        "the panic leaves the session as it was"
    );
    let what = "a crash on the facade's thread";
    one_end_each(&events, what);
    nothing_cancelled(&events, what);
    let started: Vec<&str> = events
        .iter()
        .filter(|e| e["kind"] == "task_started")
        .filter_map(|e| e["task"].as_str())
        .collect();
    assert!(
        started.len() >= 3,
        "{what}: the tasks were running: {started:?}"
    );
    let mut crashed = 0;
    for task in &started {
        let end = events
            .iter()
            .find(|e| {
                e["task"] == *task
                    && matches!(e["kind"].as_str(), Some("task_completed" | "task_failed"))
            })
            .expect("one end, by the property");
        if end["kind"] == "task_completed" {
            // Only a task that ran to its end before the crash completes.
            assert_eq!(end["outcome"], "succeeded", "{what}: {end}");
            continue;
        }
        crashed += 1;
        assert_eq!(end["code"], "internal", "{what}: {end}");
        assert_eq!(end["message"], CHECKPOINT_BROKE, "{what}: {end}");
        assert_eq!(end["scope"], "task", "{what}: {end}");
        assert_eq!(end["retryable"], false, "{what}: {end}");
    }
    assert!(
        crashed >= 2,
        "{what}: the kernel and the sink were running and fail: {events:#?}"
    );
    assert!(
        events.iter().all(|e| e["code"] != "kernel_error"),
        "{what}: the run never told its end, so the kernel's failure is not told as one"
    );
}

/// PY-T27 a_crash_is_a_failure, before any task started: the source's plan panics on the
/// facade's thread after the tasks are registered. No task started, so each is skipped because
/// the attempt failed, and the attempt fails `internal` with what the panic said.
#[test]
fn py_t27_a_crash_before_any_task_started_fails_the_attempt() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t27_plan");
    let mut job = document(&scratch, &["pass"], json!("terminate"));
    job.source = python_entry();
    let (said, events) = run_crashing(job, &test_kinds());
    assert_eq!(said.as_deref(), Some("the source's plan broke"));
    let what = "a crash before any task started";
    one_end_each(&events, what);
    nothing_cancelled(&events, what);
    let skipped: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"] == "task_skipped")
        .collect();
    assert!(!skipped.is_empty(), "{what}: {events:#?}");
    for end in skipped {
        assert_eq!(end["reason"], "attempt_failed", "{end}");
    }
    let failed: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"] == "task_failed")
        .collect();
    assert_eq!(failed.len(), 1, "{what}: {failed:?}");
    assert_eq!(failed[0]["task"], Value::Null, "{}", failed[0]);
    assert_eq!(failed[0]["scope"], "attempt", "{}", failed[0]);
    assert_eq!(failed[0]["code"], "internal", "{}", failed[0]);
    assert_eq!(
        failed[0]["message"], "the source's plan broke",
        "{}",
        failed[0]
    );
}

/// The scheduler's facts of a real run of two kernel stages, recorded, and the run's tasks.
fn recorded_run(scratch: &Scratch) -> (Vec<moruna_runtime::job::TaskRefDoc>, Vec<StageEvent>) {
    let job = document(scratch, &["pass", "pass"], json!("terminate"));
    let built = build(
        &job,
        &test_kinds(),
        BuildOptions {
            strict: false,
            env: &no_env,
            notes: Vec::new(),
            packages: Default::default(),
            host_ports: None,
        },
    )
    .expect("the document builds");
    let refs = task_refs(&job, Some(&built.stage_kernels));
    let facts: Arc<Mutex<Vec<StageEvent>>> = Arc::default();
    let kept = facts.clone();
    Runtime::run_with(
        built.spec,
        CancelToken::new(),
        Components {
            stage_hook: Some(Arc::new(move |event: &StageEvent| {
                kept.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event.clone());
            })),
            ..Components::default()
        },
    )
    .expect("the recorded run completes");
    let facts = facts.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(facts.len() > 4, "the run told its facts: {facts:?}");
    (refs, facts)
}

/// A lifecycle whose events are kept as JSON, as the peer reads them.
fn kept_lifecycle(
    refs: Vec<moruna_runtime::job::TaskRefDoc>,
) -> (Arc<Lifecycle>, Arc<Mutex<Vec<Value>>>) {
    let told: Arc<Mutex<Vec<Value>>> = Arc::default();
    let kept = told.clone();
    let lifecycle = Lifecycle::new(
        refs,
        Instant::now(),
        Box::new(move |_, event| {
            let mut value = serde_json::to_value(&event).expect("an event is JSON");
            value["type"] = json!("event");
            kept.lock().unwrap_or_else(|e| e.into_inner()).push(value);
        }),
    );
    (lifecycle, told)
}

/// PY-T27 a_crash_is_a_failure, at every point, where no one caught the panic: the recorded
/// facts are told to a fresh lifecycle up to each point of the run, and a panic unwinds past it
/// there. Every task ends once; a task that started and had not ended fails `internal`, saying
/// that what the panic said was not captured; none is cancelled.
#[test]
fn py_t27_a_lifecycle_unwound_at_any_point_fails_what_ran() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t27_unwound");
    let (refs, facts) = recorded_run(&scratch);
    for at in 0..=facts.len() {
        let what = format!("unwound after fact {at} of {}", facts.len());
        let (lifecycle, told) = kept_lifecycle(refs.clone());
        let upto = facts[..at].to_vec();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            lifecycle.begin(false);
            for fact in &upto {
                lifecycle.on(fact);
            }
            panic!("the run broke after fact {at}");
        }));
        assert!(unwound.is_err(), "{what}");
        let events = told.lock().unwrap_or_else(|e| e.into_inner()).clone();
        one_end_each(&events, &what);
        nothing_cancelled(&events, &what);
        let failed: Vec<&Value> = events
            .iter()
            .filter(|e| e["kind"] == "task_failed")
            .collect();
        assert!(!failed.is_empty(), "{what}: the crash is told: {events:#?}");
        for end in failed {
            assert_eq!(end["code"], "internal", "{what}: {end}");
            assert!(
                end["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("not captured")),
                "{what}: {end}"
            );
        }
    }
}

/// PY-T26 a_run_dropped_without_a_cancel, at every point: the scheduler's facts of a real run
/// of two kernel stages are recorded, then told to a fresh lifecycle up to each point of the run
/// and the lifecycle let go of there, with no panic, no end told and no cancel asked; every task
/// ends exactly once, a task that started is cancelled, and a second end told after the first
/// tells nothing.
#[test]
fn py_t26_a_run_dropped_without_a_cancel_at_any_point() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("py_t26_drop");
    let (refs, facts) = recorded_run(&scratch);
    for at in 0..=facts.len() {
        let what = format!("dropped after fact {at} of {}", facts.len());
        let (lifecycle, told) = kept_lifecycle(refs.clone());
        lifecycle.begin(false);
        for fact in &facts[..at] {
            lifecycle.on(fact);
        }
        drop(lifecycle);
        let events = told.lock().unwrap_or_else(|e| e.into_inner()).clone();
        one_end_each(&events, &what);
        for end in events.iter().filter(|e| e["kind"] == "task_completed") {
            assert!(
                matches!(end["outcome"].as_str(), Some("succeeded" | "cancelled")),
                "{what}: {end}"
            );
        }
        for end in events.iter().filter(|e| e["kind"] == "task_skipped") {
            assert_eq!(end["reason"], "cancelled", "{what}: {end}");
        }
        assert!(
            events.iter().all(|e| e["kind"] != "task_failed"),
            "{what}: a run let go of fails nothing: {events:#?}"
        );
    }
    // Told once: an end after the end tells nothing.
    let told: Arc<Mutex<Vec<Value>>> = Arc::default();
    let kept = told.clone();
    let lifecycle = Lifecycle::new(
        refs,
        Instant::now(),
        Box::new(move |_, _| *kept.lock().unwrap_or_else(|e| e.into_inner()) = vec![json!(1)]),
    );
    lifecycle.begin(false);
    lifecycle.end(moruna_runtime::host::lifecycle::Ending::Cancelled);
    told.lock().unwrap_or_else(|e| e.into_inner()).clear();
    lifecycle.end(moruna_runtime::host::lifecycle::Ending::Cancelled);
    drop(lifecycle);
    assert!(
        told.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "a second end, and the drop after it, tell nothing"
    );
}
