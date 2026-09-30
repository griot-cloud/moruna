//! The Vortex read (e.8).

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, AsArray, RecordBatch, RecordBatchOptions};
use moruna_kernel::arrow::datatypes::{DataType, Field};
use moruna_kernel::buffer::arena_tier_of;
use moruna_kernel::{Allocator, Payload, Result, RowRange, Split, Tier};
use vortex::array::VortexSessionExecute;
use vortex::array::stream::ArrayStreamExt;
use vortex::arrow::ArrowSessionExt;
use vortex::buffer::Alignment;
use vortex::file::OpenOptionsSessionExt;

use super::VortexSource;
use super::io::{self, Fetched};
use super::plan::FileMeta;
use crate::parquet::decode_copy::copy_batch_except;
use crate::stats::Counters;
use crate::util::{oversized, page_ceil, page_floor, resolve_range, source_err};

/// Read a split, or a row range of it, into arena buffers of `tier`.
///
/// As for Parquet (e.2), the whole read happens here and the returned future is already
/// resolved: the Vortex reader's futures run on the source's own executor, which only the
/// thread blocking on it drives (g).
pub(crate) fn read(
    source: &VortexSource,
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
    let batch = if wanted == 0 {
        RecordBatch::try_new_with_options(
            Arc::clone(&file.schema),
            file.schema
                .fields()
                .iter()
                .map(|f| moruna_kernel::arrow::array::new_empty_array(f.data_type()))
                .collect(),
            &RecordBatchOptions::new().with_row_count(Some(0)),
        )
        .map_err(|e| source_err(split.id, format!("an empty range: {e}")))?
    } else {
        let rows = entry.start + range.start..entry.start + range.end;
        scan(source, file, split, rows, alloc, tier)?
    };
    if batch.num_rows() as u64 != wanted {
        return Err(source_err(
            split.id,
            format!(
                "the decoder returned {} rows for a range of {wanted} (SO-I4)",
                batch.num_rows()
            ),
        ));
    }

    // e.8: a buffer the decoder left over the arena buffer the reactor filled is kept where it
    // is; every other buffer is copied into the arena once, the decode copy (SO-I5).
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
        "vortex read",
    );
    Ok(payload)
}

/// The bytes of every buffer of a column, its validity and children included, over the
/// visible slice.
fn buffer_bytes(data: &moruna_kernel::arrow::array::ArrayData) -> u64 {
    let own: u64 = data.buffers().iter().map(|b| b.len() as u64).sum();
    let nulls = data.nulls().map_or(0, |n| n.buffer().len() as u64);
    own + nulls + data.child_data().iter().map(buffer_bytes).sum::<u64>()
}

/// Scan file rows `rows` of the projection through the reactor into the arena, and execute
/// the result to Arrow.
fn scan(
    source: &VortexSource,
    file: &FileMeta,
    split: &Split,
    rows: std::ops::Range<u64>,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<RecordBatch> {
    let session = &source.engine.session;
    let footer = file.footer.clone();
    let len = file.len;
    let names = file.projected.clone();
    let wanted = rows.clone();
    let driven = io::drive(
        &source.engine.runtime,
        &file.url,
        len,
        |reader| async move {
            let opened = session
                .open_options()
                .with_footer(footer)
                .with_file_size(len)
                .open(reader)
                .await?;
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let projection = vortex::expr::select(names, vortex::expr::root())
                .optimize_recursive(opened.dtype())?
                .bind(opened.dtype())?;
            opened
                .scan()?
                .with_projection(projection)
                .with_row_range(wanted)
                .into_array_stream()?
                .read_all()
                .await
        },
        |offset, length, alignment| {
            fetch(source, file, split, alloc, tier, offset, length, alignment)
        },
    );
    let array = match driven {
        Ok(Ok(array)) => array,
        Ok(Err(e)) => {
            return Err(source_err(
                split.id,
                format!(
                    "decoding rows {}..{} of {}: {e}",
                    rows.start, rows.end, file.url
                ),
            ));
        }
        Err(e) => return Err(e),
    };
    let target = Field::new("", DataType::Struct(file.schema.fields().clone()), false);
    let mut ctx = session.create_execution_ctx();
    let arrow = session
        .arrow()
        .execute_arrow(array, Some(&target), &mut ctx)
        .map_err(|e| {
            source_err(
                split.id,
                format!(
                    "rows {}..{} of {} to Arrow: {e}",
                    rows.start, rows.end, file.url
                ),
            )
        })?;
    let Some(table) = arrow.as_struct_opt() else {
        return Err(source_err(
            split.id,
            format!(
                "{}: the decoder returned {:?}, not a table",
                file.url,
                arrow.data_type()
            ),
        ));
    };
    RecordBatch::try_new_with_options(
        Arc::clone(&file.schema),
        table.columns().to_vec(),
        &RecordBatchOptions::new().with_row_count(Some(table.len())),
    )
    .map_err(|e| source_err(split.id, format!("{}: {e}", file.url)))
}

/// Answer one segment request: the enclosing page-aligned range of the file, landed by the
/// reactor in an arena buffer of `tier`, and handed to Vortex as a view at the segment's offset
/// inside it (e.8). A local read may end short at the end of the file; it must still cover the
/// segment.
#[allow(clippy::too_many_arguments)]
async fn fetch(
    source: &VortexSource,
    file: &FileMeta,
    split: &Split,
    alloc: &dyn Allocator,
    tier: Tier,
    offset: u64,
    length: usize,
    alignment: Alignment,
) -> Fetched {
    let end = offset + length as u64;
    let named = |msg: String| {
        source_err(
            split.id,
            format!("reading bytes {offset}..{end} of {}: {msg}", file.url),
        )
    };
    if end > file.len {
        return Err(named(format!(
            "the segment lies past the end of the {}-byte file",
            file.len
        )));
    }
    let page = alloc.page_bytes() as u64;
    let lo = page_floor(offset, page);
    let hi = page_ceil(end, page).min(file.len);
    let span = usize::try_from(hi - lo)
        .map_err(|_| named(format!("{} bytes do not fit in memory", hi - lo)))?;
    // An arena refusal is the arena's own error, passed through as it is (SO-T3).
    let buffer = alloc.alloc(span, tier)?;
    let (buffer, filled) = if file.object {
        source
            .reactor
            .read_object(&file.url, lo, buffer)
            .await
            .map(|b| (b, span))
            .map_err(|e| named(e.to_string()))?
    } else {
        source
            .reactor
            .read_file_opt(&file.path, lo, buffer, true)
            .await
            .map_err(|e| named(e.to_string()))?
    };
    if lo + (filled as u64) < end {
        return Err(named(format!(
            "the file came back short at {}",
            lo + filled as u64
        )));
    }
    let arrow = buffer.into_arrow_buffer()?;
    Ok(io::aligned(
        arrow.slice_with_length((offset - lo) as usize, length),
        alignment,
    ))
}
