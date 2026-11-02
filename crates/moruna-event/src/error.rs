//! Why an event is not written, and why bytes are not one.

use moruna_record::{Corruption, Error as RecordError};

/// Why an event could not be encoded as a record.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// The parts are not a record (a body or an id past the record's limits).
    #[error("the event is not a record: {0}")]
    Record(RecordError),
}

/// Why bytes are not an event this build reads: nothing here panics on any input.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// Fewer bytes than the record says it has.
    #[error("the record is cut short")]
    Truncated,
    /// Bytes after the record that a frame holds exactly one of.
    #[error("{0} bytes follow the record")]
    Trailing(usize),
    /// Not a record: its length, crc, ids or headers do not hold.
    #[error("not a record: {0}")]
    Record(Corruption),
    /// The record's subject is not an event kind this build knows.
    #[error("the subject {0:#x} is not an event kind")]
    Subject(u32),
    /// The body does not carry the event schema's file identifier.
    #[error("not an event: the body does not carry the identifier MEV1")]
    Identifier,
    /// The FlatBuffers verifier refused the body: truncated, out of bounds, misaligned or past
    /// its limits.
    #[error("not an event: {0}")]
    Malformed(String),
    /// A header the event needs is missing or unreadable.
    #[error("header `{name}`: {reason}")]
    Header {
        /// The header.
        name: &'static str,
        /// What is wrong with it.
        reason: String,
    },
    /// A field of the body that verified and does not hold what an event holds: a value of an
    /// enum this build does not know, a name that is absent.
    #[error("{field}: {reason}")]
    Field {
        /// The field.
        field: &'static str,
        /// What is wrong with it.
        reason: String,
    },
    /// The record's subject, its `msg_id` or its body disagree with each other.
    #[error("the record disagrees with itself: {0}")]
    Disagree(String),
}
