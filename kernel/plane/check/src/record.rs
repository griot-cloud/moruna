//! The data plane's record check (SDD 29): a record a task hands back with `batch_put` is
//! checked here before the engine sees it, and a record that fails ends the task with the
//! sentence.
//!
//! A data-plane record is contracts e.7's page-aligned record: a schema message, then a record
//! batch message, then the body, each non-empty body buffer at a page multiple from the
//! record's start. It is never compressed and carries no dictionaries (e.7: a staging record is
//! decoded with no dictionaries and inflates nothing, `Inflate::none()` of e.8). The check reads
//! the framing only; the body is bounded by offsets and lengths and never read.
//!
//! The Linux personality turns records into an Arrow IPC stream for a program that reads one on
//! standard input ([`stream_parts`]), and a stream a program writes on standard output back
//! into records ([`reframe`]), laid out exactly as moruna-kernel's `encode_framing` lays them.

use alloc::{format, string::String, vec::Vec};
use core::ops::Range;

use crate::fb::Table;
use crate::format::{batch, compression, header, message, version};
use crate::frame::{self, CONTINUATION};
use crate::schema::{Field, check_schema};
use crate::walk::{Body, walk};

/// The page every body buffer of a record starts on a multiple of.
pub const PAGE: u64 = 4096;

/// The longest framing (both messages) a record may have: contracts e.7 keeps a framing over
/// 1 MiB out of the arena.
pub const FRAMING_MAX: usize = 1 << 20;

/// How much of a record's start its framing needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// At least this many leading bytes before the framing's length is known.
    More(usize),
    /// The framing is bytes `0 .. n`: the schema message and the record batch message; the
    /// body starts at `n`.
    Framing(usize),
}

/// One column of a record: the record offsets and lengths of its buffers (its children's
/// included), in the order arrow's decoder reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub buffers: Vec<(u64, u64)>,
}

/// A record that passed the check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Where the body starts: the end of the record batch message.
    pub body_start: u64,
    /// The body length the record batch message declares.
    pub body_length: u64,
    pub rows: u64,
    pub fields: Vec<Field>,
    /// Every buffer, as record offsets and lengths, in walk order.
    pub buffers: Vec<(u64, u64)>,
    /// The buffers again, by top-level column.
    pub columns: Vec<Column>,
}

fn word(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

/// How many leading bytes of a record its framing takes, from a `prefix` of it.
pub fn framing_need(prefix: &[u8]) -> Result<Need, String> {
    let marker = |at: usize, which: &str| -> Result<Option<u32>, String> {
        match prefix.get(at..at + 4) {
            None => Ok(None),
            Some(m) if m == CONTINUATION => Ok(word(prefix, at + 4)),
            Some(_) => Err(format!(
                "the record's {which} message does not start with the continuation marker"
            )),
        }
    };
    let Some(first) = marker(0, "first")? else {
        return Ok(Need::More(8));
    };
    let check = |len: u32, which: &str, end: usize| -> Result<(), String> {
        if len == 0 {
            return Err(format!("the record's {which} message has a length of zero"));
        }
        if end > FRAMING_MAX {
            return Err(format!(
                "the record's framing reaches byte {end}, past the {FRAMING_MAX} bytes a \
                 framing may take"
            ));
        }
        Ok(())
    };
    let rb_start = 8 + first as usize;
    check(first, "first", rb_start + 8)?;
    let Some(second) = marker(rb_start, "second")? else {
        return Ok(Need::More(rb_start + 8));
    };
    let end = rb_start + 8 + second as usize;
    check(second, "second", end)?;
    Ok(Need::Framing(end))
}

/// Where the schema message and the record batch message of a record's `framing` lie. The
/// record from the record batch message on is a stream's message and its body as they are: the
/// format allows a buffer at any offset of a multiple of 8, and a page is one.
pub fn stream_parts(framing: &[u8]) -> Result<(Range<usize>, Range<usize>), String> {
    match framing_need(framing)? {
        Need::Framing(n) if n == framing.len() => {
            let rb_start = 8 + frame::message_len(framing)?;
            Ok((0..rb_start, rb_start..n))
        }
        Need::Framing(n) => Err(format!(
            "the framing is {} bytes and its two messages take {n}",
            framing.len()
        )),
        Need::More(n) => Err(format!(
            "the framing is {} bytes and its two messages need at least {n}",
            framing.len()
        )),
    }
}

/// The root table of the whole message `bytes`.
fn message_table<'a>(bytes: &'a [u8], which: &str) -> Result<Table<'a>, String> {
    let len =
        frame::message_len(bytes).map_err(|e| format!("the record's {which} message: {e}"))?;
    Table::root(&bytes[8..8 + len])
        .map_err(|e| format!("the record's {which} message is not a flatbuffer: {e}"))
}

/// The header table of a message that must be of header type `want`.
fn header_table<'a>(message: Table<'a>, want: u8, what: &str) -> Result<Table<'a>, String> {
    let kind = message.u8(message::HEADER_TYPE, header::NONE)?;
    if kind != want {
        return Err(format!(
            "the record's {what} message is a {} message",
            header::name(kind)
        ));
    }
    message
        .table(message::HEADER)?
        .ok_or_else(|| format!("the record's {what} message has no header"))
}

/// Refuse, with a sentence, a record that is not a data-plane record: `framing` is its bytes up
/// to the body (what [`framing_need`] said), `record_len` the length the task handed back.
pub fn check_record(framing: &[u8], record_len: u64) -> Result<Record, String> {
    let (schema_range, batch_range) = stream_parts(framing)?;
    let body_start = framing.len() as u64;
    if record_len < body_start {
        return Err(format!(
            "the record is {record_len} bytes and its framing alone takes {body_start}"
        ));
    }
    let schema_message = message_table(&framing[schema_range], "first")?;
    let schema = header_table(schema_message, header::SCHEMA, "first")?;
    let fields = check_schema(schema)?;
    if let Some(field) = fields.iter().find(|f| f.kind.has_dictionary()) {
        return Err(format!(
            "field {:?} is dictionary encoded, and a data-plane record carries no dictionaries",
            field.name
        ));
    }
    let batch_message = message_table(&framing[batch_range], "second")?;
    let meta = batch_message.i16(message::VERSION, version::V1)?;
    if meta != version::V4 && meta != version::V5 {
        return Err(format!(
            "the record's batch has metadata version {}, and a data-plane record is V4 or V5",
            version::name(meta)
        ));
    }
    let record_batch = header_table(batch_message, header::RECORD_BATCH, "second")?;
    if let Some(packed) = record_batch.table(batch::COMPRESSION)? {
        return Err(format!(
            "the record batch is compressed with {}, and a data-plane record may inflate 0 \
             bytes: a record on the data plane is never compressed",
            compression::codec(packed.i8(compression::CODEC, 0)?)
        ));
    }
    let room = record_len - body_start;
    let declared = batch_message.i64(message::BODY_LENGTH, 0)?;
    let fits = matches!(u64::try_from(declared), Ok(d) if d <= room);
    if !fits {
        return Err(format!(
            "the record batch declares a body of {declared} bytes and the record holds {room} \
             after its framing"
        ));
    }
    let body = Body {
        len: usize::try_from(room).unwrap_or(usize::MAX),
        addr: framing.len(),
    };
    let walked = walk(record_batch, body, meta, &fields, None)?;
    let mut buffers = Vec::with_capacity(walked.spans.len());
    for span in &walked.spans {
        let at = body_start + span.offset as u64;
        if span.len > 0 && !at.is_multiple_of(PAGE) {
            return Err(format!(
                "a buffer of {} bytes starts at byte {at} of the record, not on a page \
                 boundary ({PAGE} bytes)",
                span.len
            ));
        }
        buffers.push((at, span.len as u64));
    }
    let columns = fields
        .iter()
        .zip(&walked.columns)
        .map(|(field, range)| Column {
            name: field.name.clone(),
            buffers: buffers
                .get(range.clone())
                .map(<[_]>::to_vec)
                .unwrap_or_default(),
        })
        .collect();
    Ok(Record {
        body_start,
        body_length: declared as u64,
        rows: record_batch.i64(batch::LENGTH, 0)? as u64,
        fields,
        buffers,
        columns,
    })
}

/// A stream's schema message and one record batch message laid out as a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reframed {
    /// The record's framing, its two messages; the page padding after it is not included.
    pub framing: Vec<u8>,
    /// The record's length: the framing and the body its batch message declares.
    pub record_len: u64,
    /// Each non-empty buffer: its offset in the stream message's body, its offset in the
    /// record, and its length.
    pub moves: Vec<(u64, u64, u64)>,
}

/// Lay out a stream's `schema_message` and `batch_message` (whole messages, as a stream carries
/// them) as a record, exactly as `encode_framing` lays out a batch: the framing padded to a
/// page, each buffer at the next page boundary, the body length rewritten.
pub fn reframe(schema_message: &[u8], batch_message: &[u8]) -> Result<Reframed, String> {
    for (bytes, which) in [(schema_message, "schema"), (batch_message, "record batch")] {
        let len = frame::message_len(bytes).map_err(|e| format!("the {which} message: {e}"))?;
        if bytes.len() != 8 + len {
            return Err(format!(
                "the {which} message is {} bytes and declares {}",
                bytes.len(),
                8 + len
            ));
        }
    }
    let rb_message = message_table(batch_message, "second")?;
    let record_batch = header_table(rb_message, header::RECORD_BATCH, "second")?;
    if let Some(packed) = record_batch.table(batch::COMPRESSION)? {
        return Err(format!(
            "the record batch is compressed with {}, and a data-plane record is never \
             compressed",
            compression::codec(packed.i8(compression::CODEC, 0)?)
        ));
    }
    let declared = rb_message.i64(message::BODY_LENGTH, 0)?;
    let body = u64::try_from(declared)
        .map_err(|_| format!("the record batch declares a body of {declared} bytes"))?;
    let entries = frame::buffer_entries(batch_message)?;
    for (i, (offset, length)) in entries.iter().enumerate() {
        if offset.checked_add(*length).is_none_or(|end| end > body) {
            return Err(format!(
                "buffer {i} of {length} bytes at {offset} is outside the body of {body} bytes"
            ));
        }
    }
    let mut framing = Vec::with_capacity(schema_message.len() + batch_message.len());
    framing.extend_from_slice(schema_message);
    framing.extend_from_slice(batch_message);
    let lengths: Vec<u64> = entries.iter().map(|(_, l)| *l).collect();
    let placed = frame::page_layout(&mut framing, &lengths, PAGE, 0)?;
    let rb_start = schema_message.len();
    let body_length = frame::body_length(&framing[rb_start..])?;
    let moves = entries
        .iter()
        .zip(&placed)
        .filter(|((_, length), _)| *length > 0)
        .map(|((offset, length), to)| (*offset, *to, *length))
        .collect();
    let record_len = framing.len() as u64 + body_length as u64;
    Ok(Reframed {
        framing,
        record_len,
        moves,
    })
}
