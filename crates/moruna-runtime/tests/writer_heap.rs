//! 6.3d (system design 3A): the Parquet sink's writer heap is bounded by the budget.
//!
//! A Parquet sink encodes on the run's workers, one lane per worker that may encode at once, and
//! each lane's `ArrowWriter` keeps on the heap, outside the arena, its in-progress row group's
//! encoded pages and every column's dictionary and open page. The sink claims its writers' need
//! above the arena (`BuildCtx::claim_writer_heap`): once the run knows the sink's schema it
//! grants one lane per worker at the claim's page, or a smaller page, or fewer lanes, before the
//! arena takes the rest (system design 3A), and what its writers hold has to stay within that
//! claim, whatever the columns and the lanes. Each column also keeps a Zstandard context in the
//! C library's memory, which this allocator does not see and the claim counts by its measured
//! size; a run whose columns one lane cannot hold is refused before it starts, naming the least
//! budget that holds them. The run's note says what the writers claimed.
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
/// The room the run's arena sizing left above the arena, as the sink's builder was given it
/// (before the writers' claim).
static HEADROOM: AtomicU64 = AtomicU64::new(0);
static ARENA: AtomicU64 = AtomicU64::new(0);
static WORKERS: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting the bytes a thread allocates and frees while it is inside the
/// measured sink. Nothing is allocated here, so the thread locals need no allocation either.
struct Counting;

fn counted(delta: i64) {
    // As the engine's allocator does: the sink measures its writers' heap.
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
                        let built = sink.build(ctx)?;
                        Ok(Box::new(Measured(built)) as Box<dyn Sink>)
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

/// Rows of `columns` small categorical columns (four values each, a byte apiece): parquet
/// encodes each with a dictionary of four entries, so its pages are a few bits a row, while
/// the writer holds every value of a column's open page as a `u64` dictionary index and an
/// `i16` level, and each column keeps a Zstandard context.
fn categorical(columns: usize) -> impl Fn(i64, i64) -> RecordBatch {
    move |from, rows| {
        let columns: Vec<(String, arrow::array::ArrayRef)> = (0..columns)
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
}

const GROUPS: i64 = 8;
const ROWS: i64 = 25_000;

/// A copy of `file` into a Parquet sink under `out` at `budget` on two workers, in a process of
/// its own; what its sink held, against what it was granted, is printed.
fn copy(scratch: &Scratch, file: &Path, out: &Path, budget: u64) -> serde_json::Value {
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
    eprintln!(
        "at {budget} bytes the sink's writers held {} bytes at most, against their claim {:?} \
         (writers, arena, decoders, their fixed need) with {} workers; the run: {}, peak {} of \
         the ceiling, {}",
        sink["held_peak"],
        claim(&outcome),
        sink["workers"],
        outcome["report"]["exit"],
        outcome["report"]["peak_fraction_of_ceiling"],
        outcome["error"]
    );
    outcome
}

/// The report's notes.
fn notes(outcome: &serde_json::Value) -> Vec<String> {
    outcome["report"]["notes"]
        .as_array()
        .map(|notes| {
            notes
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// What the run's note says the sinks' writers claimed above the arena, the arena it left, the
/// decoders' room above it, and the part of that room their columns hold whatever the batch:
/// `None` when nothing was claimed.
fn claim(outcome: &serde_json::Value) -> Option<(u64, u64, u64, u64)> {
    let note = notes(outcome)
        .into_iter()
        .find(|n| n.starts_with("the sinks' writers claim "))?;
    let number = |after: &str| -> u64 {
        note.split(after)
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{after} in {note}"))
    };
    Some((
        number("the sinks' writers claim "),
        number("the arena is "),
        number("and the "),
        number("decoders', "),
    ))
}

/// The leaf columns of `columns` categorical columns, as the sink sees them.
fn leaves_of(columns: usize) -> moruna_sinks::Leaves {
    moruna_sinks::leaves_of(&categorical(columns)(0, 1).schema()).expect("leaves")
}

fn zstd() -> parquet::basic::Compression {
    parquet::basic::Compression::ZSTD(Default::default())
}

/// 3A, 6.3d: a copy of 96 small categorical columns, two workers, the smallest budget. The
/// writers claim their need above the arena before the arena takes the rest: their codecs at the
/// claim's page do not fit two lanes, so memory bounds the page before the lanes (P1). The sink
/// writes on one lane per worker at a smaller page, the report says so with the budget that
/// keeps the claim's page, what the writers hold on the heap stays within their claim, and the
/// run completes under its ceiling.
///
/// Before the fix (2026-11-18, the Mac) the writers' heap came from parquet's defaults: 384 such
/// columns held 261 MB against a room of 79 MB, and the controller stopped the run at 1.78
/// times its ceiling.
#[test]
fn a_wide_table_writes_smaller_pages_before_fewer_lanes() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("writer_heap_pages");
    let file = scratch.path().join("in.parquet");
    grouped(&file, GROUPS, ROWS, categorical(96));
    let budget = BUDGETS[0];
    let out = scratch.path().join("out");
    let outcome = copy(&scratch, &file, &out, budget);
    let held = outcome["sink"]["held_peak"].as_i64().expect("measured");
    let (writers, arena, _, fixed) = claim(&outcome).expect("the writers claimed their need");
    assert_eq!(
        fixed,
        2 * 96 * moruna_sources::FIXED_COLUMN_DECODE_BYTES,
        "one decode per worker of 96 fixed-width columns"
    );
    let least = moruna_sinks::least_lane_bytes(&leaves_of(96), zstd());
    let at_claim =
        moruna_sinks::lane_bytes_for_page(&leaves_of(96), zstd(), moruna_sinks::CLAIM_PAGE_BYTES);
    assert!(
        writers >= 2 * least && writers < 2 * at_claim,
        "two lanes below the claim's page: {writers} ({least} least, {at_claim} at the claim)"
    );
    assert!(arena > 0);
    assert!(
        held <= writers as i64,
        "the sink's writers held {held} bytes outside the arena, more than their {writers} byte \
         claim above it"
    );
    let report = apart::within(&outcome, "a copy of 96 categorical columns", budget);
    let notes = notes(&outcome);
    assert!(
        !notes.iter().any(|n| n.contains("not one per worker")),
        "one lane per worker: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("writers' heap is measured by the counting allocator")),
        "the report says the heap was measured: {notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("writes pages of at most")
            && n.contains(&format!("keeps {}", moruna_sinks::CLAIM_PAGE_BYTES))),
        "the report notes the page and the budget that keeps the claim's: {notes:?}"
    );
    assert_eq!(report["exit"], json!("Completed"));
    assert_eq!(support::read_back_rows(&out), (GROUPS * ROWS) as u64);
}

/// A copy of `columns` such columns in groups of `rows` rows, whose decoders' fixed need and
/// one writer lane at the least page the smallest budget cannot hold beside the arena's least:
/// refused before the run starts, naming the least budget that holds them; a run at that budget
/// writes on one lane, its writers within their claim, and completes under its ceiling.
fn refused_then_held(name: &str, columns: usize, rows: i64) {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new(name);
    let file = scratch.path().join("in.parquet");
    grouped(&file, GROUPS, rows, categorical(columns));
    let out = scratch.path().join("out");
    let refused = copy(&scratch, &file, &out, BUDGETS[0]);
    let least = moruna_sinks::least_lane_bytes(&leaves_of(columns), zstd());
    let error = refused["error"].as_str().expect("refused");
    assert!(error.starts_with("config budget.host:"), "{error}");
    assert!(
        error.contains(&format!("{columns} leaf columns")),
        "{error}"
    );
    assert!(error.contains(&least.to_string()), "{error}");
    let decoders = 2 * columns as u64 * moruna_sources::FIXED_COLUMN_DECODE_BYTES;
    assert!(error.contains(&decoders.to_string()), "{error}");
    let budget: u64 = error
        .rsplit("The least budget that holds it is ")
        .next()
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("the least budget is named");
    assert!(budget > BUDGETS[0], "{budget}");
    assert!(
        !out.exists() || support::read_back_rows(&out) == 0,
        "nothing was written"
    );
    let outcome = copy(&scratch, &file, &out, budget);
    apart::within(&outcome, "a copy at the least budget named", budget);
    let held = outcome["sink"]["held_peak"].as_i64().expect("measured");
    let (writers, _, _, fixed) = claim(&outcome).expect("the claims");
    assert_eq!(fixed, decoders);
    assert!(
        writers >= least && writers < 2 * least,
        "one lane at the least page, and its room: {writers} for {least}"
    );
    assert!(held <= writers as i64, "{held} over {writers}");
    assert!(
        notes(&outcome)
            .iter()
            .any(|n| n.contains("writes on 1 lanes, not one per worker")),
        "the report notes the lanes: {:?}",
        notes(&outcome)
    );
    assert_eq!(support::read_back_rows(&out), (GROUPS * rows) as u64);
}

/// 3A, 6.3d: 384 such columns at the smallest budget: refused, then held at the least budget
/// named, on one lane.
#[test]
fn a_wide_table_one_lane_cannot_hold_is_refused_then_held_at_the_least_budget() {
    refused_then_held("writer_heap_384", 384, ROWS);
}

/// 3A, 6.3d: 768 such columns, in smaller groups, the same (before 6.3s's fix a read the
/// controller cut was sometimes pushed after its queue closed, and the run ended "staging: push:
/// stage 0 is closed" or completed a row group short).
#[test]
fn a_wider_table_is_refused_then_held_at_the_least_budget() {
    refused_then_held("writer_heap_768", 768, ROWS / 5);
}

/// 3A, 6.3d: a chain whose kernels declare nothing outside the arena (`rename`, `select`),
/// writing Parquet, at the smallest budget. The sink adds no factor: its writers claim their
/// need above the arena, as the source's decoders claim theirs, and the arena takes the whole
/// of what is left, so exactly the claims are above it. The run completes under its ceiling with every row. Before
/// 6.3d, the arena took the whole allowance and the writers had no room at any budget
/// (HO-T20, PN-T25 refused).
#[test]
fn a_chain_declaring_nothing_outside_the_arena_leaves_its_writers_their_claim() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("writer_heap_free_chain");
    let file = scratch.path().join("in.parquet");
    grouped(&file, GROUPS, ROWS, categorical(8));
    let budget = BUDGETS[0];
    let out = scratch.path().join("out");
    let outcome = apart::run(
        "child_run",
        &json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": file},
            "kernels": [
                {"kind": "std", "name": "rename", "args": {"columns": {"c0": "first"}}},
                {"kind": "std", "name": "select", "args": {"columns": ["first", "c1", "c2"]}},
            ],
            "sink": {"kind": "parquet", "url": out},
            "budget": {"memory_bytes": budget, "cpu": 2.0},
            "profiles_dir": scratch.path().join("profiles"),
        }),
    );
    apart::within(&outcome, "a free chain into Parquet", budget);
    let notes = notes(&outcome);
    let sized = notes
        .iter()
        .find(|n| n.starts_with("arena sized at"))
        .expect("the arena's note");
    assert!(
        sized.contains("less the 0 kept above the arena beside them: 0.00 of those bytes"),
        "{sized}"
    );
    assert!(
        sized.contains("by the 0.00 bytes the chain is expected to allocate"),
        "{sized}"
    );
    let (writers, _, decoders, fixed) = claim(&outcome).expect("the writers claimed their need");
    let three =
        moruna_sinks::lane_bytes_for_page(&leaves_of(3), zstd(), moruna_sinks::CLAIM_PAGE_BYTES);
    assert!(
        writers >= 2 * three,
        "two lanes of three columns at the claim's page at the least: {writers}"
    );
    assert_eq!(
        fixed,
        2 * 8 * moruna_sources::FIXED_COLUMN_DECODE_BYTES,
        "the source's eight columns, one decode per worker"
    );
    // Their least batches beside their fixed need: the least morsel, four times over, a worker.
    let least_batches = 2 * 4 * (4u64 << 20);
    assert!(
        decoders - fixed - least_batches < 64 << 10,
        "only the claims are above the arena: {decoders} for {fixed} and {least_batches}"
    );
    assert_eq!(support::read_back_rows(&out), (GROUPS * ROWS) as u64);
}

/// The encoded bytes of every row group the copy under `out` wrote, file by file, each file's
/// last group apart (it ends where the file rolls or the input ends).
fn row_groups(out: &Path) -> (Vec<i64>, Vec<i64>) {
    use parquet::file::reader::FileReader;
    let (mut whole, mut last) = (Vec::new(), Vec::new());
    for entry in std::fs::read_dir(out).expect("the output") {
        let path = entry.expect("an entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        let file = std::fs::File::open(&path).expect("a file");
        let reader = parquet::file::reader::SerializedFileReader::new(file).expect("a reader");
        let groups: Vec<i64> = reader
            .metadata()
            .row_groups()
            .iter()
            .map(|g| g.compressed_size())
            .collect();
        if let Some((end, rest)) = groups.split_last() {
            whole.extend_from_slice(rest);
            last.push(*end);
        }
    }
    (whole, last)
}

/// Rows of `columns` Int64 columns of scattered values, which encode to about their decoded
/// size.
fn scattered(columns: usize) -> impl Fn(i64, i64) -> RecordBatch {
    move |from, rows| {
        let columns: Vec<(String, arrow::array::ArrayRef)> = (0..columns)
            .map(|c| {
                let values = (from..from + rows).map(|r| {
                    (r.wrapping_mul(2_654_435_761) ^ (c as i64).wrapping_mul(40_503)) & 0xffff_ffff
                });
                (
                    format!("c{c}"),
                    Arc::new(arrow::array::Int64Array::from_iter_values(values))
                        as arrow::array::ArrayRef,
                )
            })
            .collect();
        RecordBatch::try_from_iter(columns).expect("a group")
    }
}

/// A copy of `file` into a Parquet sink with a row group target of `row_group_bytes`.
fn copy_with_target(
    scratch: &Scratch,
    file: &Path,
    out: &Path,
    budget: u64,
    row_group_bytes: u64,
) -> serde_json::Value {
    apart::run(
        "child_run",
        &json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": file},
            "sink": {"kind": "parquet", "url": out,
                     "options": {"row_group_bytes": row_group_bytes}},
            "budget": {"memory_bytes": budget, "cpu": 2.0},
            "profiles_dir": scratch.path().join("profiles"),
        }),
    )
}

/// 3A, 6.3d (PM, 2026-11-18): the writers are granted up to their room, their claim the floor.
/// At an ample budget their row groups reach the sink's own target (16 MiB here) before their
/// heap, as before 6.3d; the report says what they were granted above their claim.
#[test]
fn at_an_ample_budget_the_row_groups_reach_the_sinks_target() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("writer_heap_ample");
    let file = scratch.path().join("in.parquet");
    grouped(&file, 6, 50_000, scattered(32));
    let out = scratch.path().join("out");
    let target: i64 = 16 << 20;
    let outcome = copy_with_target(&scratch, &file, &out, BUDGETS[1], target as u64);
    apart::within(&outcome, "an ample copy", BUDGETS[1]);
    let held = outcome["sink"]["held_peak"].as_i64().expect("measured");
    let (writers, _, _, _) = claim(&outcome).expect("the claims");
    assert!(held <= writers as i64, "{held} over {writers}");
    assert!(
        notes(&outcome)
            .iter()
            .any(|n| n.contains("above their claim") && n.contains(&format!("target of {target}"))),
        "the report says the writers were granted their row groups: {:?}",
        notes(&outcome)
    );
    let (whole, _) = row_groups(&out);
    assert!(!whole.is_empty(), "several groups a file");
    for size in &whole {
        assert!(
            *size >= target * 3 / 4,
            "a group of {size} bytes stopped short of the {target} byte target: {whole:?}"
        );
    }
    assert_eq!(support::read_back_rows(&out), 6 * 50_000);
}

/// 3A, 6.3d: a row group target past what the room above the arena holds beside the claims:
/// the writers are granted all of it, their row groups are flushed by their heap, and the report
/// says so rather than that they reach the target.
#[test]
fn a_target_past_the_room_is_flushed_by_the_heap_and_said() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("writer_heap_past_room");
    let file = scratch.path().join("in.parquet");
    grouped(&file, 6, 50_000, scattered(32));
    let out = scratch.path().join("out");
    let outcome = copy_with_target(&scratch, &file, &out, BUDGETS[1], 1 << 30);
    apart::within(&outcome, "a copy past its room", BUDGETS[1]);
    let held = outcome["sink"]["held_peak"].as_i64().expect("measured");
    let (writers, _, _, _) = claim(&outcome).expect("the claims");
    assert!(held <= writers as i64, "{held} over {writers}");
    let notes = notes(&outcome);
    assert!(
        notes.iter().any(|n| n.contains("above their claim")
            && n.contains("so its row groups are flushed by its heap")),
        "the report says the heap flushes the row groups: {notes:?}"
    );
    assert!(
        !notes
            .iter()
            .any(|n| n.contains("reaches its row group target")),
        "{notes:?}"
    );
    assert_eq!(support::read_back_rows(&out), 6 * 50_000);
}

/// 3A, 6.3d: at 672 MiB a table whose writers' room is their claim (384 one-byte columns)
/// gets no more than its room: its row groups stop at the writers' heap, short of the sink's
/// target, and the run stays under its ceiling.
#[test]
fn at_a_tight_budget_the_writers_keep_to_their_room() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("writer_heap_tight");
    let file = scratch.path().join("in.parquet");
    grouped(&file, GROUPS, ROWS, categorical(384));
    let out = scratch.path().join("out");
    let budget = 672u64 << 20;
    let target: i64 = 16 << 20;
    let outcome = copy_with_target(&scratch, &file, &out, budget, target as u64);
    apart::within(&outcome, "a tight copy", budget);
    let held = outcome["sink"]["held_peak"].as_i64().expect("measured");
    let (writers, _, _, _) = claim(&outcome).expect("the claims");
    assert!(held <= writers as i64, "{held} over {writers}");
    let notes = notes(&outcome);
    let page = notes
        .iter()
        .find(|n| n.contains("writes pages of at most"))
        .expect("the page is cut: the room is the claim");
    let room: u64 = page
        .split('(')
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("the room is said");
    assert!(writers <= room, "{writers} over the room {room}");
    let (whole, last) = row_groups(&out);
    assert!(
        whole.iter().chain(&last).all(|size| *size < target),
        "every group stops at the heap, short of the target: {whole:?} {last:?}"
    );
    assert_eq!(support::read_back_rows(&out), (GROUPS * ROWS) as u64);
}
