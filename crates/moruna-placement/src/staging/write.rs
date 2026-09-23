//! Demotion to disk: laying a payload out as a segment record (e.3) and the pieces the
//! reactor writes (f.5). Nothing here copies payload bytes: the pieces are the payload's own
//! buffers, and the only bytes the engine writes with the CPU are the two metadata pages
//! (the record header and the Arrow IPC framing or the `MRB1` header), which are not payload
//! (G-I2, PL-I4).

use std::sync::Arc;

use moruna_kernel::arrow::buffer::Buffer as ArrowBuffer;
use moruna_kernel::{Allocator, MorunaError, Buffer, BufferView, ManagedTensor, Tier, mrb1, ipc};

use crate::state::PayloadRef;

use super::segment::round_up;

/// Where one piece of a record's bytes comes from. Each variant can build a fresh
/// `BufferView` as many times as a retry needs (f.10), because it owns a shared handle on
/// the bytes rather than the view itself.
#[derive(Clone)]
pub enum PieceSrc {
    /// A metadata page the engine allocated from the arena.
    Arena(Arc<Buffer>),
    /// One Arrow buffer of the batch.
    Arrow(ArrowBuffer),
    /// The tensor's contiguous bytes.
    Tensor(Arc<ManagedTensor>),
}

/// One page-aligned piece of a record, and where in the file it goes.
#[derive(Clone)]
pub struct Piece {
    /// The bytes.
    pub src: PieceSrc,
    /// Absolute offset in the segment file.
    pub offset: u64,
    /// Bytes to write, as is (e.3).
    pub len: u64,
}

impl Piece {
    /// A DMA source over this piece's bytes. Every constructor is safe (contracts d.3).
    pub fn view(&self, alloc: &dyn Allocator) -> moruna_kernel::Result<BufferView> {
        match &self.src {
            PieceSrc::Arena(buffer) => Ok(buffer.view()),
            PieceSrc::Arrow(buffer) => BufferView::of_arrow(buffer, alloc),
            PieceSrc::Tensor(tensor) => BufferView::of_tensor(tensor),
        }
    }
}

/// A record laid out but not yet placed in a segment: the offsets are relative to the body,
/// which is where `encode_framing` puts them for any page-aligned base (contracts e.7).
pub struct RecordDraft {
    /// The metadata page: the IPC framing for a table, the `MRB1` header for a tensor.
    pub meta: Buffer,
    /// Bytes of `meta` that belong to the record.
    pub meta_len: u64,
    /// Each body buffer with its offset from the start of the body.
    pub bodies: Vec<(u64, PieceSrc)>,
    /// Bytes of the body, from its first byte to the end of its last piece (e.3).
    pub payload_len: u64,
}

impl RecordDraft {
    /// The whole record: the header page plus the page-padded body (e.3).
    pub fn total(&self, page: u64) -> u64 {
        page + round_up(self.payload_len, page)
    }
}

/// Lay a payload out as a record body. The Arrow encoder is given base offset 0: every
/// buffer entry it rewrites is relative to the body, so the framing it produces is valid at
/// any page-aligned place in the file and the record can be sized before a segment is
/// chosen (e.7, f.5).
pub fn draft(
    payload: &PayloadRef,
    page: u64,
    alloc: &dyn Allocator,
) -> moruna_kernel::Result<RecordDraft> {
    match payload {
        PayloadRef::Table(batch) => {
            let (framing, placed) = ipc::encode_framing(batch, page as usize, 0, alloc)?;
            let framing_len = framing.len() as u64;
            let mut bodies = Vec::with_capacity(placed.len());
            let mut end = framing_len;
            for (offset, buffer) in placed {
                let offset = offset as u64;
                end = end.max(offset + buffer.len() as u64);
                bodies.push((offset, arena_backed(buffer, alloc)?));
            }
            Ok(RecordDraft {
                meta: framing,
                meta_len: framing_len,
                bodies,
                payload_len: end,
            })
        }
        PayloadRef::Tensor(tensor) => {
            if !tensor.is_contiguous() {
                return Err(MorunaError::Convert(
                    moruna_kernel::ConvertError::NotContiguous,
                ));
            }
            let shape = tensor.shape().to_vec();
            let data_offset = mrb1::Header::data_offset_for(shape.len(), page)?;
            let header = mrb1::Header {
                dtype: tensor.dtype(),
                shape,
                data_offset,
            };
            let mut meta = alloc.alloc(data_offset as usize, host_tier(alloc))?;
            header.write(&mut meta)?;
            let byte_len = tensor.byte_len();
            Ok(RecordDraft {
                meta,
                meta_len: data_offset,
                bodies: vec![(data_offset, PieceSrc::Tensor(Arc::clone(tensor)))],
                payload_len: data_offset + byte_len,
            })
        }
    }
}

/// The pieces of a record placed at `record_start`: the header page, the metadata page, and
/// each body buffer at its page-aligned offset (e.3).
pub fn pieces(draft: RecordDraft, header: Buffer, record_start: u64, page: u64) -> Vec<Piece> {
    let body_offset = record_start + page;
    let header_len = header.len() as u64;
    let mut out = Vec::with_capacity(draft.bodies.len() + 2);
    out.push(Piece {
        src: PieceSrc::Arena(Arc::new(header)),
        offset: record_start,
        len: header_len,
    });
    out.push(Piece {
        src: PieceSrc::Arena(Arc::new(draft.meta)),
        offset: body_offset,
        len: draft.meta_len,
    });
    for (offset, src) in draft.bodies {
        let len = match &src {
            PieceSrc::Arena(buffer) => buffer.len() as u64,
            PieceSrc::Arrow(buffer) => buffer.len() as u64,
            PieceSrc::Tensor(tensor) => tensor.byte_len(),
        };
        if len == 0 {
            // An empty Arrow buffer (a validity buffer of a batch with no nulls) carries no
            // bytes and has no arena provenance to build a view over; the record's layout
            // already accounts for it as a zero-length entry (e.7).
            continue;
        }
        out.push(Piece {
            src,
            offset: body_offset + offset,
            len,
        });
    }
    out
}

/// A body buffer as a DMA source. A buffer the arena owns is written straight from where it
/// is; one the Arrow IPC encoder allocated for itself is not addressable through a safe
/// `BufferView` (contracts d.3 requires arena provenance), so its bytes are brought into an
/// arena buffer once and the copy is declared through `Allocator::note_payload_copy`.
///
/// In arrow 59.3.0 the encoder always writes its own validity bitmap, even for a column with
/// no nulls and even when the array carries an arena-backed one, so 09 e.3's "the pieces are
/// the batch's own buffers" holds for the data buffers and not for the bitmaps. Reported as a
/// finding.
fn arena_backed(buffer: ArrowBuffer, alloc: &dyn Allocator) -> moruna_kernel::Result<PieceSrc> {
    if buffer.is_empty() || alloc.contains(buffer.as_ptr()) {
        return Ok(PieceSrc::Arrow(buffer));
    }
    let mut owned = alloc.alloc(buffer.len(), host_tier(alloc))?;
    owned[..buffer.len()].copy_from_slice(buffer.as_slice());
    alloc.note_payload_copy(buffer.len() as u64);
    Ok(PieceSrc::Arena(Arc::new(owned)))
}

/// The run's one host tier as the allocator reports it (contracts e.1).
fn host_tier(alloc: &dyn Allocator) -> Tier {
    if alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}
