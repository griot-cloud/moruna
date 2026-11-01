//! An event as a record and back (MH 4.3 version 7, SDD 21).
//!
//! [`Encoder::encode`] writes one event as one `moruna-record` record: `seq` the event's number,
//! `ts` unix nanoseconds, `subject` the kind's ([`Kind::subject`]), `msg_id`
//! `<attempt>-<start>-<seq>` (a record of output: `<attempt>-<start>-<producer>-<seq>`), the headers `run`, `attempt`, `plan`, `step`
//! and `task` where they apply, and the body the FlatBuffers of the event's fact. It reuses its
//! buffers, so once they have grown to the largest event it has written, it allocates nothing.
//!
//! [`decode`] is the other way and takes anyone's bytes: the record's own checks (length, crc,
//! ids, headers), then the FlatBuffers verifier, then every value an event holds. It never
//! panics.

use std::fmt::Write as _;

use flatbuffers::{FlatBufferBuilder, ForwardsUOffset, Vector, WIPOffset};
use moruna_record::{Decoded, decode as decode_record, encode_into, encoded_len};

use crate::error::{DecodeError, EncodeError};
use crate::generated::moruna::event::fb;
use crate::model::{
    CancelDisposition, Counters, ENGINE, Element, Event, EventMsg, FailureCode, FailureScope, Kind,
    LogReach, LogStream, Phase, SkipReason, StepOutcome, TaskOutcome, WaitReason,
};

/// The FlatBuffers file identifier of an event body, at bytes 4 to 8 of every body.
pub const EVENT_IDENTIFIER: &str = "MEV1";

/// Who tells the events of a process: what its records carry besides the fact.
#[derive(Clone, Copy, Debug)]
pub struct Stamp<'a> {
    /// The run's id (`run` header); `None` when the writer does not know it.
    pub run: Option<&'a str>,
    /// The plan's id (`plan` header); `None` for a run with no plan.
    pub plan: Option<&'a str>,
    /// The document's `report.attempt` (`attempt` header).
    pub attempt: u32,
    /// The telling process's id: 32 lowercase hex characters.
    pub start: &'a str,
}

/// Writes events as records, reusing its buffers.
pub struct Encoder {
    builder: FlatBufferBuilder<'static>,
    record: Vec<u8>,
    msg_id: String,
    attempt: String,
}

impl Default for Encoder {
    fn default() -> Encoder {
        Encoder::new()
    }
}

impl Encoder {
    /// An encoder with empty buffers.
    pub fn new() -> Encoder {
        Encoder {
            builder: FlatBufferBuilder::new(),
            record: Vec::new(),
            msg_id: String::new(),
            attempt: String::new(),
        }
    }

    /// `event` as a record, numbered `seq`, true at `t_us` microseconds on the process's clock,
    /// which is `ts_ns` in unix nanoseconds. The bytes are valid until the next call.
    pub fn encode(
        &mut self,
        stamp: &Stamp<'_>,
        seq: u64,
        ts_ns: i64,
        t_us: u64,
        event: &Event,
    ) -> Result<&[u8], EncodeError> {
        self.builder.reset();
        let body = build(&mut self.builder, stamp.start, t_us, event);
        self.builder.finish(body, Some(EVENT_IDENTIFIER));
        self.msg_id.clear();
        self.attempt.clear();
        // Writing to a String does not fail.
        let _ = write!(self.attempt, "{}", stamp.attempt);
        match event {
            Event::Log { task, .. } => {
                let producer = task.as_deref().unwrap_or(ENGINE);
                let _ = write!(
                    self.msg_id,
                    "{}-{}-{producer}-{seq}",
                    stamp.attempt, stamp.start
                );
            }
            _ => {
                let _ = write!(self.msg_id, "{}-{}-{seq}", stamp.attempt, stamp.start);
            }
        }
        let mut headers: [(&str, &str); 5] = [("", ""); 5];
        let mut n = 0;
        for (key, value) in [
            ("run", stamp.run),
            ("attempt", Some(self.attempt.as_str())),
            ("plan", stamp.plan),
            ("step", event.step()),
            ("task", event.task()),
        ] {
            if let Some(value) = value {
                headers[n] = (key, value);
                n += 1;
            }
        }
        let headers = &headers[..n];
        let body = self.builder.finished_data();
        let total = encoded_len(&self.msg_id, headers, body).map_err(EncodeError::Record)?;
        if self.record.len() < total {
            self.record.resize(total, 0);
        }
        let written = encode_into(
            &mut self.record,
            seq,
            ts_ns,
            event.kind().subject(),
            &self.msg_id,
            headers,
            body,
        )
        .map_err(EncodeError::Record)?;
        Ok(&self.record[..written])
    }
}

fn string<'a>(b: &mut FlatBufferBuilder<'a>, s: &str) -> WIPOffset<&'a str> {
    b.create_string(s)
}

fn opt_string<'a>(b: &mut FlatBufferBuilder<'a>, s: Option<&str>) -> Option<WIPOffset<&'a str>> {
    s.map(|s| b.create_string(s))
}

type Strings<'a> = WIPOffset<Vector<'a, ForwardsUOffset<&'a str>>>;

fn strings<'a>(b: &mut FlatBufferBuilder<'a>, items: &[String]) -> Option<Strings<'a>> {
    if items.is_empty() {
        return None;
    }
    let offsets: Vec<WIPOffset<&'a str>> = items.iter().map(|s| b.create_string(s)).collect();
    Some(b.create_vector(&offsets))
}

fn build<'a>(
    b: &mut FlatBufferBuilder<'a>,
    start: &str,
    t_us: u64,
    event: &Event,
) -> WIPOffset<fb::Event<'a>> {
    let start = b.create_string(start);
    let (body_type, body) = match event {
        Event::AttemptStarted { resumed, plan } => {
            let plan = opt_string(b, plan.as_deref());
            let t = fb::AttemptStarted::create(
                b,
                &fb::AttemptStartedArgs {
                    resumed: *resumed,
                    plan,
                },
            );
            (fb::Body::AttemptStarted, t.as_union_value())
        }
        Event::StepStarted { step, plan, group } => {
            let (step, plan) = (string(b, step), string(b, plan));
            let t = fb::StepStarted::create(
                b,
                &fb::StepStartedArgs {
                    step: Some(step),
                    plan: Some(plan),
                    group: *group,
                },
            );
            (fb::Body::StepStarted, t.as_union_value())
        }
        Event::StepEnded {
            step,
            plan,
            outcome,
            duration_us,
        } => {
            let (step, plan) = (string(b, step), string(b, plan));
            let t = fb::StepEnded::create(
                b,
                &fb::StepEndedArgs {
                    step: Some(step),
                    plan: Some(plan),
                    outcome: step_outcome_out(*outcome),
                    duration_us: *duration_us,
                },
            );
            (fb::Body::StepEnded, t.as_union_value())
        }
        Event::TaskRegistered {
            task,
            step,
            element,
            stage,
            upstream,
            members,
        } => {
            let task = string(b, task);
            let step = opt_string(b, step.as_deref());
            let upstream = strings(b, upstream);
            let members = strings(b, members);
            let t = fb::TaskRegistered::create(
                b,
                &fb::TaskRegisteredArgs {
                    task: Some(task),
                    step,
                    element: element_out(*element),
                    stage: *stage,
                    upstream,
                    members,
                },
            );
            (fb::Body::TaskRegistered, t.as_union_value())
        }
        Event::TaskWaiting {
            task,
            step,
            reason,
            blocking,
            port,
        } => {
            let task = string(b, task);
            let step = opt_string(b, step.as_deref());
            let blocking = strings(b, blocking);
            let port = opt_string(b, port.as_deref());
            let t = fb::TaskWaiting::create(
                b,
                &fb::TaskWaitingArgs {
                    task: Some(task),
                    step,
                    reason: wait_reason_out(*reason),
                    blocking,
                    port,
                },
            );
            (fb::Body::TaskWaiting, t.as_union_value())
        }
        Event::TaskReady { task, step } => {
            let task = string(b, task);
            let step = opt_string(b, step.as_deref());
            let t = fb::TaskReady::create(
                b,
                &fb::TaskReadyArgs {
                    task: Some(task),
                    step,
                },
            );
            (fb::Body::TaskReady, t.as_union_value())
        }
        Event::TaskStarted { task, step, phase } => {
            let task = string(b, task);
            let step = opt_string(b, step.as_deref());
            let t = fb::TaskStarted::create(
                b,
                &fb::TaskStartedArgs {
                    task: Some(task),
                    step,
                    phase: phase_out(*phase),
                },
            );
            (fb::Body::TaskStarted, t.as_union_value())
        }
        Event::TaskPhase { task, step, phase } => {
            let task = string(b, task);
            let step = opt_string(b, step.as_deref());
            let t = fb::TaskPhase::create(
                b,
                &fb::TaskPhaseArgs {
                    task: Some(task),
                    step,
                    phase: phase_out(*phase),
                },
            );
            (fb::Body::TaskPhase, t.as_union_value())
        }
        Event::TaskCompleted {
            task,
            step,
            outcome,
            duration_us,
            counters,
            cause,
        } => {
            let task = string(b, task);
            let step = opt_string(b, step.as_deref());
            let cause = opt_string(b, cause.as_deref());
            let counters = counters.map(|c| {
                fb::Counters::new(
                    c.morsels,
                    c.rows_in,
                    c.rows_out,
                    c.bytes_in,
                    c.bytes_out,
                    c.skipped,
                    c.errors,
                )
            });
            let t = fb::TaskCompleted::create(
                b,
                &fb::TaskCompletedArgs {
                    task: Some(task),
                    step,
                    outcome: task_outcome_out(*outcome),
                    duration_us: *duration_us,
                    counters: counters.as_ref(),
                    cause,
                },
            );
            (fb::Body::TaskCompleted, t.as_union_value())
        }
        Event::TaskFailed {
            task,
            step,
            code,
            message,
            scope,
            retryable,
        } => {
            let task = opt_string(b, task.as_deref());
            let step = opt_string(b, step.as_deref());
            let message = string(b, message);
            let t = fb::TaskFailed::create(
                b,
                &fb::TaskFailedArgs {
                    task,
                    step,
                    code: failure_code_out(*code),
                    message: Some(message),
                    scope: failure_scope_out(*scope),
                    retryable: *retryable,
                },
            );
            (fb::Body::TaskFailed, t.as_union_value())
        }
        Event::TaskSkipped {
            task,
            step,
            reason,
            blocking,
        } => {
            let task = string(b, task);
            let step = opt_string(b, step.as_deref());
            let blocking = opt_string(b, blocking.as_deref());
            let t = fb::TaskSkipped::create(
                b,
                &fb::TaskSkippedArgs {
                    task: Some(task),
                    step,
                    reason: skip_reason_out(*reason),
                    blocking,
                },
            );
            (fb::Body::TaskSkipped, t.as_union_value())
        }
        Event::CancelAcknowledged => {
            let t = fb::CancelAcknowledged::create(b, &fb::CancelAcknowledgedArgs {});
            (fb::Body::CancelAcknowledged, t.as_union_value())
        }
        Event::CancelEnded { disposition } => {
            let t = fb::CancelEnded::create(
                b,
                &fb::CancelEndedArgs {
                    disposition: disposition_out(*disposition),
                },
            );
            (fb::Body::CancelEnded, t.as_union_value())
        }
        Event::Logs { producers } => {
            let reaches: Vec<WIPOffset<fb::LogReach<'a>>> = producers
                .iter()
                .map(|p| {
                    let task = opt_string(b, p.task.as_deref());
                    fb::LogReach::create(
                        b,
                        &fb::LogReachArgs {
                            task,
                            records: p.records,
                            bytes: p.bytes,
                            truncated: p.truncated,
                            dropped: p.dropped,
                            dropped_bytes: p.dropped_bytes,
                            drained: p.drained,
                        },
                    )
                })
                .collect();
            let producers = (!reaches.is_empty()).then(|| b.create_vector(&reaches));
            let t = fb::Logs::create(b, &fb::LogsArgs { producers });
            (fb::Body::Logs, t.as_union_value())
        }
        Event::Log {
            task,
            stream,
            level,
            bytes,
            text,
            partial,
            continued,
            truncated,
        } => {
            let task = opt_string(b, task.as_deref());
            let level = opt_string(b, level.as_deref());
            let text = string(b, text);
            let t = fb::Log::create(
                b,
                &fb::LogArgs {
                    task,
                    stream: stream_out(*stream),
                    level,
                    bytes: *bytes,
                    text: Some(text),
                    partial: *partial,
                    continued: *continued,
                    truncated: *truncated,
                },
            );
            (fb::Body::Log, t.as_union_value())
        }
    };
    fb::Event::create(
        b,
        &fb::EventArgs {
            start: Some(start),
            t_us,
            body_type,
            body: Some(body),
        },
    )
}

fn element_out(v: Element) -> fb::Element {
    match v {
        Element::Source => fb::Element::Source,
        Element::Kernel => fb::Element::Kernel,
        Element::Sink => fb::Element::Sink,
    }
}

fn wait_reason_out(v: WaitReason) -> fb::WaitReason {
    match v {
        WaitReason::EnginePreparing => fb::WaitReason::EnginePreparing,
        WaitReason::AwaitingInput => fb::WaitReason::AwaitingInput,
        WaitReason::PortCall => fb::WaitReason::PortCall,
    }
}

fn phase_out(v: Phase) -> fb::Phase {
    match v {
        Phase::Calibrating => fb::Phase::Calibrating,
        Phase::Reading => fb::Phase::Reading,
        Phase::Applying => fb::Phase::Applying,
        Phase::Writing => fb::Phase::Writing,
        Phase::Draining => fb::Phase::Draining,
        Phase::Committing => fb::Phase::Committing,
    }
}

fn task_outcome_out(v: TaskOutcome) -> fb::TaskOutcome {
    match v {
        TaskOutcome::Succeeded => fb::TaskOutcome::Succeeded,
        TaskOutcome::Stopped => fb::TaskOutcome::Stopped,
        TaskOutcome::Cancelled => fb::TaskOutcome::Cancelled,
    }
}

fn step_outcome_out(v: StepOutcome) -> fb::StepOutcome {
    match v {
        StepOutcome::Failed => fb::StepOutcome::Failed,
        StepOutcome::Cancelled => fb::StepOutcome::Cancelled,
        StepOutcome::Stopped => fb::StepOutcome::Stopped,
        StepOutcome::Skipped => fb::StepOutcome::Skipped,
        StepOutcome::Succeeded => fb::StepOutcome::Succeeded,
    }
}

fn failure_code_out(v: FailureCode) -> fb::FailureCode {
    match v {
        FailureCode::SpecRefused => fb::FailureCode::SpecRefused,
        FailureCode::KernelError => fb::FailureCode::KernelError,
        FailureCode::BudgetExceeded => fb::FailureCode::BudgetExceeded,
        FailureCode::MemoryRefused => fb::FailureCode::MemoryRefused,
        FailureCode::SourceError => fb::FailureCode::SourceError,
        FailureCode::SinkError => fb::FailureCode::SinkError,
        FailureCode::AllocRefused => fb::FailureCode::AllocRefused,
        FailureCode::IoError => fb::FailureCode::IoError,
        FailureCode::StagingError => fb::FailureCode::StagingError,
        FailureCode::ConvertError => fb::FailureCode::ConvertError,
        FailureCode::ConfigRefused => fb::FailureCode::ConfigRefused,
        FailureCode::PlanRefused => fb::FailureCode::PlanRefused,
        FailureCode::ResumeRefused => fb::FailureCode::ResumeRefused,
        FailureCode::Unsupported => fb::FailureCode::Unsupported,
        FailureCode::Cancelled => fb::FailureCode::Cancelled,
        FailureCode::Internal => fb::FailureCode::Internal,
    }
}

fn failure_scope_out(v: FailureScope) -> fb::FailureScope {
    match v {
        FailureScope::Task => fb::FailureScope::Task,
        FailureScope::Attempt => fb::FailureScope::Attempt,
    }
}

fn skip_reason_out(v: SkipReason) -> fb::SkipReason {
    match v {
        SkipReason::FailedTask => fb::SkipReason::FailedTask,
        SkipReason::AttemptFailed => fb::SkipReason::AttemptFailed,
        SkipReason::Cancelled => fb::SkipReason::Cancelled,
    }
}

fn disposition_out(v: CancelDisposition) -> fb::CancelDisposition {
    match v {
        CancelDisposition::Cancelled => fb::CancelDisposition::Cancelled,
        CancelDisposition::Completed => fb::CancelDisposition::Completed,
        CancelDisposition::Failed => fb::CancelDisposition::Failed,
    }
}

fn stream_out(v: LogStream) -> fb::LogStream {
    match v {
        LogStream::Stdout => fb::LogStream::Stdout,
        LogStream::Stderr => fb::LogStream::Stderr,
        LogStream::Structured => fb::LogStream::Structured,
        LogStream::Platform => fb::LogStream::Platform,
    }
}

// ---- reading ----

/// How far the verifier walks a body before it refuses it.
fn verifier_options() -> flatbuffers::VerifierOptions {
    flatbuffers::VerifierOptions {
        max_depth: 16,
        max_tables: 1 << 20,
        max_apparent_size: 1 << 26,
        ignore_missing_null_terminator: false,
    }
}

fn field(field: &'static str, reason: impl Into<String>) -> DecodeError {
    DecodeError::Field {
        field,
        reason: reason.into(),
    }
}

fn unknown<T: core::fmt::Debug>(name: &'static str, value: T) -> DecodeError {
    field(name, format!("{value:?} is not a value this build knows"))
}

fn required(name: &'static str, s: Option<&str>) -> Result<String, DecodeError> {
    s.map(str::to_string)
        .ok_or_else(|| field(name, "is absent"))
}

fn listed(v: Option<Vector<'_, ForwardsUOffset<&str>>>) -> Vec<String> {
    v.map(|v| v.iter().map(str::to_string).collect())
        .unwrap_or_default()
}

fn header<'a>(headers: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Read one record that is an event: the whole of `bytes`, which may be anyone's. Refused, with
/// the reason, when it is not a record, not an event, or an event that disagrees with its own
/// record (the subject is not the body's kind, the `msg_id` is not the one its parts make).
pub fn decode(bytes: &[u8]) -> Result<EventMsg, DecodeError> {
    let (record, used) = match decode_record(bytes) {
        Decoded::Record(record, used) => (record, used),
        Decoded::Incomplete => return Err(DecodeError::Truncated),
        Decoded::Corrupt(c) => return Err(DecodeError::Record(c)),
    };
    if used != bytes.len() {
        return Err(DecodeError::Trailing(bytes.len() - used));
    }
    let kind = Kind::of_subject(record.subject).ok_or(DecodeError::Subject(record.subject))?;
    let body = record.body;
    if body.len() < 8 || !fb::event_buffer_has_identifier(body) {
        return Err(DecodeError::Identifier);
    }
    let root = fb::root_as_event_with_opts(&verifier_options(), body)
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;
    let event = body_of(&root)?;
    if event.kind() != kind {
        return Err(DecodeError::Disagree(format!(
            "the subject names `{}` and the body is `{}`",
            kind.name(),
            event.kind().name()
        )));
    }
    let attempt = header(&record.headers, "attempt")
        .ok_or(DecodeError::Header {
            name: "attempt",
            reason: "is absent".to_string(),
        })?
        .parse::<u32>()
        .map_err(|e| DecodeError::Header {
            name: "attempt",
            reason: e.to_string(),
        })?;
    let start = required("start", root.start())?;
    let expected = match &event {
        Event::Log { task, .. } => format!(
            "{attempt}-{start}-{}-{}",
            task.as_deref().unwrap_or(ENGINE),
            record.seq
        ),
        _ => format!("{attempt}-{start}-{}", record.seq),
    };
    if record.msg_id != expected {
        return Err(DecodeError::Disagree(format!(
            "the msg_id is `{}` and its parts make `{expected}`",
            record.msg_id
        )));
    }
    Ok(EventMsg {
        run: header(&record.headers, "run").map(str::to_string),
        plan: header(&record.headers, "plan").map(str::to_string),
        attempt,
        start,
        seq: record.seq,
        ts_ns: record.ts,
        t_us: root.t_us(),
        msg_id: record.msg_id.to_string(),
        body: event,
    })
}

fn body_of(root: &fb::Event<'_>) -> Result<Event, DecodeError> {
    let missing = || field("body", "is absent");
    let opt = |s: Option<&str>| s.map(str::to_string);
    Ok(match root.body_type() {
        fb::Body::AttemptStarted => {
            let t = root.body_as_attempt_started().ok_or_else(missing)?;
            Event::AttemptStarted {
                resumed: t.resumed(),
                plan: opt(t.plan()),
            }
        }
        fb::Body::StepStarted => {
            let t = root.body_as_step_started().ok_or_else(missing)?;
            Event::StepStarted {
                step: required("step", t.step())?,
                plan: required("plan", t.plan())?,
                group: t.group(),
            }
        }
        fb::Body::StepEnded => {
            let t = root.body_as_step_ended().ok_or_else(missing)?;
            Event::StepEnded {
                step: required("step", t.step())?,
                plan: required("plan", t.plan())?,
                outcome: step_outcome_in(t.outcome())?,
                duration_us: t.duration_us(),
            }
        }
        fb::Body::TaskRegistered => {
            let t = root.body_as_task_registered().ok_or_else(missing)?;
            Event::TaskRegistered {
                task: required("task", t.task())?,
                step: opt(t.step()),
                element: element_in(t.element())?,
                stage: t.stage(),
                upstream: listed(t.upstream()),
                members: listed(t.members()),
            }
        }
        fb::Body::TaskWaiting => {
            let t = root.body_as_task_waiting().ok_or_else(missing)?;
            Event::TaskWaiting {
                task: required("task", t.task())?,
                step: opt(t.step()),
                reason: wait_reason_in(t.reason())?,
                blocking: listed(t.blocking()),
                port: opt(t.port()),
            }
        }
        fb::Body::TaskReady => {
            let t = root.body_as_task_ready().ok_or_else(missing)?;
            Event::TaskReady {
                task: required("task", t.task())?,
                step: opt(t.step()),
            }
        }
        fb::Body::TaskStarted => {
            let t = root.body_as_task_started().ok_or_else(missing)?;
            Event::TaskStarted {
                task: required("task", t.task())?,
                step: opt(t.step()),
                phase: phase_in(t.phase())?,
            }
        }
        fb::Body::TaskPhase => {
            let t = root.body_as_task_phase().ok_or_else(missing)?;
            Event::TaskPhase {
                task: required("task", t.task())?,
                step: opt(t.step()),
                phase: phase_in(t.phase())?,
            }
        }
        fb::Body::TaskCompleted => {
            let t = root.body_as_task_completed().ok_or_else(missing)?;
            Event::TaskCompleted {
                task: required("task", t.task())?,
                step: opt(t.step()),
                outcome: task_outcome_in(t.outcome())?,
                duration_us: t.duration_us(),
                counters: t.counters().map(|c| Counters {
                    morsels: c.morsels(),
                    rows_in: c.rows_in(),
                    rows_out: c.rows_out(),
                    bytes_in: c.bytes_in(),
                    bytes_out: c.bytes_out(),
                    skipped: c.skipped(),
                    errors: c.errors(),
                }),
                cause: opt(t.cause()),
            }
        }
        fb::Body::TaskFailed => {
            let t = root.body_as_task_failed().ok_or_else(missing)?;
            Event::TaskFailed {
                task: opt(t.task()),
                step: opt(t.step()),
                code: failure_code_in(t.code())?,
                message: required("message", t.message())?,
                scope: failure_scope_in(t.scope())?,
                retryable: t.retryable(),
            }
        }
        fb::Body::TaskSkipped => {
            let t = root.body_as_task_skipped().ok_or_else(missing)?;
            Event::TaskSkipped {
                task: required("task", t.task())?,
                step: opt(t.step()),
                reason: skip_reason_in(t.reason())?,
                blocking: opt(t.blocking()),
            }
        }
        fb::Body::CancelAcknowledged => {
            root.body_as_cancel_acknowledged().ok_or_else(missing)?;
            Event::CancelAcknowledged
        }
        fb::Body::CancelEnded => {
            let t = root.body_as_cancel_ended().ok_or_else(missing)?;
            Event::CancelEnded {
                disposition: disposition_in(t.disposition())?,
            }
        }
        fb::Body::Logs => {
            let t = root.body_as_logs().ok_or_else(missing)?;
            Event::Logs {
                producers: t
                    .producers()
                    .map(|v| {
                        v.iter()
                            .map(|p| LogReach {
                                task: opt(p.task()),
                                records: p.records(),
                                bytes: p.bytes(),
                                truncated: p.truncated(),
                                dropped: p.dropped(),
                                dropped_bytes: p.dropped_bytes(),
                                drained: p.drained(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        }
        fb::Body::Log => {
            let t = root.body_as_log().ok_or_else(missing)?;
            Event::Log {
                task: opt(t.task()),
                stream: stream_in(t.stream())?,
                level: opt(t.level()),
                bytes: t.bytes(),
                text: required("text", t.text())?,
                partial: t.partial(),
                continued: t.continued(),
                truncated: t.truncated(),
            }
        }
        other => return Err(unknown("body", other.0)),
    })
}

fn element_in(v: fb::Element) -> Result<Element, DecodeError> {
    Ok(match v {
        fb::Element::Source => Element::Source,
        fb::Element::Kernel => Element::Kernel,
        fb::Element::Sink => Element::Sink,
        other => return Err(unknown("element", other.0)),
    })
}

fn wait_reason_in(v: fb::WaitReason) -> Result<WaitReason, DecodeError> {
    Ok(match v {
        fb::WaitReason::EnginePreparing => WaitReason::EnginePreparing,
        fb::WaitReason::AwaitingInput => WaitReason::AwaitingInput,
        fb::WaitReason::PortCall => WaitReason::PortCall,
        other => return Err(unknown("reason", other.0)),
    })
}

fn phase_in(v: fb::Phase) -> Result<Phase, DecodeError> {
    Ok(match v {
        fb::Phase::Calibrating => Phase::Calibrating,
        fb::Phase::Reading => Phase::Reading,
        fb::Phase::Applying => Phase::Applying,
        fb::Phase::Writing => Phase::Writing,
        fb::Phase::Draining => Phase::Draining,
        fb::Phase::Committing => Phase::Committing,
        other => return Err(unknown("phase", other.0)),
    })
}

fn task_outcome_in(v: fb::TaskOutcome) -> Result<TaskOutcome, DecodeError> {
    Ok(match v {
        fb::TaskOutcome::Succeeded => TaskOutcome::Succeeded,
        fb::TaskOutcome::Stopped => TaskOutcome::Stopped,
        fb::TaskOutcome::Cancelled => TaskOutcome::Cancelled,
        other => return Err(unknown("outcome", other.0)),
    })
}

fn step_outcome_in(v: fb::StepOutcome) -> Result<StepOutcome, DecodeError> {
    Ok(match v {
        fb::StepOutcome::Failed => StepOutcome::Failed,
        fb::StepOutcome::Cancelled => StepOutcome::Cancelled,
        fb::StepOutcome::Stopped => StepOutcome::Stopped,
        fb::StepOutcome::Skipped => StepOutcome::Skipped,
        fb::StepOutcome::Succeeded => StepOutcome::Succeeded,
        other => return Err(unknown("outcome", other.0)),
    })
}

fn failure_code_in(v: fb::FailureCode) -> Result<FailureCode, DecodeError> {
    Ok(match v {
        fb::FailureCode::SpecRefused => FailureCode::SpecRefused,
        fb::FailureCode::KernelError => FailureCode::KernelError,
        fb::FailureCode::BudgetExceeded => FailureCode::BudgetExceeded,
        fb::FailureCode::MemoryRefused => FailureCode::MemoryRefused,
        fb::FailureCode::SourceError => FailureCode::SourceError,
        fb::FailureCode::SinkError => FailureCode::SinkError,
        fb::FailureCode::AllocRefused => FailureCode::AllocRefused,
        fb::FailureCode::IoError => FailureCode::IoError,
        fb::FailureCode::StagingError => FailureCode::StagingError,
        fb::FailureCode::ConvertError => FailureCode::ConvertError,
        fb::FailureCode::ConfigRefused => FailureCode::ConfigRefused,
        fb::FailureCode::PlanRefused => FailureCode::PlanRefused,
        fb::FailureCode::ResumeRefused => FailureCode::ResumeRefused,
        fb::FailureCode::Unsupported => FailureCode::Unsupported,
        fb::FailureCode::Cancelled => FailureCode::Cancelled,
        fb::FailureCode::Internal => FailureCode::Internal,
        other => return Err(unknown("code", other.0)),
    })
}

fn failure_scope_in(v: fb::FailureScope) -> Result<FailureScope, DecodeError> {
    Ok(match v {
        fb::FailureScope::Task => FailureScope::Task,
        fb::FailureScope::Attempt => FailureScope::Attempt,
        other => return Err(unknown("scope", other.0)),
    })
}

fn skip_reason_in(v: fb::SkipReason) -> Result<SkipReason, DecodeError> {
    Ok(match v {
        fb::SkipReason::FailedTask => SkipReason::FailedTask,
        fb::SkipReason::AttemptFailed => SkipReason::AttemptFailed,
        fb::SkipReason::Cancelled => SkipReason::Cancelled,
        other => return Err(unknown("reason", other.0)),
    })
}

fn disposition_in(v: fb::CancelDisposition) -> Result<CancelDisposition, DecodeError> {
    Ok(match v {
        fb::CancelDisposition::Cancelled => CancelDisposition::Cancelled,
        fb::CancelDisposition::Completed => CancelDisposition::Completed,
        fb::CancelDisposition::Failed => CancelDisposition::Failed,
        other => return Err(unknown("disposition", other.0)),
    })
}

fn stream_in(v: fb::LogStream) -> Result<LogStream, DecodeError> {
    Ok(match v {
        fb::LogStream::Stdout => LogStream::Stdout,
        fb::LogStream::Stderr => LogStream::Stderr,
        fb::LogStream::Structured => LogStream::Structured,
        fb::LogStream::Platform => LogStream::Platform,
        other => return Err(unknown("stream", other.0)),
    })
}

/// Every event in `bytes`, which are records back to back as the output store's segments and the
/// run's events file are: the first thing in them that is not a whole event is an error naming
/// where it is.
pub fn read_records(bytes: &[u8]) -> Result<Vec<EventMsg>, String> {
    let mut events = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let used = match decode_record(rest) {
            Decoded::Record(_, used) => used,
            Decoded::Incomplete => return Err(format!("a record at byte {at} is cut short")),
            Decoded::Corrupt(c) => return Err(format!("not a record at byte {at}: {c}")),
        };
        events.push(decode(&rest[..used]).map_err(|e| format!("the record at byte {at}: {e}"))?);
        at += used;
    }
    Ok(events)
}
