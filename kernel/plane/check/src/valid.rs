//! Validity bitmaps of all ones, left out of a record.
//!
//! arrow-ipc's writer gives every column and every child a validity bitmap, all ones when none
//! of its values is null. The format lets a node with no nulls have a bitmap of zero length, and
//! in a page-aligned record (contracts e.7) each non-empty buffer takes at least a page of its
//! own, so every writer of records leaves such a bitmap out: the engine's `encode_framing`
//! (moruna-kernel) and the function SDK's writer (`kernel/function/rust`) both call
//! [`leave_out_all_valid`] on the stream messages arrow-ipc wrote, before the page layout. The
//! batch read back is the same: the node already counts no nulls, and a reader takes a
//! zero-length bitmap for none.
//!
//! The bitmaps are found by the walk the check itself makes ([`crate::walk`]), in the order
//! arrow-ipc reads them; a compressed batch is left as it is, since its bitmaps are compressed
//! bytes.

use alloc::{string::String, vec::Vec};

use crate::fb::Table;
use crate::format::{batch, header, message, version};
use crate::frame;
use crate::schema::check_schema;
use crate::walk::{Body, Plan, Rule, Span, walk};

/// The header table of the whole message `bytes` (its marker and length, then the flatbuffer),
/// which must be of header type `want`.
fn header_table(bytes: &[u8], want: u8) -> Result<(Table<'_>, Table<'_>), String> {
    let len = frame::message_len(bytes)?;
    let message = Table::root(&bytes[8..8 + len])?;
    if message.u8(message::HEADER_TYPE, header::NONE)? != want {
        return Err(String::from(
            "a message of another type than the record needs",
        ));
    }
    let table = message
        .table(message::HEADER)?
        .ok_or_else(|| String::from("a message without its header"))?;
    Ok((message, table))
}

/// Every buffer the walk reaches, numbered in entry order, and of the validity bitmaps among
/// them, the entry, its span and the bits it covers.
#[derive(Default)]
struct Bitmaps {
    next: usize,
    found: Vec<(usize, Span, usize)>,
}

impl Plan for Bitmaps {
    fn take(&mut self, _: &str, _: &'static str, span: Span, rule: Rule) -> Result<Span, String> {
        if let Rule::Validity(bits) = rule {
            self.found.push((self.next, span, bits));
        }
        self.next += 1;
        Ok(span)
    }
}

/// Whether the first `bits` bits of `bytes` are all set, least significant bit first.
pub fn all_set(bytes: &[u8], bits: usize) -> bool {
    let (whole, rest) = (bits / 8, bits % 8);
    if bytes.len() < bits.div_ceil(8) {
        return false;
    }
    let mask = (1u8 << rest).wrapping_sub(1);
    bytes[..whole].iter().all(|b| *b == 0xff) && (rest == 0 || bytes[whole] & mask == mask)
}

/// Leaves out every validity bitmap of `batch_message` whose bits are all set: its entry's
/// length becomes zero, in place, and its offset is kept. `schema_message` and `batch_message`
/// are whole stream messages (marker, length, flatbuffer and padding), as arrow-ipc writes
/// them; `read(offset, out)` fills `out` with the body's bytes from `offset`, `false` when it
/// cannot (the bitmap is then kept). The entries left out, in order. A batch the walk refuses
/// is an error, and a compressed batch is left as it is.
pub fn leave_out_all_valid(
    schema_message: &[u8],
    batch_message: &mut [u8],
    read: &mut dyn FnMut(u64, &mut [u8]) -> bool,
) -> Result<Vec<usize>, String> {
    let (_, schema) = header_table(schema_message, header::SCHEMA)?;
    let fields = check_schema(schema)?;
    let body = Body {
        len: usize::try_from(frame::body_length(batch_message)?)
            .map_err(|_| String::from("the record batch declares a negative body"))?,
        addr: 0,
    };
    let mut bitmaps = Bitmaps::default();
    {
        let (message, record_batch) = header_table(batch_message, header::RECORD_BATCH)?;
        if record_batch.table(batch::COMPRESSION)?.is_some() {
            return Ok(Vec::new());
        }
        let meta = message.i16(message::VERSION, version::V1)?;
        walk(record_batch, body, meta, &fields, Some(&mut bitmaps))?;
    }
    let mut left_out = Vec::new();
    let mut bytes = Vec::new();
    for (entry, span, bits) in bitmaps.found {
        if span.len == 0 {
            continue;
        }
        bytes.clear();
        bytes.resize(bits.div_ceil(8), 0);
        if read(span.offset as u64, &mut bytes) && all_set(&bytes, bits) {
            frame::write_entry(batch_message, entry, span.offset as i64, 0)?;
            left_out.push(entry);
        }
    }
    Ok(left_out)
}
