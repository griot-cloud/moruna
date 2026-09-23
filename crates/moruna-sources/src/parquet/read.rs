//! The Parquet read (e.2) and its byte ranges (f.3).

use moruna_kernel::arrow::array::RecordBatch;
use moruna_kernel::{Allocator, BoxFuture, Payload, Result, RowRange, Split, Tier};
use bytes::Bytes;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
};

use super::ParquetSource;
use super::reader::ArenaChunks;
use crate::stats::Counters;
use crate::util::{oversized, page_ceil, page_floor, resolve_range, source_err};

/// Projected column chunks closer together than this are fetched as one range (f.3).
const COALESCE_GAP: u64 = 1024 * 1024;

/// Read a split, or a row range of it, into arena buffers of `tier`.
///
/// `Source::read` (contracts d.6) returns `BoxFuture<'_, _>`, whose one elided lifetime is
/// `&self`'s, so the `&dyn Allocator` the call receives cannot be captured by the future. A
/// Parquet read has to allocate after its bytes arrive, because the decode copy's sizes are
/// only known once the decoder has run, so the whole read happens here and the future it
/// returns is already resolved. The ranged reads are still submitted together and so are
/// still concurrent with each other; what is lost is concurrency between one read and the
/// next, which the scheduler's read-ahead would otherwise give. Reported as an escalation,
/// with the one-word contracts change that restores it (`alloc: &'a dyn Allocator`).
pub(crate) fn read<'a>(
    source: &'a ParquetSource,
    split: &Split,
    rows: Option<RowRange>,
    alloc: &dyn Allocator,
    tier: Tier,
) -> BoxFuture<'a, Result<Payload>> {
    let outcome = read_now(source, split, rows, alloc, tier);
    Box::pin(async move { outcome })
}

fn read_now(
    source: &ParquetSource,
    split: &Split,
    rows: Option<RowRange>,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<Payload> {
    let (file, entry) = source.entry(split)?;
    let range = resolve_range(split, rows)?;
    Counters::add(&source.counters().reads, 1);

    let group = file
        .metadata
        .row_groups()
        .get(entry.row_group)
        .ok_or_else(|| source_err(split.id, "the plan names no such row group"))?;
    let page = alloc.page_bytes() as u64;
    let file_len = std::fs::metadata(&file.path)
        .map(|m| m.len())
        .map_err(|e| source_err(split.id, format!("{}: {e}", file.path.display())))?;

    // f.3: one contiguous range over the projected chunks while the gaps are small.
    let mut spans: Vec<(u64, u64)> = Vec::new();
    for leaf in &file.projected {
        let chunk = group.column(*leaf);
        let (start, length) = chunk.byte_range();
        spans.push((start, start + length));
    }
    spans.sort_unstable();
    let ranges = coalesce(&spans, page, file_len);

    let mut fetched: Vec<(u64, Bytes)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        let length = usize::try_from(end - start).map_err(|_| {
            source_err(
                split.id,
                format!("a range of {} bytes does not fit in memory", end - start),
            )
        })?;
        let buffer = alloc.alloc(length, tier)?;
        let (buffer, filled) = source
            .reactor()
            .read_file_opt(&file.path, start, buffer, true)
            .wait()
            .map_err(|e| {
                source_err(
                    split.id,
                    format!(
                        "reading bytes {start}..{end} of {}: {e}",
                        file.path.display()
                    ),
                )
            })?;
        if (filled as u64) < end - start && start + filled as u64 != file_len {
            return Err(source_err(
                split.id,
                format!(
                    "{}: bytes {start}..{end} came back short at {}",
                    file.path.display(),
                    start + filled as u64
                ),
            ));
        }
        let bytes = Bytes::from_owner(buffer).slice(..filled);
        fetched.push((start, bytes));
    }

    let chunks = ArenaChunks::new(file_len, fetched);
    let mask = ProjectionMask::leaves(
        file.metadata.file_metadata().schema_descr(),
        file.projected.iter().copied(),
    );
    let arrow_meta = ArrowReaderMetadata::try_new(
        std::sync::Arc::clone(&file.metadata),
        ArrowReaderOptions::new(),
    )
    .map_err(|e| source_err(split.id, e.to_string()))?;
    let wanted = (range.end - range.start).max(1);
    let selection = RowSelection::from_consecutive_ranges(
        std::iter::once(range.start as usize..range.end as usize),
        split.rows as usize,
    );
    let mut builder = ParquetRecordBatchReaderBuilder::new_with_metadata(chunks, arrow_meta)
        .with_row_groups(vec![entry.row_group])
        .with_projection(mask)
        .with_batch_size(usize::try_from(wanted).unwrap_or(usize::MAX));
    if range.start != 0 || range.end != split.rows {
        builder = builder.with_row_selection(selection);
    }
    let started = std::time::Instant::now();
    let reader = builder
        .build()
        .map_err(|e| source_err(split.id, format!("building the row group reader: {e}")))?;
    let mut batches = Vec::new();
    for batch in reader {
        batches.push(batch.map_err(|e| {
            source_err(
                split.id,
                format!(
                    "decoding row group {} of {}: {e}",
                    entry.row_group,
                    file.path.display()
                ),
            )
        })?);
    }
    let decoded = concat(&file.schema, batches, split.id)?;
    if decoded.num_rows() as u64 != range.end - range.start {
        return Err(source_err(
            split.id,
            format!(
                "the decoder returned {} rows for a range of {} (SO-I4)",
                decoded.num_rows(),
                range.end - range.start
            ),
        ));
    }

    let (batch, copied) = super::decode_copy::copy_batch(&decoded, alloc, tier)?;
    drop(decoded);
    Counters::add(&source.counters().decode_bytes, copied);
    // G-I2 grants a source decoding a non-layout-preserving format exactly one CPU copy of a
    // morsel's payload, and this is where it is counted (contracts d.3).
    alloc.note_payload_copy(copied);

    let payload = Payload::table(batch)?;
    if oversized(payload.rows(), payload.bytes()) {
        Counters::add(&source.counters().oversized_rows, 1);
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
        direct = source.reactor().paths().direct_io,
        "parquet read",
    );
    Ok(payload)
}

/// Merge spans whose gap is under `COALESCE_GAP`, then round each outward to page boundaries
/// and clip to the file (f.3). The result is ascending and non-overlapping.
fn coalesce(spans: &[(u64, u64)], page: u64, file_len: u64) -> Vec<(u64, u64)> {
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if *start <= last.1 + COALESCE_GAP => last.1 = last.1.max(*end),
            _ => merged.push((*start, *end)),
        }
    }
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(merged.len());
    for (start, end) in merged {
        let aligned = (page_floor(start, page), page_ceil(end, page).min(file_len));
        match out.last_mut() {
            Some(last) if aligned.0 <= last.1 => last.1 = last.1.max(aligned.1),
            _ => out.push(aligned),
        }
    }
    out
}

/// One batch out of what the reader produced; a range is read with a batch size that covers
/// it, so this is normally a single batch already.
fn concat(
    schema: &moruna_kernel::arrow::datatypes::SchemaRef,
    mut batches: Vec<RecordBatch>,
    split: moruna_kernel::SplitId,
) -> Result<RecordBatch> {
    match batches.len() {
        0 => RecordBatch::try_new_with_options(
            std::sync::Arc::clone(schema),
            schema
                .fields()
                .iter()
                .map(|f| moruna_kernel::arrow::array::new_empty_array(f.data_type()))
                .collect(),
            &moruna_kernel::arrow::array::RecordBatchOptions::new().with_row_count(Some(0)),
        )
        .map_err(|e| source_err(split, format!("an empty row group: {e}"))),
        1 => Ok(batches.remove(0)),
        _ => moruna_kernel::arrow::compute::concat_batches(schema, batches.iter())
            .map_err(|e| source_err(split, format!("joining the decoded batches: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_spans_become_one_page_aligned_range() {
        let spans = [(4100u64, 5000u64), (5100, 6000)];
        assert_eq!(coalesce(&spans, 4096, 1_000_000), vec![(4096, 8192)]);
    }

    #[test]
    fn a_wide_gap_stays_two_ranges_and_clips_at_the_file_end() {
        let spans = [(0u64, 100), (2 * COALESCE_GAP, 2 * COALESCE_GAP + 100)];
        let out = coalesce(&spans, 4096, 2 * COALESCE_GAP + 50);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], (0, 4096));
        assert_eq!(out[1].1, 2 * COALESCE_GAP + 50);
    }

    #[test]
    fn rounding_that_overlaps_merges() {
        let spans = [(0u64, 10), (4090, 4100)];
        assert_eq!(coalesce(&spans, 4096, 1_000_000), vec![(0, 8192)]);
    }
}
