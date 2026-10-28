//! The record (design 21 a): one layout, identical in a ring, in a file and on the wire.
//!
//! ```text
//! offset  size  field
//! 0       4     len         u32 LE, the number of bytes after this field
//! 4       4     crc         crc32c of bytes [8, 4 + len)
//! 8       8     seq         u64
//! 16      8     ts          i64 unix nanoseconds
//! 24      4     subject     u32, opaque to this crate
//! 28      2     msg_id_len  u16
//! 30      2     hdr_count   u16
//! 32      4     body_len    u32, at most MAX_BODY
//! 36      ...   msg_id, hdr_count x (u16 klen, key, u16 vlen, value), body
//! ```
//!
//! Everything is little-endian. `subject` is a number this crate attaches no meaning to.

use alloc::vec;
use alloc::vec::Vec;

use crate::crc::crc32c;
use crate::error::{Corruption, Error};

/// Bytes before the variable part.
pub const FIXED: usize = 36;
/// The largest body.
pub const MAX_BODY: usize = 1 << 20;
/// The largest record: the largest body with room for its id and headers.
pub const MAX_RECORD: usize = MAX_BODY + 64 * 1024;

/// One decoded record, borrowing its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record<'a> {
    /// The sequence number.
    pub seq: u64,
    /// Unix nanoseconds.
    pub ts: i64,
    /// The subject, opaque to this crate.
    pub subject: u32,
    /// The message id.
    pub msg_id: &'a str,
    /// The headers, in order.
    pub headers: Vec<(&'a str, &'a str)>,
    /// The body.
    pub body: &'a [u8],
}

/// What the bytes at a position hold.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded<'a> {
    /// A whole record and the bytes it takes.
    Record(Record<'a>, usize),
    /// Fewer bytes than the record says it has: read more, or a torn tail.
    Incomplete,
    /// Not a record, and why.
    Corrupt(Corruption),
}

/// Whether a record with these parts fits the format's fields (design 21 b.2).
pub fn check_fits(msg_id: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<(), Error> {
    size_of_parts(msg_id, headers, body).map(|_| ())
}

/// The bytes a record with these parts takes, or why it cannot be one.
pub fn encoded_len(msg_id: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<usize, Error> {
    size_of_parts(msg_id, headers, body)
}

fn size_of_parts(msg_id: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<usize, Error> {
    let max_field = usize::from(u16::MAX);
    if body.len() > MAX_BODY {
        return Err(Error::BodyTooLarge { len: body.len() });
    }
    if msg_id.len() > max_field {
        return Err(Error::MsgIdTooLong { len: msg_id.len() });
    }
    if headers.len() > max_field {
        return Err(Error::TooManyHeaders {
            count: headers.len(),
        });
    }
    let mut size = FIXED + msg_id.len() + body.len();
    for (index, (k, v)) in headers.iter().enumerate() {
        if k.len() > max_field || v.len() > max_field {
            return Err(Error::HeaderTooLong { index });
        }
        size += 4 + k.len() + v.len();
    }
    if size > MAX_RECORD {
        return Err(Error::RecordTooLarge { size });
    }
    Ok(size)
}

/// Encode one record into the front of `buf` without allocating; the bytes written.
/// A refusal writes nothing.
pub fn encode_into(
    buf: &mut [u8],
    seq: u64,
    ts: i64,
    subject: u32,
    msg_id: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<usize, Error> {
    let total = size_of_parts(msg_id, headers, body)?;
    if buf.len() < total {
        return Err(Error::BufferTooSmall {
            need: total,
            have: buf.len(),
        });
    }
    let out = &mut buf[..total];
    out[8..16].copy_from_slice(&seq.to_le_bytes());
    out[16..24].copy_from_slice(&ts.to_le_bytes());
    out[24..28].copy_from_slice(&subject.to_le_bytes());
    out[28..30].copy_from_slice(&(msg_id.len() as u16).to_le_bytes());
    out[30..32].copy_from_slice(&(headers.len() as u16).to_le_bytes());
    out[32..36].copy_from_slice(&(body.len() as u32).to_le_bytes());
    let mut at = FIXED;
    let mut put = |out: &mut [u8], bytes: &[u8]| {
        out[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    put(out, msg_id.as_bytes());
    for (k, v) in headers {
        put(out, &(k.len() as u16).to_le_bytes());
        put(out, k.as_bytes());
        put(out, &(v.len() as u16).to_le_bytes());
        put(out, v.as_bytes());
    }
    put(out, body);
    let len = (total - 4) as u32;
    let crc = crc32c(&out[8..]);
    out[0..4].copy_from_slice(&len.to_le_bytes());
    out[4..8].copy_from_slice(&crc.to_le_bytes());
    Ok(total)
}

/// Encode one record into a new vector.
pub fn encode(
    seq: u64,
    ts: i64,
    subject: u32,
    msg_id: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    let total = size_of_parts(msg_id, headers, body)?;
    let mut out = vec![0u8; total];
    encode_into(&mut out, seq, ts, subject, msg_id, headers, body)?;
    Ok(out)
}

fn u16_at(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// Decode the record at the start of `buf`, checking its crc and its fields (design 21 b.3).
pub fn decode(buf: &[u8]) -> Decoded<'_> {
    if buf.len() < 4 {
        return Decoded::Incomplete;
    }
    let len = u32_at(buf, 0) as usize;
    if !(FIXED - 4..=MAX_RECORD).contains(&len) {
        return Decoded::Corrupt(Corruption::Length(len));
    }
    let total = 4 + len;
    if buf.len() < total {
        return Decoded::Incomplete;
    }
    let stored = u32_at(buf, 4);
    let actual = crc32c(&buf[8..total]);
    if stored != actual {
        return Decoded::Corrupt(Corruption::Crc { stored, actual });
    }
    let seq = u64_at(buf, 8);
    let ts = u64_at(buf, 16) as i64;
    let subject = u32_at(buf, 24);
    let id_len = u16_at(buf, 28);
    let hdr_count = u16_at(buf, 30);
    let body_len = u32_at(buf, 32) as usize;
    let field = |at: usize, n: usize| -> Option<&[u8]> {
        if at.saturating_add(n) <= total {
            Some(&buf[at..at + n])
        } else {
            None
        }
    };
    let mut at = FIXED;
    let Some(id) = field(at, id_len) else {
        return Decoded::Corrupt(Corruption::MsgIdPastEnd);
    };
    let Ok(msg_id) = core::str::from_utf8(id) else {
        return Decoded::Corrupt(Corruption::MsgIdNotUtf8);
    };
    at += id_len;
    let mut headers = Vec::with_capacity(hdr_count);
    for _ in 0..hdr_count {
        let Some(kl) = field(at, 2) else {
            return Decoded::Corrupt(Corruption::HeaderPastEnd);
        };
        let kl = u16_at(kl, 0);
        let Some(k) = field(at + 2, kl) else {
            return Decoded::Corrupt(Corruption::HeaderKeyPastEnd);
        };
        at += 2 + kl;
        let Some(vl) = field(at, 2) else {
            return Decoded::Corrupt(Corruption::HeaderPastEnd);
        };
        let vl = u16_at(vl, 0);
        let Some(v) = field(at + 2, vl) else {
            return Decoded::Corrupt(Corruption::HeaderValuePastEnd);
        };
        at += 2 + vl;
        let (Ok(k), Ok(v)) = (core::str::from_utf8(k), core::str::from_utf8(v)) else {
            return Decoded::Corrupt(Corruption::HeaderNotUtf8);
        };
        headers.push((k, v));
    }
    if at + body_len != total {
        return Decoded::Corrupt(Corruption::Body {
            body_len,
            at,
            total,
        });
    }
    Decoded::Record(
        Record {
            seq,
            ts,
            subject,
            msg_id,
            headers,
            body: &buf[at..total],
        },
        total,
    )
}
