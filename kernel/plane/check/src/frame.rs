//! The framing of the page-aligned Arrow IPC record (contracts e.7): its two messages' lengths,
//! the record batch message's buffer entries and body length, and the page-aligned layout of
//! its body, rewritten in place.
//!
//! A record is the Arrow IPC stream framing with one deviation the format permits: every body
//! buffer is placed at a multiple of a page from the record's start rather than of 8, so each
//! can be mapped, written by direct IO or handed to a device with no copy. The message bytes
//! handled here are whole IPC messages as a stream carries them: the continuation marker, a
//! little-endian `u32` length, then the flatbuffer.

use alloc::{format, string::String, vec::Vec};

use crate::fb::{Table, pair};
use crate::format::{batch, header, message};

/// The IPC continuation marker that precedes every message length.
pub const CONTINUATION: [u8; 4] = [0xff; 4];

/// The end-of-stream marker: a continuation marker and a length of zero.
pub const END_OF_STREAM: [u8; 8] = [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0];

/// One flatbuffer `Buffer` struct: an `i64` offset then an `i64` length, inline.
pub const BUFFER_ENTRY: usize = batch::ENTRY;

/// `value` rounded up to a multiple of `to`.
pub fn round_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to).saturating_mul(to)
}

/// The declared length of the message whose continuation marker starts `bytes`, which holds
/// the whole message.
pub fn message_len(bytes: &[u8]) -> Result<usize, String> {
    if bytes.len() < 8 {
        return Err(String::from("a message header needs 8 bytes"));
    }
    if bytes[0..4] != CONTINUATION {
        return Err(String::from("the continuation marker is missing"));
    }
    let len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if len == 0 {
        return Err(String::from("the message length is zero"));
    }
    if bytes.len() - 8 < len {
        return Err(format!(
            "a message of {len} bytes is past the end of the buffer"
        ));
    }
    Ok(len)
}

/// `Some(total framing length)` once `framing` holds both complete message headers, `None`
/// while either is still incomplete.
pub fn framing_target(framing: &[u8]) -> Result<Option<usize>, String> {
    if framing.len() < 8 {
        return Ok(None);
    }
    if framing[0..4] != CONTINUATION {
        return Err(String::from("the continuation marker is missing"));
    }
    let first = u32::from_le_bytes([framing[4], framing[5], framing[6], framing[7]]) as usize;
    let rb_start = 8 + first;
    if framing.len() < rb_start + 8 {
        return Ok(None);
    }
    if framing[rb_start..rb_start + 4] != CONTINUATION {
        return Err(String::from(
            "the record batch message's continuation marker is missing",
        ));
    }
    let second = u32::from_le_bytes([
        framing[rb_start + 4],
        framing[rb_start + 5],
        framing[rb_start + 6],
        framing[rb_start + 7],
    ]) as usize;
    Ok(Some(rb_start + 8 + second))
}

/// The message flatbuffer of the whole message `bytes`, and its root table.
fn root(bytes: &[u8]) -> Result<(&[u8], Table<'_>), String> {
    let len = message_len(bytes)?;
    let flatbuffer = &bytes[8..8 + len];
    let table = Table::root(flatbuffer).map_err(|e| format!("record batch message: {e}"))?;
    Ok((flatbuffer, table))
}

/// The record batch table of the whole message `bytes`.
fn record_batch(bytes: &[u8]) -> Result<Table<'_>, String> {
    let (_, message) = root(bytes)?;
    if message.u8(message::HEADER_TYPE, header::NONE)? != header::RECORD_BATCH {
        return Err(String::from("the message is not a record batch"));
    }
    message
        .table(message::HEADER)?
        .ok_or_else(|| String::from("the message is not a record batch"))
}

/// Where the record batch message's buffer entries begin inside `message` (the 8-byte header
/// and the flatbuffer), and how many there are.
pub fn entries_span(message: &[u8]) -> Result<(usize, usize), String> {
    let batch = record_batch(message)?;
    let buffers = batch
        .vector(batch::BUFFERS, BUFFER_ENTRY)?
        .ok_or_else(|| String::from("the record batch has no buffers"))?;
    Ok((8 + buffers.start(), buffers.len()))
}

/// The `(offset, length)` of every buffer entry of a record batch message.
pub fn buffer_entries(message: &[u8]) -> Result<Vec<(u64, u64)>, String> {
    let (start, count) = entries_span(message)?;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = start + i * BUFFER_ENTRY;
        let (offset, length) = message
            .get(at..at + BUFFER_ENTRY)
            .and_then(pair)
            .ok_or_else(|| String::from("a buffer entry is outside the message"))?;
        if offset < 0 || length < 0 {
            return Err(String::from(
                "a buffer entry has a negative offset or length",
            ));
        }
        out.push((offset as u64, length as u64));
    }
    Ok(out)
}

/// Where the record batch message's `bodyLength` lives in `message` (the 8-byte header and the
/// flatbuffer), found through the flatbuffer's own vtable. `None` when the field is absent,
/// which flatbuffers does for a value of zero.
pub fn body_length_at(message: &[u8]) -> Result<Option<usize>, String> {
    let (flatbuffer, table) = root(message)?;
    match table.position(message::BODY_LENGTH)? {
        Some(at) if at + 8 <= flatbuffer.len() => Ok(Some(8 + at)),
        Some(_) => Err(String::from("the message flatbuffer is truncated")),
        None => Ok(None),
    }
}

/// The body length the whole message `bytes` declares.
pub fn body_length(bytes: &[u8]) -> Result<i64, String> {
    let (_, table) = root(bytes)?;
    table.i64(message::BODY_LENGTH, 0)
}

/// Set the record batch message's `bodyLength` to `length`, in place (KA11). A flatbuffer omits
/// a field that is zero, so a zero length is the message as the encoder wrote it, and a length
/// the message has no room for is refused rather than written somewhere else.
pub fn write_body_length(message: &mut [u8], length: u64) -> Result<(), String> {
    match body_length_at(message)? {
        Some(at) => message[at..at + 8].copy_from_slice(&(length as i64).to_le_bytes()),
        None if length == 0 => {}
        None => {
            return Err(String::from(
                "the record batch message has no body length to set",
            ));
        }
    }
    if body_length(message)? as u64 != length {
        return Err(String::from(
            "the record batch message's body length did not take",
        ));
    }
    Ok(())
}

/// Rewrite one buffer entry of a record batch message in place.
pub fn write_entry(
    message: &mut [u8],
    index: usize,
    offset: i64,
    length: i64,
) -> Result<(), String> {
    let (start, count) = entries_span(message)?;
    if index >= count {
        return Err(format!("buffer entry {index} of {count} does not exist"));
    }
    let at = start + index * BUFFER_ENTRY;
    message[at..at + 8].copy_from_slice(&offset.to_le_bytes());
    message[at + 8..at + 16].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

/// Lay out a record's body page by page, in place: `framing` is the schema message and the
/// record batch message, to be written at `base_offset` (a multiple of `page`), and `lengths`
/// are the body buffers' lengths in entry order. The framing is page-rounded, then every buffer
/// starts at the next page boundary after the previous one; each entry's offset is rewritten
/// to its place and the message's body length to the span from the end of the message to the
/// end of the last byte of a buffer, padded to 8, which is what a file holds (KA11). An empty
/// buffer's offset is clamped to that span's end, since a reader slices the body at every
/// entry's offset, empty ones included. Returns each buffer's absolute offset.
pub fn page_layout(
    framing: &mut [u8],
    lengths: &[u64],
    page: u64,
    base_offset: u64,
) -> Result<Vec<u64>, String> {
    if page == 0 {
        return Err(String::from("page size is zero"));
    }
    if !base_offset.is_multiple_of(page) {
        return Err(format!(
            "base offset {base_offset} is not a multiple of {page}"
        ));
    }
    let rb_start = message_len(framing)? + 8;
    let framing_len = framing.len() as u64;
    let mut cursor = base_offset + round_up(framing_len, page);
    let body_base = base_offset + framing_len;
    let mut placed = Vec::with_capacity(lengths.len());
    let mut layout = Vec::with_capacity(lengths.len());
    let mut body_end = 0u64;
    for &length in lengths {
        let absolute = cursor;
        let relative = absolute - body_base;
        if length != 0 {
            body_end = body_end.max(relative + length);
        }
        layout.push((relative, length));
        cursor = round_up(absolute + length, page);
        placed.push(absolute);
    }
    let body_len = round_up(body_end, 8);
    let rb = &mut framing[rb_start..];
    for (i, (relative, length)) in layout.into_iter().enumerate() {
        let at = if length == 0 {
            relative.min(body_len)
        } else {
            relative
        };
        write_entry(rb, i, at as i64, length as i64)?;
    }
    write_body_length(rb, body_len)?;
    Ok(placed)
}
