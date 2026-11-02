//! The facts of a run as Rust values (MH 4.3 `event`, version 7), and how they render as the JSON
//! people read.
//!
//! A task is one task of the job's DAG, named by the id the document gives it, and
//! `task_registered` names the tasks it takes its input from, which are the job's edges (SDD 22
//! e.4). Every task event names the step of the run's plan that runs the task, `step`: `None`
//! only when the document was refused before it was planned (SDD 26).

use serde::Serialize;

/// The version of the `event` this build writes (MH 4.3). Version 7 is the first whose event is a
/// record (SDD 21) carrying its fact as FlatBuffers; version 6 was a line of JSON.
pub const EVENTS_VERSION: u32 = 7;

/// The facts of a run: the task lifecycle, the plan's steps, the output store's reach and a
/// record of output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// First: this process runs the document's attempt; `resumed` when from a manifest.
    AttemptStarted {
        /// A resume of an earlier process's run.
        resumed: bool,
        /// The id of the plan the attempt runs; `None` when the document was refused before it
        /// was planned.
        plan: Option<String>,
    },
    /// A step of the plan started: the first of its tasks started.
    StepStarted {
        /// The step's id.
        step: String,
        /// The plan's id.
        plan: String,
        /// Its group in the plan.
        group: u32,
    },
    /// A step of the plan ended: every one of its tasks ended. A step that never started ends
    /// too, `skipped`, with no `step_started` before it.
    StepEnded {
        /// The step's id.
        step: String,
        /// The plan's id.
        plan: String,
        /// How: the first of `failed`, `cancelled`, `stopped`, `skipped`, `succeeded` any of its
        /// tasks ended with.
        outcome: StepOutcome,
        /// From its start to its end; `None` when it never started.
        duration_us: Option<u64>,
    },
    /// Once per task, before anything else is said of it.
    TaskRegistered {
        /// The task.
        task: String,
        /// The plan step that runs it.
        step: Option<String>,
        /// `source`, `kernel` or `sink`.
        element: Element,
        /// Its place in the run: 0 a source, 1..=n the kernel stages in an order in which every
        /// edge goes forward, n+1 a sink (SDD 22 e.4).
        stage: u16,
        /// The tasks whose output it takes, by its input ports in order: its edges. Empty for a
        /// source.
        upstream: Vec<String>,
        /// The host's tasks it carries as one group (`report.tasks`); empty for one task.
        members: Vec<String>,
    },
    /// It cannot start yet, and why; or, running, it waits on a port's answer (`port_call`),
    /// until a `task_phase` says what it does again.
    TaskWaiting {
        /// The task.
        task: String,
        /// The plan step that runs it.
        step: Option<String>,
        /// Why.
        reason: WaitReason,
        /// The tasks it waits on.
        blocking: Vec<String>,
        /// The port it waits on, for `port_call`; `None` otherwise.
        port: Option<String>,
    },
    /// Its input is there; it starts when a worker or a drive takes it.
    TaskReady {
        /// The task.
        task: String,
        /// The plan step that runs it.
        step: Option<String>,
    },
    /// It took its first input (a source: issued its first read; a sink: was handed its first
    /// morsel, or began its commit with none).
    TaskStarted {
        /// The task.
        task: String,
        /// The plan step that runs it.
        step: Option<String>,
        /// What it is doing.
        phase: Phase,
    },
    /// What it is doing changed.
    TaskPhase {
        /// The task.
        task: String,
        /// The plan step that runs it.
        step: Option<String>,
        /// What it is doing now.
        phase: Phase,
    },
    /// It ended without failing itself: drained and done, stopped by another's failure, or
    /// cancelled.
    TaskCompleted {
        /// The task.
        task: String,
        /// The plan step that runs it.
        step: Option<String>,
        /// How.
        outcome: TaskOutcome,
        /// From its start to its end on this process's clock; `None` when it never started (a
        /// stage that never had input).
        duration_us: Option<u64>,
        /// What it did, when it ran to its end (`succeeded`); `None` otherwise.
        counters: Option<Counters>,
        /// The task whose failure stopped it.
        cause: Option<String>,
    },
    /// It failed, or the attempt did (`scope: attempt`, `task: None`) for a reason no one task
    /// owns. `message` is the engine's own sentence about the failure and quotes no data; the
    /// failure's full words are in the report. For `internal`, a crash, which has no report,
    /// `message` is what Moruna's panic said.
    TaskFailed {
        /// The task; `None` when the scope is the attempt.
        task: Option<String>,
        /// The plan step that runs it.
        step: Option<String>,
        /// A stable code.
        code: FailureCode,
        /// A sentence that names stages, morsels and sizes, never a value.
        message: String,
        /// `task` or `attempt`.
        scope: FailureScope,
        /// Whether the same document may succeed when run again.
        retryable: bool,
    },
    /// It never started and never will.
    TaskSkipped {
        /// The task.
        task: String,
        /// The plan step that runs it.
        step: Option<String>,
        /// Why.
        reason: SkipReason,
        /// The task whose failure skipped it.
        blocking: Option<String>,
    },
    /// The peer's `cancel` reached the run.
    CancelAcknowledged,
    /// The run has stopped since a `cancel` reached it, and how it ended.
    CancelEnded {
        /// `cancelled`, or `completed` / `failed` when the run ended first.
        disposition: CancelDisposition,
    },
    /// How far the output store reaches for each producer whose reach changed, and, last, for
    /// every producer with `drained` set. The counts only grow: a peer that hears one late or
    /// twice is not misled.
    Logs {
        /// One entry per producer.
        producers: Vec<LogReach>,
    },
    /// One record of a producer's output as the store keeps it. Its record's `seq` counts the
    /// producer's records from 1 with no gap.
    Log {
        /// The task; `None` for output no task can be named for, which is the engine's.
        task: Option<String>,
        /// Which stream.
        stream: LogStream,
        /// The level a logging framework said; `None` for a stream, which says none.
        level: Option<String>,
        /// Bytes of `text`, as UTF-8.
        bytes: u64,
        /// The record: one line without its newline, a group of lines (an exception), or a piece
        /// of a longer line.
        text: String,
        /// The line goes on in the next record.
        partial: bool,
        /// This record goes on from the last one's line.
        continued: bool,
        /// Bytes of the record that were cut and are nowhere.
        truncated: u64,
    },
}

/// The directory name of the engine's own output, and the producer of a record with no task: no
/// task id starts with `_`.
pub const ENGINE: &str = "_engine";

/// One producer's records in the output store so far (MH 4.3 `logs`): a task's, or the
/// engine's own (`task: None`), in this process.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LogReach {
    /// The task; `None` for output no task can be named for.
    pub task: Option<String>,
    /// Records `1..=records` of the producer are in the store, whole.
    pub records: u64,
    /// The bytes of their text.
    pub bytes: u64,
    /// Records the store cut short.
    pub truncated: u64,
    /// Records dropped before the store, written faster than it keeps or past what it holds.
    pub dropped: u64,
    /// Bytes dropped with them.
    pub dropped_bytes: u64,
    /// This process writes nothing more for the producer.
    pub drained: bool,
}

/// What a record of output is: a task's stream, a logging framework's record, or Moruna's own
/// word about the output (records dropped, the store's end). Only Moruna writes `platform`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    /// Standard output.
    Stdout,
    /// Standard error; it says nothing about severity.
    Stderr,
    /// A logging framework's record, with the level it said.
    Structured,
    /// Moruna's own.
    Platform,
}

/// How a plan step ended: the first of these any of its tasks ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// A task of it failed.
    Failed,
    /// A task of it was cancelled.
    Cancelled,
    /// A task of it was stopped by another's failure.
    Stopped,
    /// A task of it never started.
    Skipped,
    /// Every task of it succeeded.
    Succeeded,
}

/// What a task is (MH 4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Element {
    /// The chain's source.
    Source,
    /// One kernel stage.
    Kernel,
    /// The chain's sink.
    Sink,
}

/// Why a task waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// The engine is still being made ready: discovery, the arena, the plan.
    EnginePreparing,
    /// No input has reached it yet.
    AwaitingInput,
    /// It called a port and waits for the answer (MH 4.10).
    PortCall,
}

/// What a running task is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Working on the morsels the controller measures the chain with, before the run proper.
    Calibrating,
    /// A source reading.
    Reading,
    /// A kernel applying.
    Applying,
    /// A sink writing.
    Writing,
    /// Its input is closed: working through what is left.
    Draining,
    /// A sink committing what it wrote.
    Committing,
}

/// How a task that did not fail itself ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    /// Drained and done: a source exhausted, a kernel through its last input, a sink committed.
    Succeeded,
    /// Running when another task's failure ended the attempt.
    Stopped,
    /// Running when a `cancel` ended the attempt.
    Cancelled,
}

/// What a task did, measured where it happened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counters {
    /// Morsels taken (a kernel, the sink) or produced (the source).
    pub morsels: u64,
    /// Rows taken.
    pub rows_in: u64,
    /// Rows produced; the sink's, rows written.
    pub rows_out: u64,
    /// Bytes taken.
    pub bytes_in: u64,
    /// Bytes produced; the sink's, bytes written.
    pub bytes_out: u64,
    /// Morsels the error policy skipped.
    pub skipped: u64,
    /// Morsels whose kernel failed.
    pub errors: u64,
}

/// A failure's stable code (MH 4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// The document was refused.
    SpecRefused,
    /// A kernel failed and the error policy ended the run.
    KernelError,
    /// A morsel's footprint exceeded the budget.
    BudgetExceeded,
    /// The allocator guard refused a kernel's allocation past the ceiling.
    MemoryRefused,
    /// The source could not read.
    SourceError,
    /// The sink could not write or commit.
    SinkError,
    /// An allocation exceeded its tier's budget.
    AllocRefused,
    /// An IO operation failed.
    IoError,
    /// Staging or placement failed.
    StagingError,
    /// A table-to-tensor conversion failed.
    ConvertError,
    /// A configuration value was refused.
    ConfigRefused,
    /// The plan was refused.
    PlanRefused,
    /// Resume was refused.
    ResumeRefused,
    /// The run needs something this build does not do.
    Unsupported,
    /// The engine stopped as cancelled with no cancel asked of it.
    Cancelled,
    /// A crash: a panic of Moruna's own unwound the run, a defect of the engine and never a
    /// cancel. Every task still running fails with it, or the attempt does when none was.
    Internal,
}

/// Who a failure belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureScope {
    /// The task the event names.
    Task,
    /// The attempt as a whole: no one task owns the cause.
    Attempt,
}

/// Why a task was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Another task failed first (`blocking`).
    FailedTask,
    /// The attempt failed for a reason no task owns.
    AttemptFailed,
    /// The run was cancelled first.
    Cancelled,
}

/// How a cancelled run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelDisposition {
    /// The cancel stopped it.
    Cancelled,
    /// It completed before the cancel could stop it.
    Completed,
    /// It failed before the cancel could stop it.
    Failed,
}

/// The kind of an event, which is also the subject of its record: [`SUBJECT_BASE`] plus the
/// kind's number in the schema's `Body` union.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Kind {
    /// `attempt_started`.
    AttemptStarted = 1,
    /// `step_started`.
    StepStarted = 2,
    /// `step_ended`.
    StepEnded = 3,
    /// `task_registered`.
    TaskRegistered = 4,
    /// `task_waiting`.
    TaskWaiting = 5,
    /// `task_ready`.
    TaskReady = 6,
    /// `task_started`.
    TaskStarted = 7,
    /// `task_phase`.
    TaskPhase = 8,
    /// `task_completed`.
    TaskCompleted = 9,
    /// `task_failed`.
    TaskFailed = 10,
    /// `task_skipped`.
    TaskSkipped = 11,
    /// `cancel_acknowledged`.
    CancelAcknowledged = 12,
    /// `cancel_ended`.
    CancelEnded = 13,
    /// `logs`.
    Logs = 14,
    /// `log`.
    Log = 15,
}

/// The subject of a record of kind `k` is this plus `k as u32`: the ASCII "ME" in the high half,
/// so no small number another writer of records uses is an event's.
pub const SUBJECT_BASE: u32 = 0x4D45_0000;

impl Kind {
    /// Every kind, in the order of the numbers.
    pub const ALL: [Kind; 15] = [
        Kind::AttemptStarted,
        Kind::StepStarted,
        Kind::StepEnded,
        Kind::TaskRegistered,
        Kind::TaskWaiting,
        Kind::TaskReady,
        Kind::TaskStarted,
        Kind::TaskPhase,
        Kind::TaskCompleted,
        Kind::TaskFailed,
        Kind::TaskSkipped,
        Kind::CancelAcknowledged,
        Kind::CancelEnded,
        Kind::Logs,
        Kind::Log,
    ];

    /// The record subject of this kind.
    pub fn subject(self) -> u32 {
        SUBJECT_BASE + self as u32
    }

    /// The kind a record subject names, or `None` for a subject that is not an event's.
    pub fn of_subject(subject: u32) -> Option<Kind> {
        let number = subject.checked_sub(SUBJECT_BASE)?;
        Kind::ALL.iter().copied().find(|k| *k as u32 == number)
    }

    /// The name the JSON rendering gives (`kind`).
    pub fn name(self) -> &'static str {
        match self {
            Kind::AttemptStarted => "attempt_started",
            Kind::StepStarted => "step_started",
            Kind::StepEnded => "step_ended",
            Kind::TaskRegistered => "task_registered",
            Kind::TaskWaiting => "task_waiting",
            Kind::TaskReady => "task_ready",
            Kind::TaskStarted => "task_started",
            Kind::TaskPhase => "task_phase",
            Kind::TaskCompleted => "task_completed",
            Kind::TaskFailed => "task_failed",
            Kind::TaskSkipped => "task_skipped",
            Kind::CancelAcknowledged => "cancel_acknowledged",
            Kind::CancelEnded => "cancel_ended",
            Kind::Logs => "logs",
            Kind::Log => "log",
        }
    }
}

impl Event {
    /// The kind of this event.
    pub fn kind(&self) -> Kind {
        match self {
            Event::AttemptStarted { .. } => Kind::AttemptStarted,
            Event::StepStarted { .. } => Kind::StepStarted,
            Event::StepEnded { .. } => Kind::StepEnded,
            Event::TaskRegistered { .. } => Kind::TaskRegistered,
            Event::TaskWaiting { .. } => Kind::TaskWaiting,
            Event::TaskReady { .. } => Kind::TaskReady,
            Event::TaskStarted { .. } => Kind::TaskStarted,
            Event::TaskPhase { .. } => Kind::TaskPhase,
            Event::TaskCompleted { .. } => Kind::TaskCompleted,
            Event::TaskFailed { .. } => Kind::TaskFailed,
            Event::TaskSkipped { .. } => Kind::TaskSkipped,
            Event::CancelAcknowledged => Kind::CancelAcknowledged,
            Event::CancelEnded { .. } => Kind::CancelEnded,
            Event::Logs { .. } => Kind::Logs,
            Event::Log { .. } => Kind::Log,
        }
    }

    /// The task the event is about; `None` for an event of the attempt, of a step or of the
    /// output store as a whole, and for a record of the engine's own output.
    pub fn task(&self) -> Option<&str> {
        match self {
            Event::TaskRegistered { task, .. }
            | Event::TaskWaiting { task, .. }
            | Event::TaskReady { task, .. }
            | Event::TaskStarted { task, .. }
            | Event::TaskPhase { task, .. }
            | Event::TaskCompleted { task, .. }
            | Event::TaskSkipped { task, .. } => Some(task),
            Event::TaskFailed { task, .. } | Event::Log { task, .. } => task.as_deref(),
            Event::AttemptStarted { .. }
            | Event::StepStarted { .. }
            | Event::StepEnded { .. }
            | Event::CancelAcknowledged
            | Event::CancelEnded { .. }
            | Event::Logs { .. } => None,
        }
    }

    /// The plan step the event is about: a step's own events name it, a task's events name the
    /// step that runs the task.
    pub fn step(&self) -> Option<&str> {
        match self {
            Event::StepStarted { step, .. } | Event::StepEnded { step, .. } => Some(step),
            Event::TaskRegistered { step, .. }
            | Event::TaskWaiting { step, .. }
            | Event::TaskReady { step, .. }
            | Event::TaskStarted { step, .. }
            | Event::TaskPhase { step, .. }
            | Event::TaskCompleted { step, .. }
            | Event::TaskFailed { step, .. }
            | Event::TaskSkipped { step, .. } => step.as_deref(),
            Event::AttemptStarted { .. }
            | Event::CancelAcknowledged
            | Event::CancelEnded { .. }
            | Event::Logs { .. }
            | Event::Log { .. } => None,
        }
    }
}

/// One event as a peer reads it: who told it and when, then the fact.
///
/// `attempt` is the document's `report.attempt`; `start` is the telling process's own id, minted
/// when it starts, so an attempt that a host ran twice is two producers; `seq` counts the
/// process's events from 1 with no gap, in the order they are written (a record of output,
/// `log`, is numbered by its own producer instead); `t_us` is microseconds on the process's
/// monotonic clock since it began the run, and `ts_ns` the same instant in unix nanoseconds. A
/// peer that has seen `(start, seq)` has seen the event, however often it is relayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventMsg {
    /// The run's id, from the record's `run` header; `None` when the writer did not know it.
    pub run: Option<String>,
    /// The plan's id, from the record's `plan` header; `None` for a run with no plan.
    pub plan: Option<String>,
    /// The document's `report.attempt`.
    pub attempt: u32,
    /// The telling process's id: 32 lowercase hex characters.
    pub start: String,
    /// The record's `seq`.
    pub seq: u64,
    /// The record's `ts`, unix nanoseconds.
    pub ts_ns: i64,
    /// When the fact became true, in microseconds since the process began the run.
    pub t_us: u64,
    /// The record's `msg_id`: `<attempt>-<start>-<seq>`, unique by construction: `start` is
    /// the telling process's own random id, so two processes of one attempt, and any two runs, tell
    /// disjoint ids; the same wherever the event is read.
    pub msg_id: String,
    /// The fact.
    pub body: Event,
}

#[derive(Serialize)]
struct Line<'a> {
    #[serde(rename = "type")]
    kind_of: &'static str,
    v: u32,
    attempt: u32,
    start: &'a str,
    seq: u64,
    t_us: u64,
    #[serde(flatten)]
    body: &'a Event,
}

#[derive(Serialize)]
struct LogLine<'a> {
    #[serde(rename = "type")]
    kind_of: &'static str,
    v: u32,
    attempt: u32,
    start: &'a str,
    seq: u64,
    t_us: u64,
    kind: &'static str,
    id: &'a str,
    task: &'a Option<String>,
    stream: LogStream,
    level: &'a Option<String>,
    bytes: u64,
    text: &'a str,
    partial: bool,
    continued: bool,
    truncated: u64,
}

impl EventMsg {
    /// The event as the JSON people and `moruna run --events-json` read, one line, newline
    /// included: `type`, who told it and when, then the fact, every field written out, nulls
    /// included. The names are the version 6 line's, with `v` 7; a record of output carries its
    /// `id`, `<start>/<producer>/<seq>`.
    pub fn json_line(&self) -> String {
        let mut text = self.json_text();
        text.push('\n');
        text
    }

    /// [`Self::json_line`] without the newline.
    pub fn json_text(&self) -> String {
        let rendered = match &self.body {
            Event::Log {
                task,
                stream,
                level,
                bytes,
                text,
                partial,
                continued,
                truncated,
            } => serde_json::to_string(&LogLine {
                kind_of: "event",
                v: EVENTS_VERSION,
                attempt: self.attempt,
                start: &self.start,
                seq: self.seq,
                t_us: self.t_us,
                kind: "log",
                id: &format!(
                    "{}/{}/{}",
                    self.start,
                    task.as_deref().unwrap_or(ENGINE),
                    self.seq
                ),
                task,
                stream: *stream,
                level,
                bytes: *bytes,
                text,
                partial: *partial,
                continued: *continued,
                truncated: *truncated,
            }),
            body => serde_json::to_string(&Line {
                kind_of: "event",
                v: EVENTS_VERSION,
                attempt: self.attempt,
                start: &self.start,
                seq: self.seq,
                t_us: self.t_us,
                body,
            }),
        };
        // Every field is a string, a number, a boolean or a list of them: serialising cannot
        // fail, and if it ever did the line says so rather than being empty.
        rendered.unwrap_or_else(|e| format!("{{\"type\":\"event\",\"render_error\":\"{e}\"}}"))
    }

    /// The event as a JSON value, for a reader that wants fields and not text.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::from_str(&self.json_text()).unwrap_or(serde_json::Value::Null)
    }
}
