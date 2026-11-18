//! The Parquet read (e.2) and its byte ranges (f.3).

use bytes::Bytes;
use moruna_kernel::arrow::array::RecordBatch;
use moruna_kernel::{Allocator, BoxFuture, Payload, Result, RowRange, Split, Tier};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
    RowSelector,
};
use parquet::file::metadata::ParquetMetaData;

use super::parts::GroupFetch;
use super::reader::ArenaChunks;
use super::{Claim, ParquetSource, append, parts};
use crate::stats::Counters;
use crate::util::{oversized, page_ceil, page_floor, resolve_range, source_err};

/// Projected column chunks closer together than this are fetched as one range (f.3).
const COALESCE_GAP: u64 = 1024 * 1024;

/// The most one request of an object's range asks for (f.3, item 6.3t): a range is fetched in
/// requests of at most this many bytes, cut where a column chunk starts so no page is split, and
/// every request of a fetch is issued before any is waited for, so the reactor runs them at once
/// (up to `reactor.object_concurrency`). One request per range read a node's year of taxi trips
/// from Blob at about 43 MB/s, one row group after another (6.2 run 3). It is the scale of the
/// least part a store takes for a write (5 MiB, `S3_PART_MIN_BYTES`), so a request's latency is
/// paid on a few megabytes, not on a byte or a whole group.
const OBJECT_REQUEST_MAX: u64 = 8 * 1024 * 1024;

/// Read a split, or a row range of it, into arena buffers of `tier`, when the future is polled.
///
/// The future borrows the allocator for `'a` (contracts d.6), so the read happens when the drive
/// polls it and not when it issues it: the drive issues up to its read-ahead before it polls
/// (10 f.5), and a read done at issue held every earlier one back from the kernels (0.6.3). The
/// ranged reads of one split are still submitted together.
pub(crate) fn read<'a>(
    source: &'a ParquetSource,
    split: &'a Split,
    rows: Option<RowRange>,
    alloc: &'a dyn Allocator,
    tier: Tier,
) -> BoxFuture<'a, Result<Payload>> {
    Box::pin(async move {
        let fetched = fetch_range(source, split, rows, alloc, tier).await?;
        decode_fetched(source, split, fetched, alloc, tier)
    })
}

/// A split's projected column chunks, fetched and not yet decoded (RB1,
/// `architecture/worker-io.md`): what the drive hands a worker to decode.
pub(crate) struct ParquetFetched {
    fetch: GroupFetch,
    range: RowRange,
}

/// The I/O half of a read: the byte ranges of a split's projected columns, into arena buffers
/// of `tier`. It awaits the reactor, so it runs on the drive, and the drive's other reads go on
/// meanwhile (6.3t).
pub(crate) fn fetch<'a>(
    source: &'a ParquetSource,
    split: &'a Split,
    rows: Option<RowRange>,
    alloc: &'a dyn Allocator,
    tier: Tier,
) -> BoxFuture<'a, Result<ParquetFetched>> {
    Box::pin(fetch_range(source, split, rows, alloc, tier))
}

/// A range of `split`: its group's fetch, which this range makes when no range of the group is
/// making it, and otherwise waits for or shares (6.3t). The drive has several reads in flight,
/// and a fetch that waited on each request in turn held every other read back: a node read a
/// year of trips from Blob one row group at a time (6.2 runs 3 and 5).
async fn fetch_range(
    source: &ParquetSource,
    split: &Split,
    rows: Option<RowRange>,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<ParquetFetched> {
    let range = resolve_range(split, rows)?;
    Counters::add(&source.counters().reads, 1);
    let last = range.end >= split.rows;
    let fetch = match source.claim_fetch(split.id, last).await {
        Claim::Ready(fetch) => fetch,
        Claim::Mine => {
            let mut claim = Claimed {
                source,
                split: split.id,
                last,
                ended: false,
            };
            let fetched = fetch_group(source, split, alloc, tier).await;
            claim.end(fetched.as_ref().ok());
            fetched?
        }
    };
    Ok(ParquetFetched { fetch, range })
}

/// A range's claim on its group's fetch. Ended, it keeps the fetch for the group's other ranges
/// (or forgets a failed one); dropped without ending, as a cancelled read is, it lets them claim
/// the fetch themselves, so none waits for a fetch that will not come.
struct Claimed<'a> {
    source: &'a ParquetSource,
    split: moruna_kernel::SplitId,
    last: bool,
    ended: bool,
}

impl Claimed<'_> {
    fn end(&mut self, fetch: Option<&GroupFetch>) {
        self.ended = true;
        self.source.fetched(self.split, fetch, self.last);
    }
}

impl Drop for Claimed<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.source.fetched(self.split, None, self.last);
        }
    }
}

/// The projected column chunks of `split`'s row group, every request in flight together.
async fn fetch_group(
    source: &ParquetSource,
    split: &Split,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<GroupFetch> {
    let (file, entry) = source.entry(split)?;
    let group = file
        .metadata
        .row_groups()
        .get(entry.row_group)
        .ok_or_else(|| source_err(split.id, "the plan names no such row group"))?;
    let page = alloc.page_bytes() as u64;
    let file_len = if file.object {
        file.len
    } else {
        std::fs::metadata(&file.path)
            .map(|m| m.len())
            .map_err(|e| source_err(split.id, format!("{}: {e}", file.path.display())))?
    };

    // f.3: one contiguous range over the projected chunks while the gaps are small. Every
    // span lies inside the file as it is now (SO-I10), so the ranges below are well formed and
    // the reader's own span arithmetic cannot fail.
    let mut spans: Vec<(u64, u64)> = Vec::new();
    for leaf in &file.projected {
        let (_, span) =
            super::plan::chunk_span(&file.path, group, entry.row_group, *leaf, file_len)
                .map_err(|msg| source_err(split.id, msg))?;
        spans.push(span);
    }
    spans.sort_unstable();
    let mut ranges = coalesce(&spans, page, file_len);
    if file.object {
        ranges = requests(&ranges, &spans, OBJECT_REQUEST_MAX);
    }

    // Every request is issued before any is waited for, so they are in flight together (6.3t);
    // a range that cannot be laid out in the arena ends the fetch, and the requests already
    // issued are dropped with their buffers.
    let mut issued = Vec::with_capacity(ranges.len());
    for &(start, end) in &ranges {
        let length = usize::try_from(end - start).map_err(|_| {
            source_err(
                split.id,
                format!("a range of {} bytes does not fit in memory", end - start),
            )
        })?;
        let buffer = alloc.alloc(length, tier)?;
        // An object's range is clipped to the length the store reported at plan time, so the
        // read is exact; a local file may be read short at its end (f.3).
        let read = if file.object {
            Issued::Object(
                source.reactor().read_object(&file.url, start, buffer),
                length,
            )
        } else {
            Issued::File(
                source
                    .reactor()
                    .read_file_opt(&file.path, start, buffer, true),
            )
        };
        issued.push((start, end, read));
    }
    let mut fetched: Vec<(u64, Bytes)> = Vec::with_capacity(issued.len());
    for (start, end, read) in issued {
        let read = match read {
            Issued::Object(completion, length) => completion.await.map(|b| (b, length)),
            Issued::File(completion) => completion.await,
        };
        let (buffer, filled) = read.map_err(|e| {
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

    let fetch = GroupFetch::new(ArenaChunks::new(file_len, fetched));
    Counters::add(&source.counters().group_fetches, 1);
    Ok(fetch)
}

/// The CPU half of a read: decode a fetched split into a payload in arena buffers of `tier`.
/// Never waits on the reactor, so any worker may run it, several at once (RB1).
pub(crate) fn decode_fetched(
    source: &ParquetSource,
    split: &Split,
    fetched: ParquetFetched,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<Payload> {
    let (file, entry) = source.entry(split)?;
    let ParquetFetched {
        fetch: GroupFetch { chunks, decode },
        range,
    } = fetched;
    // A range of a group decodes only the pages it covers when they can be located (`pages`):
    // the group alone, with its page locations, as row group 0.
    let partial = range.start != 0 || range.end != split.rows;
    let indexed = partial
        .then(|| source.paged(split, file, entry, &chunks))
        .flatten();
    if indexed.is_some() {
        Counters::add(&source.counters().ranges_paged, 1);
    }
    if partial {
        Counters::add(&source.counters().ranges_decoded, 1);
    }
    let wanted = (range.end - range.start).max(1);
    // 3A (6.3a): a range whose columns are flat is decoded a batch at a time into its arena
    // buffers (`append`), so what a read holds outside the arena never exceeds its own decoded
    // bytes: one batch of its fixed-width columns, and the bytes of string and binary columns
    // that have no exact figure (a part of a group, or a file that does not state them). Any
    // other range is decoded whole and copied, as before.
    let appended = append::writable(&file.schema);
    let batch_rows = if appended {
        batch_rows(source, split, file, entry, &chunks, wanted)
    } else {
        wanted
    };
    let batch_size = usize::try_from(batch_rows).unwrap_or(usize::MAX);
    // 6.3l: what this decode's page readers take from the fetch is counted, so a group read in
    // parts shows what its parts decode again (`SourceStats::page_bytes_decoded`).
    let taken = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let chunks = chunks.counting(std::sync::Arc::clone(&taken));
    let started = std::time::Instant::now();
    let built = match &indexed {
        // A part of a group whose pages are located takes the dictionary pages the group's parts
        // share, decompressed once by the first of them to decode (`parts`, 6.3l).
        Some(indexed) => {
            let chunks = std::sync::Arc::new(chunks);
            let kept = decode.kept(file, indexed, &chunks, alloc, tier, split.id)?;
            parts::reader(indexed, &chunks, &kept, batch_size, part_selection(range))
        }
        None => {
            let arrow_meta = ArrowReaderMetadata::try_new(
                std::sync::Arc::clone(&file.metadata),
                ArrowReaderOptions::new(),
            )
            .map_err(|e| source_err(split.id, e.to_string()))?;
            let mask = ProjectionMask::leaves(
                file.metadata.file_metadata().schema_descr(),
                file.projected.iter().copied(),
            );
            let mut builder =
                ParquetRecordBatchReaderBuilder::new_with_metadata(chunks, arrow_meta)
                    .with_row_groups(vec![entry.row_group])
                    .with_projection(mask)
                    .with_batch_size(batch_size);
            if partial {
                builder = builder.with_row_selection(part_selection(range));
            }
            builder.build()
        }
    };
    let reader =
        built.map_err(|e| source_err(split.id, format!("building the row group reader: {e}")))?;
    let decode_error = |e: moruna_kernel::arrow::error::ArrowError| {
        source_err(
            split.id,
            format!(
                "decoding row group {} of {}: {e}",
                entry.row_group,
                file.path.display()
            ),
        )
    };
    let rows = usize::try_from(range.end - range.start).unwrap_or(usize::MAX);
    let mut batches = Vec::new();
    let mut into: Option<append::Appender> = None;
    for batch in reader {
        let batch = batch.map_err(&decode_error)?;
        source
            .counters()
            .batch(batch.get_array_memory_size() as u64);
        if !appended {
            batches.push(batch);
            continue;
        }
        if into.is_none() {
            // A whole group's decoded bytes are exact when its file states them (`plan`), and
            // only then; a share of them by rows bounds no part of it.
            let exact = (!partial && !split.estimated).then_some(split.column_bytes.as_slice());
            into = Some(append::Appender::reserve(
                split.id,
                &batch.schema(),
                rows,
                exact,
                alloc,
                tier,
            )?);
        }
        if let Some(into) = into.as_mut() {
            into.append(&batch)?;
        }
    }
    Counters::add(
        &source.counters().page_bytes_decoded,
        taken.load(std::sync::atomic::Ordering::Relaxed),
    );
    let (batch, copied) = match into {
        Some(into) => {
            let batch = into.finish()?;
            let copied = append::payload_bytes(&batch);
            (batch, copied)
        }
        None => {
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
            super::decode_copy::copy_batch(&decoded, alloc, tier)?
        }
    };
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

/// The rows a part of a row group selects (3A, 6.3l): the rows before it skipped, and its own,
/// and nothing after them. A selection that ran to the group's end had its decoder skip the rest
/// of the group once its rows were read, value by value through the rest of the page it ended in
/// (0.16 ms a part on the NYC year), for rows it never returns.
fn part_selection(range: RowRange) -> RowSelection {
    let mut selectors = Vec::with_capacity(2);
    if range.start > 0 {
        selectors.push(RowSelector::skip(
            usize::try_from(range.start).unwrap_or(usize::MAX),
        ));
    }
    selectors.push(RowSelector::select(
        usize::try_from(range.end - range.start).unwrap_or(usize::MAX),
    ));
    RowSelection::from(selectors)
}

/// The rows a batch of a range of `wanted` rows decodes at once (3A, 6.3a): as many as the
/// source's decode batch holds (`ParquetSource::decode_batches_of`, the headroom above the arena
/// over the workers that decode at once), by the split's decoded bytes a row, and never fewer
/// than a page holds, in the projected column whose pages hold the fewest rows. A source with no
/// decode batch, or a group whose pages cannot be located, decodes its range in one batch.
fn batch_rows(
    source: &ParquetSource,
    split: &Split,
    file: &super::plan::FileMeta,
    entry: &super::plan::Entry,
    chunks: &ArenaChunks,
    wanted: u64,
) -> u64 {
    let Some(bytes) = source.decode_batch_bytes() else {
        return wanted;
    };
    let per_row = (split.uncompressed_bytes / split.rows.max(1)).max(1);
    let rows = bytes / per_row;
    if rows >= wanted {
        // The range fits one batch: its pages need not be located for a floor.
        return wanted;
    }
    let Some(floor) = source
        .paged(split, file, entry, chunks)
        .and_then(|indexed| page_rows(&indexed, &file.projected))
    else {
        return wanted;
    };
    rows.max(floor).clamp(1, wanted)
}

/// The rows a page holds in the `projected` column whose pages hold the fewest, each column
/// counted by its largest page; `None` when no projected column has a located page.
fn page_rows(indexed: &ParquetMetaData, projected: &[usize]) -> Option<u64> {
    let group = indexed.offset_index()?.first()?;
    let rows = indexed.row_group(0).num_rows();
    projected
        .iter()
        .filter_map(|&leaf| {
            let pages = group.get(leaf)?.page_locations();
            let starts: Vec<i64> = pages.iter().map(|page| page.first_row_index).collect();
            starts
                .iter()
                .zip(starts.iter().skip(1).chain(std::iter::once(&rows)))
                .map(|(start, end)| u64::try_from(end - start).unwrap_or(0))
                .max()
        })
        .filter(|&rows| rows > 0)
        .min()
}

/// A request of a fetch, issued and not yet waited for.
enum Issued {
    /// An object's bytes, and the length asked for (an object's read is exact).
    Object(moruna_kernel::Completion<moruna_kernel::Buffer>, usize),
    /// A file's bytes and how many came.
    File(moruna_kernel::Completion<(moruna_kernel::Buffer, usize)>),
}

/// `ranges` cut into requests of at most `most` bytes (6.3t). A cut falls only where one of
/// `spans` (the projected column chunks) starts, so every page lies inside one request and
/// the requests' bytes are the ranges' bytes, ascending and non-overlapping. A column chunk
/// longer than `most` is one request of its own.
fn requests(ranges: &[(u64, u64)], spans: &[(u64, u64)], most: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(ranges.len());
    for &(start, end) in ranges {
        let mut from = start;
        for &(span_start, span_end) in spans {
            // Only a span that starts inside the request, past its start, is a place to cut.
            if span_start <= from || span_start >= end {
                continue;
            }
            // Cut before this span when taking it would carry the request past `most`.
            if span_end.min(end) - from > most {
                out.push((from, span_start));
                from = span_start;
            }
        }
        out.push((from, end));
    }
    out
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
    use super::requests;

    /// 6.3t: a range is cut into requests of at most `most` bytes, only where a column chunk
    /// starts, so each chunk (and every page in it) lies in one request, and the requests cover
    /// the range exactly, in order.
    #[test]
    fn a_range_is_cut_into_requests_at_chunk_starts() {
        let mb = 1u64 << 20;
        // Six chunks of 3 MiB, one range over them (page-rounded at its two ends).
        let spans: Vec<(u64, u64)> = (0..6)
            .map(|i| (100 + i * 3 * mb, 100 + (i + 1) * 3 * mb))
            .collect();
        let range = (0, 100 + 18 * mb + 4000);
        let out = requests(&[range], &spans, 8 * mb);
        assert_eq!(out.first().map(|r| r.0), Some(range.0));
        assert_eq!(out.last().map(|r| r.1), Some(range.1));
        for pair in out.windows(2) {
            assert_eq!(pair[0].1, pair[1].0, "contiguous: {out:?}");
        }
        for (start, end) in &out {
            assert!(
                end - start <= 8 * mb + 4000,
                "at most the cap, past the rounding: {out:?}"
            );
            assert!(
                *start == range.0 || spans.iter().any(|s| s.0 == *start),
                "a cut is a chunk's start: {out:?}"
            );
        }
        for (s, e) in &spans {
            assert!(
                out.iter().any(|(a, b)| a <= s && e <= b),
                "chunk {s}..{e} in one request: {out:?}"
            );
        }
        assert_eq!(out.len(), 3, "18 MiB in requests of at most 8 MiB: {out:?}");
    }

    /// A chunk longer than the cap is one request of its own, and a range under the cap stays
    /// one request.
    #[test]
    fn a_long_chunk_is_one_request_and_a_short_range_stays_whole() {
        let mb = 1u64 << 20;
        let spans = vec![(0, 20 * mb), (20 * mb, 21 * mb), (21 * mb, 22 * mb)];
        let out = requests(&[(0, 22 * mb)], &spans, 8 * mb);
        assert_eq!(out, vec![(0, 20 * mb), (20 * mb, 22 * mb)]);
        let short = requests(&[(0, 3 * mb)], &[(0, mb), (mb, 3 * mb)], 8 * mb);
        assert_eq!(short, vec![(0, 3 * mb)]);
        assert!(requests(&[], &spans, 8 * mb).is_empty());
    }

    use super::*;

    #[test]
    fn a_part_selects_its_rows_and_nothing_after_them() {
        let selectors = |start, end| -> Vec<(bool, usize)> {
            part_selection(RowRange { start, end })
                .iter()
                .map(|s| (s.skip, s.row_count))
                .collect()
        };
        assert_eq!(selectors(0, 10), vec![(false, 10)]);
        assert_eq!(selectors(25, 40), vec![(true, 25), (false, 15)]);
        assert_eq!(
            part_selection(RowRange { start: 25, end: 40 }).row_count(),
            15
        );
    }

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
