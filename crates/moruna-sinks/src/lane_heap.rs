//! What a Parquet lane's writer holds on the heap, and the writer's limits that keep it within
//! its allowance (system design 3A, item 6.3d).
//!
//! A sink whose writer keeps a lane's state on the heap claims it above the arena before the
//! arena takes the rest: the run fits its writers once it knows the sink's schema
//! ([`fit_writers`]), one lane per worker at [`CLAIM_PAGE_BYTES`] when the budget holds them,
//! and grants each lane its need (the runtime's `BuildCtx::claim_writer_heap`). A lane's
//! `ArrowWriter` holds, outside the arena:
//!
//! - each leaf column's codec: for Zstandard a decompression context made with every column
//!   writer and never used to write, and a compression context whose tables follow the largest
//!   page it has compressed, data or dictionary. They are the C library's, outside every
//!   allocator Moruna counts, and are counted here by their measured sizes ([`codec_bytes`]);
//! - each leaf column's open page and dictionary, the in-progress row group's encoded pages and
//!   their headers, the file's page index and row group metadata, and while it writes, a
//!   batch's conversions: the writer's Rust heap. It is measured, never estimated: the sink
//!   counts the heap its own calls hold into a `moruna_kernel::Held` (`enter_heap`), through the
//!   counting allocator the engine and the Python extension install. A program without one
//!   falls back to parquet's estimate, `ArrowWriter::memory_size`, times [`FALLBACK_FACTOR`].
//!
//! Every number here was measured, by `tests/lane_heap_measure.rs` (M1 to M8, parquet 59.3.0,
//! zstd 1.5.7, 2026-11-18) unless it says otherwise. Memory bounds the size of the unit of work,
//! the page, never how many lanes there are while one fits (3A's rule for the read side): a
//! lane writes [`CLAIM_PAGE_BYTES`] pages unless its codecs would take more of it than its
//! writer ([`lane_page_bytes`]). Only row group, page, encoding and file boundaries move; the
//! rows are the same.

use parquet::basic::{Compression, Type};
use parquet::schema::types::ColumnPath;

use moruna_kernel::arrow::datatypes::SchemaRef;
use moruna_kernel::{MorunaError, Result};

/// What the writer's heap is taken to be, over parquet's estimate (`ArrowWriter::memory_size`),
/// where no counting allocator measures it: 2 for its buffers' capacity, which one slice can
/// double for every column at once (M8: 64 equal columns grew from under 1.8 MB to 2.69 MB in
/// one slice of 163 rows), times 1.07, the most the heap held over the estimate at its peak
/// (M8). An embedder that installs no counting allocator gets row groups smaller by this.
pub const FALLBACK_FACTOR: f64 = 2.14;

/// The least lane's margin over the parts it counts: the heap over parquet's estimate at its
/// peak (M8: 1.07 at the most for the schemas measured).
const LEAST_MARGIN: f64 = 1.07;

/// The margin over the measured Zstandard compression context (M1): 10%, for what another build
/// of zstd or another level would add.
const CODEC_MARGIN: f64 = 1.10;

/// A Zstandard decompression context, `ZSTD_sizeof_DCtx` (M1): parquet's codec makes one with
/// every column writer, whether it ever decompresses or not.
const ZSTD_DCTX_BYTES: u64 = 95_968;

/// A Zstandard compression context at level 3 after its largest page, `ZSTD_sizeof_CCtx` (M1):
/// (largest page, bytes). Its tables are sized by the page, so it steps up between these points
/// and never down; a context that has compressed a larger page keeps that size (M1).
const ZSTD_CCTX: [(u64, u64); 16] = [
    (256, 29_648),
    (512, 33_680),
    (1 << 10, 41_744),
    (2 << 10, 57_872),
    (4 << 10, 90_128),
    (8 << 10, 154_640),
    (16 << 10, 283_664),
    (32 << 10, 541_712),
    (48 << 10, 603_152),
    (64 << 10, 664_592),
    (96 << 10, 787_472),
    (128 << 10, 910_352),
    (192 << 10, 1_041_424),
    (256 << 10, 1_041_424),
    (512 << 10, 1_303_568),
    (u64::MAX, 1_303_568),
];

/// A Snappy encoder (snap 1.1): its inline table of 1,024 `u16` and the table of up to 16,384
/// it grows for larger blocks, from snap's source; 11,259 bytes measured after a 32 KiB page (M7).
const SNAPPY_BYTES: u64 = 2 * (1 << 10) + 2 * (1 << 14);

/// What an empty dictionary holds before its first value, by its column's kind (M2): a
/// fixed-width column's interner starts its hash index at 4,096 entries; a byte array's starts
/// empty.
const EMPTY_FIXED_DICTIONARY: u64 = 73_792;

/// What a dictionary holds on the heap per byte of its dictionary page, at the most (M2: per
/// distinct value, four-byte strings 57.4 bytes for 8 bytes of page, Int16 and Int32 24.6 for 4,
/// Int64, Float64 and timestamps 31.2 for 8, 32-byte strings 103.3 for 36): its values and its
/// hash index. A dictionary page is bounded by the lane's page, so a full one bounds it.
const DICTIONARY_HEAP_PER_PAGE_BYTE: f64 = 57.4 / 8.0;

/// What an open page holds per value at the most while dictionary encoded: a `u64` index in a
/// buffer that doubles (M3, every kind measured). The least lane counts it.
const OPEN_PAGE_BYTES_PER_VALUE: u64 = 16;

/// What an in-progress page holds besides its encoded bytes: its header's 1,024-byte buffer and
/// its index entries (M4: 1,276.2 a page). The least lane counts it.
const PAGE_UNSEEN_BYTES: u64 = 1_276;

/// What a page costs its file until the file is closed, per leaf column (M6: 58 a page beyond a
/// row group's first).
const INDEX_BYTES_PER_PAGE: u64 = 58;

/// What a row group costs its file until the file is closed, per leaf column, its first page
/// included (M4: 992 for one column; M6: 845 a column of eight).
const METADATA_BYTES_PER_ROW_GROUP: u64 = 992;

/// What a write holds per value while it writes, beyond what it keeps (M5: at most 25.01, an
/// eight-byte or two-byte dictionary column); a byte array holds its bytes besides, which the
/// slice's byte bound limits ([`Bounds::slice`]).
const WRITE_BYTES_PER_VALUE: u64 = 25;

/// The fewest rows a page holds at the least lane.
const MIN_PAGE_ROWS: u64 = 128;

/// The smallest page a lane is cut to: below it a page is mostly its header (M4: 1,276 bytes).
const LEAST_PAGE_BYTES: u64 = 1 << 10;

/// The part of the writer's share a slice's transients may take: an eighth, so a lane flushes
/// its row group by what it holds and not by what one write converts.
const SLICE_SHARE: u64 = 8;

/// The part of a page one slice may bring of its byte arrays' values: an eighth of its byte
/// limit, so a byte array's page closes within an eighth of its limit whatever its values'
/// width (M8: a slice of 1,024 two-KiB strings held 5.8 MB beside a 0.8 MB estimate; of 32,
/// 1.1 MB). A fixed-width column's page is held there by the write batch ([`bounds`]), so its
/// values do not narrow the slice: bounding them too cut the NYC year's slices to 15 rows at
/// 14.5 KB pages and tripled its wall (6.3d host table, 2026-11-18).
const SLICE_PAGE_SHARE: u64 = 8;

/// What a flush holds besides what the writer held before it: the open page it finishes, copied
/// and compressed, one column at a time (see `bounds`).
const FLUSH_PAGES: u64 = 2;

/// The part of what the writer is flushed by that the file's page index and row group metadata
/// may take before the file rolls: a quarter.
const INDEX_SHARE: u64 = 4;

/// The leaf columns parquet writes a schema as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaves {
    /// How many there are.
    pub count: usize,
    /// The fixed-width ones, by path: their dictionaries start with a hash index (M2).
    pub fixed: Vec<ColumnPath>,
    /// The widest fixed-width value, in bytes; a byte array counts its four-byte length.
    pub widest: u64,
    /// How many are byte arrays (strings, binaries): the leaves whose values have no width.
    pub arrays: usize,
    /// The decoded bytes a row's fixed-width top-level fields take (`DataType::primitive_width`);
    /// the rest of a row's decoded bytes may be one byte array's.
    pub fixed_row: u64,
}

/// The leaf columns parquet writes `schema` as (6.3d).
pub fn leaves_of(schema: &SchemaRef) -> Result<Leaves> {
    let descr = parquet::arrow::ArrowSchemaConverter::new()
        .convert(schema)
        .map_err(|e| MorunaError::Sink(format!("parquet writer: {e}")))?;
    let mut fixed = Vec::new();
    let mut widest = 1;
    let mut arrays = 0;
    for column in descr.columns() {
        let width = match column.physical_type() {
            Type::BOOLEAN => 1,
            Type::BYTE_ARRAY => 4,
            Type::INT32 | Type::FLOAT => 4,
            Type::INT64 | Type::DOUBLE => 8,
            Type::INT96 => 12,
            Type::FIXED_LEN_BYTE_ARRAY => column.type_length().max(1) as u64,
        };
        widest = widest.max(width);
        arrays += usize::from(column.physical_type() == Type::BYTE_ARRAY);
        // A fixed-width dictionary starts with a hash index (M2); a byte array's starts empty
        // and a boolean column has none. A fixed-length byte array, not measured, is counted
        // as a fixed width.
        if !matches!(column.physical_type(), Type::BYTE_ARRAY | Type::BOOLEAN) {
            fixed.push(column.path().clone());
        }
    }
    let fixed_row = schema
        .fields()
        .iter()
        .filter_map(|field| field.data_type().primitive_width())
        .map(|width| width as u64)
        .sum();
    Ok(Leaves {
        count: descr.num_columns(),
        fixed,
        widest,
        arrays,
        fixed_row,
    })
}

impl Leaves {
    fn count(&self) -> u64 {
        self.count.max(1) as u64
    }
}

/// What a column writer's codec keeps when its largest page, data or dictionary, is `page`
/// bytes: Zstandard's two contexts (M1, with [`CODEC_MARGIN`] on the compression context),
/// Snappy's tables; the other codecs keep nothing between pages.
pub fn codec_bytes(compression: Compression, page: u64) -> u64 {
    match compression {
        Compression::ZSTD(_) => {
            let cctx = ZSTD_CCTX
                .iter()
                .find(|(largest, _)| page <= *largest)
                .map_or(ZSTD_CCTX[ZSTD_CCTX.len() - 1].1, |(_, bytes)| *bytes);
            ZSTD_DCTX_BYTES + (cctx as f64 * CODEC_MARGIN) as u64
        }
        Compression::SNAPPY => SNAPPY_BYTES,
        _ => 0,
    }
}

/// The largest page a column writer compresses when its page limit is `page`: a page closes
/// once a write batch has taken it past its limit, and the write batch is cut so that is at most
/// an eighth more ([`Bounds::write_batch`]).
fn page_input(page: u64) -> u64 {
    page + page / 8
}

/// What a lane's codecs keep at a page limit of `page` bytes.
fn lane_codecs(leaves: &Leaves, compression: Compression, page: u64) -> u64 {
    leaves
        .count()
        .saturating_mul(codec_bytes(compression, page_input(page)))
}

/// What a lane's writer needs at the least, its codecs aside: an open page of [`MIN_PAGE_ROWS`]
/// rows, a page header and a row group's metadata a column, and one row's transients, under the
/// margin. A lane of fixed-width columns with dictionaries needs their empty indexes besides.
fn writer_least(leaves: &Leaves) -> u64 {
    let column = MIN_PAGE_ROWS * OPEN_PAGE_BYTES_PER_VALUE
        + PAGE_UNSEEN_BYTES
        + METADATA_BYTES_PER_ROW_GROUP
        + WRITE_BYTES_PER_VALUE;
    (leaves.count().saturating_mul(column) as f64 * LEAST_MARGIN) as u64
}

/// The page a lane of `lane` bytes writes, Parquet's `default` unless its codecs would take more
/// of the lane than its writer (system design 3A, 6.3d): then the largest page at which they
/// take no more than the writer, at least [`LEAST_PAGE_BYTES`]. The pages tried are those a
/// compression context steps at (M1), each cut so its largest input, an eighth over the page
/// ([`page_input`]), stays within its step.
pub fn lane_page_bytes(lane: u64, leaves: &Leaves, compression: Compression, default: u64) -> u64 {
    let default = default.max(LEAST_PAGE_BYTES);
    let fits = |page: u64| 2 * lane_codecs(leaves, compression, page) <= lane;
    if fits(default) {
        return default;
    }
    ZSTD_CCTX
        .iter()
        .rev()
        .filter(|(point, _)| *point != u64::MAX)
        .map(|(point, _)| (point * 8 / 9).clamp(LEAST_PAGE_BYTES, default))
        .find(|&page| fits(page))
        .unwrap_or(LEAST_PAGE_BYTES)
}

/// The least lane at which a lane keeps a page of `page` bytes: its codecs at that page twice
/// over, and at least [`least_lane_bytes`].
pub fn lane_bytes_for_page(leaves: &Leaves, compression: Compression, page: u64) -> u64 {
    (2 * lane_codecs(leaves, compression, page)).max(least_lane_bytes(leaves, compression))
}

/// What a lane needs to flush its row groups at `target` encoded bytes, the sink's own target,
/// rather than at its heap, at a page of `page` bytes (system design 3A, 6.3d): its codecs, and a
/// writer whose flush point ([`Bounds::flush_at`]) holds the row group's encoded pages twice over
/// (their buffers grow by doubling, M3), every column's open page (M3, M8) and every column's
/// dictionary full (M2), beside a slice's transients and a flush's two pages. Never less than the lane's need at that page
/// ([`lane_bytes_for_page`]). What the run grants a lane above its claim, up to its room, is
/// what this asks.
pub fn lane_bytes_for_target(
    leaves: &Leaves,
    compression: Compression,
    page: u64,
    target: u64,
) -> u64 {
    let open = leaves.count().saturating_mul(page_input(page));
    // Each column's dictionary, full: its page at most (the lane's) and its hash index (M2).
    let dictionaries = leaves.count().saturating_mul(
        EMPTY_FIXED_DICTIONARY + (page as f64 * DICTIONARY_HEAP_PER_PAGE_BYTE) as u64,
    );
    // A flush point of `held`: the writer less its transients, a slice's (an eighth of it at
    // most) and a flush's, so the writer is the point and those over seven eighths.
    let held = target
        .saturating_mul(2)
        .saturating_add(open)
        .saturating_add(dictionaries)
        .saturating_add(page / SLICE_PAGE_SHARE + FLUSH_PAGES * page);
    let writer = held.saturating_mul(SLICE_SHARE).div_ceil(SLICE_SHARE - 1);
    lane_codecs(leaves, compression, page)
        .saturating_add(writer)
        .max(lane_bytes_for_page(leaves, compression, page))
}

/// The page a lane writes when the budget holds it (system design 3A, 6.3d): the smallest page
/// with no measured loss in bytes or time against the best. A lane's need steps with its
/// compression context (M1), so the page is the largest within the step that showed no loss,
/// the 256 KiB step's: a context of 1.04 MB against 1.30 MB at parquet's 1 MiB, a lane needing a
/// fifth less. Measured on the NYC year copied Parquet to Parquet on 4 workers at 3 GiB, each
/// lane's page forced (6.3d page curve, 2026-11-18): 16 KiB wrote 843 MB with 29% more CPU than
/// 1 MiB, 64 KiB 718 MB, 128 KiB 665-666 MB, this page 665 MB, 256 KiB 669-670 MB and 1 MiB
/// 688-691 MB; against 1 MiB over 8 paired runs this page took 0.4% more wall and 0.3% more CPU
/// (each run to run spread 8%), so no loss in time, and 3.5% fewer bytes.
pub const CLAIM_PAGE_BYTES: u64 = (256 << 10) * 8 / 9;

/// The pages the run tries a sink's writers at, largest first: `top`, then each page a
/// compression context steps at below it (M1), cut so its largest input stays within its step,
/// then [`LEAST_PAGE_BYTES`].
fn pages_from(top: u64) -> Vec<u64> {
    let mut pages = vec![top.max(LEAST_PAGE_BYTES)];
    for (point, _) in ZSTD_CCTX.iter().rev() {
        if *point == u64::MAX {
            continue;
        }
        let page = (point * 8 / 9).max(LEAST_PAGE_BYTES);
        if pages.last().is_some_and(|&last| page < last) {
            pages.push(page);
        }
    }
    if pages.last().is_some_and(|&last| last > LEAST_PAGE_BYTES) {
        pages.push(LEAST_PAGE_BYTES);
    }
    pages
}

/// What the run grants one sink's writers (system design 3A, 6.3d): its lanes, each lane's
/// need, and the page the lanes write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterFit {
    /// The lanes the sink writes on: the run's workers, or fewer when one lane each at the least
    /// page is all the room holds.
    pub lanes: usize,
    /// Each lane's need: its codecs at the page twice over, and at least [`least_lane_bytes`].
    pub lane_bytes: u64,
    /// The page each lane writes ([`lane_page_bytes`] of its need).
    pub page: u64,
}

impl WriterFit {
    /// Every lane's need together.
    pub fn bytes(&self) -> u64 {
        self.lane_bytes.saturating_mul(self.lanes as u64)
    }
}

/// Fit the writers of `sinks` (each its leaf columns and compression) within `room` bytes above
/// the arena (system design 3A, 6.3d): one lane per worker of `workers` at
/// [`CLAIM_PAGE_BYTES`] when that fits; else, memory bounding the page before the lanes (P1),
/// one lane per worker at the largest page that fits; else as many lanes, the same for every
/// sink, as fit at the least page. Each lane is granted its need and no more, so the room left
/// goes to the arena. `Err` with the least room that holds one lane of every sink.
pub fn fit_writers(
    room: u64,
    workers: usize,
    sinks: &[(Leaves, Compression)],
) -> std::result::Result<Vec<WriterFit>, u64> {
    let workers = workers.max(1);
    let fit =
        |lanes: usize, lane_bytes: u64, leaves: &Leaves, compression: Compression| WriterFit {
            lanes,
            lane_bytes,
            page: lane_page_bytes(lane_bytes, leaves, compression, CLAIM_PAGE_BYTES),
        };
    for page in pages_from(CLAIM_PAGE_BYTES) {
        let fits: Vec<WriterFit> = sinks
            .iter()
            .map(|(leaves, compression)| {
                let need = lane_bytes_for_page(leaves, *compression, page);
                fit(workers, need, leaves, *compression)
            })
            .collect();
        let total = fits
            .iter()
            .fold(0u64, |total, fit| total.saturating_add(fit.bytes()));
        if total <= room {
            return Ok(fits);
        }
    }
    let least: Vec<u64> = sinks
        .iter()
        .map(|(leaves, compression)| least_lane_bytes(leaves, *compression))
        .collect();
    let one = least.iter().fold(0u64, |total, l| total.saturating_add(*l));
    if one > room {
        return Err(one);
    }
    let lanes = usize::try_from(room / one.max(1))
        .unwrap_or(usize::MAX)
        .clamp(1, workers);
    Ok(sinks
        .iter()
        .zip(&least)
        .map(|((leaves, compression), &least)| fit(lanes, least, leaves, *compression))
        .collect())
}

/// The least one lane's allowance can be for `leaves` compressed with `compression`: its codecs
/// at the smallest page in no more than half of it, and the writer's least heap beside them.
pub fn least_lane_bytes(leaves: &Leaves, compression: Compression) -> u64 {
    let codecs = lane_codecs(leaves, compression, LEAST_PAGE_BYTES);
    (2 * codecs).max(codecs + writer_least(leaves))
}

/// What a lane's codecs are counted at for a lane of `lane` bytes ([`lane_page_bytes`] at
/// parquet's default page of `default` bytes).
pub fn lane_codec_bytes(lane: u64, leaves: &Leaves, compression: Compression, default: u64) -> u64 {
    lane_codecs(
        leaves,
        compression,
        lane_page_bytes(lane, leaves, compression, default),
    )
}

/// How a Parquet sink's writers share the heap the run gave them (system design 3A, 6.3d).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterLanes {
    /// The lanes the sink writes on: the run's workers, or fewer when one lane's least is more
    /// than a worker's part of the share.
    pub lanes: usize,
    /// Each lane's allowance: the share over the lanes.
    pub lane_bytes: u64,
    /// The least a lane can be for this schema ([`least_lane_bytes`]).
    pub least_lane_bytes: u64,
}

/// The lanes `share` bytes of writer heap hold for `leaves` compressed with `compression`, at
/// most `workers`: as many as each hold at least [`least_lane_bytes`]. `Err` with the least share
/// that holds one lane when even one does not fit.
pub fn writer_lanes(
    share: u64,
    workers: usize,
    leaves: &Leaves,
    compression: Compression,
) -> std::result::Result<WriterLanes, u64> {
    let least = least_lane_bytes(leaves, compression);
    let lanes = usize::try_from(share / least.max(1))
        .unwrap_or(usize::MAX)
        .min(workers.max(1));
    if lanes == 0 {
        return Err(least);
    }
    Ok(WriterLanes {
        lanes,
        lane_bytes: share / lanes as u64,
        least_lane_bytes: least,
    })
}

/// The writer limits of one lane with a heap allowance (6.3d).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Bounds {
    /// The lane's allowance, bytes.
    pub allowance: u64,
    /// What its columns' codecs keep, out of the allowance.
    pub codec_bytes: u64,
    /// Flush the row group before the writer's heap passes this ([`Bounds::flush`]): the
    /// allowance less the codecs and a slice's transients.
    pub flush_at: u64,
    /// `data_page_size_limit`: parquet's, or the page [`lane_page_bytes`] cuts it to.
    pub page_bytes: usize,
    /// `dictionary_page_size_limit`: parquet's, no larger than the page, since a dictionary
    /// page goes through the same codec as a data page.
    pub dictionary_bytes: usize,
    /// `data_page_row_count_limit`: parquet's.
    pub page_rows: usize,
    /// `write_batch_size`: so a page runs no more than an eighth past its byte limit.
    pub write_batch: usize,
    /// The fixed-width columns to write without a dictionary: all of them when their empty hash
    /// indexes would not fit beside the writer's least, else none.
    pub plain: Vec<ColumnPath>,
    /// The most rows of a batch written at once, by their conversions' transients.
    pub slice_rows: usize,
    /// Whether a slice is bounded by its bytes too: the schema has byte arrays.
    pub arrays: bool,
    /// The decoded bytes a row's fixed-width fields take ([`Leaves::fixed_row`]).
    pub fixed_row: u64,
    /// Roll the file before a morsel would take its page index and row group metadata past
    /// this.
    pub index_share: u64,
    /// The schema's leaf columns.
    pub leaves: usize,
}

/// The limits that keep one lane's writer and its columns' codecs within `allowance` bytes for
/// `leaves` compressed with `compression`, from parquet's own limits (`page_rows`,
/// `page_bytes`, `dictionary_bytes` and `write_batch` as the writer's properties have them),
/// which are only ever lowered.
pub(crate) fn bounds(
    allowance: u64,
    leaves: &Leaves,
    compression: Compression,
    page_rows: usize,
    page_bytes: usize,
    dictionary_bytes: usize,
    write_batch: usize,
) -> Bounds {
    let count = leaves.count();
    let page = lane_page_bytes(allowance, leaves, compression, page_bytes as u64);
    let codec = lane_codecs(leaves, compression, page);
    let writer = allowance.saturating_sub(codec);
    let slice_rows = (writer / SLICE_SHARE / (count * WRITE_BYTES_PER_VALUE)).max(1);
    // A slice's transients: its values' conversions (M5), and the bytes it brings, an eighth of
    // a page at the most ([`Bounds::slice`]). A flush's: it finishes one column's open page at a
    // time, copying it whole and compressing the copy (parquet 59.3.0,
    // `GenericColumnWriter::add_data_page`), two pages at the most (M8: a flush peaked 0.1 MB
    // over the write before it for 1 MiB pages; si_lane_heap.rs's 2 KiB strings, 0.18 MB at
    // 87 KiB pages).
    let transient =
        slice_rows * count * WRITE_BYTES_PER_VALUE + page / SLICE_PAGE_SHARE + FLUSH_PAGES * page;
    let flush_at = writer.saturating_sub(transient);
    let empties = leaves.fixed.len() as u64 * EMPTY_FIXED_DICTIONARY;
    let plain = if (empties as f64 * LEAST_MARGIN) as u64 + writer_least(leaves) > flush_at {
        leaves.fixed.clone()
    } else {
        Vec::new()
    };
    let to_usize = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
    let page = to_usize(page).min(page_bytes);
    Bounds {
        allowance,
        codec_bytes: codec,
        flush_at,
        page_bytes: page,
        dictionary_bytes: dictionary_bytes.min(page),
        page_rows,
        write_batch: write_batch.min(to_usize((page as u64 / (8 * leaves.widest.max(1))).max(1))),
        plain,
        slice_rows: to_usize(slice_rows),
        arrays: leaves.arrays > 0,
        fixed_row: leaves.fixed_row,
        index_share: flush_at / INDEX_SHARE,
        leaves: leaves.count.max(1),
    }
}

impl Bounds {
    /// The pages `rows` rows of `bytes` decoded bytes make: a page closes at `page_rows` rows or
    /// `page_bytes` bytes, whichever comes first, and each row group starts a page per column.
    fn pages(&self, rows: u64, bytes: u64, row_groups: u64) -> u64 {
        let leaves = self.leaves as u64;
        leaves
            .saturating_mul(
                rows.div_ceil((self.page_rows as u64).max(1))
                    .saturating_add(row_groups),
            )
            .saturating_add(bytes / (self.page_bytes as u64).max(1))
    }

    /// What a file's page index and row group metadata hold for `rows` rows of `bytes` decoded
    /// bytes in `row_groups` row groups (M4, M6): an upper estimate, which decides when a file
    /// rolls; what it holds is in the measured heap besides.
    pub(crate) fn index(&self, rows: u64, bytes: u64, row_groups: u64) -> u64 {
        self.pages(rows, bytes, row_groups)
            .saturating_mul(INDEX_BYTES_PER_PAGE)
            .saturating_add(
                (self.leaves as u64)
                    .saturating_mul(row_groups)
                    .saturating_mul(METADATA_BYTES_PER_ROW_GROUP),
            )
    }

    /// The rows of a morsel of `rows` rows and `bytes` decoded bytes to write at once: at most
    /// [`Bounds::slice_rows`], and where the schema has byte arrays, no more than bring an
    /// eighth of a page of their values, taken to be all of a row's bytes beyond its fixed-width
    /// fields and all in one column.
    pub(crate) fn slice(&self, rows: usize, bytes: u64) -> usize {
        if !self.arrays {
            return self.slice_rows.max(1);
        }
        let per_row = bytes.div_ceil(rows.max(1) as u64);
        let variable = per_row.saturating_sub(self.fixed_row).max(1);
        let by_bytes = (self.page_bytes as u64 / SLICE_PAGE_SHARE / variable).max(1);
        self.slice_rows
            .min(usize::try_from(by_bytes).unwrap_or(usize::MAX))
            .max(1)
    }

    /// Whether the row group is flushed now. Measured (`held`, the heap the writer's calls
    /// hold, and `jump`, the most it grew in one slice of this row group but its first): once
    /// that and twice the jump reach [`Bounds::flush_at`], since a slice grows the writer by its
    /// buffers doubling and the next doubling is at most twice the last (M3, M8). Unmeasured
    /// (`memory`, parquet's estimate): once that times [`FALLBACK_FACTOR`] does.
    pub(crate) fn flush(&self, measured: Option<(u64, u64)>, memory: u64) -> bool {
        match measured {
            Some((held, jump)) => held.saturating_add(jump.saturating_mul(2)) >= self.flush_at,
            None => (memory as f64 * FALLBACK_FACTOR) as u64 >= self.flush_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
    use parquet::basic::ZstdLevel;

    use super::*;

    const ROWS: usize = 20_000;
    const MIB: usize = 1 << 20;

    fn zstd() -> Compression {
        Compression::ZSTD(ZstdLevel::default())
    }

    fn leaves(types: &[DataType]) -> Leaves {
        let fields: Vec<Field> = types
            .iter()
            .enumerate()
            .map(|(i, t)| Field::new(format!("c{i}"), t.clone(), true))
            .collect();
        leaves_of(&Arc::new(Schema::new(fields))).expect("leaves")
    }

    /// NYC's 19 columns: four Int32, four Int64 (two of them timestamps), ten Float64, a string.
    fn taxi() -> Leaves {
        let mut types = vec![DataType::Int32; 4];
        types.extend(vec![DataType::Int64; 4]);
        types.extend(vec![DataType::Float64; 10]);
        types.push(DataType::Utf8);
        leaves(&types)
    }

    #[test]
    fn the_leaves_are_counted_by_kind() {
        let l = leaves(&[
            DataType::Int8,
            DataType::Utf8,
            DataType::Boolean,
            DataType::Float64,
        ]);
        assert_eq!(l.count, 4);
        assert_eq!(l.fixed.len(), 2, "Int8 and Float64 start a hash index");
        assert_eq!(l.widest, 8);
        assert_eq!(l.arrays, 1);
        assert_eq!(
            l.fixed_row,
            1 + 8,
            "Int8 and Float64; a boolean has no byte width"
        );
        assert_eq!(leaves(&[DataType::Utf8]).widest, 4);
        assert!(leaves(&[DataType::Utf8]).fixed.is_empty());
    }

    /// The codecs' bytes are over every measured context (M1) by the stated margin, and step up
    /// with the page, never down.
    #[test]
    fn a_codec_is_counted_over_what_was_measured() {
        for (page, measured) in ZSTD_CCTX.iter().take(ZSTD_CCTX.len() - 1) {
            let counted = codec_bytes(zstd(), *page);
            assert_eq!(
                counted,
                ZSTD_DCTX_BYTES + (*measured as f64 * CODEC_MARGIN) as u64
            );
        }
        assert_eq!(
            codec_bytes(zstd(), 3 << 10),
            codec_bytes(zstd(), 4 << 10),
            "between points, the next one"
        );
        assert_eq!(codec_bytes(zstd(), 64 << 20), codec_bytes(zstd(), 1 << 20));
        let mut last = 0;
        for page in (0..24).map(|s| 1u64 << s) {
            assert!(codec_bytes(zstd(), page) >= last);
            last = codec_bytes(zstd(), page);
        }
        assert_eq!(codec_bytes(Compression::SNAPPY, 1 << 20), SNAPPY_BYTES);
        assert_eq!(codec_bytes(Compression::UNCOMPRESSED, 1 << 20), 0);
        assert_eq!(codec_bytes(Compression::LZ4_RAW, 1 << 20), 0);
    }

    /// P1: a lane keeps parquet's page while its codecs take no more of it than its writer; a
    /// smaller lane cuts the page, halving, until they do.
    #[test]
    fn a_lane_keeps_parquets_page_while_its_codecs_take_no_more_than_its_writer() {
        let taxi = taxi();
        let default = 1 << 20;
        let keep = lane_bytes_for_page(&taxi, zstd(), default);
        assert_eq!(lane_page_bytes(keep, &taxi, zstd(), default), default);
        let cut = lane_page_bytes(keep - 1, &taxi, zstd(), default);
        assert!(cut < default && 2 * lane_codecs(&taxi, zstd(), cut) < keep);
        for lane in [26_000_000u64, 52_000_000, 80_000_000] {
            let page = lane_page_bytes(lane, &taxi, zstd(), default);
            assert!(
                2 * lane_codecs(&taxi, zstd(), page) <= lane,
                "{lane}: {page}"
            );
            // The page sits at a step of the context, cut so its largest input stays in it.
            assert!(
                page == default || ZSTD_CCTX.iter().any(|(point, _)| page == point * 8 / 9),
                "{lane}: {page}"
            );
            assert_eq!(
                lane_codec_bytes(lane, &taxi, zstd(), default),
                lane_codecs(&taxi, zstd(), page)
            );
        }
        // Without a codec that keeps state, the page is parquet's whatever the lane.
        assert_eq!(
            lane_page_bytes(1 << 20, &taxi, Compression::UNCOMPRESSED, default),
            default
        );
        // A page is never cut below the least.
        assert_eq!(lane_page_bytes(1, &taxi, zstd(), default), LEAST_PAGE_BYTES);
    }

    /// A share holds as many lanes as each hold the least a lane can be, at most the workers;
    /// none is refused with the least share.
    #[test]
    fn the_lanes_are_capped_by_the_least_lane() {
        let wide = leaves(&vec![DataType::Int8; 96]);
        let least = least_lane_bytes(&wide, zstd());
        assert_eq!(least, 2 * lane_codecs(&wide, zstd(), LEAST_PAGE_BYTES));
        assert_eq!(
            writer_lanes(least * 2, 2, &wide, zstd()),
            Ok(WriterLanes {
                lanes: 2,
                lane_bytes: least,
                least_lane_bytes: least
            })
        );
        let capped = writer_lanes(least * 3 / 2, 2, &wide, zstd()).expect("one lane");
        assert_eq!((capped.lanes, capped.lane_bytes), (1, least * 3 / 2));
        assert_eq!(writer_lanes(least - 1, 2, &wide, zstd()), Err(least));
        assert_eq!(
            writer_lanes(10 << 30, 8, &taxi(), zstd()).map(|l| l.lanes),
            Ok(8)
        );
        // Without a codec's state, a lane needs the writer's least alone.
        let plain = leaves(&vec![DataType::Int64; 10]);
        assert_eq!(
            least_lane_bytes(&plain, Compression::UNCOMPRESSED),
            writer_least(&plain)
        );
    }

    /// At a large lane the writer keeps parquet's limits; the codecs are counted at parquet's
    /// page; dictionaries stay.
    #[test]
    fn a_large_lane_keeps_parquets_limits() {
        let b = bounds(10 << 30, &taxi(), zstd(), ROWS, MIB, MIB, 1024);
        assert_eq!(
            (b.page_bytes, b.dictionary_bytes, b.page_rows),
            (MIB, MIB, ROWS)
        );
        assert_eq!(b.write_batch, 1024);
        let writer = (10u64 << 30) - b.codec_bytes;
        assert_eq!(
            b.slice_rows as u64,
            writer / 8 / (19 * WRITE_BYTES_PER_VALUE)
        );
        assert!(b.plain.is_empty());
        assert_eq!(b.codec_bytes, lane_codecs(&taxi(), zstd(), MIB as u64));
        assert_eq!(
            b.flush_at,
            writer
                - b.slice_rows as u64 * 19 * WRITE_BYTES_PER_VALUE
                - MIB as u64 / 8
                - 2 * MIB as u64
        );
        assert_eq!(b.index_share, b.flush_at / 4);
    }

    /// A small lane cuts the page and the dictionary page with it, writes in smaller slices and
    /// batches, and turns fixed-width dictionaries off when their empty indexes do not fit.
    #[test]
    fn a_small_lane_cuts_its_pages_and_slices() {
        let taxi = taxi();
        let b = bounds(26_000_000, &taxi, zstd(), ROWS, MIB, MIB, 1024);
        assert!(b.page_bytes < MIB && b.dictionary_bytes == b.page_bytes);
        assert!(b.codec_bytes <= 13_000_000);
        assert!(b.write_batch as u64 <= (b.page_bytes as u64 / 64).max(1));
        assert!(b.plain.is_empty(), "19 empty indexes fit: {b:?}");
        assert!(b.flush_at + b.codec_bytes <= 26_000_000);
        let wide = leaves(&vec![DataType::Int8; 384]);
        let least = least_lane_bytes(&wide, zstd());
        let tight = bounds(least, &wide, zstd(), ROWS, MIB, MIB, 1024);
        // The least page's codec context is the 2 KiB step's, so the lane writes the largest
        // page within that step.
        assert_eq!(tight.page_bytes as u64, (2 << 10) * 8 / 9);
        assert!(tight.slice_rows < 1024, "{tight:?}");
        assert!(
            tight.plain.is_empty(),
            "the least lane holds the empty indexes"
        );
        let plain = bounds(
            2 * writer_least(&wide),
            &wide,
            Compression::UNCOMPRESSED,
            ROWS,
            MIB,
            MIB,
            1024,
        );
        assert_eq!(
            plain.plain.len(),
            384,
            "384 empty indexes do not fit: {plain:?}"
        );
        // A writer's own lower limits stand.
        let own = bounds(10 << 30, &taxi, zstd(), 500, 4096, 2048, 64);
        assert_eq!(
            (
                own.page_rows,
                own.page_bytes,
                own.dictionary_bytes,
                own.write_batch
            ),
            (500, 4096, 2048, 64)
        );
    }

    #[test]
    fn the_index_grows_with_pages_row_groups_and_columns() {
        let b = bounds(
            10 << 30,
            &leaves(&[DataType::Int64]),
            zstd(),
            1000,
            MIB,
            MIB,
            1024,
        );
        assert_eq!(b.index(0, 0, 0), 0);
        let one = b.index(1000, 0, 1);
        assert_eq!(one, 2 * INDEX_BYTES_PER_PAGE + METADATA_BYTES_PER_ROW_GROUP);
        assert!(b.index(2000, 0, 1) > one);
        assert_eq!(b.index(0, 3 * MIB as u64, 0), 3 * INDEX_BYTES_PER_PAGE);
    }

    /// Measured, a row group is flushed once the heap the writer's calls hold and twice the most
    /// it grew in one slice reach the threshold; unmeasured, once parquet's estimate times the
    /// fallback factor does.
    #[test]
    fn a_row_group_is_flushed_by_the_measured_heap_or_the_fallback() {
        let b = bounds(64 << 20, &taxi(), zstd(), ROWS, MIB, MIB, 1024);
        let at = b.flush_at;
        assert!(!b.flush(Some((at - 2001, 1_000)), 0));
        assert!(b.flush(Some((at - 2000, 1_000)), 0), "twice the last jump");
        assert!(
            !b.flush(Some((at - 1, 0)), 1 << 40),
            "the estimate is not read"
        );
        assert!(b.flush(Some((at, 0)), 0));
        let fallback = (at as f64 / FALLBACK_FACTOR) as u64 + 1;
        assert!(b.flush(None, fallback));
        assert!(!b.flush(None, fallback - 2));
    }

    /// A slice brings no more than an eighth of a page of byte arrays' values, taking a row's
    /// bytes beyond its fixed-width fields to be one column's; a schema without byte arrays is
    /// sliced by its conversions alone, its pages held by the write batch.
    #[test]
    fn a_slice_brings_no_more_than_an_eighth_of_a_page_of_byte_arrays() {
        let b = bounds(10 << 30, &taxi(), zstd(), ROWS, MIB, MIB, 1024);
        assert_eq!(taxi().fixed_row, 128);
        assert_eq!(
            b.slice(5_000, 5_000 * 152),
            MIB / 8 / (152 - 128),
            "narrow rows: an eighth of a MiB page of the string's 24 bytes a row"
        );
        assert_eq!(
            b.slice(5_000, 5_000 * 4096),
            MIB / 8 / (4096 - 128),
            "a wide string"
        );
        assert_eq!(b.slice(10, 10 << 30), 1, "at least a row");
        assert_eq!(b.slice(0, 0), MIB / 8);
        let numbers = leaves(&vec![DataType::Int64; 4]);
        let n = bounds(26_000_000, &numbers, zstd(), ROWS, MIB, MIB, 1024);
        assert_eq!(n.slice(100_000, 100_000 * 32), n.slice_rows);
        assert_eq!(n.slice(100_000, 100_000 << 20), n.slice_rows);
        assert!(n.slice_rows > 1024, "no fixed cap on rows: {n:?}");
    }
    /// The run fits the writers in order (6.3d): one lane per worker at the claim's page when
    /// the room holds it; else the largest page that holds one lane per worker (P1); else fewer
    /// lanes at the least page; else the least room that holds one lane each. Each lane is
    /// granted its need and no more.
    #[test]
    fn the_writers_are_fitted_page_first_then_lanes() {
        let taxi = taxi();
        let sinks = [(taxi.clone(), zstd())];
        let at_claim = lane_bytes_for_page(&taxi, zstd(), CLAIM_PAGE_BYTES);
        assert_eq!(
            at_claim,
            2 * 19 * codec_bytes(zstd(), page_input(CLAIM_PAGE_BYTES)),
            "the claim's page is in the 256 KiB step"
        );
        assert_eq!(
            codec_bytes(zstd(), page_input(CLAIM_PAGE_BYTES)),
            ZSTD_DCTX_BYTES + (1_041_424.0 * CODEC_MARGIN) as u64
        );
        let ample = fit_writers(10 << 30, 4, &sinks).expect("fits");
        assert_eq!(
            ample,
            vec![WriterFit {
                lanes: 4,
                lane_bytes: at_claim,
                page: CLAIM_PAGE_BYTES
            }]
        );
        assert_eq!(ample[0].bytes(), 4 * at_claim);
        assert_eq!(fit_writers(4 * at_claim, 4, &sinks), Ok(ample.clone()));
        // A little less: the page shrinks, the lanes stay.
        let cut = fit_writers(4 * at_claim - 1, 4, &sinks).expect("fits")[0];
        assert_eq!(cut.lanes, 4);
        assert!(cut.page < CLAIM_PAGE_BYTES, "{cut:?}");
        assert!(cut.bytes() < 4 * at_claim);
        assert_eq!(cut.lane_bytes, lane_bytes_for_page(&taxi, zstd(), cut.page));
        // Less than four lanes at the least page: fewer lanes, each its least.
        let least = least_lane_bytes(&taxi, zstd());
        let few = fit_writers(3 * least, 4, &sinks).expect("fits")[0];
        assert_eq!((few.lanes, few.lane_bytes), (3, least));
        assert_eq!(fit_writers(least - 1, 4, &sinks), Err(least));
        // Two sinks share the room, the same page and lanes each.
        let int64 = leaves(&[DataType::Int64]);
        let two = [(taxi.clone(), zstd()), (int64.clone(), zstd())];
        let both = fit_writers(10 << 30, 2, &two).expect("fits");
        assert_eq!(both.len(), 2);
        assert!(
            both.iter()
                .all(|fit| fit.lanes == 2 && fit.page == CLAIM_PAGE_BYTES)
        );
        let one = least + least_lane_bytes(&int64, zstd());
        assert_eq!(fit_writers(one - 1, 2, &two), Err(one));
        assert!(
            fit_writers(one, 2, &two)
                .expect("fits")
                .iter()
                .all(|f| f.lanes == 1)
        );
        // The pages tried step down to the least.
        let pages = pages_from(CLAIM_PAGE_BYTES);
        assert_eq!(pages.first(), Some(&CLAIM_PAGE_BYTES));
        assert_eq!(pages.last(), Some(&LEAST_PAGE_BYTES));
        assert!(pages.windows(2).all(|w| w[0] > w[1]), "{pages:?}");
    }
    /// A lane granted its row group target's need flushes at the target and not at its heap: its
    /// flush point holds the target's pages twice over, every open page and every dictionary
    /// full; it never needs less than its page's need.
    #[test]
    fn a_lane_granted_its_target_flushes_at_the_target() {
        let taxi = taxi();
        let page = CLAIM_PAGE_BYTES;
        assert!(
            lane_bytes_for_target(&taxi, zstd(), page, 0)
                >= lane_bytes_for_page(&taxi, zstd(), page)
        );
        let target = 16u64 << 20;
        let lane = lane_bytes_for_target(&taxi, zstd(), page, target);
        // As the sink asks: parquet's limits, the page lowered to the claim's.
        let b = bounds(lane, &taxi, zstd(), ROWS, page as usize, MIB, 1024);
        assert_eq!(b.page_bytes as u64, page, "the claim's page");
        assert!(
            b.flush_at >= 2 * target + 19 * page_input(page) + 19 * 7 * page,
            "{b:?}"
        );
        assert!(
            lane < lane_codecs(&taxi, zstd(), page) + 3 * target + 19 * 10 * page,
            "{lane}"
        );
        assert_eq!(crate::lane_file_bytes(1 << 30, 4), crate::LANE_MAX_BYTES);
        assert_eq!(crate::lane_file_bytes(32 << 20, 4), 16 << 20);
        assert_eq!(crate::lane_file_bytes(0, 4), 64 << 10);
    }
}
