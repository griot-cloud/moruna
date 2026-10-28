//! The host protocol's messages (MH 4.3): newline-delimited JSON, one object per line,
//! `type` first. Moruna sends `hello`, `heartbeat`, `limits_changed`, `event`, `port_call`,
//! `port_cancel`, `report` and `exit`; it reads `spec`, `cancel`, `checkpoint` and `port_reply`,
//! and nothing else. No message a peer can send enlarges a run.
//!
//! An `event` is one fact of the run's execution, versioned on its own (`v`, [`EVENTS_VERSION`])
//! and named by its `kind`: version 1 had the task lifecycle; version 2 added `logs`, what the
//! run's output store holds for each producer so far; version 3 added a running task's wait on a
//! port (`task_waiting` with `reason: port_call` and the `port`, MH 4.10); version 4 adds the
//! failure code `internal`, a crash: a panic of Moruna's own that unwound the run. A peer refuses
//! a version it does not read rather than guess at it.
//!
//! What a task prints is never on the socket. Each record of it is an `event` of kind `log`
//! ([`LogLine`]) in the run's output store (`report.logs`), numbered by its own producer; the
//! socket says only how far each producer's records reach, how many bytes they hold, and how
//! many were cut or dropped.
//!
//! The port messages (MH 4.10) are versioned on their own (`v`, [`PORTS_VERSION`]): a stage's
//! call to a host port is a `port_call`, answered by the peer's `port_reply` for the same `call`;
//! a call Moruna stops waiting for is a `port_cancel`. The bytes travel as standard base64.

use moruna_kernel::Limits;
use serde::{Deserialize, Serialize};

/// The limits as the protocol carries them (MH 4.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimitsMsg {
    /// The host memory ceiling in bytes.
    pub memory_ceiling: u64,
    /// Where the host would kill the process, when known.
    pub memory_kill: Option<u64>,
    /// CPUs, in cores.
    pub cpu_quota: f64,
    /// `Cgroup`, `Os` or `Explicit`.
    pub source: String,
}

impl LimitsMsg {
    /// The protocol's view of discovered limits.
    pub fn of(limits: &Limits) -> LimitsMsg {
        LimitsMsg {
            memory_ceiling: limits.memory_ceiling,
            memory_kill: limits.memory_kill,
            cpu_quota: limits.cpu_quota,
            source: format!("{:?}", limits.source),
        }
    }
}

/// One heartbeat (MH 4.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Milliseconds since the process began the run.
    pub t_ms: u64,
    /// The sink's commit watermark.
    pub committed_seq: Option<u64>,
    /// Rows the last kernel stage produced so far.
    pub rows_out: u64,
    /// Bytes the last kernel stage produced so far.
    pub bytes_out: u64,
    /// Workers allowed to take tasks.
    pub active_workers: u16,
    /// The memory ceiling in force.
    pub ceiling_bytes: Option<u64>,
    /// The process's anonymous memory.
    pub anon_bytes: Option<u64>,
    /// The controller's latest classification.
    pub bottleneck: Option<String>,
    /// Bytes on the disk tier.
    pub staging_bytes: u64,
}

impl Heartbeat {
    /// A heartbeat from what the observer knows, at `t_ms`.
    pub fn of(progress: &crate::observe::Progress, t_ms: u64) -> Heartbeat {
        Heartbeat {
            t_ms,
            committed_seq: progress.committed_seq,
            rows_out: progress.rows_out,
            bytes_out: progress.bytes_out,
            active_workers: progress.active_workers,
            ceiling_bytes: progress.ceiling_bytes,
            anon_bytes: progress.anon_bytes,
            bottleneck: progress.bottleneck.clone(),
            staging_bytes: progress.staging_bytes,
        }
    }
}

/// The version of the `event` message this build writes (MH 4.3).
pub const EVENTS_VERSION: u32 = 4;

/// The version of the port messages this build writes and reads (MH 4.10).
pub const PORTS_VERSION: u32 = 1;

/// One `event` (MH 4.3): who produced it and when, then the fact.
///
/// `attempt` is the document's `report.attempt`; `start` is this process's own id, minted when it
/// starts, so an attempt that a host ran twice is two producers; `seq` counts this process's
/// events from 1 with no gap, in the order they are written; `t_us` is microseconds on this
/// process's monotonic clock since it began the run. A peer that has seen `(start, seq)` has
/// seen the event, however often it is relayed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventMsg {
    /// [`EVENTS_VERSION`].
    pub v: u32,
    /// The document's `report.attempt`.
    pub attempt: u32,
    /// This process's id: 32 lowercase hex characters.
    pub start: String,
    /// From 1, with no gap.
    pub seq: u64,
    /// When the fact became true, in microseconds since the process began the run.
    pub t_us: u64,
    /// The fact.
    #[serde(flatten)]
    pub body: Event,
}

/// The facts of version 4: the task lifecycle, the output store's reach (MH 4.3), a running
/// task's waits on its ports (MH 4.10) and a crash's failures (`internal`). A task is one element of the chain, named by the id the
/// document's `report.tasks` gives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// First: this process runs the document's attempt; `resumed` when from a manifest.
    AttemptStarted {
        /// A resume of an earlier process's run.
        resumed: bool,
    },
    /// Once per task, before anything else is said of it.
    TaskRegistered {
        /// The task.
        task: String,
        /// `source`, `kernel` or `sink`.
        element: Element,
        /// Its place in the chain: 0 the source, 1..=n the kernels, n+1 the sink.
        stage: u16,
        /// The task whose output it takes; `null` for the source.
        upstream: Option<String>,
        /// The host's tasks it carries as one group (`report.tasks`); empty for one task.
        members: Vec<String>,
    },
    /// It cannot start yet, and why; or, running, it waits on a port's answer (`port_call`), until
    /// a `task_phase` says what it does again.
    TaskWaiting {
        /// The task.
        task: String,
        /// Why.
        reason: WaitReason,
        /// The tasks it waits on.
        blocking: Vec<String>,
        /// The port it waits on, for `port_call`; `null` otherwise.
        port: Option<String>,
    },
    /// Its input is there; it starts when a worker or a drive takes it.
    TaskReady {
        /// The task.
        task: String,
    },
    /// It took its first input (a source: issued its first read; a sink: was handed its first
    /// morsel, or began its commit with none).
    TaskStarted {
        /// The task.
        task: String,
        /// What it is doing.
        phase: Phase,
    },
    /// What it is doing changed.
    TaskPhase {
        /// The task.
        task: String,
        /// What it is doing now.
        phase: Phase,
    },
    /// It ended without failing itself: drained and done, stopped by another's failure, or
    /// cancelled.
    TaskCompleted {
        /// The task.
        task: String,
        /// How.
        outcome: TaskOutcome,
        /// From its start to its end on this process's clock; `null` when it never started (a
        /// stage that never had input).
        duration_us: Option<u64>,
        /// What it did, when it ran to its end (`succeeded`); `null` otherwise.
        counters: Option<Counters>,
        /// The task whose failure stopped it.
        cause: Option<String>,
    },
    /// It failed, or the attempt did (`scope: attempt`, `task: null`) for a reason no one task
    /// owns. `message` is the engine's own sentence about the failure and quotes no data; the
    /// failure's full words are in the report. For `internal`, a crash, which has no report,
    /// `message` is what Moruna's panic said.
    TaskFailed {
        /// The task; `null` when the scope is the attempt.
        task: Option<String>,
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
    /// How far the output store reaches for each producer whose reach changed (at most one
    /// such event every [`crate::host::logs::LOGS_EVERY`]), and, last, for every producer with
    /// `drained` set. The counts only grow: a peer that hears one late or twice is not misled.
    Logs {
        /// One entry per producer.
        producers: Vec<LogReach>,
    },
}

/// One producer's records in the output store so far (MH 4.3 `logs`): a task's, or the
/// engine's own (`task: null`), in this process.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LogReach {
    /// The task; `null` for output no task can be named for.
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

/// One record of a producer's output as the store keeps it (MH 4.3 `log`): an `event` whose
/// `seq` counts the producer's records from 1 with no gap, never written to the socket.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogLine {
    /// `"event"`.
    #[serde(rename = "type")]
    pub kind_of: &'static str,
    /// [`EVENTS_VERSION`].
    pub v: u32,
    /// The document's `report.attempt`.
    pub attempt: u32,
    /// This process's id.
    pub start: String,
    /// The producer's count, from 1.
    pub seq: u64,
    /// When the bytes were handed over, in microseconds since the process began the run.
    pub t_us: u64,
    /// `"log"`.
    pub kind: &'static str,
    /// `<start>/<producer>/<seq>`: the record's id, the same wherever it is read.
    pub id: String,
    /// The task; `null` for output no task can be named for.
    pub task: Option<String>,
    /// Which stream.
    pub stream: LogStream,
    /// The level a logging framework said; `null` for a stream, which says none.
    pub level: Option<&'static str>,
    /// Bytes of `text`, as UTF-8.
    pub bytes: u64,
    /// The record: one line without its newline, a group of lines (an exception), or a piece
    /// of a longer line.
    pub text: String,
    /// The line goes on in the next record.
    pub partial: bool,
    /// This record goes on from the last one's line.
    pub continued: bool,
    /// Bytes of the record that were cut and are nowhere.
    pub truncated: u64,
}

impl LogLine {
    /// The record as one line, newline included.
    pub fn line(&self) -> String {
        let mut text = serde_json::to_string(self).unwrap_or_default();
        text.push('\n');
        text
    }
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

impl Counters {
    /// The protocol's view of the scheduler's counters.
    pub fn of(c: &moruna_kernel::StageCounters) -> Counters {
        Counters {
            morsels: c.morsels,
            rows_in: c.rows_in,
            rows_out: c.rows_out,
            bytes_in: c.bytes_in,
            bytes_out: c.bytes_out,
            skipped: c.skipped,
            errors: c.errors,
        }
    }
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

/// A message from Moruna to its peer (MH 4.3).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outbound {
    /// First, on connect.
    Hello {
        /// `moruna --version`.
        moruna_version: String,
        /// The job document's digest; `null` in serve mode, where no document has arrived yet.
        spec_digest: Option<String>,
        /// The limits as discovered; `null` when discovery failed.
        limits: Option<LimitsMsg>,
    },
    /// At least every 2 s while the process lives.
    Heartbeat(Heartbeat),
    /// When discovery observes a change (MH 4.4; sent by the elastic budget, F8.2).
    LimitsChanged {
        /// Before.
        old: LimitsMsg,
        /// After.
        new: LimitsMsg,
        /// `memory` or `cpu`.
        reason: String,
    },
    /// One fact of the run's execution (MH 4.3), between `hello` and `report`.
    Event(EventMsg),
    /// A stage calls a host port (MH 4.10); the peer answers with `port_reply`.
    PortCall {
        /// [`PORTS_VERSION`].
        v: u32,
        /// The call's id, unique in this process.
        call: u64,
        /// The kernel stage that calls, 1..=n.
        stage: u16,
        /// The host's name for the port, as the stage's entry gives it (`{"host": "<name>"}`).
        port: String,
        /// How long the stage waits for the answer, in milliseconds.
        deadline_ms: u64,
        /// The request, standard base64.
        request: String,
    },
    /// Moruna stopped waiting for a call (MH 4.10); a `port_reply` that still comes is ignored.
    PortCancel {
        /// [`PORTS_VERSION`].
        v: u32,
        /// The call.
        call: u64,
        /// `deadline` or `cancelled`.
        reason: String,
    },
    /// The full run report, before `exit`, whenever the run produced one.
    Report {
        /// `RunReport` as JSON (04 d.1).
        report: serde_json::Value,
    },
    /// Last.
    Exit {
        /// The process's exit code (MH 4.2).
        code: i32,
        /// Why, when the code is not 0.
        diagnostic: Option<String>,
    },
}

impl Outbound {
    /// The message as one line, newline included.
    pub fn line(&self) -> String {
        let mut text = serde_json::to_string(self)
            .unwrap_or_else(|e| format!("{{\"type\":\"exit\",\"code\":1,\"diagnostic\":\"{e}\"}}"));
        text.push('\n');
        text
    }
}

/// A message from the peer to Moruna (MH 4.3).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Inbound {
    /// The job document, in serve mode; once.
    Spec {
        /// The document.
        spec: serde_json::Value,
    },
    /// Cancel the run: the manifest is written and the process exits 130.
    Cancel,
    /// Write the manifest now.
    Checkpoint,
    /// The answer to a `port_call` (MH 4.10): exactly one of `ok` and `error`.
    PortReply {
        /// [`PORTS_VERSION`].
        v: u32,
        /// The call it answers.
        call: u64,
        /// The answer, standard base64.
        #[serde(default)]
        ok: Option<String>,
        /// The port's refusal, handed to the stage.
        #[serde(default)]
        error: Option<String>,
    },
}

impl Inbound {
    /// Read one line. An unreadable line is an `Err` with the reason, which the session notes
    /// and otherwise ignores.
    pub fn parse(line: &str) -> Result<Inbound, String> {
        serde_json::from_str(line.trim_end()).map_err(|e| e.to_string())
    }
}

/// The longest line read before a `spec` has arrived, which is the document itself (MH 4.3).
pub const MAX_SPEC_LINE: usize = 16 << 20;
/// The longest line read once the run is going: a `cancel` or a `checkpoint` is a few bytes.
pub const MAX_CONTROL_LINE: usize = 64 << 10;
/// The longest line read from a peer that serves host ports: a `port_reply` carrying the largest
/// answer a call may have, in base64 (MH 4.10).
pub const MAX_PORT_LINE: usize = moruna_kernel::ports::PORT_BYTES_MAX / 3 * 4 + (64 << 10);

/// Read one line of at most `max` bytes. `Ok(None)` at the end of the stream; a longer line is
/// an error, so a peer cannot make the process hold an unbounded buffer.
pub fn read_line(reader: &mut dyn std::io::BufRead, max: usize) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&buf).into_owned())
            });
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if buf.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("a line longer than {max} bytes"),
            ));
        }
        if done {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MH 4.3, byte for byte: `type` first, then the fields in the order the table lists them,
    /// nulls written out.
    #[test]
    fn ho_t15_protocol_is_byte_exact() {
        let hello = Outbound::Hello {
            moruna_version: "0.1.1".into(),
            spec_digest: None,
            limits: Some(LimitsMsg {
                memory_ceiling: 1024,
                memory_kill: None,
                cpu_quota: 2.0,
                source: "Explicit".into(),
            }),
        };
        assert_eq!(
            hello.line(),
            "{\"type\":\"hello\",\"moruna_version\":\"0.1.1\",\"spec_digest\":null,\"limits\":\
             {\"memory_ceiling\":1024,\"memory_kill\":null,\"cpu_quota\":2.0,\"source\":\"Explicit\"}}\n"
        );
        let beat = Outbound::Heartbeat(Heartbeat {
            t_ms: 1000,
            committed_seq: Some(3),
            rows_out: 10,
            bytes_out: 80,
            active_workers: 2,
            ceiling_bytes: Some(1024),
            anon_bytes: Some(512),
            bottleneck: Some("Compute".into()),
            staging_bytes: 0,
        });
        assert_eq!(
            beat.line(),
            "{\"type\":\"heartbeat\",\"t_ms\":1000,\"committed_seq\":3,\"rows_out\":10,\
             \"bytes_out\":80,\"active_workers\":2,\"ceiling_bytes\":1024,\"anon_bytes\":512,\
             \"bottleneck\":\"Compute\",\"staging_bytes\":0}\n"
        );
        let limits = LimitsMsg {
            memory_ceiling: 1,
            memory_kill: Some(2),
            cpu_quota: 0.5,
            source: "Os".into(),
        };
        let changed = Outbound::LimitsChanged {
            old: limits.clone(),
            new: limits,
            reason: "memory".into(),
        };
        assert!(
            changed
                .line()
                .starts_with("{\"type\":\"limits_changed\",\"old\":{\"memory_ceiling\":1,")
        );
        assert!(changed.line().ends_with(",\"reason\":\"memory\"}\n"));
        let report = Outbound::Report {
            report: serde_json::json!({"run_id": "ab"}),
        };
        assert_eq!(
            report.line(),
            "{\"type\":\"report\",\"report\":{\"run_id\":\"ab\"}}\n"
        );
        let exit = Outbound::Exit {
            code: 130,
            diagnostic: Some("cancelled".into()),
        };
        assert_eq!(
            exit.line(),
            "{\"type\":\"exit\",\"code\":130,\"diagnostic\":\"cancelled\"}\n"
        );
    }

    /// An `event` (MH 4.3): `type`, then who produced it and when, then its `kind` and the fact,
    /// nulls written out.
    #[test]
    fn an_event_is_byte_exact() {
        let event = |body| {
            Outbound::Event(EventMsg {
                v: EVENTS_VERSION,
                attempt: 2,
                start: "0f".repeat(16),
                seq: 7,
                t_us: 1500,
                body,
            })
            .line()
        };
        assert_eq!(
            event(Event::TaskStarted {
                task: "orders".into(),
                phase: Phase::Reading,
            }),
            format!(
                "{{\"type\":\"event\",\"v\":4,\"attempt\":2,\"start\":\"{}\",\"seq\":7,\
                 \"t_us\":1500,\"kind\":\"task_started\",\"task\":\"orders\",\"phase\":\"reading\"}}\n",
                "0f".repeat(16)
            )
        );
        let failed = event(Event::TaskFailed {
            task: None,
            code: FailureCode::IoError,
            message: "the read_file operation failed".into(),
            scope: FailureScope::Attempt,
            retryable: true,
        });
        assert!(
            failed.ends_with(
                "\"kind\":\"task_failed\",\"task\":null,\"code\":\"io_error\",\
                 \"message\":\"the read_file operation failed\",\"scope\":\"attempt\",\
                 \"retryable\":true}\n"
            ),
            "{failed}"
        );
        let crashed = event(Event::TaskFailed {
            task: Some("orders".into()),
            code: FailureCode::Internal,
            message: "index out of bounds".into(),
            scope: FailureScope::Task,
            retryable: false,
        });
        assert!(
            crashed.ends_with(
                "\"kind\":\"task_failed\",\"task\":\"orders\",\"code\":\"internal\",\
                 \"message\":\"index out of bounds\",\"scope\":\"task\",\"retryable\":false}\n"
            ),
            "{crashed}"
        );
        assert!(event(Event::CancelAcknowledged).ends_with("\"kind\":\"cancel_acknowledged\"}\n"));
        let waiting = event(Event::TaskWaiting {
            task: "triage".into(),
            reason: WaitReason::PortCall,
            blocking: Vec::new(),
            port: Some("model".into()),
        });
        assert!(
            waiting.ends_with(
                "\"kind\":\"task_waiting\",\"task\":\"triage\",\"reason\":\"port_call\",\
                 \"blocking\":[],\"port\":\"model\"}\n"
            ),
            "{waiting}"
        );
        let done = event(Event::TaskCompleted {
            task: "t1".into(),
            outcome: TaskOutcome::Succeeded,
            duration_us: None,
            counters: Some(Counters::default()),
            cause: None,
        });
        assert!(
            done.ends_with(
                "\"outcome\":\"succeeded\",\"duration_us\":null,\"counters\":{\"morsels\":0,\
                 \"rows_in\":0,\"rows_out\":0,\"bytes_in\":0,\"bytes_out\":0,\"skipped\":0,\
                 \"errors\":0},\"cause\":null}\n"
            ),
            "{done}"
        );
    }

    /// `logs` on the socket and a `log` record in the store (MH 4.3): the same `event` framing,
    /// every field written out, nulls included; the store's record carries the text, the socket
    /// only counts.
    #[test]
    fn output_is_counted_on_the_socket_and_kept_in_the_store() {
        let logs = Outbound::Event(EventMsg {
            v: EVENTS_VERSION,
            attempt: 1,
            start: "0f".repeat(16),
            seq: 9,
            t_us: 10,
            body: Event::Logs {
                producers: vec![
                    LogReach {
                        task: Some("k1".into()),
                        records: 3,
                        bytes: 40,
                        truncated: 1,
                        dropped: 2,
                        dropped_bytes: 7,
                        drained: true,
                    },
                    LogReach::default(),
                ],
            },
        })
        .line();
        assert!(
            logs.ends_with(
                "\"kind\":\"logs\",\"producers\":[{\"task\":\"k1\",\"records\":3,\"bytes\":40,\
                 \"truncated\":1,\"dropped\":2,\"dropped_bytes\":7,\"drained\":true},\
                 {\"task\":null,\"records\":0,\"bytes\":0,\"truncated\":0,\"dropped\":0,\
                 \"dropped_bytes\":0,\"drained\":false}]}\n"
            ),
            "{logs}"
        );
        let record = LogLine {
            kind_of: "event",
            v: EVENTS_VERSION,
            attempt: 1,
            start: "0f".repeat(16),
            seq: 4,
            t_us: 77,
            kind: "log",
            id: format!("{}/k1/4", "0f".repeat(16)),
            task: Some("k1".into()),
            stream: LogStream::Stderr,
            level: None,
            bytes: 2,
            text: "hi".into(),
            partial: false,
            continued: true,
            truncated: 0,
        };
        assert_eq!(
            record.line(),
            format!(
                "{{\"type\":\"event\",\"v\":4,\"attempt\":1,\"start\":\"{s}\",\"seq\":4,\
                 \"t_us\":77,\"kind\":\"log\",\"id\":\"{s}/k1/4\",\"task\":\"k1\",\
                 \"stream\":\"stderr\",\"level\":null,\"bytes\":2,\"text\":\"hi\",\
                 \"partial\":false,\"continued\":true,\"truncated\":0}}\n",
                s = "0f".repeat(16)
            )
        );
    }

    /// The four inbound messages and nothing else; an unknown type is refused.
    #[test]
    fn inbound_is_four_messages() {
        assert_eq!(
            Inbound::parse("{\"type\":\"cancel\"}\n"),
            Ok(Inbound::Cancel)
        );
        assert_eq!(
            Inbound::parse("{\"type\":\"checkpoint\"}"),
            Ok(Inbound::Checkpoint)
        );
        assert_eq!(
            Inbound::parse("{\"type\":\"spec\",\"spec\":{\"a\":1}}"),
            Ok(Inbound::Spec {
                spec: serde_json::json!({"a": 1})
            })
        );
        assert!(Inbound::parse("{\"type\":\"resize\",\"memory\":1}").is_err());
        // A field beside `cancel` or `checkpoint` changes nothing: the message is what it says.
        assert_eq!(
            Inbound::parse("{\"type\":\"cancel\",\"budget\":1}"),
            Ok(Inbound::Cancel)
        );
        assert!(Inbound::parse("{\"type\":\"spec\",\"spec\":{},\"budget\":1}").is_err());
        assert!(Inbound::parse("not json").is_err());
        assert_eq!(
            Inbound::parse("{\"type\":\"port_reply\",\"v\":1,\"call\":7,\"ok\":\"aGk=\"}"),
            Ok(Inbound::PortReply {
                v: 1,
                call: 7,
                ok: Some("aGk=".into()),
                error: None
            })
        );
        assert!(
            Inbound::parse("{\"type\":\"port_reply\",\"v\":1,\"call\":7,\"budget\":1}").is_err()
        );
    }

    /// MH 4.10, byte for byte: a `port_call` and a `port_cancel`.
    #[test]
    fn the_port_messages_are_byte_exact() {
        let call = Outbound::PortCall {
            v: PORTS_VERSION,
            call: 3,
            stage: 1,
            port: "mcp".into(),
            deadline_ms: 600_000,
            request: "aGk=".into(),
        };
        assert_eq!(
            call.line(),
            "{\"type\":\"port_call\",\"v\":1,\"call\":3,\"stage\":1,\"port\":\"mcp\",\
             \"deadline_ms\":600000,\"request\":\"aGk=\"}\n"
        );
        let cancel = Outbound::PortCancel {
            v: PORTS_VERSION,
            call: 3,
            reason: "deadline".into(),
        };
        assert_eq!(
            cancel.line(),
            "{\"type\":\"port_cancel\",\"v\":1,\"call\":3,\"reason\":\"deadline\"}\n"
        );
    }

    #[test]
    fn lines_are_bounded() {
        let mut short = std::io::Cursor::new(b"one\ntwo".to_vec());
        assert_eq!(
            read_line(&mut short, 16).expect("one"),
            Some("one\n".into())
        );
        assert_eq!(read_line(&mut short, 16).expect("two"), Some("two".into()));
        assert_eq!(read_line(&mut short, 16).expect("end"), None);
        let mut long = std::io::Cursor::new(vec![b'x'; 100]);
        assert!(read_line(&mut long, 16).is_err());
    }
}
