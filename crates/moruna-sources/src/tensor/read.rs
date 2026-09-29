//! The tensor read (e.4): one allocation, one reactor read, one safe view. No copy.
//!
//! Everything that touches the allocator happens before the future is built. `Source::read`
//! (contracts d.6) returns `BoxFuture<'_, _>`, whose one elided lifetime is `&self`'s, so the
//! `&dyn Allocator` the call receives cannot be captured by the future; that is reported as an
//! escalation. Here it costs nothing: the buffer is allocated up front, the reactor read is
//! submitted, and the future awaits the completion and builds the view, which needs no
//! allocator.

use moruna_kernel::{
    Allocator, BoxFuture, Buffer, Completion, ManagedTensor, Payload, Result, RowRange, Split, Tier,
};

use super::TensorSource;
use crate::stats::Counters;
use crate::util::{oversized, page_ceil, page_floor, resolve_range, source_err};

/// What the synchronous half of a read produced.
struct Pending {
    completion: Completion<(Buffer, usize)>,
    split: moruna_kernel::SplitId,
    path: std::path::PathBuf,
    base: u64,
    top: u64,
    hi: u64,
    direct: bool,
    byte_offset: u64,
    dtype: moruna_kernel::DType,
    shape: Vec<i64>,
    start: u64,
    end: u64,
    payload_bytes: u64,
}

/// Read rows `[start, end)` of a tensor split into one arena buffer and return a view into it.
pub(crate) fn read<'a>(
    source: &'a TensorSource,
    split: &Split,
    rows: Option<RowRange>,
    alloc: &dyn Allocator,
    tier: Tier,
) -> BoxFuture<'a, Result<Payload>> {
    match submit(source, split, rows, alloc, tier) {
        Err(e) => Box::pin(async move { Err(e) }),
        Ok(Ok(payload)) => Box::pin(async move { payload }),
        Ok(Err(pending)) => Box::pin(finish(pending)),
    }
}

/// The half that needs the allocator: check the range, size the enclosing range, allocate and
/// submit. An empty split resolves here with no IO (SO-I7).
fn submit(
    source: &TensorSource,
    split: &Split,
    rows: Option<RowRange>,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<std::result::Result<Result<Payload>, Pending>> {
    let entry = source.entry(split)?;
    let range = resolve_range(split, rows)?;
    Counters::add(&source.counters().reads, 1);

    let row_bytes = entry.row_bytes();
    let taken_rows = range.end - range.start;
    let mut shape = entry.shape.clone();
    if let Some(leading) = shape.first_mut() {
        *leading = taken_rows as i64;
    }
    let payload_bytes = taken_rows * row_bytes;

    if oversized(taken_rows, payload_bytes) {
        Counters::add(&source.counters().oversized_rows, 1);
        tracing::warn!(
            target: "source.oversized_row",
            split = split.id,
            row = range.start,
            bytes = payload_bytes,
            "a single tensor row is larger than any legal morsel maximum (SO-I9)",
        );
    }

    if payload_bytes == 0 {
        let buffer = alloc.alloc(0, tier)?;
        let tensor = ManagedTensor::from_buffer(buffer, 0, entry.dtype, shape)
            .map_err(|e| source_err(split.id, e.to_string()))?;
        return Ok(Ok(Payload::tensor(tensor)));
    }

    let page = alloc.page_bytes() as u64;
    let lo = entry.data_offset + range.start * row_bytes;
    let hi = lo + payload_bytes;
    // The tensor is a view at `lo - base` inside the buffer, and a view has to start on an item
    // boundary (contracts d.4, CT-T19). A safetensors data section starts wherever its JSON
    // header ends, which is a multiple of eight when the reference writer padded it and
    // anything at all when another did not; a `float32` tensor at byte 99 has a start no page
    // floor can make aligned. Such a range is read from its own first byte instead, buffered
    // (the reactor takes any offset on that path, 06 e.2), so the view starts at zero. The
    // aligned case keeps the page-floored read, which is what makes it direct (2026-09-29;
    // this was the first thing the first real tensor job found).
    let item = entry.dtype.item_size() as u64;
    let aligned = lo.is_multiple_of(item);
    let base = if aligned { page_floor(lo, page) } else { lo };
    let top = page_ceil(hi, page).min(entry.file_len);
    let enclosing = top - base;

    let length = usize::try_from(enclosing).map_err(|_| {
        source_err(
            split.id,
            format!("an enclosing range of {enclosing} bytes does not fit in memory"),
        )
    })?;
    let buffer = alloc.alloc(length, tier)?;
    // Direct when the reactor selected the direct path and the range is a whole number of
    // pages; a file that ends mid-page makes the last read buffered (e.4).
    let direct = source.reactor().paths().direct_io
        && base.is_multiple_of(page)
        && enclosing.is_multiple_of(page);
    if direct {
        Counters::add(&source.counters().tensor_direct_reads, 1);
    } else {
        Counters::add(&source.counters().tensor_buffered_reads, 1);
    }
    let completion = source
        .reactor()
        .read_file_opt(&entry.path, base, buffer, true);
    Ok(Err(Pending {
        completion,
        split: split.id,
        path: entry.path.clone(),
        base,
        top,
        hi,
        direct,
        byte_offset: lo - base,
        dtype: entry.dtype,
        shape,
        start: range.start,
        end: range.end,
        payload_bytes,
    }))
}

/// The half that waits: the bytes land in the buffer the reactor was given, and the tensor is
/// a view at `byte_offset` inside it (e.4). No copy, no mapping, no `unsafe`.
async fn finish(pending: Pending) -> Result<Payload> {
    let Pending {
        completion,
        split,
        path,
        base,
        top,
        hi,
        direct,
        byte_offset,
        dtype,
        shape,
        start,
        end,
        payload_bytes,
    } = pending;
    let (buffer, filled) = completion.await.map_err(|e| {
        source_err(
            split,
            format!("reading bytes {base}..{top} of {}: {e}", path.display()),
        )
    })?;
    if base + (filled as u64) < hi {
        return Err(source_err(
            split,
            format!(
                "{} ends at {} but the tensor needs bytes to {hi}",
                path.display(),
                base + filled as u64
            ),
        ));
    }
    tracing::trace!(
        target: "source.read",
        split,
        start,
        end,
        bytes = payload_bytes,
        decode_us = 0u64,
        direct,
        "tensor read",
    );
    let tensor = ManagedTensor::from_buffer(buffer, byte_offset, dtype, shape)
        .map_err(|e| source_err(split, e.to_string()))?;
    Payload::tensor(tensor)
}
