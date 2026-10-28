//! Compressed Arrow IPC record batches, inflated within a ceiling (contracts e.8).
//!
//! A record batch whose message sets `compression` (codec `LZ4_FRAME` or `ZSTD`, method `BUFFER`)
//! stores each body buffer as an 8-byte little-endian uncompressed length followed by the
//! compressed bytes, or as -1 followed by the bytes themselves when the buffer did not shrink.
//! arrow-ipc reserves whatever length a buffer declares before it decompresses it, so the
//! lengths are untrusted. Here every buffer's size is worked out from the message alone before
//! a byte is allocated for the batch:
//!
//! | buffer | expected uncompressed size |
//! |---|---|
//! | validity bitmap | `ceil(length / 8)`, or none |
//! | fixed-width values, dictionary keys, views, type ids, union offsets, sizes | `length x width` |
//! | boolean values | `ceil(length / 8)` |
//! | offsets of a list or a map | `(length + 1) x width` (none for no values) |
//! | offsets of a string or binary column | the same, inflated and checked first: the first is 0 and none is below the one before |
//! | data of a string or binary column | the last of those offsets |
//! | data buffers of a view column | stated by no one: bounded by the ceiling alone |
//!
//! A writer may pad: Arrow C++ writes a sliced array's buffers up to the next multiple of 8 and
//! the data of a string or binary column up to the next multiple of 64, so those two sizes are
//! also allowed (and, since every other size is a lie, nothing between them and the next
//! multiple is refused either). A declared size outside what is allowed is refused, naming the
//! column and the buffer, and so is a batch whose sizes (each placed at a multiple of 64)
//! together pass the ceiling. Only then is the body allocated, once, from the arena, and each
//! buffer inflated into its place with the output bound at its declared size: output past it, or
//! short of it, is a refusal. The result is an uncompressed message and body that arrow-ipc
//! reads as it reads any other.

use std::io::Read;

use arrow::buffer::Buffer as ArrowBuffer;
use arrow::datatypes::{DataType, Field};
use arrow::ipc;

use super::check::Span;
use super::host_tier;
use crate::ALIGNMENT;
use crate::buffer::{AllocStats, Allocator, Buffer};
use crate::error::MorunaError;
use crate::tier::Tier;

/// The ceiling of a reader whose arena states no budget: 1 GiB for one batch.
pub const UNBUDGETED_CEILING: u64 = 1 << 30;

/// The bytes before a compressed buffer's data: its uncompressed length.
const PREFIX: usize = 8;

/// The length that marks a buffer stored as it is.
const STORED: i64 = -1;

/// What a reader may spend on inflating one batch: the arena it allocates the inflated body from,
/// and the most bytes that body (with the offsets set aside to size its string data) may take.
/// The caller passes the stage's share of its memory budget as the ceiling.
#[derive(Clone, Copy)]
pub struct Inflate<'a> {
    alloc: &'a dyn Allocator,
    ceiling: u64,
}

impl<'a> Inflate<'a> {
    /// Inflation from `alloc`, up to `ceiling` bytes for one batch.
    pub fn new(alloc: &'a dyn Allocator, ceiling: u64) -> Inflate<'a> {
        Inflate { alloc, ceiling }
    }

    /// Half of what the arena's host tier can still give (the other half is for what the reader
    /// does with the batch), or [`UNBUDGETED_CEILING`] when the arena states no budget.
    pub fn share(alloc: &'a dyn Allocator) -> Inflate<'a> {
        let ceiling = alloc
            .available(host_tier(alloc))
            .map_or(UNBUDGETED_CEILING, |free| free / 2);
        Inflate { alloc, ceiling }
    }

    /// No inflation at all: a ceiling of zero over an arena that has nothing. A reader of
    /// streams Moruna wrote itself (a spill file, the trace) passes this, and a compressed batch
    /// in one is refused with the ceiling as the reason.
    pub fn none() -> Inflate<'static> {
        static NO_ARENA: NoArena = NoArena;
        Inflate {
            alloc: &NO_ARENA,
            ceiling: 0,
        }
    }

    /// The most bytes one batch may take to inflate.
    pub fn ceiling(&self) -> u64 {
        self.ceiling
    }
}

/// An arena that has no memory: every allocation is refused.
pub struct NoArena;

impl Allocator for NoArena {
    fn alloc(&self, bytes: usize, tier: Tier) -> crate::Result<Buffer> {
        Err(MorunaError::Alloc {
            bytes: bytes as u64,
            tier,
            budget: 0,
            in_use: 0,
        })
    }

    fn page_bytes(&self) -> usize {
        4096
    }

    fn stats(&self) -> AllocStats {
        AllocStats::default()
    }

    fn is_pinned(&self) -> bool {
        false
    }
}

/// The size a buffer must have, from the message and what is already inflated.
#[derive(Clone, Copy, Debug)]
pub(super) enum Rule {
    /// No size is stated: the ceiling bounds it.
    Free,
    /// Exactly this many bytes.
    Exact(u64),
    /// A validity bitmap of `len` bits, or none.
    Validity(usize),
    /// The offsets of a list: `len + 1` of `width` bytes, or none for no values.
    Offsets { len: usize, width: usize },
    /// The offsets of a string or binary column: as `Offsets`, inflated and checked, and their
    /// last states the size of the data after them.
    Text { len: usize, width: usize },
    /// The data of the string or binary column whose offsets are in `slot`.
    Data { offsets: usize },
}

impl Rule {
    /// `len` values of `width` bytes.
    pub(super) fn values(len: usize, width: usize) -> Rule {
        Rule::Exact((len as u64).saturating_mul(width as u64))
    }

    /// The values buffer of a column of `data_type` and `len` values.
    pub(super) fn of_values(data_type: &DataType, len: usize) -> Rule {
        match data_type {
            DataType::Boolean => Rule::Exact((len as u64).div_ceil(8)),
            DataType::FixedSizeBinary(width) => {
                Rule::values(len, usize::try_from(*width).unwrap_or(usize::MAX))
            }
            other => other
                .primitive_width()
                .map_or(Rule::Free, |width| Rule::values(len, width)),
        }
    }
}

/// How a buffer is stored in the body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
    /// No bytes: an empty buffer.
    Empty,
    /// After the length, the bytes as they are.
    Stored,
    /// After the length, a frame of the codec.
    Coded,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    Lz4,
    Zstd,
}

/// One buffer of the plan.
struct Slot {
    /// The column and the buffer it is, for a refusal.
    column: String,
    what: &'static str,
    /// Where the stored buffer starts in the message's body, and its length.
    at: usize,
    stored: usize,
    form: Form,
    /// The uncompressed size, held to the expected one.
    size: u64,
    /// For the offsets of a string or binary column: the size of the data after them.
    data: u64,
    /// For those offsets: the inflated bytes, kept to sit in the body without a second inflation.
    offsets: Option<Buffer>,
}

/// What a compressed message becomes: the other half of [`Wrap`].
#[derive(Clone, Copy, Debug)]
pub(super) enum Wrap {
    Batch,
    Dictionary { id: i64, is_delta: bool },
}

/// The plan to inflate one batch, built while the walk checks the message: every buffer taken
/// with its expected size, and the bytes counted against the ceiling.
pub(super) struct Packed<'a, 'i> {
    codec: Codec,
    body: &'a [u8],
    inflate: Inflate<'i>,
    slots: Vec<Slot>,
    /// The body so far, each buffer placed at a multiple of 64.
    used: u64,
    /// The inflated offsets set aside to size the data after them.
    set_aside: u64,
    zstd: Option<zstd::bulk::Decompressor<'static>>,
}

impl<'a, 'i> Packed<'a, 'i> {
    /// The plan for a batch compressed as `compression` says, or the refusal of a codec or a
    /// method Moruna does not read.
    pub(super) fn new(
        compression: ipc::BodyCompression<'_>,
        body: &'a [u8],
        version: ipc::MetadataVersion,
        inflate: Inflate<'i>,
    ) -> Result<Packed<'a, 'i>, String> {
        if version < ipc::MetadataVersion::V5 {
            return Err(format!(
                "the record batch is compressed and its metadata version is {version:?}; \
                 compression needs V5"
            ));
        }
        if compression.method() != ipc::BodyCompressionMethod::BUFFER {
            return Err(format!(
                "the record batch is compressed by method {:?}, and Moruna reads BUFFER only",
                compression.method()
            ));
        }
        let codec = match compression.codec() {
            ipc::CompressionType::LZ4_FRAME => Codec::Lz4,
            ipc::CompressionType::ZSTD => Codec::Zstd,
            other => {
                return Err(format!(
                    "the record batch is compressed with {other:?}, and Moruna reads LZ4_FRAME \
                     and ZSTD only"
                ));
            }
        };
        Ok(Packed {
            codec,
            body,
            inflate,
            slots: Vec::new(),
            used: 0,
            set_aside: 0,
            zstd: None,
        })
    }

    /// Take the buffer at `entry` for the column `field`: its declared size held to the one
    /// `rule` gives and to what is left of the ceiling, then planned. A string column's offsets
    /// are inflated now, because the size of its data is their last. Nothing is allocated for a
    /// buffer that is refused.
    pub(super) fn take(
        &mut self,
        field: &Field,
        what: &'static str,
        entry: Span,
        rule: Rule,
    ) -> Result<Span, String> {
        let name = field.name().to_string();
        let name = name.as_str();
        let bytes = self
            .body
            .get(entry.offset..entry.offset + entry.len)
            .ok_or_else(|| format!("column {name:?} has a {what} buffer outside the body"))?;
        let (form, size) = declared(name, what, bytes)?;

        // The sizes the message allows: the one it implies, and up to the next multiple of 8
        // (Arrow C++ writes a sliced array's buffers padded that far) or, for the data of a
        // string or binary column, of 64 (it pads those to its IPC alignment); and whether
        // none at all is also right. A writer that is exact (arrow-rs) and one that pads are
        // both read, and a size anywhere else is a lie.
        let (allowed, or_none) = match rule {
            Rule::Free => (None, false),
            Rule::Exact(n) => (Some((n, round_up(n, 8))), false),
            Rule::Validity(len) => {
                let n = (len as u64).div_ceil(8);
                (Some((n, round_up(n, 8))), true)
            }
            Rule::Offsets { len, width } | Rule::Text { len, width } => {
                let n = (len as u64).saturating_add(1).saturating_mul(width as u64);
                (Some((n, round_up(n, 8))), len == 0)
            }
            Rule::Data { offsets } => {
                let slot = self.slots.get(offsets).ok_or_else(|| {
                    format!("column {name:?} has a {what} buffer with no offsets before it")
                })?;
                (Some((slot.data, round_up(slot.data, 64))), false)
            }
        };
        if let Some((low, high)) = allowed
            && !(low..=high).contains(&size)
            && !(or_none && size == 0)
        {
            let implied = if low == high {
                format!("{low}")
            } else {
                format!("{low} to {high}")
            };
            return Err(format!(
                "column {name:?} has a {what} buffer that declares {size} uncompressed bytes \
                 where the message implies {implied}"
            ));
        }

        let placed = round_up(size, ALIGNMENT as u64);
        let aside = if matches!(rule, Rule::Text { .. }) {
            size
        } else {
            0
        };
        let held = self.used.saturating_add(self.set_aside);
        let left = self.inflate.ceiling.saturating_sub(held);
        if placed.saturating_add(aside) > left {
            return Err(format!(
                "column {name:?} has a {what} buffer of {size} uncompressed bytes, and {left} of \
                 the {} bytes this reader may inflate for one batch are left (the batch has \
                 {held} planned)",
                self.inflate.ceiling
            ));
        }

        let mut slot = Slot {
            column: name.to_string(),
            what,
            at: entry.offset,
            stored: entry.len,
            form,
            size,
            data: 0,
            offsets: None,
        };
        if let Rule::Text { width, .. } = rule {
            self.check_working_set(name, what, form, bytes, left.saturating_sub(aside))?;
            if size > 0 {
                let tier = host_tier(self.inflate.alloc);
                let len = usize::try_from(size).map_err(|_| {
                    format!("column {name:?} has {what} of {size} bytes, more than fits in memory")
                })?;
                let mut buffer = self
                    .inflate
                    .alloc
                    .alloc(len, tier)
                    .map_err(|e| format!("column {name:?} {what}: {e}"))?;
                self.fill(name, what, form, bytes, &mut buffer[..len])?;
                // Only the offsets themselves, not the padding after them.
                let count = usize::try_from(allowed.map_or(size, |(low, _)| low)).unwrap_or(len);
                slot.data = last_offset(&buffer[..count.min(len)], width)
                    .map_err(|e| format!("column {name:?} has {what} that {e}"))?;
                slot.offsets = Some(buffer);
            }
        } else {
            self.check_working_set(name, what, form, bytes, left)?;
        }
        self.used = self.used.saturating_add(placed);
        self.set_aside = self.set_aside.saturating_add(aside);
        self.slots.push(slot);
        Ok(Span {
            offset: entry.offset,
            len: usize::try_from(size).unwrap_or(usize::MAX),
            slot: self.slots.len() - 1,
        })
    }

    /// For an LZ4 frame, what its decoder will hold beyond the output, from the frame's own
    /// header: it is the attacker's to choose (up to 4 MiB a block), so it is counted too.
    fn check_working_set(
        &self,
        name: &str,
        what: &str,
        form: Form,
        bytes: &[u8],
        left: u64,
    ) -> Result<(), String> {
        if self.codec != Codec::Lz4 || form != Form::Coded {
            return Ok(());
        }
        let working = lz4_working_set(&bytes[PREFIX..])
            .map_err(|e| format!("column {name:?} has a {what} buffer that {e}"))?;
        if working > left {
            return Err(format!(
                "column {name:?} has a {what} buffer whose LZ4 frame takes {working} bytes to \
                 inflate, and {left} of the {} bytes this reader may inflate for one batch are \
                 left",
                self.inflate.ceiling
            ));
        }
        Ok(())
    }

    /// Inflate one stored buffer into `into`, which is its expected size.
    fn fill(
        &mut self,
        name: &str,
        what: &str,
        form: Form,
        bytes: &[u8],
        into: &mut [u8],
    ) -> Result<(), String> {
        let at = |e: String| format!("column {name:?} has a {what} buffer that {e}");
        match form {
            Form::Empty => Ok(()),
            Form::Stored => {
                into.copy_from_slice(&bytes[PREFIX..]);
                Ok(())
            }
            Form::Coded => match self.codec {
                Codec::Lz4 => lz4(&bytes[PREFIX..], into).map_err(at),
                Codec::Zstd => {
                    if self.zstd.is_none() {
                        self.zstd =
                            Some(zstd::bulk::Decompressor::new().map_err(|e| {
                                at(format!("cannot start a zstd decompressor: {e}"))
                            })?);
                    }
                    match self.zstd.as_mut() {
                        Some(decompressor) => {
                            zstd(decompressor, &bytes[PREFIX..], into).map_err(at)
                        }
                        None => Err(at("has no zstd decompressor".into())),
                    }
                }
            },
        }
    }

    /// Allocate the body once, inflate every buffer into its place, and describe the result in
    /// a message of its own. `batch` is the compressed message's record batch: its row count,
    /// nodes and variadic counts carry over.
    pub(super) fn finish(
        mut self,
        batch: ipc::RecordBatch<'_>,
        version: ipc::MetadataVersion,
        wrap: Wrap,
    ) -> Result<Inflated, String> {
        let total = usize::try_from(self.used)
            .map_err(|_| format!("{} inflated bytes do not fit in memory", self.used))?;
        let mut body = if total == 0 {
            None
        } else {
            Some(
                self.inflate
                    .alloc
                    .alloc(total, host_tier(self.inflate.alloc))
                    .map_err(|e| format!("the inflated record batch of {total} bytes: {e}"))?,
            )
        };
        let slots = std::mem::take(&mut self.slots);
        let mut entries = Vec::with_capacity(slots.len());
        let mut at = 0usize;
        for slot in &slots {
            let size = usize::try_from(slot.size).unwrap_or(0);
            entries.push(ipc::Buffer::new(at as i64, size as i64));
            if let (Some(body), true) = (body.as_mut(), size > 0) {
                let into = body
                    .get_mut(at..at + size)
                    .ok_or("an inflated buffer is outside the body it was planned in")?;
                match &slot.offsets {
                    Some(offsets) => into.copy_from_slice(&offsets[..size]),
                    None => {
                        let stored = self
                            .body
                            .get(slot.at..slot.at + slot.stored)
                            .ok_or("a compressed buffer is outside the body")?;
                        self.fill(&slot.column, slot.what, slot.form, stored, into)?;
                    }
                }
            }
            at += usize::try_from(round_up(slot.size, ALIGNMENT as u64)).unwrap_or(0);
        }
        drop(slots);
        let body = match body {
            Some(body) => body.into_arrow_buffer().map_err(|e| e.to_string())?,
            None => ArrowBuffer::from_vec(Vec::<u8>::new()),
        };

        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let nodes: Vec<ipc::FieldNode> = batch
            .nodes()
            .map(|nodes| nodes.iter().copied().collect())
            .unwrap_or_default();
        let nodes = fbb.create_vector(&nodes);
        let buffers = fbb.create_vector(&entries);
        let counts: Vec<i64> = batch
            .variadicBufferCounts()
            .map(|counts| counts.iter().collect())
            .unwrap_or_default();
        let counts = (!counts.is_empty()).then(|| fbb.create_vector(&counts));
        let record_batch = {
            let mut builder = ipc::RecordBatchBuilder::new(&mut fbb);
            builder.add_length(batch.length());
            builder.add_nodes(nodes);
            builder.add_buffers(buffers);
            if let Some(counts) = counts {
                builder.add_variadicBufferCounts(counts);
            }
            builder.finish()
        };
        let (kind, header) = match wrap {
            Wrap::Batch => (
                ipc::MessageHeader::RecordBatch,
                record_batch.as_union_value(),
            ),
            Wrap::Dictionary { id, is_delta } => {
                let mut builder = ipc::DictionaryBatchBuilder::new(&mut fbb);
                builder.add_id(id);
                builder.add_data(record_batch);
                builder.add_isDelta(is_delta);
                (
                    ipc::MessageHeader::DictionaryBatch,
                    builder.finish().as_union_value(),
                )
            }
        };
        let message = {
            let mut builder = ipc::MessageBuilder::new(&mut fbb);
            builder.add_version(version);
            builder.add_header_type(kind);
            builder.add_header(header);
            builder.add_bodyLength(self.used as i64);
            builder.finish()
        };
        fbb.finish(message, None);
        Ok(Inflated {
            message: fbb.finished_data().to_vec(),
            body,
        })
    }
}

/// A compressed message after inflation: the message that describes the inflated body, and the
/// body, in a buffer of the arena.
pub struct Inflated {
    message: Vec<u8>,
    body: ArrowBuffer,
}

impl std::fmt::Debug for Inflated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inflated")
            .field("message", &self.message.len())
            .field("body", &self.body.len())
            .finish()
    }
}

impl Inflated {
    /// The uncompressed message, parsed.
    pub(super) fn message(&self) -> Result<ipc::Message<'_>, String> {
        ipc::root_as_message(&self.message)
            .map_err(|e| format!("the inflated message does not parse: {e}"))
    }

    /// The inflated body, which the message's buffers are offsets into.
    pub(super) fn body(&self) -> &ArrowBuffer {
        &self.body
    }
}

fn round_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to).saturating_mul(to)
}

/// How a buffer is stored and the uncompressed size it declares, from its stored bytes.
fn declared(name: &str, what: &str, bytes: &[u8]) -> Result<(Form, u64), String> {
    if bytes.is_empty() {
        return Ok((Form::Empty, 0));
    }
    let Some(prefix) = bytes.first_chunk::<PREFIX>() else {
        return Err(format!(
            "column {name:?} has a compressed {what} buffer of {} bytes, which cannot hold its \
             8-byte length",
            bytes.len()
        ));
    };
    match i64::from_le_bytes(*prefix) {
        STORED => Ok((Form::Stored, (bytes.len() - PREFIX) as u64)),
        0 => Ok((Form::Empty, 0)),
        n if n > 0 => Ok((Form::Coded, n as u64)),
        n => Err(format!(
            "column {name:?} has a {what} buffer that declares an uncompressed length of {n}"
        )),
    }
}

/// The last of `bytes` read as little-endian offsets of `width` bytes, which must start at 0 and
/// never fall: the size of the data they index.
fn last_offset(bytes: &[u8], width: usize) -> Result<u64, String> {
    let mut previous = 0i64;
    for (index, chunk) in bytes.chunks_exact(width).enumerate() {
        let value = match *chunk {
            [a, b, c, d] => i64::from(i32::from_le_bytes([a, b, c, d])),
            [a, b, c, d, e, f, g, h] => i64::from_le_bytes([a, b, c, d, e, f, g, h]),
            _ => return Err(format!("are {width}-byte offsets of no known width")),
        };
        if index == 0 && value != 0 {
            return Err(format!("start at {value}, not 0"));
        }
        if value < previous {
            return Err(format!(
                "are not monotonic: offset {index} is {value} after {previous}"
            ));
        }
        previous = value;
    }
    u64::try_from(previous).map_err(|_| format!("end at {previous}"))
}

fn short(got: usize, declared: usize) -> String {
    format!("inflates short of the {declared} bytes declared for it: {got} came out")
}

fn past(declared: usize) -> String {
    format!("inflates past the {declared} bytes declared for it")
}

/// What an LZ4 frame's decoder holds besides its output, from the frame header: the source and
/// destination blocks of the block size the header names (the destination keeps a 64 KiB window
/// too when the blocks are linked).
fn lz4_working_set(frame: &[u8]) -> Result<u64, String> {
    const MAGIC: [u8; 4] = [0x04, 0x22, 0x4D, 0x18];
    let Some([m0, m1, m2, m3, flags, descriptor]) = frame.first_chunk::<6>().copied() else {
        return Err("is not an LZ4 frame: it ends inside the frame header".into());
    };
    if [m0, m1, m2, m3] != MAGIC {
        return Err("is not an LZ4 frame: it does not start with the frame magic".into());
    }
    if flags >> 6 != 1 {
        return Err(format!(
            "is an LZ4 frame of version {}, and Moruna reads version 1",
            flags >> 6
        ));
    }
    let block: u64 = match (descriptor >> 4) & 7 {
        4 => 64 << 10,
        5 => 256 << 10,
        6 => 1 << 20,
        7 => 4 << 20,
        other => return Err(format!("is an LZ4 frame with the block size id {other}")),
    };
    let independent = flags & 0x20 != 0;
    Ok(if independent {
        2 * block
    } else {
        3 * block + (64 << 10)
    })
}

/// Inflate an LZ4 frame into exactly `into`.
fn lz4(frame: &[u8], into: &mut [u8]) -> Result<(), String> {
    let mut decoder = lz4_flex::frame::FrameDecoder::new(frame);
    let mut filled = 0;
    while filled < into.len() {
        match decoder.read(&mut into[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(format!("does not inflate: {e}")),
        }
    }
    if filled < into.len() {
        return Err(short(filled, into.len()));
    }
    // One more byte, to learn whether the frame has more than was declared (and to read its end
    // mark and checksum).
    let mut probe = [0u8; 1];
    match decoder.read(&mut probe) {
        Ok(0) => Ok(()),
        Ok(_) => Err(past(into.len())),
        Err(e) => Err(format!("does not inflate: {e}")),
    }
}

/// Inflate a zstd frame into exactly `into`.
fn zstd(
    decompressor: &mut zstd::bulk::Decompressor<'static>,
    frame: &[u8],
    into: &mut [u8],
) -> Result<(), String> {
    // A frame usually says what it holds; a wrong figure is refused before anything inflates.
    match zstd::zstd_safe::get_frame_content_size(frame) {
        Ok(Some(held)) if held > into.len() as u64 => return Err(past(into.len())),
        Ok(Some(held)) if held < into.len() as u64 => {
            return Err(short(held as usize, into.len()));
        }
        Ok(_) => {}
        Err(_) => return Err("is not a zstd frame".into()),
    }
    match decompressor.decompress_to_buffer(frame, into) {
        Ok(n) if n == into.len() => Ok(()),
        Ok(n) => Err(short(n, into.len())),
        Err(e) if e.to_string().contains("too small") => Err(past(into.len())),
        Err(e) => Err(format!("does not inflate: {e}")),
    }
}
