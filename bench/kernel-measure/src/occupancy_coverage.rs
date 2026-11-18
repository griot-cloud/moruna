//! 4G.2: Occupancy (`bench/vm/workloads.py`) through `moruna.interval_coverage_count`, against
//! the gate's Rust Occupancy (expand and count, `moruna_gate_functions::batch::occupancy`), on
//! the NYC files, batch by batch. Occupancy as written, in Python, is `occupancy_py.py`, and
//! `measure.py` runs all of them and compares them.
//!
//! ```text
//! occupancy-coverage MODE --batch ROWS [--out FILE.parquet] FILE.parquet...
//! ```
//!
//! MODE is `read` (read the batches and nothing else: the baseline of memory), `kernel` (the
//! kernel with no group domain: its sparse table), `kernel-dense` (the kernel with the domain
//! `[0, 299]`: its dense table) or `rust-expand` (the gate's function). Each file is read in
//! batches of `--batch` rows, of the three columns Occupancy reads. Every batch's output is
//! written, in order, to `--out` with Occupancy's schema (zone, minute_of_day, car_minutes, all
//! Int64). One JSON line goes to stdout: the batches, the rows in and out, the seconds in the
//! function (`compute_s`, and of that the kernel's inputs, `prep_s`), the process's seconds, and
//! the most bytes the function held at once above what was live when it was called
//! (`alloc_peak_bytes`, counted by this binary's allocator).

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use arrow::array::{Array, ArrayRef, AsArray, Int32Array, Int64Array, RecordBatch};
use arrow::datatypes::{
    DataType, Field, Int32Type, Int64Type, Schema, SchemaRef, TimestampMicrosecondType,
};
use moruna_kernels::interval_coverage_count::IntervalCoverageCount;
use parquet::arrow::ArrowWriter;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde_json::json;

/// The system allocator, counting the bytes live and their high-water mark.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded to `System` with the caller's layout and pointer unchanged; the
// counters are only arithmetic beside it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract is `System::alloc`'s.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract is `System::alloc_zeroed`'s.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is `System::dealloc`'s.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract is `System::realloc`'s.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(new_size, Ordering::Relaxed) + new_size;
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A minute in the files' microseconds.
const US_PER_MINUTE: i64 = 60_000_000;

/// The columns Occupancy reads.
const COLUMNS: [&str; 3] = ["pickup_at", "dropoff_at", "pu_location_id"];

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Read,
    Kernel,
    KernelDense,
    RustExpand,
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("occupancy-coverage: {msg}");
    std::process::exit(2)
}

fn occupancy_schema() -> SchemaRef {
    // As NumPy's arrays come out of pyarrow: Int64, nullable.
    Arc::new(Schema::new(vec![
        Field::new("zone", DataType::Int64, true),
        Field::new("minute_of_day", DataType::Int64, true),
        Field::new("car_minutes", DataType::Int64, true),
    ]))
}

/// The kernel's inputs from a batch of trips: the pickup zone as Int32, the pickup instant in
/// microseconds, and the minutes Occupancy counts, `clip(floor(do) - floor(pu), 0, 240) + 1`.
/// This is the `expr` (4.2) that Occupancy calls before the kernel; it is written here until then.
fn kernel_inputs(batch: &RecordBatch) -> RecordBatch {
    let pickup = batch.column(0).as_primitive::<TimestampMicrosecondType>();
    let dropoff = batch.column(1).as_primitive::<TimestampMicrosecondType>();
    let zone = batch.column(2).as_primitive::<Int64Type>();
    if pickup.null_count() + dropoff.null_count() + zone.null_count() > 0 {
        fail("a null in pickup_at, dropoff_at or pu_location_id: Occupancy as written refuses it");
    }
    let (pu, dropoff, zone) = (pickup.values(), dropoff.values(), zone.values());
    let group: Int32Array = zone
        .iter()
        .map(|&z| i32::try_from(z).unwrap_or_else(|_| fail(format!("zone {z} is not an Int32"))))
        .collect();
    let minutes: Int64Array = pu
        .iter()
        .zip(dropoff.iter())
        .map(|(&a, &b)| {
            (b.div_euclid(US_PER_MINUTE) - a.div_euclid(US_PER_MINUTE)).clamp(0, 240) + 1
        })
        .collect();
    let start = Int64Array::new(pu.clone(), None);
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("zone", DataType::Int32, false),
            Field::new("pickup_us", DataType::Int64, false),
            Field::new("minutes", DataType::Int64, false),
        ])),
        vec![Arc::new(group), Arc::new(start), Arc::new(minutes)],
    )
    .unwrap_or_else(|e| fail(e))
}

/// The kernel's cells in Occupancy's schema: zone and minute of day widened to Int64.
fn as_occupancy(cells: &RecordBatch) -> RecordBatch {
    let widen = |i: usize| -> ArrayRef {
        Arc::new(
            cells
                .column(i)
                .as_primitive::<Int32Type>()
                .unary::<_, Int64Type>(i64::from),
        )
    };
    RecordBatch::try_new(
        occupancy_schema(),
        vec![widen(0), widen(1), Arc::clone(cells.column(2))],
    )
    .unwrap_or_else(|e| fail(e))
}

fn main() {
    let process = Instant::now();
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("read") => Mode::Read,
        Some("kernel") => Mode::Kernel,
        Some("kernel-dense") => Mode::KernelDense,
        Some("rust-expand") => Mode::RustExpand,
        _ => fail(
            "usage: occupancy-coverage read|kernel|kernel-dense|rust-expand --batch ROWS \
             [--out FILE] FILE...",
        ),
    };
    let mut batch_rows = 0usize;
    let mut out_path = None;
    let mut files = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--batch" => {
                batch_rows = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| fail("--batch takes a row count"))
            }
            "--out" => out_path = Some(args.next().unwrap_or_else(|| fail("--out takes a path"))),
            _ => files.push(a),
        }
    }
    if batch_rows == 0 || files.is_empty() {
        fail("give --batch ROWS and at least one file");
    }
    let mut args = json!({"group": "zone", "start": "pickup_us", "length": "minutes",
                          "step": US_PER_MINUTE, "size": 1440});
    if mode == Mode::KernelDense {
        args["groups"] = json!([0, 299]);
    }
    let kernel = IntervalCoverageCount::new(&args).unwrap_or_else(|e| fail(e));
    let mut writer = out_path.as_ref().map(|p| {
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .build();
        ArrowWriter::try_new(
            File::create(p).unwrap_or_else(|e| fail(format!("{p}: {e}"))),
            occupancy_schema(),
            Some(props),
        )
        .unwrap_or_else(|e| fail(e))
    });
    let (mut batches, mut rows_in, mut rows_out) = (0u64, 0u64, 0u64);
    let (mut compute_s, mut prep_s) = (0f64, 0f64);
    let mut alloc_peak = 0usize;
    let mut sizes = Vec::new();
    for path in &files {
        let file = File::open(path).unwrap_or_else(|e| fail(format!("{path}: {e}")));
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap_or_else(|e| fail(e));
        let schema = builder.parquet_schema();
        let indices: Vec<usize> = COLUMNS
            .iter()
            .map(|c| {
                schema
                    .columns()
                    .iter()
                    .position(|d| d.name() == *c)
                    .unwrap_or_else(|| fail(format!("{path} has no column {c}")))
            })
            .collect();
        let mask = ProjectionMask::leaves(schema, indices);
        let reader = builder
            .with_projection(mask)
            .with_batch_size(batch_rows)
            .build()
            .unwrap_or_else(|e| fail(e));
        for batch in reader {
            let batch = batch.unwrap_or_else(|e| fail(e));
            // The projection keeps the file's order of columns; Occupancy's are named.
            let batch = batch
                .project(
                    &COLUMNS
                        .iter()
                        .map(|c| batch.schema().index_of(c).unwrap_or_else(|e| fail(e)))
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|e| fail(e));
            batches += 1;
            rows_in += batch.num_rows() as u64;
            sizes.push(batch.num_rows());
            let base = LIVE.load(Ordering::Relaxed);
            PEAK.store(base, Ordering::Relaxed);
            let t = Instant::now();
            let out = match mode {
                Mode::Read => None,
                Mode::Kernel | Mode::KernelDense => {
                    let inputs = kernel_inputs(&batch);
                    prep_s += t.elapsed().as_secs_f64();
                    let (cells, notes) = kernel.run(&inputs).unwrap_or_else(|e| fail(e));
                    if !notes.is_empty() {
                        eprintln!("{}", notes.text(&kernel.grid()).unwrap_or_default());
                    }
                    Some(as_occupancy(&cells))
                }
                Mode::RustExpand => Some(
                    moruna_gate_functions::batch::occupancy(&batch)
                        .unwrap_or_else(|e| fail(format!("{e:?}"))),
                ),
            };
            compute_s += t.elapsed().as_secs_f64();
            alloc_peak = alloc_peak.max(PEAK.load(Ordering::Relaxed).saturating_sub(base));
            if let Some(out) = out {
                rows_out += out.num_rows() as u64;
                if let Some(w) = writer.as_mut() {
                    w.write(&out).unwrap_or_else(|e| fail(e));
                }
            }
        }
    }
    if let Some(w) = writer {
        w.close().unwrap_or_else(|e| fail(e));
    }
    let mode_name = match mode {
        Mode::Read => "read",
        Mode::Kernel => "kernel",
        Mode::KernelDense => "kernel-dense",
        Mode::RustExpand => "rust-expand",
    };
    println!(
        "{}",
        json!({
            "mode": mode_name,
            "batch": batch_rows,
            "files": files.len(),
            "batches": batches,
            "batch_rows": sizes,
            "rows_in": rows_in,
            "rows_out": rows_out,
            "compute_s": compute_s,
            "prep_s": prep_s,
            "process_s": process.elapsed().as_secs_f64(),
            "alloc_peak_bytes": alloc_peak,
        })
    );
}
