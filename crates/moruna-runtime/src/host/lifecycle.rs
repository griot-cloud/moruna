//! One run's task lifecycle, told as the facts become true (MH 4.3 `event`, version 4).
//!
//! A task is one element of the chain, named by the document's `report.tasks`: the source, each
//! kernel stage, the sink. The scheduler says what happened to the chain on the thread on which it
//! happened ([`StageEvent`]); this turns each into the task's transition and hands it on with the
//! instant it became true. How the run ended is told once the facade returns ([`Lifecycle::end`]):
//! the task a failure belongs to fails, a task still running is stopped or cancelled, and one
//! that never started is skipped. A run that a panic of Moruna's own unwound fails every task
//! still running, `internal` (PY-I11). Nothing here reads a log, a record or a report.
//!
//! A task's states, in order: waiting, ready, running (with its phase), then one end: completed
//! (succeeded, stopped, cancelled), failed or skipped. Each is told once, in that order, and an
//! ended task is never told of again, whatever the order the scheduler's threads tell their
//! facts in (PY-I12): a producer tells `Fed` after it pushed, so its consumer, on another
//! thread, can tell its start first. A fact that moves a task past a state not yet told tells
//! that state first, at the same instant; a fact for a state the task has passed tells nothing.
//! Every event is stamped and told with the lock held, so the instants never run backwards. A
//! running kernel task that calls a port is told waiting on it (`port_call`, MH 4.10) and, when
//! no call of it waits any more, its phase again.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use moruna_kernel::{MorunaError, StageEvent, StageHook, StageId};

use super::protocol::{
    CancelDisposition, Counters, Element, Event, FailureCode, FailureScope, Phase, SkipReason,
    TaskOutcome, WaitReason,
};
use moruna_job::{JobSpec, TaskRefDoc};

/// Where the lifecycle's events go, each with the microsecond it became true.
pub type EventOut = Box<dyn Fn(u64, Event) + Send + Sync>;

/// How the facade's run ended, for [`Lifecycle::end`].
pub enum Ending<'a> {
    /// It completed.
    Completed,
    /// It failed, with this cause.
    Failed(&'a MorunaError),
    /// A cancel stopped it.
    Cancelled,
    /// A panic of Moruna's own unwound it, saying this: a crash, a defect of the engine, which
    /// is never told as a cancel.
    Crashed(&'a str),
}

/// What a lifecycle dropped by an unwind that no one caught tells as the panic's words.
pub const PANIC_NOT_CAPTURED: &str =
    "Moruna panicked and the run unwound past its end; what the panic said was not captured";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Waiting,
    Ready,
    Running { since_us: u64, phase: Phase },
    Ended,
}

struct Task {
    id: String,
    state: State,
    /// Its input is closed: everything upstream has produced all it will.
    input_closed: bool,
    /// It was told waiting on a port, and not yet working again (MH 4.10).
    on_port: bool,
}

struct Inner {
    tasks: Vec<Task>,
    /// The scheduler is `Running`: what runs now is the run proper, not calibration.
    running: bool,
    cancel_acknowledged: bool,
    /// How the run ended was told ([`Lifecycle::end`]): nothing more is.
    ended: bool,
}

/// One run's tasks and what has been told of them.
pub struct Lifecycle {
    inner: Mutex<Inner>,
    refs: Vec<TaskRefDoc>,
    clock: Instant,
    out: EventOut,
}

/// The tasks of `job`, source first: `report.tasks` when the document names them, otherwise
/// `source`, `kernel-1` .. `kernel-n` and `sink`. With `stages` (the document's kernels each
/// stage carries, once the document is built), a stage that carries several kernels is one task,
/// a group: the first one's id, and as its members the tasks each carried (a kernel's own members,
/// or its id).
pub fn task_refs(job: &JobSpec, stages: Option<&[Vec<usize>]>) -> Vec<TaskRefDoc> {
    let one = |id: String| TaskRefDoc {
        id,
        members: Vec::new(),
    };
    let (source, kernels, sink) = match &job.report.tasks {
        Some(tasks) => (
            tasks.source.clone(),
            tasks.kernels.clone(),
            tasks.sink.clone(),
        ),
        None => (
            one("source".into()),
            (1..=job.kernels.len())
                .map(|k| one(format!("kernel-{k}")))
                .collect(),
            one("sink".into()),
        ),
    };
    let staged: Vec<TaskRefDoc> = match stages {
        None => kernels,
        Some(stages) => stages
            .iter()
            .map(|carried| match carried.as_slice() {
                [only] => kernels[*only].clone(),
                many => TaskRefDoc {
                    id: kernels[many[0]].id.clone(),
                    members: many
                        .iter()
                        .flat_map(|k| match kernels[*k].members.as_slice() {
                            [] => vec![kernels[*k].id.clone()],
                            members => members.to_vec(),
                        })
                        .collect(),
                },
            })
            .collect(),
    };
    std::iter::once(source)
        .chain(staged)
        .chain(std::iter::once(sink))
        .collect()
}

impl Lifecycle {
    /// The lifecycle of a chain of `refs` (source, kernels, sink), on `clock`, told to `out`.
    pub fn new(refs: Vec<TaskRefDoc>, clock: Instant, out: EventOut) -> Arc<Lifecycle> {
        let tasks = refs
            .iter()
            .map(|r| Task {
                id: r.id.clone(),
                state: State::Waiting,
                input_closed: false,
                on_port: false,
            })
            .collect();
        Arc::new(Lifecycle {
            inner: Mutex::new(Inner {
                tasks,
                running: false,
                cancel_acknowledged: false,
                ended: false,
            }),
            refs,
            clock,
            out,
        })
    }

    /// The instant, read with the lock held, so events told in the lock's order are stamped in
    /// it too.
    fn now(&self, _held: &Inner) -> u64 {
        self.clock.elapsed().as_micros() as u64
    }

    fn tell(&self, t_us: u64, event: Event) {
        (self.out)(t_us, event);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The attempt starts: every task is registered, and waits.
    pub fn begin(&self, resumed: bool) {
        let inner = self.lock();
        let t = self.now(&inner);
        self.tell(t, Event::AttemptStarted { resumed });
        let last = self.refs.len() - 1;
        for (pos, r) in self.refs.iter().enumerate() {
            let element = match pos {
                0 => Element::Source,
                p if p == last => Element::Sink,
                _ => Element::Kernel,
            };
            let upstream = (pos > 0).then(|| self.refs[pos - 1].id.clone());
            self.tell(
                t,
                Event::TaskRegistered {
                    task: r.id.clone(),
                    element,
                    stage: pos as u16,
                    upstream: upstream.clone(),
                    members: r.members.clone(),
                },
            );
        }
        for (pos, task) in inner.tasks.iter().enumerate() {
            let (reason, blocking) = match pos {
                0 => (WaitReason::EnginePreparing, Vec::new()),
                _ => (
                    WaitReason::AwaitingInput,
                    vec![inner.tasks[pos - 1].id.clone()],
                ),
            };
            self.tell(
                t,
                Event::TaskWaiting {
                    task: task.id.clone(),
                    reason,
                    blocking,
                    port: None,
                },
            );
        }
    }

    /// The hook the scheduler tells its facts to.
    pub fn hook(self: &Arc<Self>) -> StageHook {
        let me = self.clone();
        Arc::new(move |event| me.on(event))
    }

    /// What a running task at `pos` is doing, on this side of `Running`.
    fn phase_of(&self, inner: &Inner, pos: usize) -> Phase {
        if !inner.running {
            return Phase::Calibrating;
        }
        let task = &inner.tasks[pos];
        if pos > 0 && task.input_closed {
            return Phase::Draining;
        }
        match pos {
            0 => Phase::Reading,
            p if p == inner.tasks.len() - 1 => Phase::Writing,
            _ => Phase::Applying,
        }
    }

    fn ready(&self, inner: &mut Inner, t: u64, pos: usize) {
        let Some(task) = inner.tasks.get_mut(pos) else {
            return;
        };
        if task.state == State::Waiting {
            task.state = State::Ready;
            self.tell(
                t,
                Event::TaskReady {
                    task: task.id.clone(),
                },
            );
        }
    }

    /// The task at `pos` starts. One still waiting had what it needed, and its readiness may
    /// not have been told yet (its producer's `Fed` comes from another thread, after the push
    /// this start took from): it is told ready first (PY-I12).
    fn start(&self, inner: &mut Inner, t: u64, pos: usize, phase: Phase) {
        self.ready(inner, t, pos);
        let task = &mut inner.tasks[pos];
        if task.state == State::Ready {
            task.state = State::Running { since_us: t, phase };
            self.tell(
                t,
                Event::TaskStarted {
                    task: task.id.clone(),
                    phase,
                },
            );
        }
    }

    fn phase(&self, inner: &mut Inner, t: u64, pos: usize, phase: Phase) {
        let task = &mut inner.tasks[pos];
        if let State::Running {
            since_us,
            phase: was,
        } = task.state
            && was != phase
        {
            task.state = State::Running { since_us, phase };
            self.tell(
                t,
                Event::TaskPhase {
                    task: task.id.clone(),
                    phase,
                },
            );
        }
    }

    fn complete(
        &self,
        inner: &mut Inner,
        t: u64,
        pos: usize,
        outcome: TaskOutcome,
        counters: Option<Counters>,
        cause: Option<String>,
    ) {
        let task = &mut inner.tasks[pos];
        let duration_us = match task.state {
            State::Ended => return,
            State::Running { since_us, .. } => Some(t.saturating_sub(since_us)),
            State::Waiting | State::Ready => None,
        };
        task.state = State::Ended;
        self.tell(
            t,
            Event::TaskCompleted {
                task: task.id.clone(),
                outcome,
                duration_us,
                counters,
                cause,
            },
        );
    }

    /// One fact from the scheduler.
    pub fn on(&self, event: &StageEvent) {
        let mut inner = self.lock();
        let t = self.now(&inner);
        let inner = &mut *inner;
        let sink = inner.tasks.len() - 1;
        match event {
            StageEvent::Running => {
                inner.running = true;
                self.ready(inner, t, 0);
                for pos in 0..inner.tasks.len() {
                    if let State::Running {
                        phase: Phase::Calibrating,
                        ..
                    } = inner.tasks[pos].state
                    {
                        let phase = self.phase_of(inner, pos);
                        self.phase(inner, t, pos, phase);
                    }
                }
            }
            StageEvent::SourceReading => {
                let phase = self.phase_of(inner, 0);
                self.start(inner, t, 0, phase);
            }
            StageEvent::Fed { queue } => self.ready(inner, t, usize::from(*queue) + 1),
            StageEvent::StageStarted { stage } => {
                let pos = usize::from(*stage);
                if pos >= 1 && pos < sink {
                    let phase = self.phase_of(inner, pos);
                    self.start(inner, t, pos, phase);
                }
            }
            StageEvent::QueueClosed { queue, counters } => {
                let producer = usize::from(*queue);
                if producer >= sink {
                    return;
                }
                self.complete(
                    inner,
                    t,
                    producer,
                    TaskOutcome::Succeeded,
                    Some(Counters::of(counters)),
                    None,
                );
                let consumer = producer + 1;
                inner.tasks[consumer].input_closed = true;
                if let State::Running { phase, .. } = inner.tasks[consumer].state
                    && matches!(phase, Phase::Applying | Phase::Writing)
                {
                    self.phase(inner, t, consumer, Phase::Draining);
                }
            }
            StageEvent::SinkWriting => {
                let phase = self.phase_of(inner, sink);
                self.start(inner, t, sink, phase);
            }
            StageEvent::SinkCommitting => {
                self.start(inner, t, sink, Phase::Committing);
                self.phase(inner, t, sink, Phase::Committing);
            }
            StageEvent::PortWaiting { stage, port } => {
                let pos = usize::from(*stage);
                if pos >= 1 && pos < sink && matches!(inner.tasks[pos].state, State::Running { .. })
                {
                    inner.tasks[pos].on_port = true;
                    self.tell(
                        t,
                        Event::TaskWaiting {
                            task: inner.tasks[pos].id.clone(),
                            reason: WaitReason::PortCall,
                            blocking: Vec::new(),
                            port: Some(port.clone()),
                        },
                    );
                }
            }
            StageEvent::PortAnswered { stage } => {
                let pos = usize::from(*stage);
                if pos >= 1 && pos < sink && inner.tasks[pos].on_port {
                    inner.tasks[pos].on_port = false;
                    if let State::Running { phase, .. } = inner.tasks[pos].state {
                        self.tell(
                            t,
                            Event::TaskPhase {
                                task: inner.tasks[pos].id.clone(),
                                phase,
                            },
                        );
                    }
                }
            }
            StageEvent::SinkFinished { counters } => {
                self.complete(
                    inner,
                    t,
                    sink,
                    TaskOutcome::Succeeded,
                    Some(Counters::of(counters)),
                    None,
                );
            }
        }
    }

    /// The peer's `cancel` reached the run.
    pub fn cancel_acknowledged(&self) {
        let mut inner = self.lock();
        let t = self.now(&inner);
        if !inner.cancel_acknowledged {
            inner.cancel_acknowledged = true;
            self.tell(t, Event::CancelAcknowledged);
        }
    }

    /// How the run ended: the end of every task not yet ended, then, when a cancel reached the
    /// run, how the cancel ended. Told once: a second call tells nothing, so no task ends twice
    /// and the cancel's end is told at most once (PY-I11).
    pub fn end(&self, ending: Ending<'_>) {
        let mut inner = self.lock();
        let t = self.now(&inner);
        let inner = &mut *inner;
        if inner.ended {
            return;
        }
        inner.ended = true;
        let disposition = match ending {
            Ending::Completed => {
                // The scheduler tells every task's end on a completed run; one that was not is
                // the scheduler's defect, said here rather than papered over.
                for task in inner.tasks.iter().filter(|t| t.state != State::Ended) {
                    tracing::error!(
                        target: "moruna.lifecycle",
                        task = %task.id,
                        "the run completed and this task's end was never observed"
                    );
                }
                CancelDisposition::Completed
            }
            Ending::Cancelled => {
                for pos in 0..inner.tasks.len() {
                    self.stop(
                        inner,
                        t,
                        pos,
                        TaskOutcome::Cancelled,
                        SkipReason::Cancelled,
                        None,
                    );
                }
                CancelDisposition::Cancelled
            }
            Ending::Crashed(said) => {
                // Every task still running fails with what the panic said; when none was, the
                // attempt does, so a crash is always told. One that never started is skipped:
                // the attempt failed under it.
                let running: Vec<usize> = (0..inner.tasks.len())
                    .filter(|p| matches!(inner.tasks[*p].state, State::Running { .. }))
                    .collect();
                let failed = |task: Option<String>| Event::TaskFailed {
                    task: task.clone(),
                    code: FailureCode::Internal,
                    message: said.to_string(),
                    scope: match task {
                        Some(_) => FailureScope::Task,
                        None => FailureScope::Attempt,
                    },
                    retryable: false,
                };
                if running.is_empty() {
                    self.tell(t, failed(None));
                }
                for p in running {
                    inner.tasks[p].state = State::Ended;
                    self.tell(t, failed(Some(inner.tasks[p].id.clone())));
                }
                for p in 0..inner.tasks.len() {
                    self.stop(
                        inner,
                        t,
                        p,
                        TaskOutcome::Stopped,
                        SkipReason::AttemptFailed,
                        None,
                    );
                }
                CancelDisposition::Failed
            }
            Ending::Failed(error) => {
                let (owner, code, message, retryable) = attribute(error, inner.tasks.len());
                // A task already ended cannot end again: its failure is the attempt's then, said
                // as the defect it is.
                let pos = owner.filter(|p| inner.tasks[*p].state != State::Ended);
                if let Some(p) = owner.filter(|p| inner.tasks[*p].state == State::Ended) {
                    tracing::error!(
                        target: "moruna.lifecycle",
                        task = %inner.tasks[p].id,
                        "the run failed for a task whose end was already told"
                    );
                }
                let failed = pos.map(|p| inner.tasks[p].id.clone());
                if let Some(p) = pos {
                    inner.tasks[p].state = State::Ended;
                }
                self.tell(
                    t,
                    Event::TaskFailed {
                        task: failed.clone(),
                        code,
                        message,
                        scope: match pos {
                            Some(_) => FailureScope::Task,
                            None => FailureScope::Attempt,
                        },
                        retryable,
                    },
                );
                let skipped = match pos {
                    Some(_) => SkipReason::FailedTask,
                    None => SkipReason::AttemptFailed,
                };
                for p in 0..inner.tasks.len() {
                    self.stop(inner, t, p, TaskOutcome::Stopped, skipped, failed.clone());
                }
                CancelDisposition::Failed
            }
        };
        if inner.cancel_acknowledged {
            self.tell(t, Event::CancelEnded { disposition });
        }
    }

    /// A task still running ends with `outcome`; one that never started is skipped.
    fn stop(
        &self,
        inner: &mut Inner,
        t: u64,
        pos: usize,
        outcome: TaskOutcome,
        skipped: SkipReason,
        cause: Option<String>,
    ) {
        match inner.tasks[pos].state {
            State::Ended => {}
            State::Running { .. } => self.complete(inner, t, pos, outcome, None, cause),
            State::Waiting | State::Ready => {
                let task = &mut inner.tasks[pos];
                task.state = State::Ended;
                self.tell(
                    t,
                    Event::TaskSkipped {
                        task: task.id.clone(),
                        reason: skipped,
                        blocking: cause,
                    },
                );
            }
        }
    }
}

/// A lifecycle dropped before its run's end was told tells that end now. Let go of by an
/// embedder, with no panic, it is a cancel: the scheduler's shutdown on drop is a cancel
/// (preamble 4.3), so every task still running is cancelled and every task that never started
/// is skipped as cancelled. Dropped by an unwind, it is a crash: the session catches Moruna's
/// panics where it drives the run and tells their words first, so this is an unwind no one
/// caught, and every task still running fails `internal`, saying the panic's words were not
/// captured. Every task that started has its one end whatever ended the run (PY-I11).
impl Drop for Lifecycle {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.end(Ending::Crashed(PANIC_NOT_CAPTURED));
        } else {
            self.end(Ending::Cancelled);
        }
    }
}

/// The task an error belongs to (its position in a chain of `tasks`, source first), its code,
/// the engine's sentence about it, which names stages, morsels and sizes and quotes nothing a
/// kernel or a source said, and whether running the document again may succeed.
pub fn attribute(error: &MorunaError, tasks: usize) -> (Option<usize>, FailureCode, String, bool) {
    let sink = tasks - 1;
    let kernel = |stage: StageId| {
        let pos = usize::from(stage);
        (pos >= 1 && pos < sink).then_some(pos)
    };
    match error {
        MorunaError::Kernel { stage, seq, .. } => (
            kernel(*stage),
            FailureCode::KernelError,
            format!("kernel stage {stage} failed on morsel {seq}"),
            false,
        ),
        MorunaError::Budget {
            stage,
            seq,
            footprint,
            budget,
            ..
        } => (
            kernel(*stage),
            FailureCode::BudgetExceeded,
            format!(
                "morsel {seq} of stage {stage} needed {footprint} bytes, over the budget of \
                 {budget}"
            ),
            false,
        ),
        MorunaError::Refused {
            stage,
            seq,
            requested,
            ceiling,
            ..
        } => (
            kernel(*stage),
            FailureCode::MemoryRefused,
            format!(
                "stage {stage} asked for {requested} bytes on morsel {seq}, past the ceiling of \
                 {ceiling}"
            ),
            false,
        ),
        MorunaError::Source { split, .. } => (
            Some(0),
            FailureCode::SourceError,
            format!("the source could not read split {split}"),
            true,
        ),
        MorunaError::Sink(_) => (
            Some(sink),
            FailureCode::SinkError,
            "the sink could not write or commit".to_string(),
            true,
        ),
        MorunaError::Alloc {
            bytes,
            tier,
            budget,
            ..
        } => (
            None,
            FailureCode::AllocRefused,
            format!("an allocation of {bytes} bytes in {tier:?} exceeded its budget of {budget}"),
            true,
        ),
        MorunaError::Io { op, .. } => (
            None,
            FailureCode::IoError,
            format!("the {op} operation failed"),
            true,
        ),
        MorunaError::Staging(_) => (
            None,
            FailureCode::StagingError,
            "staging failed".to_string(),
            true,
        ),
        MorunaError::Convert(_) => (
            None,
            FailureCode::ConvertError,
            "a conversion between a table and a tensor failed".to_string(),
            false,
        ),
        MorunaError::Config { name, .. } if *name == "spec" => (
            None,
            FailureCode::SpecRefused,
            "the document was refused".to_string(),
            false,
        ),
        MorunaError::Config { name, .. } => (
            None,
            FailureCode::ConfigRefused,
            format!("the configuration `{name}` was refused"),
            false,
        ),
        MorunaError::Plan(_) => (
            None,
            FailureCode::PlanRefused,
            "the plan was refused".to_string(),
            false,
        ),
        MorunaError::Resume(_) => (
            None,
            FailureCode::ResumeRefused,
            "the resume was refused".to_string(),
            false,
        ),
        MorunaError::Unsupported(what) => (
            None,
            FailureCode::Unsupported,
            format!("{what} is not supported"),
            false,
        ),
        // A cancel the run was asked for is an `Ending::Cancelled`; this is the engine stopping
        // as cancelled with none asked of it (a completion whose sender went away).
        MorunaError::Cancelled => (
            None,
            FailureCode::Cancelled,
            "the engine stopped as cancelled, and no cancel was asked of it".to_string(),
            true,
        ),
    }
}

#[cfg(test)]
mod tests;
