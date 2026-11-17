//! The engine, as the server reaches it (system design, section 3A): a trait the node's `api`
//! program implements over `node-admission` and the engine's channel, and the host protocol's
//! messages (`moruna-hosted-engine.md`, section 4.3) read from the engine's bytes.

use alloc::{string::String, vec::Vec};
use moruna_api::Refusal;
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

/// What the server asks of the node to run jobs: the engine's check of a document, a fresh
/// engine for a job, and the host protocol's bytes to and from it. `api` does not link
/// `moruna-job`: a document is checked by the engine itself.
///
/// The server never waits on a call here: each answers at once, and the server asks again on
/// its next poll.
pub trait EngineHost {
    /// Starts the engine's check of `document` (a version 1 document's JSON, or a version 2
    /// document's bytes) in a short-lived process. One check runs at a time: the server starts
    /// the next when [`checked`](EngineHost::checked) has answered.
    fn check(&mut self, document: &[u8]) -> Result<(), HostError>;

    /// The verdict of the check under way, once it has one: the engine's refusals, none when
    /// the document is sound.
    fn checked(&mut self) -> Option<Result<Vec<Refusal>, HostError>>;

    /// Starts a fresh engine for one job and sends it `spec`, the host protocol's `spec`
    /// message: one line, its line break included.
    fn start(&mut self, spec: &[u8]) -> Result<(), HostError>;

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
    fn check(&mut self, _document: &[u8]) -> Result<(), HostError> {
        Err(HostError::Unavailable)
    }

    fn checked(&mut self) -> Option<Result<Vec<Refusal>, HostError>> {
        None
    }

    fn start(&mut self, _spec: &[u8]) -> Result<(), HostError> {
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
                    if !line.is_empty() && at <= ENGINE_LINE_LIMIT {
                        return Some(Message::Line(line));
                    }
                }
                None => {
                    if rest.len() > ENGINE_LINE_LIMIT {
                        self.passing = true;
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
