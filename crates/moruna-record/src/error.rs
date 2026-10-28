//! The errors of the record and the ring.

use core::fmt;

/// Why bytes are not a record (design 21 b.3). The text of each is the reason a decode of
/// that case reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corruption {
    /// The `len` field is outside `FIXED - 4 ..= MAX_RECORD`.
    Length(usize),
    /// The stored crc is not the crc of the bytes after it.
    Crc {
        /// What the record says.
        stored: u32,
        /// What its bytes give.
        actual: u32,
    },
    /// The msg_id runs past the record's end.
    MsgIdPastEnd,
    /// The msg_id is not UTF-8.
    MsgIdNotUtf8,
    /// A header's length fields run past the record's end.
    HeaderPastEnd,
    /// A header key runs past the record's end.
    HeaderKeyPastEnd,
    /// A header value runs past the record's end.
    HeaderValuePastEnd,
    /// A header key or value is not UTF-8.
    HeaderNotUtf8,
    /// The body does not end where the record does.
    Body {
        /// The `body_len` field.
        body_len: usize,
        /// Where the body would start.
        at: usize,
        /// The record's whole size.
        total: usize,
    },
}

impl fmt::Display for Corruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Corruption::Length(len) => write!(f, "a record length of {len}"),
            Corruption::Crc { stored, actual } => {
                write!(f, "crc {stored:08x}, the bytes give {actual:08x}")
            }
            Corruption::MsgIdPastEnd => f.write_str("msg_id past the record's end"),
            Corruption::MsgIdNotUtf8 => f.write_str("msg_id is not UTF-8"),
            Corruption::HeaderPastEnd => f.write_str("a header past the record's end"),
            Corruption::HeaderKeyPastEnd => f.write_str("a header key past the record's end"),
            Corruption::HeaderValuePastEnd => f.write_str("a header value past the record's end"),
            Corruption::HeaderNotUtf8 => f.write_str("a header is not UTF-8"),
            Corruption::Body {
                body_len,
                at,
                total,
            } => write!(
                f,
                "a body of {body_len} bytes at {at} in a record of {total}"
            ),
        }
    }
}

/// Every refusal of this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A body longer than `MAX_BODY`.
    BodyTooLarge {
        /// The body's length.
        len: usize,
    },
    /// A msg_id longer than 65535 bytes.
    MsgIdTooLong {
        /// The msg_id's length.
        len: usize,
    },
    /// More than 65535 headers.
    TooManyHeaders {
        /// The count.
        count: usize,
    },
    /// A header key or value longer than 65535 bytes.
    HeaderTooLong {
        /// The header's index.
        index: usize,
    },
    /// A whole record larger than `MAX_RECORD`.
    RecordTooLarge {
        /// The record's size.
        size: usize,
    },
    /// `encode_into` was given a buffer smaller than the record.
    BufferTooSmall {
        /// What the record takes.
        need: usize,
        /// What the buffer has.
        have: usize,
    },
    /// The region does not start on a 64-byte boundary.
    Misaligned,
    /// The region cannot hold the ring's header and the smallest data area.
    RegionTooSmall {
        /// The region's size.
        len: usize,
        /// The smallest size that holds a ring.
        need: usize,
    },
    /// The region does not start with the ring's magic: it was never formatted by `init`.
    BadMagic,
    /// The header is inconsistent: the reason.
    BadHeader(&'static str),
    /// This handle already gave out its consumer.
    ConsumerTaken,
    /// A slot's header is not one a producer writes, at this position.
    CorruptSlot {
        /// The slot's position.
        pos: u64,
    },
    /// A slot was claimed and its producer died before it wrote the slot's length, so no one
    /// knows where the slot ends (design 21 e.5).
    StaleExtentUnknown {
        /// The slot's position.
        pos: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::BodyTooLarge { len } => {
                write!(f, "a body of {len} bytes; at most {}", crate::MAX_BODY)
            }
            Error::MsgIdTooLong { len } => write!(f, "a msg_id of {len} bytes; at most 65535"),
            Error::TooManyHeaders { count } => write!(f, "{count} headers; at most 65535"),
            Error::HeaderTooLong { index } => {
                write!(
                    f,
                    "header {index} has a key or value longer than 65535 bytes"
                )
            }
            Error::RecordTooLarge { size } => {
                write!(f, "a record of {size} bytes; at most {}", crate::MAX_RECORD)
            }
            Error::BufferTooSmall { need, have } => {
                write!(
                    f,
                    "a record of {need} bytes does not fit a buffer of {have}"
                )
            }
            Error::Misaligned => {
                f.write_str("the ring's region does not start on a 64-byte boundary")
            }
            Error::RegionTooSmall { len, need } => {
                write!(f, "a region of {len} bytes; a ring needs at least {need}")
            }
            Error::BadMagic => f.write_str("the region does not start with the ring's magic"),
            Error::BadHeader(why) => write!(f, "the ring's header is inconsistent: {why}"),
            Error::ConsumerTaken => f.write_str("this handle already gave out the ring's consumer"),
            Error::CorruptSlot { pos } => write!(f, "the slot at position {pos} is corrupt"),
            Error::StaleExtentUnknown { pos } => write!(
                f,
                "the slot at position {pos} was claimed and never given a length; its extent is unknown"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

#[cfg(feature = "std")]
impl std::error::Error for Corruption {}
