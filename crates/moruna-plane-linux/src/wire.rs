//! The messages on a task's plane socket (a `SOCK_SEQPACKET` pair, one message a call or an
//! answer): a header of [`HEADER`] bytes, little endian, and for a port call the request after
//! it. A batch, a window or a port reply is never in a message: it is a memfd passed beside the
//! header (`SCM_RIGHTS`), which the task maps.
//!
//! ```text
//! offset 0   u32 kind
//! offset 4   u32 slot, as the task's plane numbers it (inputs first, then outputs)
//! offset 8   u64 a
//! offset 16  u64 b
//! offset 24  u64 c
//! ```
//!
//! | kind | from | a | b | c | memfd |
//! |---|---|---|---|---|---|
//! | [`GET`] | task | | | | |
//! | [`PUT`] | task | offset in the window | length | | |
//! | [`CALL`] | task | token | port | length of the request after the header | |
//! | [`BATCH`] | engine | where the record starts in the memfd | its length | | sealed |
//! | [`WINDOW`] | engine | | its length | | writable |
//! | [`END`] | engine | | | | |
//! | [`PUT_DONE`] | engine | | | | |
//! | [`REPLY`] | engine | token | zero, or the call's error code | the reply's length | sealed, when a and c are not zero |
//! | [`ERROR`] | engine | the kind refused | the error code | | |

use moruna_abi::Error;

/// The header's length.
pub const HEADER: usize = 32;

/// `batch_get` on the slot.
pub const GET: u32 = 1;
/// `batch_put` of the window's record.
pub const PUT: u32 = 2;
/// `port_call`.
pub const CALL: u32 = 3;
/// An input's next batch.
pub const BATCH: u32 = 0x11;
/// An output's window.
pub const WINDOW: u32 = 0x12;
/// An input's end.
pub const END: u32 = 0x13;
/// The window's record is the engine's: the task's `batch_put` returns.
pub const PUT_DONE: u32 = 0x14;
/// A port call's answer.
pub const REPLY: u32 = 0x15;
/// A `GET` or a `PUT` refused, with the kernel's error code.
pub const ERROR: u32 = 0x16;

/// The longest message: a header and the largest request (`moruna_abi::plane::request::MAX`),
/// the kernel's channel bound.
pub const MESSAGE_MAX: usize = HEADER + moruna_abi::plane::request::MAX;

/// A message's header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// What the message is.
    pub kind: u32,
    /// The slot it is about.
    pub slot: u32,
    /// Its first number.
    pub a: u64,
    /// Its second.
    pub b: u64,
    /// Its third.
    pub c: u64,
}

impl Header {
    /// A header of `kind` on `slot`, its numbers zero.
    pub fn new(kind: u32, slot: u32) -> Header {
        Header {
            kind,
            slot,
            ..Header::default()
        }
    }

    /// The header's bytes.
    pub fn bytes(&self) -> [u8; HEADER] {
        let mut out = [0u8; HEADER];
        out[0..4].copy_from_slice(&self.kind.to_le_bytes());
        out[4..8].copy_from_slice(&self.slot.to_le_bytes());
        out[8..16].copy_from_slice(&self.a.to_le_bytes());
        out[16..24].copy_from_slice(&self.b.to_le_bytes());
        out[24..32].copy_from_slice(&self.c.to_le_bytes());
        out
    }

    /// The header at the start of `message`; `None` when it is shorter than one.
    pub fn read(message: &[u8]) -> Option<Header> {
        let word = |at: usize| -> Option<u64> {
            let b = message.get(at..at + 8)?;
            let mut w = [0u8; 8];
            w.copy_from_slice(b);
            Some(u64::from_le_bytes(w))
        };
        let half = |at: usize| -> Option<u32> {
            let b = message.get(at..at + 4)?;
            Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        Some(Header {
            kind: half(0)?,
            slot: half(4)?,
            a: word(8)?,
            b: word(16)?,
            c: word(24)?,
        })
    }

    /// An `ERROR` refusing a request of `kind` on `slot` with `error`.
    pub fn error(kind: u32, slot: u32, error: Error) -> Header {
        Header {
            kind: ERROR,
            slot,
            a: u64::from(kind),
            b: u64::from(error.code()),
            c: 0,
        }
    }

    /// The error an `ERROR` or a `REPLY` carries, if any.
    pub fn status(&self) -> Option<Error> {
        let code = match self.kind {
            ERROR => self.b,
            REPLY => self.b,
            _ => return None,
        };
        if code == 0 {
            return None;
        }
        Some(
            u32::try_from(code)
                .ok()
                .and_then(Error::from_code)
                .unwrap_or(Error::BadState),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_reads_back_what_was_written() {
        let h = Header {
            kind: CALL,
            slot: 7,
            a: 1,
            b: u64::MAX,
            c: 3,
        };
        assert_eq!(Header::read(&h.bytes()), Some(h));
        assert_eq!(Header::read(&h.bytes()[..31]), None);
        let e = Header::error(PUT, 2, Error::OutOfRange);
        assert_eq!(e.status(), Some(Error::OutOfRange));
        assert_eq!((e.kind, e.slot, e.a), (ERROR, 2, u64::from(PUT)));
        let mut reply = Header::new(REPLY, 0);
        assert_eq!(reply.status(), None);
        reply.b = u64::from(Error::AccessDenied.code());
        assert_eq!(reply.status(), Some(Error::AccessDenied));
        reply.b = 999;
        assert_eq!(reply.status(), Some(Error::BadState), "a code no error has");
        assert_eq!(Header::new(BATCH, 1).status(), None);
        assert_eq!(MESSAGE_MAX, 65536);
    }
}
