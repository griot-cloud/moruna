//! Why an event is not written, and why bytes are not one.

use alloc::string::String;
use core::fmt;

use moruna_record::{Corruption, Error as RecordError};

/// Why an event could not be encoded as a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The parts are not a record (a body or an id past the record's limits).
    Record(RecordError),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::Record(error) => write!(f, "the event is not a record: {error}"),
        }
    }
}

impl core::error::Error for EncodeError {}

/// Why bytes are not an event this build reads: nothing here panics on any input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the record says it has.
    Truncated,
    /// Bytes after the record that a frame holds exactly one of.
    Trailing(usize),
    /// Not a record: its length, crc, ids or headers do not hold.
    Record(Corruption),
    /// The record's subject is not an event kind this build knows.
    Subject(u32),
    /// The body does not carry the event schema's file identifier.
    Identifier,
    /// The FlatBuffers verifier refused the body: truncated, out of bounds, misaligned or past
    /// its limits.
    Malformed(String),
    /// A header the event needs is missing or unreadable.
    Header {
        /// The header.
        name: &'static str,
        /// What is wrong with it.
        reason: String,
    },
    /// A field of the body that verified and does not hold what an event holds: a value of an
    /// enum this build does not know, a name that is absent.
    Field {
        /// The field.
        field: &'static str,
        /// What is wrong with it.
        reason: String,
    },
    /// The record's subject, its `msg_id` or its body disagree with each other.
    Disagree(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => f.write_str("the record is cut short"),
            DecodeError::Trailing(bytes) => write!(f, "{bytes} bytes follow the record"),
            DecodeError::Record(corruption) => write!(f, "not a record: {corruption}"),
            DecodeError::Subject(subject) => {
                write!(f, "the subject {subject:#x} is not an event kind")
            }
            DecodeError::Identifier => {
                f.write_str("not an event: the body does not carry the identifier MEV1")
            }
            DecodeError::Malformed(reason) => write!(f, "not an event: {reason}"),
            DecodeError::Header { name, reason } => write!(f, "header `{name}`: {reason}"),
            DecodeError::Field { field, reason } => write!(f, "{field}: {reason}"),
            DecodeError::Disagree(reason) => {
                write!(f, "the record disagrees with itself: {reason}")
            }
        }
    }
}

impl core::error::Error for DecodeError {}
