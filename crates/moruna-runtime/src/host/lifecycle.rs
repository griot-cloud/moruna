//! One run's task lifecycle, told as the facts become true (MH 4.3 `event`, version 4).
//!
//! A task is one task of the job's DAG, named by the id the document gives it: each source, each
//! kernel stage (several fused standard kernels being one task, a group), each sink. The
//! scheduler says what happened to the run on the thread on which it happened ([`StageEvent`]),
//! naming queues, stages, sources and sinks by their numbers in the run's topology; this turns
//! each into the task's transition and hands it on with the
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
use crate::job::Parts;
use moruna_job::{Job, Role, TaskRefDoc};

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
    /// Its input queues still open.
    open_inputs: usize,
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
    plan: Plan,
    clock: Instant,
    out: EventOut,
}

/// One task of the run as its events name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Its id, and the members of a group.
    pub task: TaskRefDoc,
    /// `source`, `kernel` or `sink`.
    pub element: Element,
    /// Its place: 0 a source, the stage's number for a kernel, the stages plus one for a sink.
    pub stage: u16,
    /// The tasks whose output it takes, by its input ports, as places in [`Plan::nodes`].
    pub upstream: Vec<usize>,
}

/// The run's tasks for its events: the sources, then the kernel stages in their numbered order,
/// then the sinks; and which of them each queue of the run's topology joins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Every task.
    pub nodes: Vec<Node>,
    /// Source `i`'s place.
    pub sources: Vec<usize>,
    /// Stage `s`'s place (`stages[s - 1]`).
    pub stages: Vec<usize>,
    /// Sink `i`'s place.
    pub sinks: Vec<usize>,
    /// Queue `q`'s producer and consumer, by place.
    pub queues: Vec<(usize, usize)>,
}

impl Plan {
    /// The plan of `job` as it was built (`parts`), or, refused before it was built, as it was
    /// written: each kernel its own stage, in an order in which every edge goes forward.
    pub fn of(job: &Job, parts: Option<&Parts>) -> Plan {
        let parts = match parts {
            Some(parts) => parts.clone(),
            None => {
                let order = job
                    .topological_order()
                    .unwrap_or_else(|_| (0..job.tasks.len()).collect());
                let of = |role: Role| -> Vec<usize> {
                    (0..job.tasks.len())
                        .filter(|&i| job.tasks[i].role == role)
                        .collect()
                };
                Parts {
                    sources: of(Role::Source),
                    stages: order
                        .into_iter()
                        .filter(|&i| job.tasks[i].role == Role::Kernel)
                        .map(|i| vec![i])
                        .collect(),
                    sinks: of(Role::Sink),
                }
            }
        };
        // The place of each of the job's tasks: a fused kernel's is its stage's.
        let mut place: Vec<Option<usize>> = vec![None; job.tasks.len()];
        let mut nodes = Vec::new();
        let one = |i: usize| TaskRefDoc {
            id: job.tasks[i].id.clone(),
            members: job.tasks[i].members.clone(),
        };
        let mut sources = Vec::new();
        for &i in &parts.sources {
            place[i] = Some(nodes.len());
            sources.push(nodes.len());
            nodes.push(Node {
                task: one(i),
                element: Element::Source,
                stage: 0,
                upstream: Vec::new(),
            });
        }
        let mut stages = Vec::new();
        for (at, carried) in parts.stages.iter().enumerate() {
            for &i in carried {
                place[i] = Some(nodes.len());
            }
            stages.push(nodes.len());
            let task = match carried.as_slice() {
                [only] => one(*only),
                many => TaskRefDoc {
                    id: job.tasks[many[0]].id.clone(),
                    members: many
                        .iter()
                        .flat_map(|&k| match job.tasks[k].members.as_slice() {
                            [] => vec![job.tasks[k].id.clone()],
                            members => members.to_vec(),
                        })
                        .collect(),
                },
            };
            nodes.push(Node {
                task,
                element: Element::Kernel,
                stage: (at + 1) as u16,
                upstream: Vec::new(),
            });
        }
        let last = (parts.stages.len() + 1) as u16;
        let mut sinks = Vec::new();
        for &i in &parts.sinks {
            place[i] = Some(nodes.len());
            sinks.push(nodes.len());
            nodes.push(Node {
                task: one(i),
                element: Element::Sink,
                stage: last,
                upstream: Vec::new(),
            });
        }
        // Each task's upstream, by its input ports; an edge inside a fused stage is no input.
        let mut queues = Vec::new();
        for (i, task) in job.tasks.iter().enumerate() {
            let Some(at) = place[i] else {
                continue;
            };
            for port in &task.inputs {
                for e in job.edges_into(&task.id, port) {
                    let Some(from) = job
                        .task_index(&job.edges[e].from.task)
                        .and_then(|f| place[f])
                    else {
                        continue;
                    };
                    if from == at {
                        continue;
                    }
                    if !nodes[at].upstream.contains(&from) {
                        nodes[at].upstream.push(from);
                    }
                }
            }
        }
        for edge in &job.edges {
            let (Some(from), Some(to)) = (
                job.task_index(&edge.from.task).and_then(|f| place[f]),
                job.task_index(&edge.to.task).and_then(|t| place[t]),
            ) else {
                continue;
            };
            if from != to {
                queues.push((from, to));
            }
        }
        Plan {
            nodes,
            sources,
            stages,
            sinks,
            queues,
        }
    }

    /// The ids of the tasks, in place order.
    pub fn ids(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.task.id.clone()).collect()
    }
}

impl Lifecycle {
    /// The lifecycle of the tasks of `plan`, on `clock`, told to `out`.
    pub fn new(plan: Plan, clock: Instant, out: EventOut) -> Arc<Lifecycle> {
        let tasks = plan
            .nodes
            .iter()
            .enumerate()
            .map(|(at, n)| Task {
                id: n.task.id.clone(),
                state: State::Waiting,
                input_closed: false,
                open_inputs: plan.queues.iter().filter(|(_, to)| *to == at).count(),
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
            plan,
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
        for node in &self.plan.nodes {
            self.tell(
                t,
                Event::TaskRegistered {
                    task: node.task.id.clone(),
                    element: node.element,
                    stage: node.stage,
                    upstream: node
                        .upstream
                        .iter()
                        .map(|&u| self.plan.nodes[u].task.id.clone())
                        .collect(),
                    members: node.task.members.clone(),
                },
            );
        }
        for (pos, task) in inner.tasks.iter().enumerate() {
            let node = &self.plan.nodes[pos];
            let (reason, blocking) = match node.element {
                Element::Source => (WaitReason::EnginePreparing, Vec::new()),
                _ => (
                    WaitReason::AwaitingInput,
                    node.upstream
                        .iter()
                        .map(|&u| inner.tasks[u].id.clone())
                        .collect(),
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
        let element = self.plan.nodes[pos].element;
        if element != Element::Source && task.input_closed {
            return Phase::Draining;
        }
        match element {
            Element::Source => Phase::Reading,
            Element::Sink => Phase::Writing,
            Element::Kernel => Phase::Applying,
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
        let plan = &self.plan;
        let stage_at = |stage: StageId| {
            usize::from(stage)
                .checked_sub(1)
                .and_then(|i| plan.stages.get(i))
                .copied()
        };
        match event {
            StageEvent::Running => {
                inner.running = true;
                for &pos in &plan.sources {
                    self.ready(inner, t, pos);
                }
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
            StageEvent::SourceReading { source } => {
                if let Some(&pos) = plan.sources.get(usize::from(*source)) {
                    let phase = self.phase_of(inner, pos);
                    self.start(inner, t, pos, phase);
                }
            }
            StageEvent::Fed { queue } => {
                if let Some(&(_, consumer)) = plan.queues.get(usize::from(*queue)) {
                    self.ready(inner, t, consumer);
                }
            }
            StageEvent::StageStarted { stage } => {
                if let Some(pos) = stage_at(*stage) {
                    let phase = self.phase_of(inner, pos);
                    self.start(inner, t, pos, phase);
                }
            }
            StageEvent::QueueClosed { queue, counters } => {
                let Some(&(producer, consumer)) = plan.queues.get(usize::from(*queue)) else {
                    return;
                };
                // A producer's queues close together: the first close ends it.
                self.complete(
                    inner,
                    t,
                    producer,
                    TaskOutcome::Succeeded,
                    Some(Counters::of(counters)),
                    None,
                );
                let task = &mut inner.tasks[consumer];
                task.open_inputs = task.open_inputs.saturating_sub(1);
                if task.open_inputs == 0 {
                    task.input_closed = true;
                    if let State::Running { phase, .. } = task.state
                        && matches!(phase, Phase::Applying | Phase::Writing)
                    {
                        self.phase(inner, t, consumer, Phase::Draining);
                    }
                }
            }
            StageEvent::SinkWriting { sink } => {
                if let Some(&pos) = plan.sinks.get(usize::from(*sink)) {
                    let phase = self.phase_of(inner, pos);
                    self.start(inner, t, pos, phase);
                }
            }
            StageEvent::SinkCommitting { sink } => {
                if let Some(&pos) = plan.sinks.get(usize::from(*sink)) {
                    self.start(inner, t, pos, Phase::Committing);
                    self.phase(inner, t, pos, Phase::Committing);
                }
            }
            StageEvent::PortWaiting { stage, port } => {
                if let Some(pos) = stage_at(*stage)
                    && matches!(inner.tasks[pos].state, State::Running { .. })
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
                if let Some(pos) = stage_at(*stage)
                    && inner.tasks[pos].on_port
                {
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
            StageEvent::SinkFinished { sink, counters } => {
                if let Some(&pos) = plan.sinks.get(usize::from(*sink)) {
                    self.complete(
                        inner,
                        t,
                        pos,
                        TaskOutcome::Succeeded,
                        Some(Counters::of(counters)),
                        None,
                    );
                }
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
                let (owner, code, message, retryable) = attribute(error, &self.plan);
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

/// The task an error belongs to (its place in `plan`), its code, the engine's sentence about
/// it, which names stages, morsels and sizes and quotes nothing a kernel or a source said, and
/// whether running the document again may succeed. A source's or a sink's failure belongs to it
/// when the run has one; with several, the error does not say which, and it is the attempt's.
pub fn attribute(error: &MorunaError, plan: &Plan) -> (Option<usize>, FailureCode, String, bool) {
    let kernel = |stage: StageId| {
        usize::from(stage)
            .checked_sub(1)
            .and_then(|i| plan.stages.get(i))
            .copied()
    };
    let only = |of: &[usize]| match of {
        [one] => Some(*one),
        _ => None,
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
            only(&plan.sources),
            FailureCode::SourceError,
            format!("the source could not read split {split}"),
            true,
        ),
        MorunaError::Sink(_) => (
            only(&plan.sinks),
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
