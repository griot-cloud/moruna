//! Actual resident payloads exported as the existing native staging record, not another
//! format or a promise to requery a source. Payload copies occur only into explicit charged
//! upload chunks and are reported; metadata/pieces retain their existing owners.
use crate::{
    manifest::DiskRef,
    staging::{
        segment::{HEADER_FIXED, RecordHeader},
        write::{self, Piece},
    },
    state::PayloadRef,
};
use moruna_kernel::{Allocator, Buffer, BufferView, MorunaError, Seq, StageId, StagingCodec, Tier};

/// Exact native one-record segment and the retained source pieces that compose it.
pub struct PortableRecord {
    /// Unique synthetic segment id and actual body offset/length for Placement.restore.
    disk: DiskRef,
    /// Full native segment bytes including header, sparse holes and page-rounded tail.
    segment_bytes: u64,
    /// Existing native staging pieces, sorted by absolute segment offset.
    pieces: Vec<Piece>,
    views: Vec<BufferView>,
}
/// Honest application copy accounting for one bounded upload chunk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Materialized {
    /// Actual bytes copied from retained piece views, including encoded metadata.
    pub copied_bytes: u64,
    /// Actual explicit zero-filled holes/tail, not source data or a reported payload copy.
    pub zero_bytes: u64,
}
/// Build a real native segment using only existing placement/kernel encoders. The caller
/// owns global synthetic segment uniqueness and source snapshot/barrier consistency.
/// During a paused snapshot this allocator MUST consume an already admitted metadata/
/// encoding pool with no underlying allocation fallback or room-making wait. Draft may
/// need encoder-owned validity buffers as well as framing; exhaustion is returned intact
/// so the engine can release its barrier and refuse/retry, never deadlock for memory.
pub fn encode(
    payload: &PayloadRef,
    stage: StageId,
    seq: Seq,
    segment: u32,
    alloc: &dyn Allocator,
    page: u64,
) -> moruna_kernel::Result<PortableRecord> {
    if page < HEADER_FIXED as u64 || !page.is_power_of_two() || usize::try_from(page).is_err() {
        return Err(MorunaError::Staging("portable record page geometry".into()));
    }
    let draft = write::draft(payload, page, alloc)?;
    let rounded = draft
        .payload_len
        .checked_add(page - 1)
        .map(|n| n / page)
        .and_then(|n| n.checked_mul(page))
        .ok_or_else(|| MorunaError::Staging("portable record size overflow".into()))?;
    let segment_bytes = page
        .checked_add(rounded)
        .ok_or_else(|| MorunaError::Staging("portable record size overflow".into()))?;
    let disk = DiskRef {
        segment,
        offset: page,
        len: draft.payload_len,
    };
    let tier = if alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    };
    let mut header = alloc.alloc(page as usize, tier)?;
    if header.host_ptr().is_none() || header.len() != page as usize {
        return Err(MorunaError::Staging(
            "portable record header arena shape".into(),
        ));
    }
    RecordHeader {
        seq,
        stage,
        kind: payload.kind(),
        codec: StagingCodec::Raw,
        payload_len: draft.payload_len,
        body_offset: page,
    }
    .write(header.as_mut())?;
    let mut pieces = write::pieces(draft, header, 0, page);
    pieces.sort_by_key(|piece| piece.offset);
    let mut end = 0;
    let mut views = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        let next = piece
            .offset
            .checked_add(piece.len)
            .ok_or_else(|| MorunaError::Staging("portable record piece overflow".into()))?;
        if piece.offset < end || next > segment_bytes {
            return Err(MorunaError::Staging(
                "portable record overlapping/out-of-range pieces".into(),
            ));
        }
        let view = piece.view(alloc)?;
        if view.as_host_slice().is_none() || view.len() as u64 != piece.len {
            return Err(MorunaError::Staging(
                "portable record needs actual host resident pieces".into(),
            ));
        }
        views.push(view);
        end = next;
    }
    Ok(PortableRecord {
        disk,
        segment_bytes,
        pieces,
        views,
    })
}
impl PortableRecord {
    /// Actual native body reference; caller assigns the globally unique segment id.
    pub fn disk(&self) -> &DiskRef {
        &self.disk
    }
    /// Full page-rounded one-record segment length.
    pub fn segment_bytes(&self) -> u64 {
        self.segment_bytes
    }
    /// Existing immutable piece descriptors in exact segment order.
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }
    /// Existing retained host piece views paired with their native descriptors.
    pub fn piece_views(&self) -> impl Iterator<Item = (&Piece, &BufferView)> {
        self.pieces.iter().zip(&self.views)
    }

    /// Fill one exact caller-owned chunk of the native segment. CPU copies and explicit
    /// zero holes are reported; no payload-sized temporary Vec or hidden staging is used.
    pub fn materialize(
        &self,
        offset: u64,
        destination: &mut Buffer,
    ) -> moruna_kernel::Result<Materialized> {
        self.materialize_prefix(offset, destination.len(), destination)
    }
    /// Fill only the requested prefix of a pre-reserved upload buffer. The unmodified
    /// suffix and full charged allocation stay reusable across records/short final chunks.
    pub fn materialize_prefix(
        &self,
        offset: u64,
        length: usize,
        destination: &mut Buffer,
    ) -> moruna_kernel::Result<Materialized> {
        if length > destination.len() {
            return Err(MorunaError::Staging(
                "portable record chunk capacity".into(),
            ));
        }
        let finish = offset
            .checked_add(length as u64)
            .ok_or_else(|| MorunaError::Staging("portable record chunk overflow".into()))?;
        if finish > self.segment_bytes || destination.host_ptr().is_none() {
            return Err(MorunaError::Staging(
                "portable record charged host chunk range".into(),
            ));
        }
        destination.as_mut()[..length].fill(0);
        let mut copied = 0u64;
        for (piece, view) in self.pieces.iter().zip(&self.views) {
            let end = piece
                .offset
                .checked_add(piece.len)
                .ok_or_else(|| MorunaError::Staging("portable record piece overflow".into()))?;
            if end <= offset {
                continue;
            }
            if piece.offset >= finish {
                break;
            }
            let from = piece.offset.max(offset);
            let to = end.min(finish);
            let bytes = view.as_host_slice().ok_or_else(|| {
                MorunaError::Staging("portable record source is not host resident".into())
            })?;
            let source_start = usize::try_from(from - piece.offset)
                .map_err(|_| MorunaError::Staging("portable record piece width".into()))?;
            let count = usize::try_from(to - from)
                .map_err(|_| MorunaError::Staging("portable record chunk width".into()))?;
            let destination_start = usize::try_from(from - offset)
                .map_err(|_| MorunaError::Staging("portable record destination width".into()))?;
            let source = bytes
                .get(source_start..source_start + count)
                .ok_or_else(|| {
                    MorunaError::Staging("portable record source view changed".into())
                })?;
            destination.as_mut()[destination_start..destination_start + count]
                .copy_from_slice(source);
            copied += count as u64;
        }
        Ok(Materialized {
            copied_bytes: copied,
            zero_bytes: length as u64 - copied,
        })
    }
}
