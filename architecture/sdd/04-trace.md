# Amoru SDD 04: Trace writer and run report (`amoru-trace`)

**Document type:** software design document, component 4 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` section 5.9; criteria S9; global invariant G-I4
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.13 (`TraceRecord`, `TraceSink`, `Outcome`), e.5 (schema)
**Component location:** `crates/amoru-trace`, Rust
**Consumes:** contracts (1). **Consumed by:** scheduler (10, emits), controller (11, reads), python surface (12, report)

**Decisions worth your eye:** (1) the trace is kept in memory as Arrow batches up to a limit and then overflows to a file in the staging directory, so a run with no trace path still produces a complete report; (2) the report's numbers are defined here as formulas over trace columns, so two implementations of the report agree; (3) the writer never drops a record; backpressure is on the writer thread, not on workers.

---

## a. Purpose and boundary

The trace is the runtime's memory of what happened, one record per morsel per stage. The writer takes records from any thread through a bounded channel, batches them into Arrow, keeps them in memory up to a limit, spills the rest to a file, and can write the whole trace as an Arrow IPC file when a path was given. The report is a pure function of the trace plus the discovered limits; it is what the user reads and what S1 to S4 are measured from.

It owns: the channel and writer thread; in-memory batching; overflow; the IPC file; the report computation and its rendering.

It refuses to know: what a morsel is beyond its record; why a knob changed; anything about the host except what `Limits` says.

## b. Vocabulary

**Chunk.** An in-memory Arrow `RecordBatch` of up to 4,096 trace records.

**Overflow file.** `trace-overflow-<run id>.arrow` in the staging directory, Arrow IPC stream, appended chunk by chunk when memory chunks exceed the limit.

**Run id.** A 16-hex-character random id assigned by the runtime facade at start and carried by every record's file name and report.

**Report.** `RunReport`, the struct in d.1, plus its `Display` and JSON renderings.

## c. Invariants

**TR-I1. No record is dropped.** Every `record` call results in the record appearing in the final trace; if the channel is full, the caller blocks for the push (bounded by the writer's drain rate, which is faster than any worker's completion rate by construction, since a drain is a memcpy into a builder). Upholds G-I4.

**TR-I2. Order is per stage and per sequence.** The final trace, read back, sorted by `(stage, seq)`, has no gaps within a stage for sequences that entered that stage, and each `(stage, seq)` appears exactly once.

**TR-I3. The report is a pure function.** `RunReport::from(trace, limits, exit)` is deterministic; two calls on the same inputs produce equal structs. Upholds S9.

**TR-I4. Memory is bounded.** In-memory chunks never exceed `trace.memory_limit` (64 MiB default); beyond it, older chunks are written to the overflow file and freed.

**TR-I5. Flush on every exit.** Completion, termination and cancellation all end with `flush`, which writes remaining chunks, closes the IPC file if any, and makes the trace readable; a crash between records loses at most one channel's worth.

**TR-I6. The schema hash is checked at open.** The writer asserts `TraceRecord::SCHEMA_HASH` against the schema it builds; mismatch is a `Config` error at start (a build inconsistency), never a silent divergence.

## d. Interfaces

### d.1 Exposed

```rust
pub struct TraceConfig {
    pub path: Option<std::path::PathBuf>,       // final IPC file; None = in-memory + overflow only
    pub staging_dir: std::path::PathBuf,        // for overflow
    pub channel_capacity: usize,                // trace.channel_capacity
    pub memory_limit: u64,                      // trace.memory_limit
    pub run_id: [u8; 8],
}

pub struct TraceWriter { /* private */ }
impl TraceWriter {
    pub fn start(cfg: TraceConfig) -> Result<std::sync::Arc<TraceWriter>>;   // spawns the writer thread
    /// Read-only view over everything recorded so far (in-memory chunks + overflow); used by the controller.
    pub fn snapshot(&self) -> TraceView;
    /// TR-I5. Idempotent.
    pub fn finish(&self) -> Result<TraceView>;
}
impl TraceSink for TraceWriter { /* contracts d.13 */ }

pub struct TraceView { /* Arc'd chunks + overflow reader */ }
impl TraceView {
    pub fn len(&self) -> u64;
    pub fn batches(&self) -> impl Iterator<Item = arrow::record_batch::RecordBatch>;
    /// Last `n` records for stage `stage`, newest last (controller's window).
    pub fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord>;
    pub fn to_ipc_file(&self, path: &std::path::Path) -> Result<()>;
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct StageReport {
    pub stage: StageId,
    pub morsels: u64, pub rows_in: u64, pub rows_out: u64, pub bytes_in: u64, pub bytes_out: u64,
    pub wall_s: f64, pub kernel_busy_s: f64,
    pub rows_per_s: f64, pub bytes_per_s: f64,
    pub amplification_p50: f64, pub amplification_p95: f64,
    pub placement_miss_wait_s: f64, pub errors: u64, pub skipped: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub exit: ExitReason,                     // Completed | Terminated { diagnostic } | Cancelled
    pub wall_s: f64,
    pub limits: LimitsSummary,                // ceiling, kill, cpu quota, source, devices, paths taken
    pub peak_anon_bytes: u64,
    pub peak_fraction_of_ceiling: f64,        // S1
    pub worker_busy_fraction: f64,            // S4
    pub cpu_throttled_fraction: f64,
    pub source_bytes_per_s: f64,
    pub staging_bytes_written: u64,
    pub staging_engaged: bool,
    pub gil_serialised: bool,
    pub sizer_used: String, pub sizer_fallback_at: Option<Seq>,
    pub bottleneck_timeline: Vec<(f64, String)>,   // (seconds, classification) from the controller's records
    pub stages: Vec<StageReport>,
    pub notes: Vec<String>,                   // discovery notes + runtime notes
}

impl RunReport {
    pub fn compute(trace: &TraceView, limits: &Limits, meta: &RunMeta) -> RunReport;   // TR-I3
    pub fn to_json(&self) -> String;
}
impl core::fmt::Display for RunReport { /* section e.3 layout */ }
```

`RunMeta` carries what is not in the trace: exit reason, start and end instants, discovery notes, `IoPaths`, GIL state, sizer name and fallback point, the controller's bottleneck timeline. The facade assembles it.

### d.2 Consumed

`amoru_kernel::{TraceRecord, TraceSink, Outcome, Limits, StageId, Seq, AmoruError}`; `arrow` (builders, IPC stream writer and reader); `crossbeam_channel::bounded`; `serde_json`.

## e. Data model, formats and state machines

### e.1 Writer state machine

`Running` → (`finish`) → `Flushing` → `Finished`. `record` in `Finished` is a no-op counted in `late_records` (a diagnostic; should be zero). `finish` is idempotent.

### e.2 Files

Overflow: Arrow IPC stream format, one message per chunk, in `staging_dir/trace-overflow-<run_id>.arrow`; deleted on `finish` after the final file is written, or kept if no `path` was given and `TraceView` is still referenced (the view reads it lazily). Final: Arrow IPC **file** format (random access) at `path`, schema per contracts e.5, one batch per chunk, footer written on `finish`.

### e.3 Report rendering

`Display` produces a fixed layout of at most 40 lines: header (run id, exit, wall), limits line, memory line (peak, fraction, ceiling), CPU line (busy fraction, throttled fraction, workers), IO line (source bytes per second, path taken), staging line (engaged, bytes), one line per stage (morsels, rows/s, amplification p50/p95, misses), sizer line, and notes. Numbers use binary units for bytes and three significant figures.

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
- `state_bytes_max` per stage = max(`state_bytes`); `state_growth` per stage = last `state_bytes` − first non-zero `state_bytes` (reported so a stateful kernel whose state grows with morsels seen is visible in the report; RC f.6 StateGrowth).

**f.3 `tail(stage, n)`.** Walk in-memory chunks from newest; filter by stage; stop at n. Does not read the overflow file (the controller's window is recent by definition; a `tail` larger than what is in memory returns fewer records and says so through `len()`).

## g. Concurrency within the component

One writer thread. `record` is callable from any thread; the channel is the only shared structure on that path. The chunk vector's mutex is held by the writer during append and by `snapshot`/`tail` readers briefly; lock order position 5 in preamble 4.2. `finish` joins the writer thread.

## h. Behaviour

**Normal path.** `start`; workers `record` per morsel; controller calls `tail` every tick; at completion `finish` writes the file, returns the view; the facade computes the report.

**Edge cases.** Zero morsels (empty source): trace is empty, report has zero stages' rows and `wall_s` from meta; no division by zero (rates are 0.0). A stage with only a probe record: amplification from the probe alone; `morsels = 1`. Records arriving after `finish` (a worker finishing late during cancellation): counted in `late_records`, reported.

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

**TR-T8 tail.** `tail(stage, n)` returns the newest n for that stage in order. f.3.

**TR-T9 disk_full.** Overflow directory on a 1 MiB tmpfs; records still counted; `overflow_failed` set. h failures.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/writer.rs` (f.1, e.1), `src/view.rs` (`TraceView`, overflow reader), `src/report.rs` (f.2, e.3, JSON), `src/render.rs` (`Display`). No `unsafe`.

Use Arrow builders per column, not row-to-batch conversion of `Vec<TraceRecord>`; the `feat_column_bytes`, `q_bytes_before`, `q_bytes_after` lists use `ListBuilder<UInt64Builder>`.

Anti-patterns: no unbounded `Vec<TraceRecord>`; no `record` path that allocates per call beyond the channel slot; no report field computed anywhere but `report.rs`.

## m. Open items

None. (`trace.memory_limit` is in the preamble's table.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S9, G-I4 | TR-I1, TR-I2, TR-I3 | TR-T1, TR-T2, TR-T3, TR-T7 |
| S1, S4 (measurement) | f.2 | TR-T7 |
| G-I8 (diagnostic durability) | TR-I5 | TR-T5 |
