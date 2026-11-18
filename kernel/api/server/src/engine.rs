//! The engine, as the server reaches it (system design, section 3A): a trait the node's `api`
//! program implements over `node-admission` and the engine's channel, and the host protocol's
//! messages (`moruna-hosted-engine.md`, section 4.3) read from the engine's bytes.

use alloc::{string::String, vec::Vec};
use moruna_api::{Function, Refusal};
use moruna_event::frame::FRAME;
use moruna_record::{FIXED, MAX_RECORD};

/// The longest line read from the engine: its `report`, at most. A longer line is passed over.
pub const ENGINE_LINE_LIMIT: usize = 1024 * 1024;

/// Why the host did not do what was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    /// Not now: nothing has arrived, or the host cannot take the request yet.
    WouldBlock,
    /// The node refuses the request, and says why: a budget past what the node gives a job,
    /// say. The call is answered `invalid` with these words.
    Refused(String),
    /// The host failed. The call is answered `internal` with these words.
    Failed(String),
    /// The node runs no jobs. The call is answered `busy`.
    Unavailable,
}

/// A volume a job names, by its serial, and whether the job writes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Volume {
    /// The serial the cloud gives the disk (`disk://<serial>/...`).
    pub serial: String,
    /// Whether the job writes it: a sink's.
    pub write: bool,
    /// Whether the job stages on it (`staging.dir` names it, item 3.3): the node then sizes the
    /// job's memory with no scratch for its staging.
    pub staging: bool,
}

/// What a job needs of the node beyond its budget, as the engine's check answers it: what
/// `node-admission` keeps with the job for its grants (items 3.3, 3.4 and 3.6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    /// The hosts it reaches.
    pub hosts: Vec<String>,
    /// The volumes it names.
    pub volumes: Vec<Volume>,
    /// The registered functions it uses.
    pub functions: Vec<String>,
}

/// The engine's check of a document: its refusals, none when it would run, and what the job
/// would take of the node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Verdict {
    /// Every refusal, by field.
    pub refusals: Vec<Refusal>,
    /// The budget the job runs with, in bytes: the document's, or the node's most when it
    /// names none.
    pub memory_bytes: Option<u64>,
    /// What else it needs of the node.
    pub needs: Needs,
    /// The functions the job uses as the node registered them, in `needs.functions` order
    /// (system design 3E, item 3.6): the server fills them from its registry before it starts
    /// the job, for `node-admission` to fetch and check each against. The check's own verdict
    /// has none.
    pub registered: Vec<Function>,
}

/// The host protocol's `check` message (`moruna-hosted-engine.md`, `check-job --listen`) for
/// `document`, as [`EngineHost::check`] is given it, with the names of the job's secrets: one
/// line, its line break included. A version 1 document is its JSON; a version 2 document's
/// bytes go as their standard base64, as `spec` carries them.
pub fn check_line(document: &[u8], secrets: &[String]) -> Vec<u8> {
    let mut line = Vec::with_capacity(document.len() * 4 / 3 + 64);
    line.extend_from_slice(b"{\"type\":\"check\",\"spec\":");
    if document.first() == Some(&b'{') {
        line.extend_from_slice(document);
    } else {
        line.push(b'"');
        line.extend_from_slice(crate::token::to_base64(document).as_bytes());
        line.push(b'"');
    }
    line.extend_from_slice(b",\"secrets\":");
    line.extend_from_slice(
        serde_json::to_string(secrets)
            .unwrap_or_else(|_| String::from("[]"))
            .as_bytes(),
    );
    line.extend_from_slice(b"}\n");
    line
}

/// The `checked` line, as the engine's check answers it.
#[derive(serde::Deserialize)]
struct CheckedLine {
    #[serde(rename = "type")]
    kind: String,
    ok: bool,
    #[serde(default)]
    refusals: Vec<Refusal>,
    #[serde(default)]
    memory_bytes: Option<u64>,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    volumes: Vec<VolumeLine>,
    #[serde(default)]
    functions: Vec<String>,
}

#[derive(serde::Deserialize)]
struct VolumeLine {
    serial: String,
    write: bool,
    #[serde(default)]
    staging: bool,
}

/// The verdict the engine's check answers in its `checked` line (its line break taken off or
/// not): its refusals, the job's budget and its needs. A line that is not a `checked` message,
/// or that refuses the document without saying why, is the check's failure.
pub fn verdict(line: &[u8]) -> Result<Verdict, HostError> {
    let checked: CheckedLine = serde_json::from_slice(line.trim_ascii()).map_err(|_| {
        HostError::Failed(String::from(
            "the engine's check answered a line that is not its `checked` message",
        ))
    })?;
    if checked.kind != "checked" {
        return Err(HostError::Failed(alloc::format!(
            "the engine's check answered `{}`, not `checked`",
            checked.kind
        )));
    }
    if !checked.ok && checked.refusals.is_empty() {
        return Err(HostError::Failed(String::from(
            "the engine's check refused the document without saying why",
        )));
    }
    Ok(Verdict {
        refusals: checked.refusals,
        memory_bytes: checked.memory_bytes,
        needs: Needs {
            hosts: checked.hosts,
            volumes: checked
                .volumes
                .into_iter()
                .map(|v| Volume {
                    serial: v.serial,
                    write: v.write,
                    staging: v.staging,
                })
                .collect(),
            functions: checked.functions,
        },
        registered: Vec::new(),
    })
}

/// What the server asks of the node to run jobs: the engine's check of a document, a fresh
/// engine for a job, and the host protocol's bytes to and from it. `api` does not link
/// `moruna-job`: a document is checked by the engine itself.
///
/// The server never waits on a call here: each answers at once, and the server asks again on
/// its next poll.
pub trait EngineHost {
    /// Starts the engine's check of `document` (a version 1 document's JSON, or a version 2
    /// document's bytes) in a short-lived process, with the names of the job's secrets, so that
    /// a reference to one not given is refused. One check runs at a time: the server starts the
    /// next when [`checked`](EngineHost::checked) has answered.
    fn check(&mut self, document: &[u8], secrets: &[String]) -> Result<(), HostError>;

    /// The verdict of the check under way, once it has one: the engine's refusals, none when
    /// the document is sound, and the budget and needs of the job it would be.
    fn checked(&mut self) -> Option<Result<Verdict, HostError>>;

    /// Starts a fresh engine for one job and sends it `spec`, the host protocol's `spec`
    /// message: one line, its line break included, with the job's secrets in its `secrets`
    /// member. `verdict` is the engine's check of the same document, sound: the job's budget
    /// and needs, which the node starts the engine with.
    fn start(&mut self, spec: &[u8], verdict: &Verdict) -> Result<(), HostError>;

    /// Sends the engine one line of the host protocol, its line break included: `cancel`.
    fn send(&mut self, line: &[u8]) -> Result<(), HostError>;

    /// Reads bytes the engine has sent, the host protocol's, into `buf`. `Ok(0)` means the
    /// engine's channel has ended.
    fn receive(&mut self, buf: &mut [u8]) -> Result<usize, HostError>;
}

/// A node that runs no jobs: every job operation that needs the engine is answered `busy`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEngine;

impl EngineHost for NoEngine {
    fn check(&mut self, _document: &[u8], _secrets: &[String]) -> Result<(), HostError> {
        Err(HostError::Unavailable)
    }

    fn checked(&mut self) -> Option<Result<Verdict, HostError>> {
        None
    }

    fn start(&mut self, _spec: &[u8], _verdict: &Verdict) -> Result<(), HostError> {
        Err(HostError::Unavailable)
    }

    fn send(&mut self, _line: &[u8]) -> Result<(), HostError> {
        Err(HostError::Unavailable)
    }

    fn receive(&mut self, _buf: &mut [u8]) -> Result<usize, HostError> {
        Err(HostError::WouldBlock)
    }
}

/// One message of the host protocol as it arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Message {
    /// A line of JSON, its line break taken off.
    Line(Vec<u8>),
    /// The record of one event, the frame's mark taken off.
    Record(Vec<u8>),
    /// The bytes are not the protocol's, and nothing after them can be read.
    Garbled,
}

/// The engine's bytes, split into the protocol's messages: a message that begins with the NUL
/// byte is an event's frame, whose record says its own length; any other is a line.
#[derive(Debug, Default)]
pub(crate) struct Wire {
    bytes: Vec<u8>,
    /// How many of `bytes` have been read as messages.
    read: usize,
    /// A line longer than the limit is being passed over, up to its line break.
    passing: bool,
    garbled: bool,
    /// Lines passed over for their length since last asked.
    skipped: u64,
}

impl Wire {
    /// Bytes from the engine.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if self.garbled {
            return;
        }
        if self.read > 0 && self.read == self.bytes.len() {
            self.bytes.clear();
            self.read = 0;
        }
        self.bytes.extend_from_slice(bytes);
    }

    /// Forgets everything: a new engine begins.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// How many lines have been passed over for their length since last asked.
    pub(crate) fn take_skipped(&mut self) -> u64 {
        core::mem::take(&mut self.skipped)
    }

    /// Lets go of the bytes already read, once every whole message has been taken.
    pub(crate) fn settle(&mut self) {
        self.bytes.drain(..self.read);
        self.read = 0;
        self.bytes.shrink_to(64 * 1024);
    }

    /// The next whole message, if one has arrived.
    pub(crate) fn next(&mut self) -> Option<Message> {
        loop {
            let rest = &self.bytes[self.read..];
            if self.garbled || rest.is_empty() {
                return None;
            }
            if self.passing {
                match rest.iter().position(|b| *b == b'\n') {
                    Some(at) => {
                        self.read += at + 1;
                        self.passing = false;
                    }
                    None => {
                        self.read = self.bytes.len();
                        return None;
                    }
                }
                continue;
            }
            if rest[0] == FRAME {
                let length = rest.get(1..5)?;
                let length = u32::from_le_bytes([length[0], length[1], length[2], length[3]]);
                let length = length as usize;
                if !(FIXED - 4..=MAX_RECORD).contains(&length) {
                    self.garbled = true;
                    self.bytes = Vec::new();
                    self.read = 0;
                    return Some(Message::Garbled);
                }
                let record = rest.get(1..5 + length)?.to_vec();
                self.read += 5 + length;
                return Some(Message::Record(record));
            }
            match rest.iter().position(|b| *b == b'\n') {
                Some(at) => {
                    let line = rest[..at].trim_ascii().to_vec();
                    self.read += at + 1;
                    if at > ENGINE_LINE_LIMIT {
                        self.skipped += 1;
                    } else if !line.is_empty() {
                        return Some(Message::Line(line));
                    }
                }
                None => {
                    if rest.len() > ENGINE_LINE_LIMIT {
                        self.passing = true;
                        self.skipped += 1;
                        continue;
                    }
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
