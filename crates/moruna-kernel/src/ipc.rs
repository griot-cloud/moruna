//! The page-aligned Arrow IPC record encoding (contracts e.7), and every Arrow IPC read Moruna
//! makes: each message checked before arrow-ipc sees it, a compressed batch inflated within a
//! ceiling (`check`, `Inflate`, CT-I13), and streams read message by message through that check
//! (`read_stream`, `ReadStream`, e.8).
//!
//! It is the Arrow IPC stream framing with one deviation the format permits: every body buffer
//! of a record batch is placed at a multiple of `page_bytes` rather than of 8, so each can be
//! written from and read into an arena buffer by direct IO or GDS without a copy. The encoder
//! lives here because two components write it (the staging segments of 09 e.3 and the
//! `ArrowIpcSink` of 08 e.3) and neither may drift from the other. A compressed record
//! (`encode_compressed`, the sink's `compression` setting) keeps the format's own 8-byte
//! alignment instead: its buffers are inflated into memory, never read into place by direct IO,
//! so there is nothing for a page boundary to serve.

use std::collections::HashMap;

use arrow::buffer::Buffer as ArrowBuffer;
use arrow::ipc::reader::RecordBatchDecoder;
use arrow::ipc::writer::{IpcWriteOptions, StreamEncoder};
use arrow::ipc::{CompressionType, MetadataVersion, root_as_message};
use arrow::record_batch::RecordBatch;
use moruna_ipc_check::frame::{self, buffer_entries, framing_target, message_len, round_up};

mod check;
mod inflate;
mod stream;

pub use check::{Checked, Passed, check};
pub use inflate::{Inflate, NoArena, UNBUDGETED_CEILING};
pub use stream::{Decoder, ReadStream, read_stream};

use crate::buffer::{Allocator, Buffer};
use crate::error::MorunaError;
use crate::tier::Tier;

fn bad(msg: impl Into<String>) -> MorunaError {
    MorunaError::Io {
        op: "ipc",
        target: "record".into(),
        msg: msg.into(),
    }
}

/// How the body buffers of a written record are compressed (08 e.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// Not at all: the page-aligned layout of e.7.
    #[default]
    None,
    /// Zstandard frames at this level.
    Zstd(i32),
    /// LZ4 frames.
    Lz4,
}

impl Compression {
    /// Zstandard at the level the sink uses when none is given.
    pub const ZSTD_DEFAULT_LEVEL: i32 = 3;

    /// The compression a document or a Python call names: `none`, `zstd` (at `level`, or at
    /// [`Compression::ZSTD_DEFAULT_LEVEL`]) or `lz4`. A level is zstd's only, and zstd takes the
    /// levels its library does; anything else is a sentence naming what is wrong.
    pub fn parse(name: &str, level: Option<i32>) -> Result<Compression, String> {
        match (name, level) {
            ("none", None) => Ok(Compression::None),
            ("lz4", None) => Ok(Compression::Lz4),
            ("none" | "lz4", Some(level)) => Err(format!(
                "level {level} given with `{name}`; a level is for `zstd`"
            )),
            ("zstd", level) => {
                let level = level.unwrap_or(Compression::ZSTD_DEFAULT_LEVEL);
                let range = zstd::compression_level_range();
                if range.contains(&level) {
                    Ok(Compression::Zstd(level))
                } else {
                    Err(format!(
                        "zstd level {level}; zstd takes levels from {} to {}",
                        range.start(),
                        range.end()
                    ))
                }
            }
            (other, _) => Err(format!(
                "unknown compression `{other}` (use \"none\", \"zstd\" or \"lz4\")"
            )),
        }
    }

    /// The name [`Compression::parse`] reads for this compression.
    pub fn name(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Zstd(_) => "zstd",
            Compression::Lz4 => "lz4",
        }
    }

    /// This compression, or the refusal of a zstd level the library does not take: a sink asks
    /// when it is built, so a bad level is the run's error before any file is opened and not
    /// the first write's.
    pub fn checked(self) -> crate::Result<Compression> {
        self.options().map(|_| self)
    }

    /// The options arrow-ipc writes this compression with: 8-byte alignment, metadata V5, and the
    /// codec and level named. A level outside zstd's range is arrow-ipc's refusal.
    fn options(self) -> crate::Result<IpcWriteOptions> {
        let options = IpcWriteOptions::try_new(8, false, MetadataVersion::V5)
            .map_err(|e| bad(format!("write options: {e}")))?;
        let refused = |e: arrow::error::ArrowError| bad(format!("compression: {e}"));
        match self {
            Compression::None => Ok(options),
            Compression::Zstd(level) => options
                .try_with_compression(Some(CompressionType::ZSTD))
                .and_then(|o| o.try_with_compression_level(Some(level)))
                .map_err(refused),
            Compression::Lz4 => options
                .try_with_compression(Some(CompressionType::LZ4_FRAME))
                .map_err(refused),
        }
    }
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
    encode(
        batch,
        page_bytes,
        IpcWriteOptions::default(),
        base_offset,
        alloc,
    )
}

/// Encode one record batch with each body buffer compressed (`LZ4_FRAME` or `ZSTD`, method
/// `BUFFER`), in the 8-aligned layout the format itself has: each buffer is its uncompressed
/// length, as 8 little-endian bytes, then its compressed bytes, or -1 and the buffer as it is
/// when compression did not shrink it. `base_offset` is a multiple of 8, and the result is as
/// `encode_framing`'s, except that the buffers are not the batch's own (they are the compressed
/// bytes) and sit at 8-byte boundaries. `Compression::None` is refused: use `encode_framing`.
pub fn encode_compressed(
    batch: &RecordBatch,
    compression: Compression,
    base_offset: u64,
    alloc: &dyn Allocator,
) -> crate::Result<(Buffer, Vec<(usize, ArrowBuffer)>)> {
    if compression == Compression::None {
        return Err(bad(
            "encode_compressed is given no compression; encode_framing writes that",
        ));
    }
    encode(batch, 8, compression.options()?, base_offset, alloc)
}

fn encode(
    batch: &RecordBatch,
    page_bytes: usize,
    options: IpcWriteOptions,
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
    let mut encoder = StreamEncoder::try_new_with_options(&schema, options)
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
                framing_len = framing_target(&framing).map_err(bad)?;
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
    let rb_start = message_len(&framing).map_err(bad)? + 8;
    let entries = buffer_entries(&framing[rb_start..]).map_err(bad)?;

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

    // The page-aligned layout (e.7, KA11): the framing is page-rounded, every body buffer starts
    // at the next page boundary after the previous one, and the message's entries and body length
    // are rewritten to match, in place.
    let framing_padded = round_up(framing_len as u64, page);
    let lengths: Vec<u64> = bodies.iter().map(|b| b.len() as u64).collect();
    let offsets = frame::page_layout(&mut framing, &lengths, page, base_offset).map_err(bad)?;
    let mut placed = Vec::with_capacity(bodies.len());
    for (absolute, body) in offsets.into_iter().zip(bodies) {
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
    let schema_len = message_len(bytes).map_err(bad)?;
    let schema_message = root_as_message(&bytes[8..8 + schema_len])
        .map_err(|e| bad(format!("schema message: {e}")))?;
    // A staging record is never compressed, so nothing may be inflated for it: a compressed one
    // is refused by the check with a ceiling of zero.
    let inflate = Inflate::none();
    let nothing = ArrowBuffer::from_vec(Vec::<u8>::new());
    let Checked::Schema(schema_fb) =
        check(&schema_message, &nothing, None, &inflate).map_err(bad)?
    else {
        return Err(bad("the first message is not a schema"));
    };
    let schema = std::sync::Arc::new(arrow::ipc::convert::fb_to_schema(schema_fb));
    let rb_start = 8 + schema_len;
    let rb_len = message_len(&bytes[rb_start..]).map_err(bad)?;
    let rb_message = root_as_message(&bytes[rb_start + 8..rb_start + 8 + rb_len])
        .map_err(|e| bad(format!("record batch message: {e}")))?;
    let version = match rb_message.version() {
        MetadataVersion::V4 => MetadataVersion::V4,
        MetadataVersion::V5 => MetadataVersion::V5,
        other => return Err(bad(format!("unsupported IPC metadata version {other:?}"))),
    };
    let body_start = (rb_start + 8 + rb_len) as u64;
    // CT-I13: the body is the rest of the record, as the decoder reads it below.
    let body = buf.slice(body_start as usize);
    let Checked::Batch(passed) = check(&rb_message, &body, Some(&schema), &inflate).map_err(bad)?
    else {
        return Err(bad("the second message is not a record batch"));
    };
    for (offset, length) in buffer_entries(&bytes[rb_start..rb_start + 8 + rb_len]).map_err(bad)? {
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
    let dictionaries: HashMap<i64, arrow::array::ArrayRef> = HashMap::new();
    passed
        .with_batch(&body, |record_batch, body| {
            RecordBatchDecoder::try_new(body, record_batch, schema, &dictionaries, &version)
                .and_then(|d| d.read_record_batch())
                .map_err(|e| bad(format!("decode: {e}")))
        })
        .map_err(bad)?
}

/// The schema an IPC file's footer holds: checked as a schema message is (CT-I13), then
/// converted. The footer is untrusted input like every other byte of the file, and arrow-ipc's
/// converter panics on a type it cannot map.
pub fn footer_schema(
    schema: arrow::ipc::Schema<'_>,
) -> Result<arrow::datatypes::SchemaRef, String> {
    check::check_schema(schema)?;
    Ok(std::sync::Arc::new(arrow::ipc::convert::fb_to_schema(
        schema,
    )))
}

/// The tier a framing buffer is allocated in: the run's one host tier (e.1).
fn host_tier(alloc: &dyn Allocator) -> Tier {
    if alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}
