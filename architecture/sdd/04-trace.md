# Moruna SDD 04: Trace writer and run report (`moruna-trace`)

**Document type:** software design document, component 4 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/moruna-runtime-design.md` section 5.9; criteria S9; global invariant G-I4
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.1 (`RunId`), d.13 (`TraceRecord`, `TraceSink`, `TraceTail`, `Outcome`), e.5 (schema)
**Component location:** `crates/moruna-trace`, Rust
**Consumes:** contracts (1). **Consumed by:** scheduler (10, emits and flushes), controller (11, reads through `TraceTail`), runtime facade (12, `finish` and the report)

**Decisions worth your eye:** (1) the trace is kept in memory as Arrow batches up to a limit and then overflows to a file in the staging directory, so a run with no trace path still produces a complete report; (2) the report's numbers are defined here as formulas over trace columns, so two implementations of the report agree; (3) the writer never drops a record; backpressure is on the writer thread, not on workers; (4) the writer is the contract's `TraceTail`, so the controller reads the recent window through a trait and never sees this crate's types.

---

## a. Purpose and boundary

The trace is the runtime's memory of what happened, one record per morsel per stage. The writer takes records from any thread through a bounded channel, batches them into Arrow, keeps them in memory up to a limit, spills the rest to a file, and can write the whole trace as an Arrow IPC file when a path was given. The report is a pure function of the trace plus the discovered limits; it is what the user reads and what S1 to S4 are measured from.

It owns: the channel and writer thread; in-memory batching; overflow; the IPC file; the report computation and its rendering.

It refuses to know: what a morsel is beyond its record; why a knob changed; anything about the host except what `Limits` says.

## b. Vocabulary

**Chunk.** An in-memory Arrow `RecordBatch` of up to 4,096 trace records.

**Overflow file.** `trace-overflow-<run id>.arrow` in the staging directory, Arrow IPC stream, appended chunk by chunk when memory chunks exceed the limit.

**Run id.** The contracts' `RunId` (d.1): 16 random bytes printed as 32 lowercase hex characters. The runtime facade mints it with `getrandom` on a fresh run and reads it from the manifest header on a resumed run (12 f.7); the writer carries it into the overflow file name, the final file name and the report.

**Flush.** The contract's `TraceSink::flush`: every record recorded before the call has been appended to a chunk and, when a final path was given, written to the final file; callable any number of times, from any thread, by the scheduler at every exit.

**Finish.** This crate's `finish`: a flush, then the IPC footer, then the writer thread joins. Called exactly once, by the facade.

**Report.** `RunReport`, the struct in d.1, plus its `Display` and JSON renderings.

## c. Invariants

**TR-I1. No record is dropped.** Every `record` call results in the record appearing in the final trace; if the channel is full, the caller blocks for the push (bounded by the writer's drain rate, which is faster than any worker's completion rate by construction, since a drain is a memcpy into a builder). Upholds G-I4.

**TR-I2. Order is per stage and per sequence.** The final trace, read back, sorted by `(stage, seq)`, has no gaps within a stage for sequences that entered that stage, and each `(stage, seq)` appears exactly once.

**TR-I3. The report is a pure function.** `RunReport::compute(trace, limits, meta)` is deterministic (d.1 and 12 f.2 name it `compute`, and this invariant said `from`; the name is `compute`, PM 2026-09-22); two calls on the same inputs produce equal structs. Upholds S9.

**TR-I4. Memory is bounded.** In-memory chunks never exceed `trace.memory_limit` (64 MiB default); beyond it, older chunks are written to the overflow file and freed.

**TR-I5. Flush on every exit.** Completion, termination and cancellation all end with the scheduler calling `flush` (SC f.10, e.2), which writes remaining chunks and makes the trace readable, followed by the facade calling `finish` exactly once, which closes the IPC file if any and joins the writer; a crash between records loses at most one channel's worth.

**TR-I6. The schema hash is checked at open.** The writer asserts `TraceRecord::SCHEMA_HASH` against the schema it builds; mismatch is a `Config` error at start (a build inconsistency), never a silent divergence.

## d. Interfaces

### d.1 Exposed

```rust
pub struct TraceConfig {
    pub path: Option<std::path::PathBuf>,       // final IPC file; None = in-memory + overflow only
    pub staging_dir: std::path::PathBuf,        // for overflow
    pub channel_capacity: usize,                // trace.channel_capacity
    pub memory_limit: u64,                      // trace.memory_limit
    pub run_id: RunId,                          // contracts d.1; minted or read by the facade (12 f.1, f.7)
}

pub struct TraceWriter { /* private */ }
impl TraceWriter {
    pub fn start(cfg: TraceConfig) -> Result<std::sync::Arc<TraceWriter>>;   // spawns the writer thread
    /// Read-only view over everything recorded so far (in-memory chunks + overflow); used by the facade for the report.
    pub fn snapshot(&self) -> TraceView;
    /// TR-I5. Flush, footer, join. Idempotent; the facade calls it once after the scheduler returned.
    pub fn finish(&self) -> Result<TraceView>;
}
impl TraceSink for TraceWriter { /* contracts d.13: `record` (f.1), `flush` (b, called by the scheduler at every exit) */ }
/// Contracts d.13. The controller holds the writer as `Arc<dyn TraceTail>` and never names this crate.
impl TraceTail for TraceWriter { /* `tail(stage, n)`: f.3, in-memory chunks only */ }

pub struct TraceView { /* Arc'd chunks + overflow reader */ }
impl TraceView {
    pub fn len(&self) -> u64;
    pub fn batches(&self) -> impl Iterator<Item = arrow::record_batch::RecordBatch>;
    /// Last `n` records for stage `stage`, newest last; the same walk as `TraceTail::tail` (f.3).
    pub fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord>;
    pub fn to_ipc_file(&self, path: &std::path::Path) -> Result<()>;
}

/// The effective interpreter state of one Python stage is `moruna_kernel::GilState`
/// (contracts d.7), as the adapter observed it (05 d.1 `PyKernel::gil_state`); this
/// crate serialises it by name (`"FreeThreaded"`, `"Serialised"`) and defines no type of its own.
use moruna_kernel::GilState;

#[derive(Clone, Debug, serde::Serialize)]
pub enum ExitReason { Completed, Terminated { diagnostic: String }, Cancelled }

/// What the report needs that is not in the trace. Assembled by the facade (12 f.2).
#[derive(Clone, Debug)]
pub struct RunMeta {
    pub run_id: RunId,
    pub exit: ExitReason,
    pub start_ns: u64, pub end_ns: u64,
    pub resumed: bool,                          // the run continued from a manifest (12 f.7)
    pub manifest: Option<std::path::PathBuf>,   // last manifest written, when one was
    pub notes: Vec<String>,                     // discovery notes, facade clamps (12 f.3), runtime notes
    pub gil: Vec<(StageId, GilState)>,          // one entry per Python stage (05 d.1)
    pub io_paths: IoPaths,                      // reactor.paths()
    pub sizer: &'static str, pub sizer_fallback_at: Option<Seq>,
    pub bottleneck_timeline: Vec<(f64, String)>,   // from ControllerSummary (11 d.1)
    pub controller_notes: Vec<String>,          // ControllerSummary.notes, appended to `notes` in the report
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct StageReport {
    pub stage: StageId,
    pub morsels: u64, pub rows_in: u64, pub rows_out: u64, pub bytes_in: u64, pub bytes_out: u64,
    pub wall_s: f64, pub kernel_busy_s: f64,
    pub rows_per_s: f64, pub bytes_per_s: f64,
    pub amplification_p50: f64, pub amplification_p95: f64,
    pub placement_miss_wait_s: f64, pub errors: u64, pub skipped: u64,
    /// The largest `TraceRecord::state_bytes` seen for this stage, and the difference
    /// between the last and the first. f.2 computes both and the text says they are
    /// reported; d.1 omitted them (PM, 2026-09-22, on the component 4 agent's report).
    /// A stage whose state grows with morsels seen is what RC f.3 budgets for.
    pub state_bytes_max: u64, pub state_growth: i64,
}

/// The discovered limits as the report carries them. Named by d.1 and never defined
/// until now (PM, 2026-09-22); the report is read by people and by the bench runner,
/// so it carries values rather than the `Limits` struct's shape.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LimitsSummary {
    pub memory_ceiling: u64,
    pub memory_kill: Option<u64>,
    pub cpu_quota: f64,
    pub source: String,              // LimitSource: "cgroup", "os" or "explicit"
    pub devices: Vec<String>,        // one line per device: id, name, total bytes
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RunReport {
    pub run_id: String,                       // meta.run_id as 32 lowercase hex characters
    pub exit: ExitReason,                     // Completed | Terminated { diagnostic } | Cancelled
    pub resumed: bool,                        // meta.resumed
    pub manifest: Option<String>,             // meta.manifest, when the run is resumable
    pub wall_s: f64,
    pub limits: LimitsSummary,                // defined below
    /// A trace record that could not reach the file because the overflow path failed
    /// (h), and a record that arrived after `finish` (e.1). Both are conditions the
    /// report must surface, because G-I4 says every morsel leaves exactly one record
    /// and these are how that stops being true; TR-T9 asserts them. Added to d.1 on
    /// 2026-09-22 (PM), which named neither while h and e.1 required them.
    pub overflow_failed: bool,
    pub late_records: u64,
    pub io_paths: IoPaths,                    // paths taken (meta.io_paths)
    pub peak_anon_bytes: u64,
    pub peak_fraction_of_ceiling: f64,        // S1
    pub worker_busy_fraction: f64,            // S4
    pub cpu_throttled_fraction: f64,
    pub source_bytes_per_s: f64,
    pub source_bandwidth: f64,                // bytes per second over the whole run (f.2); shown beside staging_bandwidth
    pub staging_bandwidth: f64,               // bytes moved to and from staging per second over the whole run (f.2)
    pub staging_bytes_written: u64,
    pub staging_engaged: bool,
    pub gil: Vec<(StageId, GilState)>,        // meta.gil
    pub gil_serialised: bool,                 // any stage Serialised
    pub sizer_used: String, pub sizer_fallback_at: Option<Seq>,
    pub bottleneck_timeline: Vec<(f64, String)>,   // (seconds, classification) from the controller's records
    pub stages: Vec<StageReport>,
    pub notes: Vec<String>,                   // meta.notes followed by meta.controller_notes
}

impl RunReport {
    pub fn compute(trace: &TraceView, limits: &Limits, meta: &RunMeta) -> RunReport;   // TR-I3
    pub fn to_json(&self) -> String;
}
impl core::fmt::Display for RunReport { /* section e.3 layout */ }
```

`RunMeta` carries what is not in the trace; the facade assembles it (12 f.2). The Python `RunReport` (12 d.2) exposes exactly these fields as attributes, so `run_id`, `manifest`, `resumed`, `notes`, `gil_serialised` and `io_paths` are report fields here and not surface additions.

### d.2 Consumed

`moruna_kernel::{TraceRecord, TraceSink, TraceTail, Outcome, Limits, IoPaths, RunId, StageId, Seq, MorunaError}`; `arrow` (builders, IPC stream writer and reader); `crossbeam_channel::bounded`; `serde_json`.

## e. Data model, formats and state machines

### e.1 Writer state machine

`Running` → (`finish`) → `Flushing` → `Finished`. `flush` does not change the state: in `Running` it drains the channel into chunks and returns when every record recorded before the call is in a chunk (and in the final file when a path was given); it may be called any number of times, by the scheduler at each exit (SC f.10) and by tests. `record` in `Finished` is a no-op counted in `late_records` (a diagnostic; should be zero). `finish` is idempotent.

### e.2 Files

Overflow: Arrow IPC stream format, one message per chunk, in `staging_dir/trace-overflow-<run_id>.arrow` (`run_id` as 32 lowercase hex characters); deleted on `finish` after the final file is written, or kept if no `path` was given and `TraceView` is still referenced (the view reads it lazily). Final: Arrow IPC **file** format (random access) at `path`, schema per contracts e.5, one batch per chunk, footer written on `finish`.

### e.3 Report rendering

`Display` produces a fixed layout of at most 40 lines: header (run id, exit, resumed, wall), limits line, memory line (peak, fraction, ceiling), CPU line (busy fraction, throttled fraction, workers), IO line (source bytes per second, paths taken), staging line (engaged, bytes, then staging bandwidth and source bandwidth side by side so a staging directory that shares a device with the source is visible, architecture 7), one line per stage (morsels, rows/s, amplification p50/p95, misses), sizer line, GIL line when any stage is Python, manifest line when one exists, and notes. Numbers use binary units for bytes and three significant figures.

## f. Algorithms and policies

**f.1 Record path.** `record(r)` pushes into the bounded channel (blocking push). The writer thread pops in batches of up to 4,096 or every 100 ms, appends to per-column builders, and on reaching 4,096 rows finalises a chunk. Chunks are kept in a `Vec<Arc<RecordBatch>>` guarded by a mutex; when their total size exceeds `memory_limit`, the oldest chunks are appended to the overflow file and dropped from memory.

**f.2 Report formulas.** Let `T` be the trace rows for a stage, `W` the wall time of the run (`meta.end - meta.start`), `N` the active worker count as recorded in `knob_active_workers` (piecewise constant).

- `wall_s(stage)` = max(`t_end`) − min(`t_start`) over T, in seconds.
- `kernel_busy_s` = Σ(`t_end − t_start`) over T with `outcome ∈ {Ok, Probe}`.
- `rows_per_s` = Σ`rows_out` / `wall_s(stage)`; `bytes_per_s` likewise with `bytes_out`.
- `amplification` per record = (`mem_anon_peak − mem_anon_before`) / `bytes_in`, over records with `bytes_in > 0`; p50 and p95 by nearest-rank.
- `placement_miss_wait_s` = Σ`placement_miss_wait_us` / 1e6.
- `peak_anon_bytes` = max(`mem_anon_peak`) over all records; `peak_fraction_of_ceiling` = that / `limits.memory_ceiling`.
- `worker_busy_fraction` = Σ over all stages of `kernel_busy_s` / (W × mean N), where mean N is time-weighted from the records.
- `cpu_throttled_fraction` = Σ`throttled_delta_us` / (W × `limits.cpu_quota` × 1e6).
- `source_bytes_per_s` = Σ`bytes_in` for stage 1 (the first kernel's input, i.e. what the source produced) / `wall_s(stage 1)`.
- `staging_bytes_written` = Σ max(`staging_bytes_delta`, 0); `staging_engaged` = that > 0.
- `staging_bandwidth` = Σ |`staging_bytes_delta`| / W (bytes demoted to and promoted from staging over the whole run, per second); `source_bandwidth` = Σ`bytes_in` for stage 1 / W. Both are over W, not over a stage's wall time, so the two are comparable side by side (architecture 7, "spill directory on the same device as the source").
- `state_bytes_max` per stage = max(`state_bytes`); `state_growth` per stage = last `state_bytes` − first non-zero `state_bytes` (reported so a stateful kernel whose state grows with morsels seen is visible in the report; RC f.6 StateGrowth). Records are grouped by `instance` (contracts d.13) when it is not `u16::MAX`, so a growing instance is not hidden by a fresh one.
- `gil_serialised` = any entry of `meta.gil` is `Serialised`; `notes` = `meta.notes` followed by `meta.controller_notes`; `run_id` = hex of `meta.run_id`.

**f.3 `tail(stage, n)` (`TraceTail`).** Walk in-memory chunks from newest, including the builder's partial chunk; filter by stage; stop at n; return oldest first (the contract's order). Does not read the overflow file (the controller's window is recent by definition; a `tail` larger than what is in memory returns fewer records). The controller calls it every tick through `Arc<dyn TraceTail>`; the walk holds the chunk mutex for the duration of the walk only, never across a channel operation.

## g. Concurrency within the component

One writer thread. `record` is callable from any thread; the channel is the only shared structure on that path. The chunk vector's mutex is held by the writer during append and by `snapshot`/`tail` readers briefly; lock order position 5 in preamble 4.2 (the controller's own state is position 6, so a `tail` from the tick thread takes 5 after 6 is released, never inside it). `flush` is a rendezvous with the writer thread (a marker pushed through the channel and waited on), so several callers may flush concurrently. `finish` joins the writer thread.

## h. Behaviour

**Normal path.** `start` (the facade's fourth startup step, 12 PY-I1); workers `record` per morsel; the controller calls `tail` every tick; at completion the scheduler calls `flush` (SC e.2), then the facade calls `finish`, which writes the file and returns the view; the facade computes the report.

**Edge cases.** Zero morsels (empty source): trace is empty, report has zero stages' rows and `wall_s` from meta; no division by zero (rates are 0.0). A stage with only a probe record: amplification from the probe alone; `morsels = 1`. Zero kernels (`kernels=[]`, SC h): no stage rows at all; `source_bandwidth` is 0.0 because no stage-1 record exists, and the report says "no kernel stages". Records arriving after `finish` (a worker finishing late during cancellation): counted in `late_records`, reported.

**Failures.** Overflow file write failure (disk full): the writer keeps chunks in memory beyond the limit and sets a `overflow_failed` flag surfaced in the report; it never drops records (TR-I1). Final file write failure: `finish` returns `Io`; the in-memory view is still returned through the error's context so the report can be produced. Channel capacity exhausted and the writer thread dead (panic): `record` would block forever; the writer thread is wrapped so a panic converts to `overflow_failed` and a drain loop that discards into a counter, and the facade terminates the run with a diagnostic naming the trace failure.

## i. Configuration

`trace.path`, `trace.channel_capacity`, `trace.memory_limit`.

## j. Observability

The trace is the observability. `tracing` events: `trace.overflow` (debug, chunk count), `trace.finished` (info, record count, file path), `trace.late_record` (warn).

## k. Tests

**TR-T1 no_drop.** 16 threads record 1 M records with the channel capacity at 256; final count is 1 M. TR-I1.

**TR-T2 order.** Records per stage with random interleaving; read back sorted by (stage, seq): no gaps, no duplicates. TR-I2.

**TR-T3 pure_report.** Same trace and meta → equal JSON, twice, across processes (golden file). TR-I3.

**TR-T4 memory_bound.** 10 M records with `memory_limit` 8 MiB; process RSS growth under 16 MiB above baseline; overflow file present. TR-I4.

**TR-T5 flush_on_exit.** For each exit reason, the final file is complete and readable by `arrow` IPC reader. TR-I5.

**TR-T6 schema_hash.** Writer refuses to start when a test-only cfg alters a field type. TR-I6.

**TR-T7 formulas.** Synthetic trace with known values; each report field equals the hand-computed value in the test. f.2.

**TR-T8 tail.** `tail(stage, n)` through `Arc<dyn TraceTail>` returns the newest n for that stage, oldest first, including records still in the builder's partial chunk. f.3.

**TR-T9 disk_full.** Overflow directory on a 1 MiB tmpfs; records still counted; `overflow_failed` set. h failures.

**TR-T10 staging_vs_source_bandwidth.** Synthetic trace over a 10 s run with 2 GiB of stage-1 `bytes_in` and `staging_bytes_delta` summing to +1 GiB and then −1 GiB: `source_bandwidth` is 204.8 MiB/s, `staging_bandwidth` is 204.8 MiB/s, both appear on the staging line of `Display` and in `to_json`; a run with no staging deltas reports 0.0 and `staging_engaged == false`. f.2, e.3 (architecture 7, spill directory on the same device as the source).

**TR-T11 flush_then_finish.** From four threads, `record` 100,000 records, then each thread calls `flush` concurrently; after every `flush` returns, `snapshot().len() == 400,000`; `finish` once afterwards writes the footer; a second `finish` is a no-op; `record` after `finish` counts in `late_records`. TR-I5, e.1.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/writer.rs` (f.1, e.1, the `TraceSink` and `TraceTail` impls, `flush`, `finish`), `src/view.rs` (`TraceView`, overflow reader), `src/report.rs` (f.2, e.3, JSON, `RunMeta`, `ExitReason`), `src/render.rs` (`Display`). No `unsafe`.

Use Arrow builders per column, not row-to-batch conversion of `Vec<TraceRecord>`; the `feat_column_bytes`, `q_bytes_before`, `q_bytes_after` lists use `ListBuilder<UInt64Builder>`.

Anti-patterns: no unbounded `Vec<TraceRecord>`; no `record` path that allocates per call beyond the channel slot; no report field computed anywhere but `report.rs`.

## m. Open items

None. (`trace.memory_limit` is in the preamble's table.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S9, G-I4 | TR-I1, TR-I2, TR-I3 | TR-T1, TR-T2, TR-T3, TR-T7 |
| S1, S4 (measurement) | f.2 | TR-T7 |
| G-I8 (diagnostic durability) | TR-I5 | TR-T5, TR-T11 |
| architecture 7 (spill directory on the source's device) | f.2 bandwidths, e.3 | TR-T10 |
| D2, RC (controller window) | f.3 `TraceTail` | TR-T8 |

## o. Deferred (post-v1)

None.
