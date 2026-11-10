//! One run's task lifecycle, told as the facts become true (MH 4.3 `event`, version 7).
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
//!
//! A run starts from its plan (SDD 26): every task event names the plan step that runs the task,
//! a step starts when the first of its tasks starts and ends when the last of them ends
//! (`step_started`, `step_ended`), and a step that starts while a step of an earlier group has
//! not is a deviation from the plan, kept with the reason the earlier step was not running
//! ([`Deviation`]). What the run did against its plan is [`Lifecycle::plan_run`], which the report
//! carries.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use moruna_kernel::{MorunaError, StageEvent, StageHook, StageId};

use serde::Serialize;

use super::protocol::{
    CancelDisposition, Counters, Element, Event, FailureCode, FailureScope, Phase, SkipReason,
    StepOutcome, TaskOutcome, WaitReason, counters_of,
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
    /// How each task ended, once it has.
    ends: Vec<Option<StepOutcome>>,
    /// What each task did, when it ran to its end.
    counters: Vec<Option<Counters>>,
    /// Each plan step's start and end, by its place in [`Steps`].
    steps: Vec<StepState>,
    /// The steps in the order they started.
    order: Vec<usize>,
    /// Every start ahead of the plan.
    deviations: Vec<Deviation>,
    /// The scheduler is `Running`: what runs now is the run proper, not calibration.
    running: bool,
    cancel_acknowledged: bool,
    /// How the run ended was told ([`Lifecycle::end`]): nothing more is.
    ended: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct StepState {
    started_us: Option<u64>,
    ended_us: Option<u64>,
    outcome: Option<StepOutcome>,
}

/// One run's tasks and what has been told of them.
pub struct Lifecycle {
    inner: Mutex<Inner>,
    layout: Layout,
    steps: Option<Steps>,
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
    /// The tasks whose output it takes, by its input ports, as places in [`Layout::nodes`].
    pub upstream: Vec<usize>,
}

/// The run's tasks for its events: the sources, then the kernel stages in their numbered order,
/// then the sinks; and which of them each queue of the run's topology joins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
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
    /// The place of each of the job's tasks, by its index in the job: a fused kernel's is its
    /// stage's.
    pub places: Vec<Option<usize>>,
}

impl Layout {
    /// The layout of `job` as it was built (`parts`), or, refused before it was built, as it was
    /// written: each kernel its own stage, in an order in which every edge goes forward.
    pub fn of(job: &Job, parts: Option<&Parts>) -> Layout {
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
        Layout {
            nodes,
            sources,
            stages,
            sinks,
            queues,
            places: place,
        }
    }

    /// The ids of the tasks, in place order.
    pub fn ids(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.task.id.clone()).collect()
    }
}

impl Lifecycle {
    /// The lifecycle of the tasks of `layout`, run by the plan `steps` describe, on `clock`, told
    /// to `out`. `steps` is `None` only for a document refused before it was planned: its tasks
    /// then name no step.
    pub fn new(
        layout: Layout,
        steps: Option<Steps>,
        clock: Instant,
        out: EventOut,
    ) -> Arc<Lifecycle> {
        let tasks: Vec<Task> = layout
            .nodes
            .iter()
            .enumerate()
            .map(|(at, n)| Task {
                id: n.task.id.clone(),
                state: State::Waiting,
                input_closed: false,
                open_inputs: layout.queues.iter().filter(|(_, to)| *to == at).count(),
                on_port: false,
            })
            .collect();
        let n = tasks.len();
        let step_count = steps.as_ref().map_or(0, |s| s.steps.len());
        Arc::new(Lifecycle {
            inner: Mutex::new(Inner {
                tasks,
                ends: vec![None; n],
                counters: vec![None; n],
                steps: vec![StepState::default(); step_count],
                order: Vec::new(),
                deviations: Vec::new(),
                running: false,
                cancel_acknowledged: false,
                ended: false,
            }),
            layout,
            steps,
            clock,
            out,
        })
    }

    /// The id of the plan step that runs the task at `pos`: the step of the task its id names
    /// (a fused group's first).
    fn step_of(&self, pos: usize) -> Option<String> {
        let steps = self.steps.as_ref()?;
        let at = (*steps.primary.get(pos)?)?;
        Some(steps.steps[at].id.clone())
    }

    /// The task at `pos` started: each step it runs that had not started starts, and a start
    /// ahead of a step of an earlier group that has not started is a deviation, with the reason
    /// that step was not running.
    fn step_started(&self, inner: &mut Inner, t: u64, pos: usize) {
        let Some(steps) = &self.steps else {
            return;
        };
        for &s in steps.of_node.get(pos).map(Vec::as_slice).unwrap_or(&[]) {
            if inner.steps[s].started_us.is_some() || inner.steps[s].ended_us.is_some() {
                continue;
            }
            inner.steps[s].started_us = Some(t);
            inner.order.push(s);
            let step = &steps.steps[s];
            self.tell(
                t,
                Event::StepStarted {
                    step: step.id.clone(),
                    plan: steps.plan.clone(),
                    group: step.group,
                },
            );
            let behind: Vec<Behind> = (0..steps.steps.len())
                .filter(|&e| {
                    steps.steps[e].group < step.group
                        && inner.steps[e].started_us.is_none()
                        && inner.steps[e].ended_us.is_none()
                })
                .map(|e| self.behind(inner, steps, e))
                .collect();
            if !behind.is_empty() {
                inner.deviations.push(Deviation {
                    step: step.id.clone(),
                    t_us: t,
                    ahead_of: behind,
                });
            }
        }
    }

    /// Why step `e`, of an earlier group, was not running when a later one started: a task of it
    /// had its input and no worker or drive had taken it yet (`ready`); it is a source the engine
    /// was still preparing (`engine_preparing`); or its input had not come, from the steps named
    /// (`awaiting_input`).
    fn behind(&self, inner: &Inner, steps: &Steps, e: usize) -> Behind {
        let nodes = &steps.nodes_of[e];
        let reason = if nodes.iter().any(|&p| inner.tasks[p].state == State::Ready) {
            DeviationReason::Ready
        } else if nodes
            .iter()
            .any(|&p| self.layout.nodes[p].element == Element::Source)
        {
            DeviationReason::EnginePreparing
        } else {
            DeviationReason::AwaitingInput
        };
        let mut blocking: Vec<String> = Vec::new();
        if reason == DeviationReason::AwaitingInput {
            for &p in nodes {
                for &u in &self.layout.nodes[p].upstream {
                    if inner.tasks[u].state == State::Ended {
                        continue;
                    }
                    if let Some(id) = self.step_of(u)
                        && !blocking.contains(&id)
                    {
                        blocking.push(id);
                    }
                }
            }
        }
        Behind {
            step: steps.steps[e].id.clone(),
            reason,
            blocking,
        }
    }

    /// The task at `pos` ended with `outcome`: each step it runs whose every task has now ended
    /// ends, with the first of its tasks' outcomes in [`StepOutcome`]'s order.
    fn node_ended(&self, inner: &mut Inner, t: u64, pos: usize, outcome: StepOutcome) {
        inner.ends[pos] = Some(outcome);
        let Some(steps) = &self.steps else {
            return;
        };
        for &s in steps.of_node.get(pos).map(Vec::as_slice).unwrap_or(&[]) {
            if inner.steps[s].ended_us.is_some() {
                continue;
            }
            let ends: Option<Vec<StepOutcome>> =
                steps.nodes_of[s].iter().map(|&p| inner.ends[p]).collect();
            let Some(ends) = ends else {
                continue;
            };
            let outcome = ends.into_iter().min().unwrap_or(StepOutcome::Skipped);
            let state = &mut inner.steps[s];
            state.ended_us = Some(t);
            state.outcome = Some(outcome);
            self.tell(
                t,
                Event::StepEnded {
                    step: steps.steps[s].id.clone(),
                    plan: steps.plan.clone(),
                    outcome,
                    duration_us: state.started_us.map(|since| t.saturating_sub(since)),
                },
            );
        }
    }

    /// How the run went against its plan (SDD 26 e.3), as far as it has gone: the plan, the
    /// order its steps started in, each step's start, end, outcome and what its tasks did, and
    /// every deviation. `None` when the run had no plan.
    pub fn plan_run(&self) -> Option<PlanRun> {
        let steps = self.steps.as_ref()?;
        let inner = self.lock();
        let runs = steps
            .steps
            .iter()
            .enumerate()
            .map(|(s, step)| {
                let mut used = StepUse::default();
                for &p in &steps.nodes_of[s] {
                    if let Some(c) = inner.counters[p] {
                        used.morsels += c.morsels;
                        used.rows_in += c.rows_in;
                        used.rows_out += c.rows_out;
                        used.bytes_in += c.bytes_in;
                        used.bytes_out += c.bytes_out;
                    }
                    if self.layout.nodes[p].element == Element::Kernel {
                        used.stages.push(self.layout.nodes[p].stage);
                    }
                }
                StepRun {
                    step: step.id.clone(),
                    tasks: step.tasks.clone(),
                    group: step.group,
                    start_us: inner.steps[s].started_us,
                    end_us: inner.steps[s].ended_us,
                    outcome: inner.steps[s].outcome,
                    resources: step.resources,
                    used,
                }
            })
            .collect();
        Some(PlanRun {
            plan: steps.plan.clone(),
            planner: steps.planner.clone(),
            job_digest: steps.job_digest.clone(),
            order: inner
                .order
                .iter()
                .map(|&s| steps.steps[s].id.clone())
                .collect(),
            steps: runs,
            deviations: inner.deviations.clone(),
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
        self.tell(
            t,
            Event::AttemptStarted {
                resumed,
                plan: self.steps.as_ref().map(|s| s.plan.clone()),
            },
        );
        for (pos, node) in self.layout.nodes.iter().enumerate() {
            self.tell(
                t,
                Event::TaskRegistered {
                    task: node.task.id.clone(),
                    step: self.step_of(pos),
                    element: node.element,
                    stage: node.stage,
                    upstream: node
                        .upstream
                        .iter()
                        .map(|&u| self.layout.nodes[u].task.id.clone())
                        .collect(),
                    members: node.task.members.clone(),
                },
            );
        }
        for (pos, task) in inner.tasks.iter().enumerate() {
            let node = &self.layout.nodes[pos];
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
                    step: self.step_of(pos),
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
        let element = self.layout.nodes[pos].element;
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
                    step: self.step_of(pos),
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
                    step: self.step_of(pos),
                    phase,
                },
            );
            self.step_started(inner, t, pos);
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
                    step: self.step_of(pos),
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
                step: self.step_of(pos),
                outcome,
                duration_us,
                counters,
                cause,
            },
        );
        inner.counters[pos] = counters;
        let ended = match outcome {
            TaskOutcome::Succeeded => StepOutcome::Succeeded,
            TaskOutcome::Stopped => StepOutcome::Stopped,
            TaskOutcome::Cancelled => StepOutcome::Cancelled,
        };
        self.node_ended(inner, t, pos, ended);
    }

    /// One fact from the scheduler.
    pub fn on(&self, event: &StageEvent) {
        let mut inner = self.lock();
        let t = self.now(&inner);
        let inner = &mut *inner;
        let layout = &self.layout;
        let stage_at = |stage: StageId| {
            usize::from(stage)
                .checked_sub(1)
                .and_then(|i| layout.stages.get(i))
                .copied()
        };
        match event {
            StageEvent::Running => {
                inner.running = true;
                for &pos in &layout.sources {
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
                if let Some(&pos) = layout.sources.get(usize::from(*source)) {
                    let phase = self.phase_of(inner, pos);
                    self.start(inner, t, pos, phase);
                }
            }
            StageEvent::Fed { queue } => {
                if let Some(&(_, consumer)) = layout.queues.get(usize::from(*queue)) {
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
                let Some(&(producer, consumer)) = layout.queues.get(usize::from(*queue)) else {
                    return;
                };
                // A producer's queues close together: the first close ends it.
                self.complete(
                    inner,
                    t,
                    producer,
                    TaskOutcome::Succeeded,
                    Some(counters_of(counters)),
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
                if let Some(&pos) = layout.sinks.get(usize::from(*sink)) {
                    let phase = self.phase_of(inner, pos);
                    self.start(inner, t, pos, phase);
                }
            }
            StageEvent::SinkCommitting { sink } => {
                if let Some(&pos) = layout.sinks.get(usize::from(*sink)) {
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
                            step: self.step_of(pos),
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
                                step: self.step_of(pos),
                                phase,
                            },
                        );
                    }
                }
            }
            StageEvent::SinkFinished { sink, counters } => {
                if let Some(&pos) = layout.sinks.get(usize::from(*sink)) {
                    self.complete(
                        inner,
                        t,
                        pos,
                        TaskOutcome::Succeeded,
                        Some(counters_of(counters)),
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
        // A peer message can arrive after end() won the lifecycle lock.
        // Such a cancellation cannot affect the completed attempt.
        if !inner.ended && !inner.cancel_acknowledged {
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
                let failed = |task: Option<String>, step: Option<String>| Event::TaskFailed {
                    task: task.clone(),
                    step,
                    code: FailureCode::Internal,
                    message: said.to_string(),
                    scope: match task {
                        Some(_) => FailureScope::Task,
                        None => FailureScope::Attempt,
                    },
                    retryable: false,
                };
                if running.is_empty() {
                    self.tell(t, failed(None, None));
                }
                for p in running {
                    inner.tasks[p].state = State::Ended;
                    self.tell(t, failed(Some(inner.tasks[p].id.clone()), self.step_of(p)));
                    self.node_ended(inner, t, p, StepOutcome::Failed);
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
                let (owner, code, message, retryable) = attribute(error, &self.layout);
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
                self.tell(
                    t,
                    Event::TaskFailed {
                        task: failed.clone(),
                        step: pos.and_then(|p| self.step_of(p)),
                        code,
                        message,
                        scope: match pos {
                            Some(_) => FailureScope::Task,
                            None => FailureScope::Attempt,
                        },
                        retryable,
                    },
                );
                if let Some(p) = pos {
                    inner.tasks[p].state = State::Ended;
                    self.node_ended(inner, t, p, StepOutcome::Failed);
                }
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
                        step: self.step_of(pos),
                        reason: skipped,
                        blocking: cause,
                    },
                );
                self.node_ended(inner, t, pos, StepOutcome::Skipped);
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

/// The plan a run started from, as its lifecycle tells its steps (SDD 26 e.3): each step, the
/// run's tasks that carry it, and the step each task's events name.
#[derive(Clone, Debug, PartialEq)]
pub struct Steps {
    plan: String,
    planner: String,
    job_digest: String,
    steps: Vec<moruna_plan::Step>,
    /// For each of the run's tasks (by place in the layout), the steps it carries.
    of_node: Vec<Vec<usize>>,
    /// For each step, the run's tasks that carry it.
    nodes_of: Vec<Vec<usize>>,
    /// For each of the run's tasks, the step of the task its id names.
    primary: Vec<Option<usize>>,
}

impl Steps {
    /// The id of the plan the steps are of.
    pub fn plan_id(&self) -> &str {
        &self.plan
    }

    /// The steps of `plan`, a plan of `job`, over the run's tasks as `layout` lays them out. A
    /// fused kernel stage carries the steps of every task fused into it.
    pub fn of(plan: &moruna_plan::Plan, job: &Job, layout: &Layout) -> Steps {
        let mut of_node: Vec<Vec<usize>> = vec![Vec::new(); layout.nodes.len()];
        let mut nodes_of: Vec<Vec<usize>> = vec![Vec::new(); plan.steps.len()];
        for (s, step) in plan.steps.iter().enumerate() {
            for task in &step.tasks {
                let Some(pos) = job
                    .task_index(task)
                    .and_then(|i| layout.places.get(i).copied().flatten())
                else {
                    continue;
                };
                if !of_node[pos].contains(&s) {
                    of_node[pos].push(s);
                }
                if !nodes_of[s].contains(&pos) {
                    nodes_of[s].push(pos);
                }
            }
        }
        let primary = layout
            .nodes
            .iter()
            .map(|n| plan.steps.iter().position(|s| s.tasks.contains(&n.task.id)))
            .collect();
        Steps {
            plan: plan.id.clone(),
            planner: plan.planner.to_string(),
            job_digest: plan.job_digest.clone(),
            steps: plan.steps.clone(),
            of_node,
            nodes_of,
            primary,
        }
    }
}

/// How a run went against its plan (SDD 26 e.3): what the report carries as `plan`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlanRun {
    /// The plan's id.
    pub plan: String,
    /// The planner that made it, `name@version`.
    pub planner: String,
    /// The job's digest the plan names.
    pub job_digest: String,
    /// The steps, by id, in the order they started.
    pub order: Vec<String>,
    /// Every step of the plan, in plan order, with what happened to it.
    pub steps: Vec<StepRun>,
    /// Every start ahead of the plan.
    pub deviations: Vec<Deviation>,
}

/// One step of the plan, as the run went.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StepRun {
    /// The step's id.
    pub step: String,
    /// The job's tasks it runs.
    pub tasks: Vec<String>,
    /// Its group in the plan.
    pub group: u32,
    /// When it started, in microseconds on the run's clock; `null` when it never did.
    pub start_us: Option<u64>,
    /// When it ended; `null` when it has not.
    pub end_us: Option<u64>,
    /// How it ended.
    pub outcome: Option<StepOutcome>,
    /// What the plan gave it.
    pub resources: moruna_plan::Resources,
    /// What it used.
    pub used: StepUse,
}

/// What a step's tasks used, measured where it happened. A kernel stage fused of several tasks
/// is measured as one, so each step it carries counts the stage's figures.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StepUse {
    /// Morsels taken or produced.
    pub morsels: u64,
    /// Rows taken.
    pub rows_in: u64,
    /// Rows produced.
    pub rows_out: u64,
    /// Bytes taken.
    pub bytes_in: u64,
    /// Bytes produced.
    pub bytes_out: u64,
    /// Worker seconds its kernel stages were busy, from the run report; `null` for a source or a
    /// sink, whose drive threads are not workers, and before the report is read.
    pub worker_seconds: Option<f64>,
    /// The largest state its kernel stages held, in bytes, from the run report.
    pub state_bytes_max: Option<u64>,
    /// The run's kernel stages that carry it, by number.
    #[serde(skip)]
    pub stages: Vec<u16>,
}

/// A step that started while a step of an earlier group had not (SDD 26 e.3).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Deviation {
    /// The step that started early.
    pub step: String,
    /// When, in microseconds on the run's clock.
    pub t_us: u64,
    /// The steps of earlier groups that had not started, and why.
    pub ahead_of: Vec<Behind>,
}

/// A step of an earlier group that had not started when a later one did, and why.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Behind {
    /// The step.
    pub step: String,
    /// Why it was not running.
    pub reason: DeviationReason,
    /// For `awaiting_input`, the steps whose output it was waiting for.
    pub blocking: Vec<String>,
}

/// Why a step of an earlier group was not running when a later one started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviationReason {
    /// Its input had not come.
    AwaitingInput,
    /// Its input had come and no worker or drive had taken it yet.
    Ready,
    /// It is a source the engine was still preparing.
    EnginePreparing,
}

/// The task an error belongs to (its place in `plan`), its code, the engine's sentence about
/// it, which names stages, morsels and sizes and quotes nothing a kernel or a source said, and
/// whether running the document again may succeed. A source's or a sink's failure belongs to it
/// when the run has one; with several, the error does not say which, and it is the attempt's.
pub fn attribute(
    error: &MorunaError,
    layout: &Layout,
) -> (Option<usize>, FailureCode, String, bool) {
    let kernel = |stage: StageId| {
        usize::from(stage)
            .checked_sub(1)
            .and_then(|i| layout.stages.get(i))
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
            only(&layout.sources),
            FailureCode::SourceError,
            format!("the source could not read split {split}"),
            true,
        ),
        MorunaError::Sink(_) => (
            only(&layout.sinks),
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
