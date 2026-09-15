# Amoru SDD 08: Sinks (`amoru-sinks`)

**Document type:** software design document, component 8 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` section 5.4; criteria S10 (slow sink), S13; escalation E3 (ordering)
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.8 (`Sink`, `SinkSummary`), d.4, e.4 (`AMB1`)
**Component location:** `crates/amoru-sinks`, Rust
**Consumes:** contracts (1), arena (2), reactor (6). **Consumed by:** scheduler (10, drives writes), python surface (12, constructs)

**Decisions worth your eye:** (1) the reorder buffer for ordered sinks is a wrapper around any sink, bounded in bytes, and stalls admission through the scheduler rather than growing; (2) Parquet output uses the writer's own encoding buffers and that encode is the symmetric exception to G-I2, counted as `encode_bytes`; (3) device payloads are demoted to pinned host by the reactor before a sink sees them, so sinks are host-only.

---

## a. Purpose and boundary

A sink absorbs morsels and produces durable output. Three implementations: `ParquetSink` (object store or local; row-group and file size targets; rolling files), `TensorSink` (safetensors or `AMB1`, one file per run or per split), `ArrowIpcSink` (Arrow IPC file with page-aligned buffers, the format staging segments also use). A fourth, `ReorderBuffer<S>`, wraps any sink and delivers morsels in sequence order within a byte bound. Writes run on the reactor.

It owns: encoding to the output format; file rolling; multipart uploads through the reactor; the reorder buffer; `SinkSummary`.

It refuses to know: what stage produced a morsel; when to stop (the scheduler calls `finish`); ordering unless wrapped.

## b. Vocabulary

**Roll.** Closing the current output file at the target size and opening the next; files are numbered `part-00000.parquet` onward under the sink's prefix.

**Encode copy.** The CPU work of encoding Arrow into Parquet pages (compression, encoding); the symmetric counterpart of the source's decode copy; counted in `encode_bytes`.

**Reorder window.** Morsels held by `ReorderBuffer` waiting for a lower sequence number to arrive.

**Commit.** The moment a file is complete and visible in the store (multipart completed, or local file closed and renamed from `.tmp`).

## c. Invariants

**SI-I1. `write` takes ownership and completes once.** After `write(payload)` resolves, the payload's arena bytes have been released (or, for `ArrowIpcSink` local writes, handed to the reactor and released on completion); the `Completion` resolves exactly once. (Preamble 1.3 row 8.)

**SI-I2. `finish` is exactly once, after the last write.** Calling `write` after `finish`, or `finish` twice, is a `Sink` error; the scheduler guarantees the ordering and the sink checks it.

**SI-I3. Committed files are complete.** A file that exists at its final name in the store is complete and valid; incomplete files exist only under `.tmp` names or as un-completed multipart uploads, which `finish` (or abort) removes. No reader ever sees a partial final file.

**SI-I4. Encode is the only CPU copy.** Per morsel, payload bytes are touched by the CPU only inside the encoder; `ArrowIpcSink` performs no copy at all (buffers are written from the arena by direct IO). Upholds G-I2 (symmetric exception).

**SI-I5. Ordered delivery within a bound.** `ReorderBuffer` delivers to the inner sink in strictly increasing `seq`; its held bytes never exceed `ordering.buffer_bytes`; when the bound would be exceeded by holding a morsel, it reports `is_stalled() == true` and the scheduler stops admitting source work until the missing sequence arrives.

**SI-I6. Sinks are host-only.** A payload arriving in `Tier::Device` is demoted by the caller (placement, on the scheduler's request) before `write`; a sink receiving a device payload returns `Sink("device payload")` rather than copying it.

**SI-I7. Summary is exact.** `SinkSummary.rows` and `bytes` equal the sums over written payloads; `files` lists every committed file in order.

## d. Interfaces

### d.1 Exposed

```rust
pub struct ParquetSinkConfig {
    pub url: String,                              // prefix; files are created under it
    pub row_group_bytes: u64,                     // default 128 MiB
    pub file_bytes: u64,                          // default 1 GiB
    pub compression: Compression,                 // default Zstd(3)
    pub writer_props: Option<parquet::file::properties::WriterProperties>,   // escape hatch; overrides the above
}
pub struct ParquetSink { /* private */ }
impl ParquetSink { pub fn new(cfg: ParquetSinkConfig, reactor: Arc<Reactor>, alloc: Arc<dyn Allocator>) -> Result<ParquetSink>; }
impl Sink for ParquetSink { /* accepts: Table on Host */ }

pub enum TensorFormat { SafeTensors, Amb1 }
pub struct TensorSinkConfig { pub path: std::path::PathBuf, pub format: TensorFormat, pub one_file_per_morsel: bool, pub name: String }
pub struct TensorSink { /* private */ }
impl Sink for TensorSink { /* accepts: Tensor on Host */ }

pub struct ArrowIpcSinkConfig { pub path: std::path::PathBuf, pub file_bytes: u64 }
pub struct ArrowIpcSink { /* private */ }
impl Sink for ArrowIpcSink { /* accepts: Table on Host; page-aligned buffers; direct IO */ }

pub struct ReorderBuffer<S: Sink> { /* private */ }
impl<S: Sink> ReorderBuffer<S> {
    pub fn new(inner: S, buffer_bytes: u64) -> Self;
    pub fn is_stalled(&self) -> bool;       // read by the scheduler each admission cycle
    pub fn next_expected(&self) -> Seq;
}
impl<S: Sink> Sink for ReorderBuffer<S> { fn requires_order(&self) -> bool { true } /* ... */ }

#[derive(Clone, Debug, Default)]
pub struct SinkStats { pub writes: u64, pub encode_bytes: u64, pub files_committed: u64, pub rolls: u64, pub multipart_parts: u64, pub reorder_held_max: u64, pub stalls: u64 }
```

`write` for `ReorderBuffer` accepts a `Morsel`-like `(seq, payload)`; since the contract's `write` takes only a `Payload`, the reorder buffer is constructed with a sequence extractor: the scheduler calls `write_seq(seq, payload)` on it (an inherent method), and the trait method `write` is a `Sink` error "use write_seq". The scheduler knows which sink kind it holds.

### d.2 Consumed

`amoru_kernel::{Sink, SinkSummary, Payload, PayloadSpec, SourceSchema, Buffer, Allocator, Seq, AmoruError, amb1}`; `amoru_reactor::Reactor` (`write_object` multipart, `write_file`); `parquet` (`ArrowWriter` over an in-memory `Vec` page buffer per row group, then `write_object`; or `AsyncArrowWriter` when the parquet crate's async writer supports `object_store` multipart directly, which the agent verifies); `safetensors` (serialize header); `arrow` IPC writer for `ArrowIpcSink` (custom, page-aligned; see e.3).

## e. Data model, formats and state machines

### e.1 Sink state machine

`Created` → (`open`) → `Open` → (`write`*) → `Open` → (`finish`) → `Finished`. Any other transition is a `Sink` error. A failure during `write` moves to `Failed`; `finish` in `Failed` aborts open uploads, deletes `.tmp` files, and returns the original error.

### e.2 Parquet file layout

Row groups of `row_group_bytes` (measured as encoded bytes; a morsel larger than the target becomes one row group); files rolled at `file_bytes`; the footer written at roll; files named `part-{index:05}.parquet`; `_SUCCESS` marker written by `finish` after all files are committed (an empty object), which is the convention downstream readers use.

### e.3 Arrow IPC (page-aligned) layout

Standard Arrow IPC file format (magic `ARROW1`, schema message, record batch messages, footer) with one deviation the format permits: every buffer within a record batch message is padded to the page size rather than 8 bytes, so each buffer starts at a page boundary in the file. Standard readers accept this (padding is allowed). It is what makes reload by direct IO or GDS possible for staging (component 9 uses this writer through this crate).

### e.4 Tensor files

`Amb1`: contracts e.4, one tensor per file, file per morsel or one file for the run (concatenated along dimension 0 with the header written at `finish` once the total is known; until then the file is `.tmp`). `SafeTensors`: header JSON per the safetensors spec, data section unaligned by the spec (noted in the report as not DMA-loadable); one file per run.

## f. Algorithms and policies

**f.1 `ParquetSink::write`.** Append the batch to the current `ArrowWriter` (host-side encode into a `Vec<u8>` owned by the sink: the encode copy); when the writer's buffered size crosses `row_group_bytes`, flush the row group; when the file's size crosses `file_bytes`, close the writer (footer), hand the file bytes to `reactor.write_object` (multipart for > 64 MiB), open the next writer. The payload's arena buffers are released as soon as the encoder has consumed them (before the completion resolves; SI-I1's "released on completion" covers `ArrowIpcSink` only).

**f.2 `ArrowIpcSink::write`.** Serialise the schema once; for each batch, write the IPC message header then each buffer as a direct-IO `write_file` from the arena buffer at a page-aligned file offset; no copy; release on completion. File rolled at `file_bytes` with a footer.

**f.3 `TensorSink::write`.** `Amb1` per-morsel: header + `write_file` of the tensor bytes from the arena at the page-aligned data offset; run-mode: append bytes at the running offset, header at `finish`. `SafeTensors`: buffer in memory up to 64 MiB then stream; the whole file is written at `finish` (the spec's header needs all offsets).

**f.4 `ReorderBuffer::write_seq`.** If `seq == next_expected`: forward to inner, advance, then drain any held consecutive sequences; else hold (`held_bytes += payload.bytes`); if `held_bytes > buffer_bytes`, set `stalled = true` (the morsel is still held; the bound is soft by one morsel, hard thereafter because the scheduler stops admitting). `stalled` clears when `held_bytes ≤ buffer_bytes / 2`.

**f.5 Commit.** Object store: multipart complete, or single `put`; local: write to `<name>.tmp`, `fsync`, rename. `finish` writes `_SUCCESS` for Parquet after every file is committed.

**f.6 Backpressure.** Sinks do not throttle; they complete when the reactor completes. A slow store manifests as reactor completions taking longer, which the placement engine sees as a growing last queue and the controller as a sink-bound classification (S10 is the placement engine's and controller's to satisfy; the sink's obligation is SI-I1 and SI-I3).

## g. Concurrency within the component

`write` may be called concurrently for different payloads (the scheduler's sink driver issues up to `sink.concurrency`); the Parquet writer is single-threaded by nature, so `ParquetSink` serialises appends with a mutex around the writer and lets encodes of different row groups overlap only at roll boundaries. `ArrowIpcSink` and per-morsel `TensorSink` are concurrent up to the reactor's file depth. `ReorderBuffer` holds a mutex across `write_seq`'s bookkeeping only, not across the inner `write`.

## h. Behaviour

**Normal path.** `open(schema)`; writes arrive in completion order; Parquet rolls files; `finish` commits the last file and the marker; `SinkSummary` returned.

**Edge cases.** Zero writes then `finish`: Parquet writes one empty file with the schema and the marker; tensor sinks write a header-only file. A morsel larger than `file_bytes`: one file containing one row group. Schema drift between morsels (a kernel returned a different schema): `Sink` error naming the differing field; the scheduler terminates. Ordered sink with a permanently missing sequence (an error morsel with `skip` policy): the scheduler informs the reorder buffer with `skip(seq)` (inherent method) so it advances.

**Failures.** Store write failure after reactor retries: `Failed`; `finish` aborts multipart uploads and returns the error; committed files remain (their names listed in the error's context so the caller can report partial output). Local disk full: same. Rename failure at commit: `Io`; the `.tmp` remains for inspection.

## i. Configuration

`ordering.required`, `ordering.buffer_bytes`, `sink.concurrency`, `sink.row_group_bytes`, `sink.file_bytes` (preamble section 5).

## j. Observability

`SinkStats`; `tracing`: `sink.roll` (info: file, bytes), `sink.commit` (info), `sink.stall` (warn, once per stall episode), `sink.encode` (trace: bytes, µs).

## k. Tests

**SI-T1 ownership_once.** After `write` resolves, `AllocStats.host_in_use` has dropped by the payload's bytes; the completion resolved once. SI-I1.

**SI-T2 finish_once.** `write` after `finish` and double `finish` error. SI-I2.

**SI-T3 no_partial_final.** Inject a failure mid-multipart; no final-named object exists; `.tmp` local files are removed by `finish`. SI-I3.

**SI-T4 encode_only_copy.** `payload_copies_total` unchanged across a run of `ParquetSink`; `encode_bytes` equals input bytes. `ArrowIpcSink`: `encode_bytes == 0`. SI-I4.

**SI-T5 reorder.** Random arrival order of 1,000 sequences; inner sink sees strictly increasing; `reorder_held_max ≤ buffer_bytes + one morsel`; stall flag raised and cleared correctly when a low sequence is delayed. SI-I5.

**SI-T6 device_rejected.** (cuda, skippable) Device payload → `Sink("device payload")`. SI-I6.

**SI-T7 summary_exact.** Rows, bytes and file list match the generator's ledger. SI-I7.

**SI-T8 parquet_roundtrip.** Written files read back by `ParquetSource` equal the input batches; row-group and file sizes within 10% of targets. e.2.

**SI-T9 ipc_page_aligned.** Every buffer offset in the written IPC file is a multiple of `page_bytes`; `arrow` reads it back equal. e.3.

**SI-T10 amb1_and_safetensors.** Both formats round-trip through `TensorSource`; the report note for unaligned safetensors is emitted. e.4.

**SI-T11 slow_store.** Fake reactor with 100 ms write latency; sink completions are slow but correct; no sink-side buffering beyond one row group. f.6.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/parquet_sink.rs` (f.1, e.2, f.5), `src/ipc_sink.rs` (f.2, e.3; the page-aligned IPC writer is a small custom encoder over `arrow::ipc` message building, not the crate's `FileWriter`, which pads to 8), `src/tensor_sink.rs` (f.3, e.4), `src/reorder.rs` (f.4), `src/commit.rs` (f.5), `src/stats.rs`. No `unsafe`.

Verify before starting: whether the pinned `parquet` version's `AsyncArrowWriter` can target `object_store` multipart directly (if so, use it and drop the `Vec<u8>` staging; `encode_bytes` semantics unchanged).

Anti-patterns: no buffering of more than one row group in `ParquetSink`; no `write` that copies an arena buffer into a `Vec` for `ArrowIpcSink`; no commit of a file that has not been fully written.

## m. Open items

None. (`sink.concurrency`, `sink.row_group_bytes` and `sink.file_bytes` are in the preamble's table.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S10 | f.6, SI-I1 | SI-T11 (with PL tests) |
| G-I2 | SI-I4 | SI-T4 |
| E3 / Q2 (ordering) | SI-I5 | SI-T5 |
| S13 (aligned formats) | e.3, e.4 | SI-T9, SI-T10 |
| G-I8 (clean failure) | SI-I3 | SI-T3 |
