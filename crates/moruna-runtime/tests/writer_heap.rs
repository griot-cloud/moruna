//! 6.3d (system design 3A): the Parquet sink's writer heap is bounded by the budget.
//!
//! A Parquet sink encodes on the run's workers, one lane per worker that may encode at once, and
//! each lane's `ArrowWriter` keeps on the heap, outside the arena, its in-progress row group's
//! encoded pages and every column's dictionary and open page. The arena sizing (12 f.1) leaves
//! the run `BuildCtx::headroom_bytes` above the arena for everything it holds there, so what the
//! sink's writers hold has to stay within that room, whatever the columns and the lanes.
//!
//! The sink is measured exactly: the run's sink is the kind registry's own Parquet sink, wrapped
//! so that every call the scheduler makes into it runs with its thread's allocations counted by
//! this binary's allocator. What the sink keeps between calls is what it holds outside the arena
//! (its file buffers are the arena's and are not counted). Each run is a process of its own
//! (`support::apart`).

#![allow(clippy::result_large_err)]

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use arrow::array::{Int8Array, RecordBatch};
use moruna_job::JobSpec;
use moruna_kernel::{
    BoxFuture, CancelToken, Encoded, Payload, PayloadSpec, Seq, Sink, SinkSummary, SourceSchema,
};
use moruna_runtime::job::{BuildOptions, build};
use moruna_runtime::kinds::Registry;
use moruna_runtime::{Runtime, SinkSpec};
use serde_json::json;
use support::apart::{self, BUDGETS};
use support::{Scratch, one_run_at_a_time};

thread_local! {
    /// Whether this thread is inside a call into the measured sink.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// Bytes this thread allocated less bytes it freed since the call began.
    static NET: Cell<i64> = const { Cell::new(0) };
    /// The most `NET` has been since the call began.
    static HIGH: Cell<i64> = const { Cell::new(0) };
}

/// What the sink holds on the heap between calls, all lanes together, and the most it has held.
static HELD: AtomicI64 = AtomicI64::new(0);
static HELD_PEAK: AtomicI64 = AtomicI64::new(0);
/// The room the run's arena sizing left above the arena, as the sink's builder was given it.
static HEADROOM: AtomicU64 = AtomicU64::new(0);
static ARENA: AtomicU64 = AtomicU64::new(0);
static WORKERS: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting the bytes a thread allocates and frees while it is inside the
/// measured sink. Nothing is allocated here, so the thread locals need no allocation either.
struct Counting;

fn counted(delta: i64) {
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

/// Run `f` with this thread's allocations counted into what the sink holds.
fn measured<T>(f: impl FnOnce() -> T) -> T {
    ARMED.with(|a| a.set(true));
    NET.with(|n| n.set(0));
    HIGH.with(|h| h.set(0));
    let out = f();
    ARMED.with(|a| a.set(false));
    let (net, high) = (NET.with(Cell::get), HIGH.with(Cell::get));
    let before = HELD.fetch_add(net, Ordering::AcqRel);
    HELD_PEAK.fetch_max(before + high.max(net), Ordering::AcqRel);
    out
}

/// The kind registry's sink, every call into it measured.
struct Measured(Box<dyn Sink>);

impl Sink for Measured {
    fn open(&mut self, schema: &SourceSchema) -> moruna_kernel::Result<()> {
        measured(|| self.0.open(schema))
    }
    fn accepts(&self) -> PayloadSpec {
        self.0.accepts()
    }
    fn requires_order(&self) -> bool {
        self.0.requires_order()
    }
    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, moruna_kernel::Result<()>> {
        Box::pin(async move {
            let encoded = self.encode(seq, payload)?;
            self.put(encoded).await
        })
    }
    fn encodes_on_workers(&self) -> bool {
        self.0.encodes_on_workers()
    }
    fn encode(&self, seq: Seq, payload: Payload) -> moruna_kernel::Result<Encoded> {
        measured(|| self.0.encode(seq, payload))
    }
    fn put(&self, encoded: Encoded) -> BoxFuture<'_, moruna_kernel::Result<()>> {
        self.0.put(encoded)
    }
    fn finish(&mut self) -> moruna_kernel::Result<SinkSummary> {
        measured(|| self.0.finish())
    }
    fn committed_seq(&self) -> Option<Seq> {
        self.0.committed_seq()
    }
    fn skip(&self, seq: Seq) {
        self.0.skip(seq)
    }
    fn checkpoint_flush(&self) -> moruna_kernel::Result<()> {
        measured(|| self.0.checkpoint_flush())
    }
    fn lagging(&self) -> bool {
        self.0.lagging()
    }
    fn checkpoint(&self) -> moruna_kernel::Result<Option<Vec<u8>>> {
        self.0.checkpoint()
    }
    fn resume(
        &mut self,
        schema: &SourceSchema,
        state: &[u8],
        committed_seq: Option<Seq>,
    ) -> moruna_kernel::Result<()> {
        self.0.resume(schema, state, committed_seq)
    }
}

/// The child: the job its parent left, run with its sink measured.
#[test]
#[ignore = "run by the tests below in a process of their own"]
fn child_run() {
    let Some(job) = apart::job() else {
        return;
    };
    let doc = JobSpec::from_value(job).expect("the document parses");
    let job = moruna_job::Job::from_v1(&doc);
    let env = |_: &str| None;
    let outcome = match build(
        &job,
        &support::planned(&job),
        &Registry::standard(),
        BuildOptions {
            strict: true,
            env: &env,
            notes: Vec::new(),
            packages: moruna_runtime::packaged::PackageHost::default(),
            host_ports: None,
            pulled_images: None,
            args: Default::default(),
            held: Vec::new(),
        },
    ) {
        Err(e) => json!({"error": e.to_string()}),
        Ok(built) => {
            let mut spec = built.spec;
            let sinks = std::mem::take(&mut spec.sinks);
            spec.sinks = sinks
                .into_iter()
                .map(|sink| {
                    SinkSpec::Build(Box::new(move |ctx| {
                        HEADROOM.store(ctx.headroom_bytes, Ordering::SeqCst);
                        ARENA.store(ctx.arena_bytes, Ordering::SeqCst);
                        WORKERS.store(u64::from(ctx.workers), Ordering::SeqCst);
                        Ok(Box::new(Measured(sink.build(ctx)?)) as Box<dyn Sink>)
                    }))
                })
                .collect();
            let run = Runtime::run(spec, CancelToken::new());
            let measured = json!({
                "held_peak": HELD_PEAK.load(Ordering::SeqCst),
                "headroom": HEADROOM.load(Ordering::SeqCst),
                "arena": ARENA.load(Ordering::SeqCst),
                "workers": WORKERS.load(Ordering::SeqCst),
            });
            match run {
                Ok(report) => json!({"report": report, "sink": measured}),
                Err(e) => {
                    json!({"error": e.error.to_string(), "report": e.report, "sink": measured})
                }
            }
        }
    };
    apart::answer(outcome);
}

/// A file of `groups` row groups of `rows` rows each, written a group at a time from `group`.
fn grouped(path: &Path, groups: i64, rows: i64, group: impl Fn(i64, i64) -> RecordBatch) {
    let first = group(0, rows);
    let props = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(rows as usize))
        .set_compression(parquet::basic::Compression::ZSTD(Default::default()))
        .build();
    let file = std::fs::File::create(path).expect("the input file");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, first.schema(), Some(props)).expect("a writer");
    writer.write(&first).expect("a group");
    writer.flush().expect("the group closed");
    for g in 1..groups {
        writer.write(&group(g * rows, rows)).expect("a group");
        writer.flush().expect("the group closed");
    }
    writer.close().expect("closed");
}

/// The columns of the wide table: a feature table of flags and codes.
const COLUMNS: usize = 384;

/// Rows of `COLUMNS` small categorical columns (four values each, a byte apiece): 77 MB decoded
/// at 200,000 rows, under a third of the smaller budget. Parquet encodes each column with a
/// dictionary of four entries, so its encoded pages are a few bits a row, while the writer holds
/// every value of a column's open page as a `u64` dictionary index and an `i16` level.
fn categorical(from: i64, rows: i64) -> RecordBatch {
    let columns: Vec<(String, arrow::array::ArrayRef)> = (0..COLUMNS)
        .map(|c| {
            let values = (from..from + rows).map(|r| ((r / 7 + c as i64) % 4) as i8);
            (
                format!("c{c}"),
                Arc::new(Int8Array::from_iter_values(values)) as arrow::array::ArrayRef,
            )
        })
        .collect();
    RecordBatch::try_from_iter(columns).expect("a group")
}

/// 3A, 6.3d: a copy of a wide table of small categorical columns, two workers, the smallest
/// budget. Each lane's writer keeps its columns' open pages, dictionaries and row group pages on
/// the heap, and what all the lanes keep stays within the room the arena sizing leaves above the
/// arena (`BuildCtx::headroom_bytes`); the run completes under its ceiling.
///
/// Fails before the fix (2026-11-18, the Mac): the writers held 261 MB against a room of 79 MB,
/// for 77 MB of rows, and the controller stopped the run at 1.78 times its ceiling. The same
/// rows into a `count` sink complete at 0.52 of it (`moruna-engine`, release).
#[test]
fn the_writers_heap_stays_within_the_room_above_the_arena() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("writer_heap");
    let file = scratch.path().join("in.parquet");
    let (groups, rows) = (8i64, 25_000i64);
    grouped(&file, groups, rows, categorical);
    let budget = BUDGETS[0];
    let out = scratch.path().join("out");
    let outcome = apart::run(
        "child_run",
        &json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": file},
            "sink": {"kind": "parquet", "url": out},
            "budget": {"memory_bytes": budget, "cpu": 2.0},
            "profiles_dir": scratch.path().join("profiles"),
        }),
    );
    let sink = &outcome["sink"];
    let held = sink["held_peak"].as_i64().expect("measured");
    let headroom = sink["headroom"].as_i64().expect("the room above the arena");
    eprintln!(
        "the sink's writers held {held} bytes at most, against {headroom} bytes above an arena of \
         {} with {} workers; the run: {}, peak {} of the ceiling, {}",
        sink["arena"],
        sink["workers"],
        outcome["report"]["exit"],
        outcome["report"]["peak_fraction_of_ceiling"],
        outcome["error"]
    );
    assert!(headroom > 0, "the run was sized: {outcome}");
    assert!(
        held <= headroom,
        "the sink's writers held {held} bytes outside the arena, more than the {headroom} bytes \
         the arena sizing leaves above it"
    );
    apart::within(&outcome, "a copy of a wide categorical table", budget);
    assert_eq!(support::read_back_rows(&out), (groups * rows) as u64);
}
