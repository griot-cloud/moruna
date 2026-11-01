//! The host protocol's messages (MH 4.3): lines of JSON, one object per line, `type` first, and
//! one framed binary message, the `event`. Moruna sends `hello`, `heartbeat`, `limits_changed`,
//! `event`, `port_call`, `port_cancel`, `report` and `exit`; it reads `spec`, `cancel`,
//! `checkpoint` and `port_reply`, and nothing else. No message a peer can send enlarges a run.
//!
//! An `event` is one fact of the run's execution, versioned on its own ([`EVENTS_VERSION`]) and
//! named by its kind: version 1 had the task lifecycle; version 2 added `logs`, what the run's
//! output store holds for each producer so far; version 3 added a running task's wait on a port
//! (`task_waiting` with `reason: port_call` and the `port`, MH 4.10); version 4 added the failure
//! code `internal`, a crash: a panic of Moruna's own that unwound the run; version 5 made
//! `task_registered`'s `upstream` the list of tasks a task takes its input from, the job's edges
//! (SDD 22 e.4); version 6 ran a job from its plan (SDD 26): every task event names the plan step
//! that runs the task (`step`), `attempt_started` names the plan (`plan`), and `step_started` and
//! `step_ended` tell each step's start and end; version 7 makes the event a record (SDD 21)
//! whose body is FlatBuffers (`moruna-event`), sent as a frame (`moruna_event::frame`) in place of
//! a line, and written to the run's output store when no peer is there. The JSON of version 6,
//! with the same field names, is the rendering of a record for people (`moruna run
//! --events-json`, [`read_json_message`]). A peer refuses a version it does not read rather than
//! guess at it.
//!
//! What a task prints is never on the socket. Each record of it is an `event` of kind `log` in
//! the run's output store (`report.logs`), numbered by its own producer; the socket says only how
//! far each producer's records reach, how many bytes they hold, and how many were cut or dropped.
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

/// The version of the port messages this build writes and reads (MH 4.10).
pub const PORTS_VERSION: u32 = 1;

/// The event types of the protocol (MH 4.3): the facts of a run, told as records by
/// `moruna-event`. They are re-exported here because they are the protocol's, and a host that
/// speaks it names them by this module.
pub use moruna_event::{
    CancelDisposition, Counters, EVENTS_VERSION, Element, Event, EventMsg, FailureCode,
    FailureScope, LogReach, LogStream, Phase, SkipReason, StepOutcome, TaskOutcome, WaitReason,
};

/// The protocol's view of the scheduler's counters.
pub fn counters_of(c: &moruna_kernel::StageCounters) -> Counters {
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

/// A message from Moruna to its peer (MH 4.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
        /// How the run went against its plan (SDD 26 e.3): the plan it ran, the order its steps
        /// started in, each step's start, end and use, and every deviation from the plan.
        plan: Option<serde_json::Value>,
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Inbound {
    /// The job document, in serve mode; once.
    Spec {
        /// The document.
        spec: serde_json::Value,
        /// The plan to run it by, its FlatBuffers as standard base64 (SDD 26); absent: the
        /// document is planned by the default planner.
        #[serde(default)]
        plan: Option<String>,
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

/// One message of the protocol as it arrives: a line of JSON, or the record of an `event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// A line, newline included.
    Line(String),
    /// The record of one event, the frame's mark taken off (`moruna_event::decode` reads it).
    Event(Vec<u8>),
}

/// Read the next message of Moruna's side of the conversation: a framed event or a line of at most
/// `max_line` bytes. `Ok(None)` at the end of the stream.
pub fn read_message(
    reader: &mut dyn std::io::BufRead,
    max_line: usize,
) -> std::io::Result<Option<Message>> {
    match moruna_event::frame::next_is_frame(reader)? {
        None => Ok(None),
        Some(true) => moruna_event::frame::read_frame(reader).map(|r| Some(Message::Event(r))),
        Some(false) => Ok(read_line(reader, max_line)?.map(Message::Line)),
    }
}

/// [`read_message`] with every message as JSON, as people and the tests of a host read it: a line
/// is parsed, an event's record is rendered as the `event` line of version 6's field names
/// (`moruna_event::EventMsg::to_value`). A line that is not JSON and a record that is not an event
/// are `InvalidData` errors.
pub fn read_json_message(
    reader: &mut dyn std::io::BufRead,
) -> std::io::Result<Option<serde_json::Value>> {
    let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
    match read_message(reader, MAX_SPEC_LINE)? {
        None => Ok(None),
        Some(Message::Line(line)) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| invalid(format!("a line that is not JSON ({e}): {line}"))),
        Some(Message::Event(record)) => moruna_event::decode(&record)
            .map(|event| Some(event.to_value()))
            .map_err(|e| invalid(format!("a frame that is not an event: {e}"))),
    }
}

/// Every message in `bytes` as JSON, as [`read_json_message`] reads them one by one.
pub fn json_messages(bytes: &[u8]) -> std::io::Result<Vec<serde_json::Value>> {
    let mut reader = std::io::Cursor::new(bytes);
    let mut messages = Vec::new();
    while let Some(message) = read_json_message(&mut reader)? {
        messages.push(message);
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An event as a peer reads it, with the parts the JSON does not show filled in.
    fn msg(attempt: u32, start: String, seq: u64, t_us: u64, body: Event) -> EventMsg {
        EventMsg {
            run: None,
            plan: None,
            attempt,
            seq,
            ts_ns: 0,
            t_us,
            msg_id: format!("{attempt}-{start}-{seq}"),
            start,
            body,
        }
    }

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
            plan: Some(serde_json::json!({"plan": "cd"})),
        };
        assert_eq!(
            report.line(),
            "{\"type\":\"report\",\"report\":{\"run_id\":\"ab\"},\"plan\":{\"plan\":\"cd\"}}\n"
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
        let event = |body| msg(2, "0f".repeat(16), 7, 1500, body).json_line();
        assert_eq!(
            event(Event::TaskStarted {
                task: "orders".into(),
                step: Some("s0123456789abcdef".into()),
                phase: Phase::Reading,
            }),
            format!(
                "{{\"type\":\"event\",\"v\":7,\"attempt\":2,\"start\":\"{}\",\"seq\":7,\
                 \"t_us\":1500,\"kind\":\"task_started\",\"task\":\"orders\",\
                 \"step\":\"s0123456789abcdef\",\"phase\":\"reading\"}}\n",
                "0f".repeat(16)
            )
        );
        let failed = event(Event::TaskFailed {
            task: None,
            step: None,
            code: FailureCode::IoError,
            message: "the read_file operation failed".into(),
            scope: FailureScope::Attempt,
            retryable: true,
        });
        assert!(
            failed.ends_with(
                "\"kind\":\"task_failed\",\"task\":null,\"step\":null,\"code\":\"io_error\",\
                 \"message\":\"the read_file operation failed\",\"scope\":\"attempt\",\
                 \"retryable\":true}\n"
            ),
            "{failed}"
        );
        let crashed = event(Event::TaskFailed {
            task: Some("orders".into()),
            step: Some("s1".into()),
            code: FailureCode::Internal,
            message: "index out of bounds".into(),
            scope: FailureScope::Task,
            retryable: false,
        });
        assert!(
            crashed.ends_with(
                "\"kind\":\"task_failed\",\"task\":\"orders\",\"step\":\"s1\",\"code\":\"internal\",\
                 \"message\":\"index out of bounds\",\"scope\":\"task\",\"retryable\":false}\n"
            ),
            "{crashed}"
        );
        assert!(event(Event::CancelAcknowledged).ends_with("\"kind\":\"cancel_acknowledged\"}\n"));
        let waiting = event(Event::TaskWaiting {
            task: "triage".into(),
            step: Some("s2".into()),
            reason: WaitReason::PortCall,
            blocking: Vec::new(),
            port: Some("model".into()),
        });
        assert!(
            waiting.ends_with(
                "\"kind\":\"task_waiting\",\"task\":\"triage\",\"step\":\"s2\",\"reason\":\"port_call\",\
                 \"blocking\":[],\"port\":\"model\"}\n"
            ),
            "{waiting}"
        );
        let done = event(Event::TaskCompleted {
            task: "t1".into(),
            step: Some("s3".into()),
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
        // Version 6: the plan's steps.
        assert!(
            event(Event::AttemptStarted {
                resumed: false,
                plan: Some("ab".into()),
            })
            .ends_with("\"kind\":\"attempt_started\",\"resumed\":false,\"plan\":\"ab\"}\n")
        );
        assert!(
            event(Event::StepStarted {
                step: "s3".into(),
                plan: "ab".into(),
                group: 2,
            })
            .ends_with("\"kind\":\"step_started\",\"step\":\"s3\",\"plan\":\"ab\",\"group\":2}\n")
        );
        assert!(
            event(Event::StepEnded {
                step: "s3".into(),
                plan: "ab".into(),
                outcome: StepOutcome::Skipped,
                duration_us: None,
            })
            .ends_with(
                "\"kind\":\"step_ended\",\"step\":\"s3\",\"plan\":\"ab\",\
                 \"outcome\":\"skipped\",\"duration_us\":null}\n"
            )
        );
    }

    /// `logs` on the socket and a `log` record in the store (MH 4.3): the same `event` framing,
    /// every field written out, nulls included; the store's record carries the text, the socket
    /// only counts.
    #[test]
    fn output_is_counted_on_the_socket_and_kept_in_the_store() {
        let logs = msg(
            1,
            "0f".repeat(16),
            9,
            10,
            Event::Logs {
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
        )
        .json_line();
        assert!(
            logs.ends_with(
                "\"kind\":\"logs\",\"producers\":[{\"task\":\"k1\",\"records\":3,\"bytes\":40,\
                 \"truncated\":1,\"dropped\":2,\"dropped_bytes\":7,\"drained\":true},\
                 {\"task\":null,\"records\":0,\"bytes\":0,\"truncated\":0,\"dropped\":0,\
                 \"dropped_bytes\":0,\"drained\":false}]}\n"
            ),
            "{logs}"
        );
        let record = msg(
            1,
            "0f".repeat(16),
            4,
            77,
            Event::Log {
                task: Some("k1".into()),
                stream: LogStream::Stderr,
                level: None,
                bytes: 2,
                text: "hi".into(),
                partial: false,
                continued: true,
                truncated: 0,
            },
        );
        assert_eq!(
            record.json_line(),
            format!(
                "{{\"type\":\"event\",\"v\":7,\"attempt\":1,\"start\":\"{s}\",\"seq\":4,\
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
                spec: serde_json::json!({"a": 1}),
                plan: None,
            })
        );
        assert_eq!(
            Inbound::parse("{\"type\":\"spec\",\"spec\":{},\"plan\":\"TVBMMQ==\"}"),
            Ok(Inbound::Spec {
                spec: serde_json::json!({}),
                plan: Some("TVBMMQ==".into()),
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
