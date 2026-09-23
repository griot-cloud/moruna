//! The page-aligned Arrow IPC record encoding (contracts e.7).
//!
//! It is the Arrow IPC stream framing with one deviation the format permits: every body buffer
//! of a record batch is placed at a multiple of `page_bytes` rather than of 8, so each can be
//! written from and read into an arena buffer by direct IO or GDS without a copy. The encoder
//! lives here because two components write it (the staging segments of 09 e.3 and the
//! `ArrowIpcSink` of 08 e.3) and neither may drift from the other.

use std::collections::HashMap;

use arrow::buffer::Buffer as ArrowBuffer;
use arrow::ipc::reader::RecordBatchDecoder;
use arrow::ipc::writer::{IpcWriteOptions, StreamEncoder};
use arrow::ipc::{MetadataVersion, root_as_message};
use arrow::record_batch::RecordBatch;

use crate::buffer::{Allocator, Buffer};
use crate::error::MorunaError;
use crate::tier::Tier;

/// The IPC continuation marker that precedes every message length.
const CONTINUATION: [u8; 4] = [0xff; 4];
/// One flatbuffer `Buffer` struct: an `i64` offset then an `i64` length, inline.
const BUFFER_ENTRY: usize = 16;

fn bad(msg: impl Into<String>) -> MorunaError {
    MorunaError::Io {
        op: "ipc",
        target: "record".into(),
        msg: msg.into(),
    }
}

fn round_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to) * to
}

/// Encode one record batch into the page-aligned layout of e.7.
///
/// `base_offset` is the absolute offset, a multiple of `page_bytes`, at which the framing will
/// be written; the framing buffer is the only allocation this function makes from `alloc`, and
/// it is page-rounded and zero-padded. The returned pairs are each body buffer of the batch, in
/// schema order, with the absolute, page-aligned offset it must be written at; the buffers are
/// the batch's own, so writing them copies nothing (G-I2).
pub fn encode_framing(
    batch: &RecordBatch,
    page_bytes: usize,
    base_offset: u64,
    alloc: &dyn Allocator,
) -> crate::Result<(Buffer, Vec<(usize, ArrowBuffer)>)> {
    if page_bytes == 0 {
        return Err(bad("page size is zero"));
    }
    let page = page_bytes as u64;
    if !base_offset.is_multiple_of(page) {
        return Err(bad(format!(
            "base offset {base_offset} is not a multiple of {page_bytes}"
        )));
    }
    let schema = batch.schema();
    let mut encoder = StreamEncoder::try_new_with_options(&schema, IpcWriteOptions::default())
        .map_err(|e| bad(format!("stream encoder: {e}")))?;
    let pieces = encoder
        .encode(batch)
        .map_err(|e| bad(format!("encode: {e}")))?;

    // The framing is the schema message and the record batch message header: the leading bytes
    // of the stream, before the first body buffer. Both are small (e.7 caps the framing at
    // 1 MiB), and copying metadata is not copying payload bytes.
    let mut framing: Vec<u8> = Vec::new();
    let mut tail: Vec<ArrowBuffer> = Vec::new();
    let mut framing_len: Option<usize> = None;
    for piece in pieces {
        match framing_len {
            None => {
                framing.extend_from_slice(piece.as_slice());
                framing_len = framing_target(&framing)?;
                if framing_len.is_some_and(|target| target < framing.len()) {
                    return Err(bad("the encoder split a message header across buffers"));
                }
            }
            Some(_) => tail.push(piece),
        }
        if let Some(target) = framing_len
            && framing.len() < target
        {
            framing_len = None;
        }
    }
    let Some(framing_len) = framing_len else {
        return Err(bad("the stream has no complete record batch message"));
    };
    if framing.len() != framing_len {
        return Err(bad(format!(
            "framing is {} bytes, the messages declare {framing_len}",
            framing.len()
        )));
    }

    // The record batch message's buffer entries, in schema order, as the encoder laid them out
    // with 8-byte alignment; each is rewritten to its page-aligned place below.
    let rb_start = message_len(&framing)? + 8;
    let entries = buffer_entries(&framing[rb_start..])?;

    // Map each entry to the body buffer the encoder pushed for it, walking the tail and its
    // padding buffers in step with the declared offsets.
    let mut bodies: Vec<ArrowBuffer> = Vec::with_capacity(entries.len());
    let mut walked = 0u64;
    let mut index = 0usize;
    for (offset, length) in &entries {
        while walked < *offset {
            let Some(pad) = tail.get(index) else {
                return Err(bad("the stream ends inside the record batch body"));
            };
            walked += pad.len() as u64;
            index += 1;
        }
        if walked != *offset {
            return Err(bad(format!(
                "body offset {offset} is not on a buffer boundary"
            )));
        }
        let Some(body) = tail.get(index) else {
            return Err(bad("the stream ends inside the record batch body"));
        };
        if body.len() as u64 != *length {
            return Err(bad(format!(
                "body buffer is {} bytes, the metadata declares {length}",
                body.len()
            )));
        }
        walked += body.len() as u64;
        index += 1;
        bodies.push(body.clone());
    }

    // The page-aligned layout: the framing is page-rounded, then every body buffer starts at
    // the next page boundary after the previous one.
    let framing_padded = round_up(framing_len as u64, page);
    let mut placed = Vec::with_capacity(bodies.len());
    let mut cursor = base_offset + framing_padded;
    let body_base = base_offset + framing_len as u64;
    for (i, body) in bodies.into_iter().enumerate() {
        let absolute = cursor;
        let relative = absolute - body_base;
        write_entry(
            &mut framing[rb_start..],
            i,
            relative as i64,
            body.len() as i64,
        )?;
        cursor = round_up(absolute + body.len() as u64, page);
        let offset = usize::try_from(absolute)
            .map_err(|_| bad(format!("body offset {absolute} does not fit this platform")))?;
        placed.push((offset, body));
    }

    let mut buf = alloc.alloc(framing_padded as usize, host_tier(alloc))?;
    let bytes: &mut [u8] = &mut buf;
    bytes.fill(0);
    bytes[..framing.len()].copy_from_slice(&framing);
    Ok((buf, placed))
}

/// Decode a record written by `encode_framing`: `buf` holds the record from its framing to the
/// end of its last body piece. The returned batch's arrays point into `buf` (no copy), which
/// CT-T18 checks by pointer comparison.
pub fn decode(buf: ArrowBuffer, page_bytes: usize) -> crate::Result<RecordBatch> {
    if page_bytes == 0 {
        return Err(bad("page size is zero"));
    }
    let page = page_bytes as u64;
    let bytes = buf.as_slice();
    let schema_len = message_len(bytes)?;
    let schema_message = root_as_message(&bytes[8..8 + schema_len])
        .map_err(|e| bad(format!("schema message: {e}")))?;
    let schema_fb = schema_message
        .header_as_schema()
        .ok_or_else(|| bad("the first message is not a schema"))?;
    let schema = std::sync::Arc::new(arrow::ipc::convert::fb_to_schema(schema_fb));
    let rb_start = 8 + schema_len;
    let rb_len = message_len(&bytes[rb_start..])?;
    let rb_message = root_as_message(&bytes[rb_start + 8..rb_start + 8 + rb_len])
        .map_err(|e| bad(format!("record batch message: {e}")))?;
    let version = match rb_message.version() {
        MetadataVersion::V4 => MetadataVersion::V4,
        MetadataVersion::V5 => MetadataVersion::V5,
        other => return Err(bad(format!("unsupported IPC metadata version {other:?}"))),
    };
    let record_batch = rb_message
        .header_as_record_batch()
        .ok_or_else(|| bad("the second message is not a record batch"))?;
    let body_start = (rb_start + 8 + rb_len) as u64;
    for (offset, length) in buffer_entries(&bytes[rb_start..rb_start + 8 + rb_len])? {
        if length == 0 {
            continue;
        }
        if !(body_start + offset).is_multiple_of(page) {
            return Err(bad(format!(
                "body offset {} is not a multiple of {page_bytes}",
                body_start + offset
            )));
        }
    }
    let body = buf.slice(body_start as usize);
    let dictionaries: HashMap<i64, arrow::array::ArrayRef> = HashMap::new();
    RecordBatchDecoder::try_new(&body, record_batch, schema, &dictionaries, &version)
        .and_then(|d| d.read_record_batch())
        .map_err(|e| bad(format!("decode: {e}")))
}

/// The tier a framing buffer is allocated in: the run's one host tier (e.1).
fn host_tier(alloc: &dyn Allocator) -> Tier {
    if alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}

/// The declared length of the message whose continuation marker starts `bytes`.
fn message_len(bytes: &[u8]) -> crate::Result<usize> {
    if bytes.len() < 8 {
        return Err(bad("a message header needs 8 bytes"));
    }
    if bytes[0..4] != CONTINUATION {
        return Err(bad("the continuation marker is missing"));
    }
    let len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if len == 0 {
        return Err(bad("the message length is zero"));
    }
    if bytes.len() < 8 + len {
        return Err(bad(format!(
            "a message of {len} bytes is past the end of the buffer"
        )));
    }
    Ok(len)
}

/// `Some(total framing length)` once `framing` holds both complete message headers, `None`
/// while either is still incomplete.
fn framing_target(framing: &[u8]) -> crate::Result<Option<usize>> {
    if framing.len() < 8 {
        return Ok(None);
    }
    if framing[0..4] != CONTINUATION {
        return Err(bad("the continuation marker is missing"));
    }
    let first = u32::from_le_bytes([framing[4], framing[5], framing[6], framing[7]]) as usize;
    let rb_start = 8 + first;
    if framing.len() < rb_start + 8 {
        return Ok(None);
    }
    if framing[rb_start..rb_start + 4] != CONTINUATION {
        return Err(bad(
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

/// Where the record batch message's buffer entries begin inside `message` (the 8-byte header
/// and the flatbuffer), and how many there are.
fn entries_span(message: &[u8]) -> crate::Result<(usize, usize)> {
    let len = message_len(message)?;
    let flatbuffer = &message[8..8 + len];
    let parsed =
        root_as_message(flatbuffer).map_err(|e| bad(format!("record batch message: {e}")))?;
    let record_batch = parsed
        .header_as_record_batch()
        .ok_or_else(|| bad("the message is not a record batch"))?;
    let buffers = record_batch
        .buffers()
        .ok_or_else(|| bad("the record batch has no buffers"))?;
    let raw = buffers.bytes();
    let base = flatbuffer.as_ptr() as usize;
    let start = raw.as_ptr() as usize;
    if start < base || start + raw.len() > base + flatbuffer.len() {
        return Err(bad("the buffer vector is outside the message"));
    }
    Ok((8 + (start - base), raw.len() / BUFFER_ENTRY))
}

/// The `(offset, length)` of every buffer entry of a record batch message.
fn buffer_entries(message: &[u8]) -> crate::Result<Vec<(u64, u64)>> {
    let (start, count) = entries_span(message)?;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = start + i * BUFFER_ENTRY;
        let mut offset = [0u8; 8];
        let mut length = [0u8; 8];
        offset.copy_from_slice(&message[at..at + 8]);
        length.copy_from_slice(&message[at + 8..at + 16]);
        let offset = i64::from_le_bytes(offset);
        let length = i64::from_le_bytes(length);
        if offset < 0 || length < 0 {
            return Err(bad("a buffer entry has a negative offset or length"));
        }
        out.push((offset as u64, length as u64));
    }
    Ok(out)
}

/// Rewrite one buffer entry of a record batch message in place.
fn write_entry(message: &mut [u8], index: usize, offset: i64, length: i64) -> crate::Result<()> {
    let (start, count) = entries_span(message)?;
    if index >= count {
        return Err(bad(format!(
            "buffer entry {index} of {count} does not exist"
        )));
    }
    let at = start + index * BUFFER_ENTRY;
    message[at..at + 8].copy_from_slice(&offset.to_le_bytes());
    message[at + 8..at + 16].copy_from_slice(&length.to_le_bytes());
    Ok(())
}
