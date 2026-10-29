//! The Arrow IPC read (e.10).

use std::sync::Arc;

use moruna_kernel::arrow::array::{RecordBatch, RecordBatchOptions, new_empty_array};
use moruna_kernel::arrow::compute::concat_batches;
use moruna_kernel::arrow::ipc;
use moruna_kernel::buffer::arena_tier_of;
use moruna_kernel::ipc::{Decoder, Inflate};
use moruna_kernel::{Allocator, Buffer, Completion, Payload, Result, RowRange, Split, Tier};

use super::ArrowIpcSource;
use super::plan::{Block, FileMeta};
use crate::parquet::decode_copy::copy_batch_except;
use crate::stats::Counters;
use crate::util::{oversized, page_ceil, page_floor, resolve_range, source_err};

/// A block whose bytes the reactor is landing in an arena buffer.
struct Pending {
    index: usize,
    /// Where the buffer's first byte is in the file: the block's offset floored to a page.
    base: u64,
    /// The first byte the file must still have for the whole block to be there.
    end: u64,
    completion: Completion<(Buffer, usize)>,
}

/// Read a split, or a row range of it, into arena buffers of `tier`.
///
/// Each block the range touches is landed whole by the reactor, as the enclosing page range of
/// the file, then decoded over that buffer (no copy). A range inside one block is a slice of the
/// decoded batch; a range across blocks is their concatenation, which is the decode copy that
/// Parquet's decoder output also takes before it is copied into the arena (SO-I5).
pub(crate) async fn read(
    source: &ArrowIpcSource,
    split: &Split,
    rows: Option<RowRange>,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<Payload> {
    let (file, entry) = source.entry(split)?;
    let range = resolve_range(split, rows)?;
    Counters::add(&source.counters.reads, 1);
    let started = std::time::Instant::now();
    let wanted = range.end - range.start;
    if wanted == 0 {
        // No I/O for no rows.
        let batch = RecordBatch::try_new_with_options(
            Arc::clone(&file.out),
            file.out
                .fields()
                .iter()
                .map(|f| new_empty_array(f.data_type()))
                .collect(),
            &RecordBatchOptions::new().with_row_count(Some(0)),
        )
        .map_err(|e| source_err(split.id, format!("an empty range: {e}")))?;
        return Payload::table(batch);
    }

    // The blocks the range touches, with the rows to take from each: every read is issued
    // before the first is awaited, so the reactor lands them together.
    let mut take: Vec<(usize, u64, u64)> = Vec::new();
    let mut first_row = 0u64;
    for index in entry.blocks.clone() {
        let rows = file.blocks[index].rows;
        let (low, high) = (first_row, first_row + rows);
        first_row = high;
        if rows == 0 || high <= range.start || low >= range.end {
            continue;
        }
        take.push((
            index,
            range.start.saturating_sub(low),
            (range.end - low).min(rows),
        ));
    }
    let mut pending = Vec::with_capacity(take.len());
    for (index, _, _) in &take {
        pending.push(land(source, file, split, *index, alloc, tier)?);
    }
    let mut parts = Vec::with_capacity(take.len());
    for (pending, (_, from, to)) in pending.into_iter().zip(&take) {
        let batch = decode(file, split, pending, alloc).await?;
        parts.push(batch.slice(*from as usize, (*to - *from) as usize));
    }
    let batch = match parts.len() {
        1 => parts.remove(0),
        _ => concat_batches(&file.out, &parts)
            .map_err(|e| source_err(split.id, format!("joining the blocks of a split: {e}")))?,
    };
    if batch.num_rows() as u64 != wanted {
        return Err(source_err(
            split.id,
            format!(
                "the blocks gave {} rows for a range of {wanted} (SO-I4)",
                batch.num_rows()
            ),
        ));
    }

    // A buffer the decoder left over the arena buffer the reactor filled is kept where it is;
    // every other buffer is copied into the arena once, the decode copy (SO-I5).
    let resident = |b: &moruna_kernel::arrow::buffer::Buffer| arena_tier_of(b) == Some(tier);
    let total: u64 = batch
        .columns()
        .iter()
        .map(|c| buffer_bytes(&c.to_data()))
        .sum();
    let (batch, copied) = copy_batch_except(&batch, alloc, tier, &resident)?;
    Counters::add(&source.counters.decode_bytes, copied);
    Counters::add(
        &source.counters.zero_copy_bytes,
        total.saturating_sub(copied),
    );
    if copied > 0 {
        alloc.note_payload_copy(copied);
    }

    let payload = Payload::table(batch)?;
    if oversized(payload.rows(), payload.bytes()) {
        Counters::add(&source.counters.oversized_rows, 1);
        tracing::warn!(
            target: "source.oversized_row",
            split = split.id,
            row = range.start,
            bytes = payload.bytes(),
            "a single row is larger than any legal morsel maximum (SO-I9)",
        );
    }
    tracing::trace!(
        target: "source.read",
        split = split.id,
        start = range.start,
        end = range.end,
        bytes = payload.bytes(),
        decode_us = started.elapsed().as_micros() as u64,
        copied,
        direct = source.reactor.paths().direct_io,
        "arrow ipc read",
    );
    Ok(payload)
}

/// Ask the reactor for block `index`: the enclosing page range of the file, in an arena buffer.
fn land(
    source: &ArrowIpcSource,
    file: &FileMeta,
    split: &Split,
    index: usize,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<Pending> {
    let block = &file.blocks[index];
    let named = |msg: String| {
        source_err(
            split.id,
            format!("block {index} of {}: {msg}", file.path.display()),
        )
    };
    let end = block.offset + block.meta_len + block.body_len;
    if end > file.len {
        return Err(named(format!(
            "it runs to byte {end}, past the end of the {}-byte file",
            file.len
        )));
    }
    let page = alloc.page_bytes() as u64;
    let base = page_floor(block.offset, page);
    let top = page_ceil(end, page).min(file.len);
    let span = usize::try_from(top - base)
        .map_err(|_| named(format!("{} bytes do not fit in memory", top - base)))?;
    // An arena refusal is the arena's own error, passed through as it is (SO-T3).
    let buffer = alloc.alloc(span, tier)?;
    let completion = source.reactor.read_file_opt(&file.path, base, buffer, true);
    Ok(Pending {
        index,
        base,
        end,
        completion,
    })
}

/// Decode one landed block: its message through the check (CT-I13) and then arrow-ipc, over
/// the buffer the bytes landed in. A compressed block is inflated by the check into one arena
/// buffer, within half of what the arena can still give (e.8), and decoded over that. Every
/// failure names the file and the block.
async fn decode(
    file: &FileMeta,
    split: &Split,
    pending: Pending,
    alloc: &dyn Allocator,
) -> Result<RecordBatch> {
    let Pending {
        index,
        base,
        end,
        completion,
    } = pending;
    let block = &file.blocks[index];
    let named = |msg: String| {
        source_err(
            split.id,
            format!("block {index} of {}: {msg}", file.path.display()),
        )
    };
    let (buffer, filled) = completion
        .await
        .map_err(|e| named(format!("reading bytes {base}..{end}: {e}")))?;
    if base + (filled as u64) < end {
        return Err(named(format!(
            "the file came back short at byte {}, and the block runs to {end}",
            base + filled as u64
        )));
    }
    let arrow = buffer.into_arrow_buffer()?;
    let at = (block.offset - base) as usize;
    let header = arrow
        .as_slice()
        .get(at..at + block.meta_len as usize)
        .ok_or_else(|| named("its message is outside the bytes read".into()))?;
    let message = message_of(header, block).map_err(&named)?;
    let body = arrow.slice_with_length(at + block.meta_len as usize, block.body_len as usize);
    let mut decoder = Decoder::with_schema(Arc::clone(&file.schema));
    let batch = decoder
        .message(&message, &body, &Inflate::share(alloc))
        .map_err(&named)?
        .ok_or_else(|| named("it is not a record batch".into()))?;
    if batch.num_rows() as u64 != block.rows {
        return Err(named(format!(
            "it holds {} rows and the plan counted {}",
            batch.num_rows(),
            block.rows
        )));
    }
    batch
        .project(&file.projected)
        .map_err(|e| named(format!("the projection: {e}")))
}

/// The message at the start of a block's header bytes, as the plan found it.
fn message_of<'a>(
    header: &'a [u8],
    block: &Block,
) -> std::result::Result<ipc::Message<'a>, String> {
    if header.len() < 8 || header[..4] != [0xff; 4] {
        return Err("its message does not start with a continuation marker".into());
    }
    let declared = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let flatbuffer = header
        .get(8..8 + declared)
        .filter(|_| declared > 0)
        .ok_or_else(|| {
            format!(
                "its message declares {declared} bytes and the block gives {}",
                block.meta_len
            )
        })?;
    ipc::root_as_message(flatbuffer).map_err(|e| format!("its message does not parse: {e}"))
}

/// The bytes of every buffer of a column, its validity and children included, over the
/// visible slice.
fn buffer_bytes(data: &moruna_kernel::arrow::array::ArrayData) -> u64 {
    let own: u64 = data.buffers().iter().map(|b| b.len() as u64).sum();
    let nulls = data.nulls().map_or(0, |n| n.buffer().len() as u64);
    own + nulls + data.child_data().iter().map(buffer_bytes).sum::<u64>()
}
