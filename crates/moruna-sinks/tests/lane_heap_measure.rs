//! 6.3d: the measurements the lane heap model (`src/lane_heap.rs`) is built from. Each prints
//! what it measured; the model's constants cite them. They are ignored in the suite because they
//! report rather than check: run them with
//!
//! ```text
//! cargo test -p moruna-sinks --test lane_heap_measure -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The heap is this binary's Rust allocations, counted exactly, one writer at a time. A
//! Zstandard context lives in the C library's memory, which no Rust allocator sees, and is read
//! with `ZSTD_sizeof_CCtx` / `ZSTD_sizeof_DCtx`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use moruna_kernel::arrow::array::{
    ArrayRef, BooleanArray, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array,
    RecordBatch, StringArray, TimestampMicrosecondArray,
};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

/// Bytes in use, and the most since the last reset.
static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

struct Counting;

fn counted(delta: i64) {
    let live = LIVE.fetch_add(delta, Ordering::SeqCst) + delta;
    PEAK.fetch_max(live, Ordering::SeqCst);
}

// SAFETY: every call is `System`'s with the arguments it was given; the counting is atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `System.alloc`.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            counted(layout.size() as i64);
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `System.alloc_zeroed`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            counted(layout.size() as i64);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as `System.dealloc`.
        unsafe { System.dealloc(ptr, layout) };
        counted(-(layout.size() as i64));
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `System.realloc`.
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            counted(new_size as i64 - layout.size() as i64);
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn live() -> i64 {
    LIVE.load(Ordering::SeqCst)
}

fn reset_peak() -> i64 {
    let now = live();
    PEAK.store(now, Ordering::SeqCst);
    now
}

fn peak() -> i64 {
    PEAK.load(Ordering::SeqCst)
}

/// The column types measured, as the arrays a writer is given, one leaf each.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Bool,
    Int8,
    Int16,
    Int32,
    Int64,
    Float64,
    Micros,
    Str(usize),
}

const KINDS: [Kind; 10] = [
    Kind::Bool,
    Kind::Int8,
    Kind::Int16,
    Kind::Int32,
    Kind::Int64,
    Kind::Float64,
    Kind::Micros,
    Kind::Str(4),
    Kind::Str(32),
    Kind::Str(256),
];

/// `rows` values of `kind` from `from`, of `distinct` distinct values (wrapping), nullable.
fn column(kind: Kind, from: i64, rows: usize, distinct: i64) -> ArrayRef {
    let v = |r: usize| (from + r as i64) % distinct.max(1);
    match kind {
        Kind::Bool => Arc::new(BooleanArray::from_iter(
            (0..rows).map(|r| Some(v(r) % 2 == 0)),
        )),
        Kind::Int8 => Arc::new(Int8Array::from_iter((0..rows).map(|r| Some(v(r) as i8)))),
        Kind::Int16 => Arc::new(Int16Array::from_iter((0..rows).map(|r| Some(v(r) as i16)))),
        Kind::Int32 => Arc::new(Int32Array::from_iter((0..rows).map(|r| Some(v(r) as i32)))),
        Kind::Int64 => Arc::new(Int64Array::from_iter((0..rows).map(|r| Some(v(r) * 7_919)))),
        Kind::Float64 => Arc::new(Float64Array::from_iter(
            (0..rows).map(|r| Some(v(r) as f64 / 7.0)),
        )),
        Kind::Micros => Arc::new(TimestampMicrosecondArray::from_iter(
            (0..rows).map(|r| Some(1_700_000_000_000_000 + v(r) * 1_000_003)),
        )),
        Kind::Str(len) => Arc::new(StringArray::from_iter((0..rows).map(|r| {
            let mut s = format!("{:0width$}", v(r), width = len.min(20));
            while s.len() < len {
                s.push(char::from(b'a' + (s.len() % 26) as u8));
            }
            Some(s)
        }))),
    }
}

fn data_type(kind: Kind) -> DataType {
    match kind {
        Kind::Bool => DataType::Boolean,
        Kind::Int8 => DataType::Int8,
        Kind::Int16 => DataType::Int16,
        Kind::Int32 => DataType::Int32,
        Kind::Int64 => DataType::Int64,
        Kind::Float64 => DataType::Float64,
        Kind::Micros => DataType::Timestamp(TimeUnit::Microsecond, None),
        Kind::Str(_) => DataType::Utf8,
    }
}

fn batch(kind: Kind, from: i64, rows: usize, distinct: i64) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![Field::new("c", data_type(kind), true)]));
    RecordBatch::try_new(schema, vec![column(kind, from, rows, distinct)]).expect("a batch")
}

fn props(dictionary: bool, page_rows: usize, compression: Compression) -> WriterProperties {
    WriterProperties::builder()
        .set_dictionary_enabled(dictionary)
        .set_dictionary_page_size_limit(64 << 20)
        .set_data_page_size_limit(64 << 20)
        .set_data_page_row_count_limit(page_rows)
        .set_write_batch_size(page_rows.min(1024))
        .set_max_row_group_row_count(None)
        .set_compression(compression)
        .build()
}

/// What a writer of `kind` holds after `rows` rows of `distinct` values, written in batches of
/// `step` rows; and the most it held while writing, both over what it held before its first row.
fn held(
    kind: Kind,
    rows: usize,
    distinct: i64,
    dictionary: bool,
    page_rows: usize,
    step: usize,
) -> (i64, i64, usize) {
    let batches: Vec<RecordBatch> = (0..rows.div_ceil(step))
        .map(|b| {
            let from = (b * step) as i64;
            batch(kind, from, step.min(rows - b * step), distinct)
        })
        .collect();
    let schema = batches[0].schema();
    let mut writer = ArrowWriter::try_new(
        Vec::<u8>::with_capacity(64 << 20),
        schema,
        Some(props(dictionary, page_rows, Compression::UNCOMPRESSED)),
    )
    .expect("a writer");
    let base = reset_peak();
    for b in &batches {
        writer.write(b).expect("written");
    }
    let after = live() - base;
    let most = peak() - base;
    let estimate = writer.memory_size();
    drop(writer);
    (after, most, estimate)
}

/// M1: Zstandard's contexts, by the largest page compressed (zstd 1.5.7, level 3).
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn m1_zstd_contexts() {
    let dctx = zstd::zstd_safe::DCtx::create().sizeof();
    println!("M1 dctx_bytes={dctx}");
    for page in [
        256usize,
        512,
        1 << 10,
        2 << 10,
        4 << 10,
        8 << 10,
        16 << 10,
        32 << 10,
        48 << 10,
        64 << 10,
        96 << 10,
        128 << 10,
        192 << 10,
        256 << 10,
        512 << 10,
        1 << 20,
        2 << 20,
        8 << 20,
    ] {
        let noisy: Vec<u8> = (0..page)
            .map(|i| ((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8)
            .collect();
        let plain: Vec<u8> = (0..page).map(|i| (i % 7) as u8).collect();
        let mut sizes = Vec::new();
        for data in [&noisy, &plain] {
            let mut c = zstd::bulk::Compressor::new(3).expect("a compressor");
            let _ = c.compress(data).expect("compressed");
            sizes.push(c.context_mut().sizeof());
        }
        // A context that compressed a larger page first and this one after keeps the larger.
        let mut kept = zstd::bulk::Compressor::new(3).expect("a compressor");
        let _ = kept.compress(&vec![1u8; page * 2]).expect("compressed");
        let _ = kept.compress(&noisy).expect("compressed");
        println!(
            "M1 page={page} cctx_noisy={} cctx_plain={} cctx_after_twice={}",
            sizes[0],
            sizes[1],
            kept.context_mut().sizeof()
        );
    }
}

/// M2: what a column's dictionary holds per distinct value, by type: the retained heap's slope
/// between 20,000 and 40,000 distinct values, pages cut at 1,024 rows so the open page stays
/// small. The slope also holds the buffered dictionary-encoded pages, a few bits a row.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn m2_dictionary_per_distinct_value() {
    for kind in KINDS {
        let (a, _, ea) = held(kind, 20_000, 20_000, true, 1024, 4096);
        let (b, _, eb) = held(kind, 40_000, 40_000, true, 1024, 4096);
        let (none, _, _) = held(kind, 1, 1, false, 1024, 1);
        let (empty, _, _) = held(kind, 1, 1, true, 1024, 1);
        println!(
            "M2 kind={kind:?} per_distinct={:.1} estimate_per_distinct={:.1} empty_dictionary={}",
            (b - a) as f64 / 20_000.0,
            (eb as f64 - ea as f64) / 20_000.0,
            empty - none
        );
    }
}

/// M3: what an open page holds per value, by type, with a dictionary of one value and without
/// one: the most retained per row while a page of up to 20,000 rows fills, rows written 1,024 at a
/// time.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn m3_open_page_per_value() {
    for kind in KINDS {
        for dictionary in [true, false] {
            let mut most = 0.0f64;
            for rows in [1025usize, 2049, 4097, 8193, 16_385, 19_999] {
                let (after, _, _) = held(kind, rows, 1, dictionary, 20_000, 1024);
                let (one, _, _) = held(kind, 1, 1, dictionary, 20_000, 1);
                most = most.max((after - one) as f64 / (rows - 1) as f64);
            }
            println!("M3 kind={kind:?} dictionary={dictionary} per_value={most:.2}");
        }
    }
}

/// M4: what a closed page of the in-progress row group holds besides its encoded bytes, and
/// what each page and each row group cost the file after the row group is written: pages of 128
/// rows, plain, uncompressed, one Int64 column.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn m4_pages_and_row_groups() {
    let kind = Kind::Int64;
    let page = 128usize;
    for pages in [64usize, 256] {
        let (after, _, _) = held(kind, page * pages, 1 << 40, false, page, page);
        let data = (page * pages * 8) as i64;
        println!(
            "M4 in_progress pages={pages} per_page_beyond_data={:.1}",
            (after - data) as f64 / pages as f64
        );
    }
    // After a flush: what the file keeps per page and per row group until it closes.
    let schema = batch(kind, 0, 1, 1).schema();
    for (groups, pages) in [(1usize, 256usize), (64, 1), (64, 4)] {
        let mut writer = ArrowWriter::try_new(
            Vec::<u8>::with_capacity(64 << 20),
            Arc::clone(&schema),
            Some(props(false, page, Compression::UNCOMPRESSED)),
        )
        .expect("a writer");
        let base = live();
        let mut from = 0i64;
        for _ in 0..groups {
            for _ in 0..pages {
                writer
                    .write(&batch(kind, from, page, 1 << 40))
                    .expect("written");
                from += page as i64;
            }
            writer.flush().expect("flushed");
        }
        // The output is a vector with room for the whole file, so what is kept is the file's
        // metadata: its row groups' column chunks and their pages' index entries.
        let kept = live() - base;
        println!(
            "M4 flushed groups={groups} pages_per_group={pages} kept={kept} per_group={:.1} per_page={:.1}",
            kept as f64 / groups as f64,
            kept as f64 / (groups * pages) as f64
        );
        drop(writer);
    }
}

/// M5: what a write holds while it writes, beyond what it keeps, per value, by type: 16,384
/// rows in one batch into a fresh plain writer.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn m5_write_transients() {
    for kind in KINDS {
        for dictionary in [true, false] {
            let (after, most, _) = held(kind, 16_384, 1 << 40, dictionary, 1 << 20, 16_384);
            println!(
                "M5 kind={kind:?} dictionary={dictionary} transient_per_value={:.2}",
                (most - after) as f64 / 16_384.0
            );
        }
    }
}

/// `columns` Int64 columns of `rows` distinct values from `from`.
fn wide(columns: usize, from: i64, rows: usize) -> RecordBatch {
    let fields: Vec<Field> = (0..columns)
        .map(|c| Field::new(format!("c{c}"), DataType::Int64, true))
        .collect();
    let arrays: Vec<ArrayRef> = (0..columns)
        .map(|c| column(Kind::Int64, from + c as i64, rows, 1 << 40))
        .collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("a batch")
}

/// M6: what the file keeps per row group and per page, per column, until it closes: 8 Int64
/// columns, plain, uncompressed, pages of 128 rows, written and flushed into a vector with room
/// for the whole file.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn m6_file_metadata_per_column() {
    let page = 128usize;
    for (groups, pages) in [(32usize, 1usize), (32, 8)] {
        let mut writer = ArrowWriter::try_new(
            Vec::<u8>::with_capacity(64 << 20),
            wide(8, 0, 1).schema(),
            Some(props(false, page, Compression::UNCOMPRESSED)),
        )
        .expect("a writer");
        let base = live();
        let mut from = 0i64;
        for _ in 0..groups {
            for _ in 0..pages {
                writer.write(&wide(8, from, page)).expect("written");
                from += page as i64;
            }
            writer.flush().expect("flushed");
        }
        let kept = live() - base;
        println!(
            "M6 groups={groups} pages_per_group={pages} kept={kept} per_group_per_column={:.1}",
            kept as f64 / (groups * 8) as f64
        );
        drop(writer);
    }
}

/// M7: what a column writer's Snappy codec keeps, on the Rust heap: the same 64 KiB page
/// compressed and not.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn m7_snappy_codec() {
    let rows = 8192usize;
    let mut kept = Vec::new();
    for compression in [Compression::UNCOMPRESSED, Compression::SNAPPY] {
        let b = batch(Kind::Int64, 0, rows, 1 << 40);
        let mut writer = ArrowWriter::try_new(
            Vec::<u8>::with_capacity(64 << 20),
            b.schema(),
            Some(props(false, rows / 2, compression)),
        )
        .expect("a writer");
        let base = live();
        writer.write(&b).expect("written");
        kept.push(live() - base);
        drop(writer);
    }
    println!("M7 snappy_beyond_uncompressed={}", kept[1] - kept[0]);
}

/// M8: how far a writer's heap goes above `memory_size` when it is flushed by it: `columns`
/// columns of `kind`, written `slice` rows at a time, the row group flushed once `memory_size`
/// reaches `flush` bytes. Reports the most the heap held, during a write and during a flush,
/// over the threshold.
fn flushed_by_estimate(
    kind: Kind,
    columns: usize,
    dictionary: bool,
    page: usize,
    slice: usize,
    flush: usize,
    compression: Compression,
) {
    let fields: Vec<Field> = (0..columns)
        .map(|c| Field::new(format!("c{c}"), data_type(kind), true))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let batches: Vec<RecordBatch> = (0..64)
        .map(|b| {
            let arrays: Vec<ArrayRef> = (0..columns)
                .map(|c| column(kind, (b * slice + c) as i64, slice, 1 << 40))
                .collect();
            RecordBatch::try_new(Arc::clone(&schema), arrays).expect("a batch")
        })
        .collect();
    let props = WriterProperties::builder()
        .set_dictionary_enabled(dictionary)
        .set_data_page_size_limit(page)
        .set_dictionary_page_size_limit(page)
        .set_write_batch_size(slice.min(1024))
        .set_compression(compression)
        .build();
    let mut writer = ArrowWriter::try_new(Vec::<u8>::with_capacity(256 << 20), schema, Some(props))
        .expect("a writer");
    let base = live();
    let (mut write_peak, mut flush_peak, mut estimate_most) = (0i64, 0i64, 0usize);
    for b in &batches {
        reset_peak();
        writer.write(b).expect("written");
        write_peak = write_peak.max(peak() - base);
        estimate_most = estimate_most.max(writer.memory_size());
        if writer.memory_size() >= flush {
            reset_peak();
            writer.flush().expect("flushed");
            flush_peak = flush_peak.max(peak() - base);
        }
    }
    println!(
        "M8 kind={kind:?} columns={columns} dictionary={dictionary} page={page} slice={slice} \
         flush={flush} write_peak={write_peak} flush_peak={flush_peak} estimate_most={estimate_most} \
         over_flush={:.2}",
        write_peak.max(flush_peak) as f64 / flush as f64
    );
}

#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn m8_heap_over_memory_size_when_flushed_by_it() {
    let zstd = Compression::ZSTD(Default::default());
    let none = Compression::UNCOMPRESSED;
    flushed_by_estimate(Kind::Int8, 64, false, 1 << 20, 163, 1_800_000, none);
    flushed_by_estimate(Kind::Int8, 64, true, 1 << 20, 163, 1_800_000, none);
    flushed_by_estimate(Kind::Int64, 19, true, 14_563, 1024, 8_000_000, zstd);
    flushed_by_estimate(Kind::Float64, 19, true, 1 << 20, 1024, 8_000_000, zstd);
    flushed_by_estimate(Kind::Str(2048), 2, true, 89_478, 1024, 2_000_000, zstd);
    flushed_by_estimate(Kind::Str(2048), 2, true, 89_478, 32, 2_000_000, zstd);
    flushed_by_estimate(Kind::Str(32), 8, true, 1 << 20, 1024, 4_000_000, zstd);
    flushed_by_estimate(Kind::Int16, 192, true, 1820, 512, 40_000_000, zstd);
}
