//! Promotion from disk: turning a segment record back into a payload (f.6).
//!
//! The arrays of a decoded table point into the buffer the reactor read into, and a tensor
//! is built over that buffer directly, so nothing here copies payload bytes either (PL-I4).

use moruna_kernel::{Buffer, MorunaError, Payload, PayloadKind, ipc, mrb1};

use super::segment::RecordHeader;

/// What a record read back gives: the header the engine wrote and the payload it described.
pub struct Record {
    /// The record's header (e.3).
    pub header: RecordHeader,
    /// The payload, resident in the tier the read buffer was allocated in.
    pub payload: Payload,
}

/// Decode a record read into one buffer starting at the record's header page.
///
/// The buffer is never split: `Buffer::split_at` hands out two halves that the arena frees
/// independently (contracts d.3), but the testkit's `FakeAllocator` frees the whole region
/// when the half holding its first byte drops, and the header page is that half. An Arrow
/// slice and `ManagedTensor::from_buffer`'s byte offset reach the body without splitting and
/// keep the whole allocation alive, which is what both readers need anyway. Reported as a
/// finding against d.15.
pub fn decode_record(buf: Buffer, page: u64) -> moruna_kernel::Result<Record> {
    let page_usize = usize::try_from(page).map_err(|_| {
        MorunaError::Staging(format!("page size {page} does not fit this platform"))
    })?;
    if buf.len() < page_usize {
        return Err(MorunaError::Staging(format!(
            "a record read of {} bytes is short of its header page",
            buf.len()
        )));
    }
    let header = RecordHeader::read(&buf[..page_usize])?;
    let body_len = (buf.len() - page_usize) as u64;
    if body_len < header.payload_len {
        return Err(MorunaError::Staging(format!(
            "record {} declares {} body bytes, the read gave {body_len}",
            header.seq, header.payload_len
        )));
    }
    let payload = match header.kind {
        PayloadKind::Table | PayloadKind::Either => {
            let arrow = buf.into_arrow_buffer()?;
            let batch = ipc::decode(arrow.slice(page_usize), page_usize)?;
            Payload::table(batch)?
        }
        PayloadKind::Tensor => {
            let parsed = mrb1::Header::read(&buf[page_usize..])?;
            let (dtype, shape, data_offset) = (parsed.dtype, parsed.shape, parsed.data_offset);
            let tensor =
                moruna_kernel::ManagedTensor::from_buffer(buf, page + data_offset, dtype, shape)?;
            Payload::tensor(tensor)?
        }
    };
    Ok(Record { header, payload })
}
