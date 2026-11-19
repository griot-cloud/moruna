//! 6.3d (system design 3A): a Parquet sink's writers keep to their heap share. The share is
//! divided among as many lanes as the schema's columns let it hold (their codecs and the
//! writer's least heap), and a schema one lane cannot hold is refused at `open`. A lane keeps
//! parquet's page unless its codecs would take more of it than its writer, its row group is
//! flushed by what the writer holds, and its file rolls before the file's page index outgrows
//! its share. The rows are the same as an unbounded writer's; only row group, page, encoding and
//! file boundaries move.
//!
//! The writer's Rust heap is counted exactly: every call into the sink runs with its thread's
//! allocations counted by this binary's allocator, its file buffers are a real arena's and its
//! reactor's copies are not counted, and the payloads stay alive outside it. Its codecs' contexts
//! are the C library's and are counted by the model (`lane_codec_bytes`, measured by
//! `lane_heap_measure.rs`), so what the sink holds on the Rust heap must stay within its lane less
//! them, whatever the schema.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use common::{CountingAlloc, Scratch, block_on, object_bytes};
use moruna_kernel::arrow::array::{
    Array, ArrayRef, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, RecordBatch,
    StringArray, TimestampMicrosecondArray,
};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use moruna_kernel::{Allocator, MorunaError, Payload, Sink, SourceSchema};
use moruna_sinks::{ParquetSink, ParquetSinkConfig, lane_codec_bytes, least_lane_bytes, leaves_of};
use moruna_testkit::{FakeAllocator, FakeReactor};
use parquet::basic::{Compression, Encoding, PageType};
use parquet::file::reader::{FileReader, SerializedFileReader};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static NET: Cell<i64> = const { Cell::new(0) };
    static HIGH: Cell<i64> = const { Cell::new(0) };
}

/// What the sink under test holds on the heap between calls, and the most it has held.
static HELD: AtomicI64 = AtomicI64::new(0);
static HELD_PEAK: AtomicI64 = AtomicI64::new(0);

struct Counting;

fn counted(delta: i64) {
    // This binary counts the heap as the engine's allocator does, so the sink measures its
    // writers' heap here rather than estimating it.
    moruna_kernel::held::heap_counted(delta);
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let net = NET.with(|n| {
                n.set(n.get() + delta);
                n.get()
            });
            HIGH.with(|h| h.set(h.get().max(net)));
        }
    });
}

// SAFETY: every call is `System`'s with the arguments it was given; the counting touches only
// thread locals that need no allocation.
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

/// Run `f` with this thread's allocations not counted: the reactor's copies of the files.
fn quiet<T>(f: impl FnOnce() -> T) -> T {
    let was = ARMED.with(|a| a.replace(false));
    let out = f();
    ARMED.with(|a| a.set(was));
    out
}

/// The test's reactor, its calls not counted: what it keeps of a file is the store's, not the
/// sink's.
struct Quiet(FakeReactor);

impl moruna_kernel::Reactor for Quiet {
    fn read_file(
        &self,
        path: &std::path::Path,
        offset: u64,
        dst: moruna_kernel::Buffer,
    ) -> moruna_kernel::Completion<moruna_kernel::Buffer> {
        quiet(|| self.0.read_file(path, offset, dst))
    }
    fn read_file_opt(
        &self,
        path: &std::path::Path,
        offset: u64,
        dst: moruna_kernel::Buffer,
        allow_short: bool,
    ) -> moruna_kernel::Completion<(moruna_kernel::Buffer, usize)> {
        quiet(|| self.0.read_file_opt(path, offset, dst, allow_short))
    }
    fn write_file(
        &self,
        path: &std::path::Path,
        offset: u64,
        src: moruna_kernel::BufferView,
    ) -> moruna_kernel::Completion<()> {
        quiet(|| self.0.write_file(path, offset, src))
    }
    fn read_object(
        &self,
        url: &str,
        offset: u64,
        dst: moruna_kernel::Buffer,
    ) -> moruna_kernel::Completion<moruna_kernel::Buffer> {
        quiet(|| self.0.read_object(url, offset, dst))
    }
    fn write_object(
        &self,
        url: &str,
        src: moruna_kernel::BufferView,
    ) -> moruna_kernel::Completion<()> {
        quiet(|| self.0.write_object(url, src))
    }
    fn copy(
        &self,
        src: moruna_kernel::CopySrc,
        dst: moruna_kernel::CopyDst,
    ) -> moruna_kernel::Completion<Option<moruna_kernel::Buffer>> {
        quiet(|| self.0.copy(src, dst))
    }
    fn register_segment(&self, segment: u32, path: &std::path::Path) -> moruna_kernel::Result<()> {
        let was = ARMED.with(|a| a.replace(false));
        let out = self.0.register_segment(segment, path);
        ARMED.with(|a| a.set(was));
        out
    }
    fn unregister_segment(&self, segment: u32) {
        quiet(|| self.0.unregister_segment(segment))
    }
    fn delete_object(&self, url: &str) -> moruna_kernel::Completion<()> {
        quiet(|| self.0.delete_object(url))
    }
    fn abort_multipart(&self, url: &str, upload_id: &str) -> moruna_kernel::Completion<()> {
        quiet(|| self.0.abort_multipart(url, upload_id))
    }
    fn paths(&self) -> moruna_kernel::IoPaths {
        self.0.paths()
    }
    fn shutdown(&self) {
        self.0.shutdown()
    }
}

/// A real arena: the sink's file buffers are its regions, never the heap counted here.
fn arena() -> Arc<dyn Allocator> {
    moruna_arena::Arena::new(moruna_arena::ArenaConfig {
        host_bytes: 1 << 30,
        host_tier: moruna_kernel::TierKind::Host,
        device_bytes: Vec::new(),
        page_bytes: 4096,
        huge_pages: moruna_kernel::Guarantee::Absent,
        memlock: moruna_kernel::Guarantee::Absent,
        register_rdma: false,
        pretouch: false,
    })
    .expect("an arena") as Arc<dyn Allocator>
}

/// Run `f` with this thread's allocations counted into what the sink holds.
fn measured<T>(f: impl FnOnce() -> T) -> T {
    ARMED.with(|a| a.set(true));
    NET.with(|n| n.set(0));
    HIGH.with(|h| h.set(0));
    let out = f();
    ARMED.with(|a| a.set(false));
    let (net, high) = (NET.with(Cell::get), HIGH.with(Cell::get));
    let before = HELD.fetch_add(net, Ordering::SeqCst);
    HELD_PEAK.fetch_max(before + high.max(net), Ordering::SeqCst);
    out
}

/// One measured sink at a time: the counters are this binary's.
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

const MORSELS: i64 = 10;
const ROWS: i64 = 5_000;
const PAGE: u64 = 1 << 20;

/// What a run wrote: its files' row groups, the most values a data page held, the rows, the sum
/// of every numeric column's values, whether any column chunk is dictionary encoded, and the
/// most the sink held on the heap, and the writes and the slices they were written in.
struct Written {
    files: usize,
    row_groups: usize,
    page_values_max: i32,
    rows: i64,
    sum: i64,
    dictionary: bool,
    held_peak: i64,
    measured: bool,
    writes: u64,
    slices: u64,
}

fn schema_of(types: &[DataType]) -> SchemaRef {
    Arc::new(Schema::new(
        types
            .iter()
            .enumerate()
            .map(|(i, t)| Field::new(format!("c{i}"), t.clone(), true))
            .collect::<Vec<_>>(),
    ))
}

/// `rows` rows from `from` of every column of `schema`: small categorical integers, measures of
/// many distinct values, timestamps, and strings of their column's length (the field's name
/// says nothing of it, so `long` columns are 2 KiB and the rest 12 bytes).
fn rows_of(schema: &SchemaRef, from: i64, rows: i64) -> RecordBatch {
    let columns: Vec<ArrayRef> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(c, field)| {
            let c = c as i64;
            let range = from..from + rows;
            match field.data_type() {
                DataType::Int8 => Arc::new(Int8Array::from_iter_values(
                    range.map(|r| ((r / 7 + c) % 4) as i8),
                )) as ArrayRef,
                DataType::Int16 => Arc::new(Int16Array::from_iter_values(
                    range.map(|r| ((r + c) % 300) as i16),
                )),
                DataType::Int32 => Arc::new(Int32Array::from_iter_values(
                    range.map(|r| ((r * 31 + c) % 265) as i32),
                )),
                DataType::Int64 => Arc::new(Int64Array::from_iter_values(range.map(|r| {
                    (r.wrapping_add(c))
                        .wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as i64)
                        .rotate_left(29)
                        >> 8
                }))),
                DataType::Float64 => Arc::new(Float64Array::from_iter_values(
                    range.map(|r| ((r * 2_654_435_761 + c) % 9_973) as f64 / 7.0),
                )),
                DataType::Timestamp(_, _) => Arc::new(TimestampMicrosecondArray::from_iter_values(
                    range.map(|r| 1_700_000_000_000_000 + r * 1_000_003 + c),
                )),
                DataType::Utf8 => {
                    let long = field.name().starts_with("long");
                    Arc::new(StringArray::from_iter_values(range.map(|r| {
                        let mut s = format!("{r:012}");
                        if long {
                            while s.len() < 2048 {
                                s.push_str(&format!("{:x}", r.wrapping_mul(s.len() as i64 + c)));
                            }
                            s.truncate(2048);
                        }
                        s
                    })))
                }
                other => panic!("no rows for {other}"),
            }
        })
        .collect();
    RecordBatch::try_new(Arc::clone(schema), columns).expect("a batch")
}

/// Write `MORSELS` morsels of `ROWS` rows through one lane, `share` its writer heap, every call
/// into the sink measured, and read every file back.
fn write(tag: &str, schema: &SchemaRef, compression: Compression, share: Option<u64>) -> Written {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    HELD.store(0, Ordering::SeqCst);
    HELD_PEAK.store(0, Ordering::SeqCst);
    let batches: Vec<RecordBatch> = (0..MORSELS)
        .map(|m| rows_of(schema, m * ROWS, ROWS))
        .collect();
    let scratch = Scratch::new(tag);
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            file_bytes: 64 << 20,
            compression,
            ..ParquetSinkConfig::default()
        },
        Arc::new(Quiet(reactor.clone())),
        arena(),
    )
    .expect("parquet sink")
    .with_lanes(1)
    .with_writer_heap(share);
    measured(|| {
        sink.open(&SourceSchema::Table(Arc::clone(schema)))
            .expect("open")
    });
    // The payloads stay alive here, so the sink's dropping them frees nothing it counts.
    for (m, batch) in batches.iter().enumerate() {
        let payload = Payload::table(batch.clone()).expect("payload");
        let encoded = measured(|| sink.encode(m as u64, payload).expect("encode"));
        block_on(sink.put(encoded)).expect("put");
    }
    let summary = measured(|| sink.finish().expect("finish"));
    let mut written = Written {
        files: summary.files.len(),
        row_groups: 0,
        page_values_max: 0,
        rows: 0,
        sum: 0,
        dictionary: false,
        held_peak: HELD_PEAK.load(Ordering::SeqCst),
        measured: sink.stats().writer_heap_measured,
        writes: sink.stats().writes,
        slices: sink.stats().slices,
    };
    for name in &summary.files {
        let url = format!("{}/{}", scratch.url(), name);
        let len = reactor
            .ops()
            .iter()
            .find(|op| op.path_or_url == url)
            .map(|op| op.len as usize)
            .expect("the file was written");
        let bytes = bytes::Bytes::from(object_bytes(&reactor, alloc.fake(), &url, len));
        let reader = SerializedFileReader::new(bytes.clone()).expect("a readable parquet file");
        written.rows += reader.metadata().file_metadata().num_rows();
        written.row_groups += reader.num_row_groups();
        for g in 0..reader.num_row_groups() {
            let group = reader.get_row_group(g).expect("a row group");
            for c in 0..group.num_columns() {
                written.dictionary |=
                    group.metadata().column(c).encodings().any(|e| {
                        matches!(e, Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY)
                    });
                for page in group.get_column_page_reader(c).expect("pages") {
                    let page = page.expect("a page");
                    if page.page_type() != PageType::DICTIONARY_PAGE {
                        written.page_values_max =
                            written.page_values_max.max(page.num_values() as i32);
                    }
                }
            }
        }
        let batches = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(bytes)
            .expect("an arrow reader")
            .build()
            .expect("a reader");
        for read in batches {
            let read = read.expect("a batch");
            for column in read.columns() {
                written.sum = written.sum.wrapping_add(sum(column.as_ref()));
            }
        }
    }
    written
}

fn sum(column: &dyn Array) -> i64 {
    let any = column.as_any();
    if let Some(v) = any.downcast_ref::<Int8Array>() {
        v.values().iter().map(|&v| i64::from(v)).sum()
    } else if let Some(v) = any.downcast_ref::<Int16Array>() {
        v.values().iter().map(|&v| i64::from(v)).sum()
    } else if let Some(v) = any.downcast_ref::<Int32Array>() {
        v.values().iter().map(|&v| i64::from(v)).sum()
    } else if let Some(v) = any.downcast_ref::<Int64Array>() {
        v.values().iter().fold(0i64, |s, &v| s.wrapping_add(v))
    } else if let Some(v) = any.downcast_ref::<TimestampMicrosecondArray>() {
        v.values().iter().fold(0i64, |s, &v| s.wrapping_add(v))
    } else if let Some(v) = any.downcast_ref::<Float64Array>() {
        v.values().iter().map(|&v| (v * 7.0) as i64).sum()
    } else if let Some(v) = any.downcast_ref::<StringArray>() {
        (0..v.len()).map(|i| v.value(i).len() as i64).sum()
    } else {
        panic!("no sum for {}", column.data_type())
    }
}

fn zstd() -> Compression {
    Compression::ZSTD(Default::default())
}

/// NYC's columns, as the taxi files have them: integers, timestamps, measures and a flag.
fn taxi() -> SchemaRef {
    let mut types = vec![DataType::Int32; 4];
    types.push(DataType::Timestamp(TimeUnit::Microsecond, None));
    types.push(DataType::Timestamp(TimeUnit::Microsecond, None));
    types.extend(vec![DataType::Int64; 2]);
    types.extend(vec![DataType::Float64; 10]);
    types.push(DataType::Utf8);
    schema_of(&types)
}

/// The model holds `schema` at a lane of `lane` bytes: what the sink held on the heap is within
/// the lane less what its codecs are counted at, and the rows are an unbounded writer's.
fn holds(tag: &str, schema: &SchemaRef, lane: u64) -> Written {
    let leaves = leaves_of(schema).expect("leaves");
    let codecs = lane_codec_bytes(lane, &leaves, zstd(), PAGE);
    let bounded = write(tag, schema, zstd(), Some(lane));
    let unbounded = write(&format!("{tag}-free"), schema, zstd(), None);
    eprintln!(
        "{tag}: held {} of a {lane} byte lane, {codecs} of it the codecs'",
        bounded.held_peak
    );
    assert_eq!(bounded.rows, MORSELS * ROWS);
    assert_eq!(bounded.sum, unbounded.sum, "the same values");
    assert!(
        bounded.measured,
        "this binary counts the heap, so the sink measured it"
    );
    assert!(
        bounded.held_peak as u64 + codecs <= lane,
        "{tag}: the writer held {} beside {codecs} of codecs, over its {lane} byte lane",
        bounded.held_peak
    );
    bounded
}

/// Without an allowance the writer keeps parquet's limits: one file, one row group, pages of
/// 20,000 values, dictionaries.
#[test]
fn without_an_allowance_parquets_limits_stand() {
    let written = write(
        "lane-heap-none",
        &schema_of(&vec![DataType::Int8; 64]),
        zstd(),
        None,
    );
    assert_eq!(written.rows, MORSELS * ROWS);
    assert_eq!((written.files, written.row_groups), (1, 1));
    assert_eq!(written.page_values_max, 20_000);
    assert!(written.dictionary);
}

/// An allowance of nothing is no allowance.
#[test]
fn an_allowance_of_nothing_keeps_parquets_limits() {
    let written = write(
        "lane-heap-zero",
        &schema_of(&vec![DataType::Int8; 4]),
        zstd(),
        Some(0),
    );
    assert_eq!(written.files, 1);
    assert_eq!(written.page_values_max, 20_000);
}

/// NYC's columns at a lane where their codecs would take more than the writer at parquet's
/// page: the page is cut, the writer holds what the model says, the rows are the same.
#[test]
fn the_model_holds_the_taxi_columns_at_a_small_lane() {
    let schema = taxi();
    let written = holds("lane-heap-taxi", &schema, 26_000_000);
    assert!(
        written.page_values_max < 20_000,
        "pages cut by bytes: {}",
        written.page_values_max
    );
    assert!(written.dictionary, "the dictionaries stay");
}

/// A wide string column, 2 KiB values, beside an id: its pages close by bytes, its dictionary
/// (a byte array's, which starts empty) falls back, and the writer holds what the model says.
#[test]
fn the_model_holds_a_wide_string_column() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("long", DataType::Utf8, true),
    ]));
    let leaves = leaves_of(&schema).expect("leaves");
    let lane = 2 * least_lane_bytes(&leaves, zstd());
    let written = holds("lane-heap-strings", &schema, lane.max(4 << 20));
    assert!(
        written.slices > written.writes,
        "a morsel of 10 MiB of strings does not fit the room: {} slices",
        written.slices
    );
}

/// A morsel that fits the lane's room under its flush point is written whole, in one slice
/// (PM, 2026-11-19): NYC's columns, a 12-byte flag among them, at a lane of 128 MB.
#[test]
fn a_morsel_that_fits_the_lanes_room_is_written_whole() {
    let written = holds("lane-heap-whole", &taxi(), 128_000_000);
    assert_eq!(written.writes, MORSELS as u64);
    assert_eq!(
        written.slices, written.writes,
        "one slice a morsel: {} in {} writes",
        written.slices, written.writes
    );
}

/// 192 small integer columns, one and two bytes, at a lane that holds their codecs at a small
/// page: each holds a `u64` index a value on the heap whatever its width, and the writer holds
/// what the model says.
#[test]
fn the_model_holds_small_integers() {
    let mut types = vec![DataType::Int8; 96];
    types.extend(vec![DataType::Int16; 96]);
    let schema = schema_of(&types);
    let leaves = leaves_of(&schema).expect("leaves");
    let lane = 3 * least_lane_bytes(&leaves, zstd());
    holds("lane-heap-small-ints", &schema, lane);
}

/// Uncompressed, a wide table keeps parquet's page whatever its lane, since it keeps no codec;
/// at a small lane its 64 fixed-width dictionaries' empty indexes do not fit beside the writer,
/// so its columns are written plain, and its row groups are flushed by what it holds.
#[test]
fn a_wide_uncompressed_table_writes_plain_and_flushes_by_its_heap() {
    let schema = schema_of(&vec![DataType::Int8; 64]);
    let bounded = write(
        "lane-heap-wide",
        &schema,
        Compression::UNCOMPRESSED,
        Some(2 << 20),
    );
    let unbounded = write(
        "lane-heap-wide-free",
        &schema,
        Compression::UNCOMPRESSED,
        None,
    );
    assert_eq!(bounded.rows, MORSELS * ROWS);
    assert_eq!(bounded.sum, unbounded.sum, "the same values");
    assert!(!bounded.dictionary, "no dictionary: {}", bounded.dictionary);
    assert!(unbounded.dictionary);
    assert!(
        bounded.row_groups > bounded.files,
        "{} row groups in {} files",
        bounded.row_groups,
        bounded.files
    );
    assert!(bounded.held_peak <= 2 << 20, "{}", bounded.held_peak);
}

/// A sink over `schema` with `lanes` lanes and `share` bytes of writer heap, compressed with
/// Zstandard.
fn zstd_sink(scratch: &Scratch, lanes: usize, share: u64) -> ParquetSink {
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor),
        alloc as Arc<dyn Allocator>,
    )
    .expect("parquet sink")
    .with_lanes(lanes)
    .with_writer_heap(Some(share))
}

/// Zstandard keeps two contexts a column, outside the heap parquet estimates: a share that
/// cannot hold one lane's is refused at `open`, naming what one lane needs.
#[test]
fn a_schema_one_lane_cannot_hold_is_refused_at_open() {
    let scratch = Scratch::new("lane-heap-refused");
    let schema = schema_of(&vec![DataType::Int8; 64]);
    let least = least_lane_bytes(&leaves_of(&schema).expect("leaves"), zstd());
    let mut sink = zstd_sink(&scratch, 2, least - 1);
    let refused = sink
        .open(&SourceSchema::Table(Arc::clone(&schema)))
        .expect_err("one lane does not fit");
    let MorunaError::Config { name, msg } = refused else {
        panic!("a budget refusal: {refused}");
    };
    assert_eq!(name, "budget.host");
    assert!(msg.contains(&least.to_string()), "{msg}");
    assert!(msg.contains("64 leaf columns"), "{msg}");
}

/// A share that holds fewer lanes than the sink's workers is divided among the lanes it holds:
/// four workers encode at once into two lanes, and every row lands once.
#[test]
fn the_lanes_are_capped_to_what_the_share_holds() {
    let scratch = Scratch::new("lane-heap-capped");
    let schema = schema_of(&vec![DataType::Int8; 64]);
    let least = least_lane_bytes(&leaves_of(&schema).expect("leaves"), zstd());
    let mut sink = zstd_sink(&scratch, 4, least * 5 / 2);
    sink.open(&SourceSchema::Table(Arc::clone(&schema)))
        .expect("open");
    assert_eq!(sink.stats().lanes, 2, "two lanes of the four workers");
    let barrier = std::sync::Barrier::new(4);
    let encoded: Vec<moruna_kernel::Encoded> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4i64)
            .map(|thread| {
                let (sink, barrier, schema) = (&sink, &barrier, &schema);
                scope.spawn(move || {
                    barrier.wait();
                    (0..3i64)
                        .map(|i| {
                            let seq = thread * 3 + i;
                            let payload = Payload::table(rows_of(schema, seq * 1_000, 1_000))
                                .expect("payload");
                            sink.encode(seq as u64, payload).expect("encode")
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("an encoder"))
            .collect()
    });
    for one in encoded {
        block_on(sink.put(one)).expect("put");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.rows, 12_000);
    assert!(summary.files.len() <= 2, "{} files", summary.files.len());
}

/// Without a writer heap the sink keeps a lane per worker.
#[test]
fn without_a_share_every_worker_has_a_lane() {
    let scratch = Scratch::new("lane-heap-free-lanes");
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut sink = ParquetSink::new(
        ParquetSinkConfig {
            url: scratch.url(),
            ..ParquetSinkConfig::default()
        },
        Arc::new(reactor),
        alloc as Arc<dyn Allocator>,
    )
    .expect("parquet sink")
    .with_lanes(4);
    sink.open(&SourceSchema::Table(schema_of(&vec![DataType::Int8; 64])))
        .expect("open");
    assert_eq!(sink.stats().lanes, 4);
}
