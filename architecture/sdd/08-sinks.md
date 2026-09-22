# Amoru SDD 08: Sinks (`amoru-sinks`)

**Document type:** software design document, component 8 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/amoru-runtime-design.md` section 5.4; criteria S10 (slow sink), S13; escalation E3 (ordering)
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.8 (`Sink`, `SinkSummary`), d.3 (`Buffer`, `BufferView`, `Allocator`), d.9 (`Reactor`, `Completion`), d.4, e.4 (`AMB1`), e.7 (page-aligned IPC record encoding, `amoru_kernel::ipc`)
**Component location:** `crates/amoru-sinks`, Rust
**Consumes:** contracts (1); the arena (2) as `Arc<dyn Allocator>` and the reactor (6) as `Arc<dyn Reactor>`, neither as a crate. **Consumed by:** scheduler (10, drives writes through `SinkHandle`), python surface (12, constructs and wraps)

**Decisions worth your eye:** (1) the reorder buffer for ordered sinks is a wrapper around any sink, bounded in bytes, and stalls admission through the scheduler rather than growing; the scheduler and the facade see one `SinkHandle` whether or not a sink is wrapped; (2) Parquet output is encoded by the parquet writer straight into arena memory and that encode is the symmetric exception to G-I2, counted as `encode_bytes`; the reactor then writes it from a `BufferView` with no second copy; (3) device payloads are demoted to the host tier by the placement engine before a sink sees them, so sinks are host-only; (4) `ArrowIpcSink` and the staging segments share one encoder, `amoru_kernel::ipc` (contracts e.7), so the two formats cannot drift.

---

## a. Purpose and boundary

A sink absorbs morsels and produces durable output. Three implementations: `ParquetSink` (object store or local; row-group and file size targets; rolling files), `TensorSink` (safetensors or `AMB1`, one file per run or per split), `ArrowIpcSink` (Arrow IPC file with page-aligned buffers, the record encoding staging segments also use). A fourth, `ReorderBuffer<S>`, wraps any sink and delivers morsels in sequence order within a byte bound; `SinkHandle` is the one type the scheduler drives, plain or ordered. Writes run on the reactor from views over the payload's own buffers.

It owns: encoding to the output format; file rolling; multipart uploads through the reactor; the reorder buffer and `SinkHandle`; `SinkSummary`.

It refuses to know: what stage produced a morsel; when to stop (the scheduler calls `finish`); ordering unless wrapped.

## b. Vocabulary

**Roll.** Closing the current output file at the target size and opening the next; files are numbered `part-00000.parquet` onward under the sink's prefix.

**Encode copy.** The CPU work of encoding Arrow into Parquet pages (compression, encoding) into arena memory; the symmetric counterpart of the source's decode copy; counted in `encode_bytes`.

**Handle.** The `SinkHandle` the facade builds from a sink and the ordering decision; what the scheduler owns for the run.

**Reorder window.** Morsels held by `ReorderBuffer` waiting for a lower sequence number to arrive.

**Commit.** The moment a file is complete and visible in the store (multipart completed, or local file closed and renamed from `.tmp`).

## c. Invariants

**SI-I1. `write` takes ownership and completes once.** After `write(seq, payload)` resolves, the payload's arena bytes have been released: for `ParquetSink` as soon as the encoder has consumed them, for `ArrowIpcSink` and `TensorSink` when the reactor's completions over the views resolve (the sink drops the payload after the last one, which releases the bytes the views kept alive); the future resolves exactly once. On a failed write the bytes are released the same way; nothing is retried inside the sink. (Preamble 1.3 row 8.)

**SI-I2. `finish` is exactly once, after the last write.** Calling `write` after `finish`, or `finish` twice, is a `Sink` error; the scheduler guarantees the ordering and the sink checks it.

**SI-I3. Committed files are complete.** A file that exists at its final name in the store is complete and valid; incomplete files exist only under `.tmp` names or as un-completed multipart uploads, which `finish` (or abort) removes. No reader ever sees a partial final file.

**SI-I4. Encode is the only CPU copy.** Per morsel, payload bytes are touched by the CPU only inside the encoder, whose output lands in arena memory and is written from there by the reactor; `ArrowIpcSink` and `TensorSink` perform no copy at all (buffers are written from the payload's own arena buffers through `BufferView` by direct IO). Upholds G-I2 (symmetric exception).

**SI-I5. Ordered delivery within a bound.** `ReorderBuffer` delivers to the inner sink in strictly increasing `seq`; its held bytes never exceed `ordering.buffer_bytes`; when the bound would be exceeded by holding a morsel, it reports `is_stalled() == true` and the scheduler stops admitting source work until the missing sequence arrives.

**SI-I6. Sinks are host-only.** A payload arriving in `Tier::Device` is demoted to the host tier by the caller (placement, through the sink queue's consumer spec `Host`) before `write`; a sink receiving a device payload returns `Sink("device payload")` rather than copying it. The host tier is whichever the run has (contracts e.1); a sink accepts either.

**SI-I7. Summary is exact.** `SinkSummary.rows` and `bytes` equal the sums over written payloads; `files` lists every committed file in order.

**SI-I8. Every committed file names its sequence range, and `committed_seq` never overstates.** A committed Parquet, IPC or per-morsel AMB1 file records the lowest and highest `seq` it holds (e.2, e.3, e.4); `committed_seq()` returns the highest `seq` such that every lower sequence number is in a committed file, computed from those ranges, never from what has merely been written. On `resume`, every file whose range lies above the given `committed_seq` is removed before the first new write. Upholds S17. Rationale: resume replays everything above the watermark; a sink that kept such a file would duplicate rows.

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
impl ParquetSink { pub fn new(cfg: ParquetSinkConfig, reactor: Arc<dyn Reactor>, alloc: Arc<dyn Allocator>) -> Result<ParquetSink>; }
impl Sink for ParquetSink { /* accepts: Table on Host */ }

pub enum TensorFormat { SafeTensors, Amb1 }
pub struct TensorSinkConfig { pub path: std::path::PathBuf, pub format: TensorFormat, pub one_file_per_morsel: bool, pub name: String }
pub struct TensorSink { /* private */ }
impl TensorSink { pub fn new(cfg: TensorSinkConfig, reactor: Arc<dyn Reactor>, alloc: Arc<dyn Allocator>) -> Result<TensorSink>; }
impl Sink for TensorSink { /* accepts: Tensor on Host */ }

pub struct ArrowIpcSinkConfig { pub path: std::path::PathBuf, pub file_bytes: u64 }
pub struct ArrowIpcSink { /* private */ }
impl ArrowIpcSink { pub fn new(cfg: ArrowIpcSinkConfig, reactor: Arc<dyn Reactor>, alloc: Arc<dyn Allocator>) -> Result<ArrowIpcSink>; }
impl Sink for ArrowIpcSink { /* accepts: Table on Host; page-aligned buffers (contracts e.7); direct IO */ }

/// Delivers to `inner` in strictly increasing `seq` within `buffer_bytes`. `S` is
/// `?Sized` so the run's handle is `ReorderBuffer<dyn Sink>` over a boxed sink
/// (`Box<dyn Sink>` cannot itself implement the foreign `Sink` trait here).
pub struct ReorderBuffer<S: Sink + ?Sized> { /* private: inner: Box<S>, held, next_expected, stalled */ }
impl<S: Sink + ?Sized> ReorderBuffer<S> {
    pub fn new(inner: Box<S>, buffer_bytes: u64) -> Self;
    pub fn is_stalled(&self) -> bool;       // read by the scheduler each admission cycle
    pub fn next_expected(&self) -> Seq;
    pub fn held_bytes(&self) -> u64;
}
/// The contract's `Sink`, every method, including `skip` (f.4); `requires_order` is true.
impl<S: Sink + ?Sized> Sink for ReorderBuffer<S> { /* f.4, d.1 text below */ }

/// What the scheduler drives and the facade builds (10 d.1 and 12 d.1 name this
/// type). One shape whether ordering is on or off.
pub enum SinkHandle {
    Plain(Box<dyn Sink>),
    Ordered(ReorderBuffer<dyn Sink>),
}
impl SinkHandle {
    /// `Ordered` when `ordered` (the user's `ordered=True`) or `sink.requires_order()`; `Plain` otherwise.
    pub fn wrap(sink: Box<dyn Sink>, ordered: bool, buffer_bytes: u64) -> SinkHandle;
    /// `false` for `Plain`.
    pub fn is_stalled(&self) -> bool;
    /// `None` for `Plain`.
    pub fn next_expected(&self) -> Option<Seq>;
    pub fn is_ordered(&self) -> bool;
}
/// Delegates every method to the variant, so the scheduler calls `open`, `write`,
/// `skip`, `committed_seq`, `checkpoint`, `resume` and `finish` on the handle alone.
impl Sink for SinkHandle { /* delegation */ }

#[derive(Clone, Debug, Default)]
pub struct SinkStats { pub writes: u64, pub encode_bytes: u64, pub files_committed: u64, pub rolls: u64, pub multipart_parts: u64, pub reorder_held_max: u64, pub stalls: u64, pub resumed_files_removed: u64 }
```

`write(seq, payload)` carries the sequence number in the contract, so `ReorderBuffer` needs no side channel: it reorders on `seq` and forwards `write(seq, payload)` to the inner sink in order. `ParquetSink`, `ArrowIpcSink` and per-morsel `TensorSink` implement the four resume methods of the contract (`committed_seq`, `skip`, `checkpoint`, `resume`; e.5, f.7, f.8); run-mode and `SafeTensors` `TensorSink` leave the defaults, so `checkpoint()` returns `None` at startup, which is how the scheduler detects a non-resumable sink (contracts d.8, SC f.11), and the run says so. `ReorderBuffer` implements the contract's `skip` by advancing its own `next_expected` past the sequence, draining any held consecutive sequences, and forwarding `skip` to the inner sink; it delegates `committed_seq`, `checkpoint` and `resume` to the inner sink, adding nothing to the checkpoint: on resume its `next_expected` is `committed_seq + 1` (or 0 when `None`), and everything above the watermark is replayed to it in whatever order it arrives. `ReorderBuffer<dyn Sink>` keeps the box inside the buffer, which is what Rust's orphan rule allows, and it is the spelling this crate exports.

Every constructor takes `Arc<dyn Reactor>` and `Arc<dyn Allocator>`: the reactor for `write_file` and `write_object`, the allocator for the encoder's output buffers (Parquet), the framing and header buffers (IPC, AMB1) and for `BufferView::of_arrow(buf, alloc)` over the payload's own buffers.

### d.2 Consumed

`amoru_kernel::{Sink, SinkSummary, Payload, PayloadSpec, SourceSchema, Buffer, BufferView, Allocator, Reactor, Completion, Seq, RunId, AmoruError, amb1, ipc}` (`ipc::encode_framing` and `ipc::decode`, contracts e.7; `write_file` and `write_object` of contracts d.9; no `amoru_reactor` dependency); `serde_json` (the sink checkpoint, e.5); `base64`; `parquet` (`ArrowWriter` over a `std::io::Write` implementation that appends into arena buffers, f.1; or `AsyncArrowWriter` when the parquet crate's async writer supports `object_store` multipart directly, which the agent verifies but which must still not copy through a `Vec`); `safetensors` (serialize header); `arrow` (`ipc` feature for the file footer flatbuffer only; the record encoding is the contracts crate's).

## e. Data model, formats and state machines

### e.1 Sink state machine

`Created` → (`open` | `resume`) → `Open` → (`write`*) → `Open` → (`finish`) → `Finished`. Any other transition is a `Sink` error. A failure during `write` moves to `Failed`; `finish` in `Failed` aborts open uploads, deletes `.tmp` files, and returns the original error.

### e.2 Parquet file layout

Row groups of `row_group_bytes` (measured as encoded bytes; a morsel larger than the target becomes one row group); files rolled at `file_bytes`; the footer written at roll; files named `part-{index:05}.parquet`; `_SUCCESS` marker written by `finish` after all files are committed (an empty object), which is the convention downstream readers use. Each file's footer carries key-value metadata `amoru.run_id` (hex), `amoru.seq_min` and `amoru.seq_max` (decimal), the range of sequence numbers whose rows the file holds (SI-I8); for an unordered inner sink the range may have gaps, which is why the checkpoint (e.5) lists the sequence numbers explicitly rather than the range.

### e.3 Arrow IPC (page-aligned) layout

Standard Arrow IPC file format (magic `ARROW1`, schema message, record batch messages, footer) whose record batch messages are the page-aligned records of contracts e.7 (`amoru_kernel::ipc::encode_framing`): every buffer within a record batch starts at a page boundary in the file. Standard readers accept this (padding is allowed). It is what makes reload by direct IO or GDS possible; the staging segments (09 e.3) use the same records, so a segment body and an IPC file record batch are byte-identical for the same batch, and the encoder lives in the contracts crate so neither this crate nor the placement engine can drift from the other. The differences from a segment: the file carries one Schema message, at the start, not one per record (f.2), and a footer. The schema message's custom metadata carries `amoru.run_id`, `amoru.seq_min` and `amoru.seq_max` as in e.2 (`seq_min`/`seq_max` are rewritten in the footer's custom metadata at roll, since they are known only then); files are named `part-{index:05}.arrow` and rolled at `file_bytes`.

### e.4 Tensor files

`Amb1`: contracts e.4, one tensor per file, file per morsel or one file for the run (concatenated along dimension 0 with the header written at `finish` once the total is known; until then the file is `.tmp`). Per-morsel files are named `{name}-{seq:012}.amb1`, so the sequence number is the file name and no metadata is needed (SI-I8). `SafeTensors`: header JSON per the safetensors spec, data section unaligned by the spec (noted in the report as not DMA-loadable); one file per run.

### e.5 Sink checkpoint

The bytes `Sink::checkpoint` returns and `Sink::resume` receives, JSON: `{ "version": 1, "kind": "parquet" | "ipc" | "amb1_per_morsel", "next_index": u32, "committed": [ { "name": string, "seq_min": u64, "seq_max": u64, "rows": u64, "bytes": u64 } ] }`. `committed` lists committed files in index order; it is what `SinkSummary.files` is rebuilt from on resume (SI-I7 holds across a resume). `next_index` is the index the next rolled file takes, so a resumed run never reuses a name. The checkpoint reflects only committed files; the file being written is not in it, by construction, and is removed by `resume` (f.8).

## f. Algorithms and policies

**f.1 `ParquetSink::write`.** Append the batch to the current `ArrowWriter`, whose output is a `std::io::Write` implementation over one arena buffer per open file (`alloc(file_bytes + 1 MiB for the footer, host tier)` at `open` and at every roll; the arena's large-allocation path serves it); the encode into that buffer is the encode copy. When the writer's buffered size crosses `row_group_bytes`, flush the row group; when the file's size crosses `file_bytes` (the writer's `bytes_written` plus the pending row group; a morsel that would overflow the buffer rolls first), close the writer (footer), and hand the file to `reactor.write_object(url, buf.view().slice(0, used))` as one view; the reactor does the multipart split for files over 64 MiB (06 f.4), so the sink never issues parts itself. The `Arc<Buffer>` of a rolled file is held by the sink until its completion resolves, then dropped; a failed `write_object` leaves the bytes intact for the error report (RE-I1). One file buffer is live per open file plus at most `sink.concurrency` rolled files in flight, which the facade counts against the host budget it hands the controller (`sink.file_bytes × (1 + sink.concurrency)`, a note in the report). The payload's arena buffers are released as soon as the encoder has consumed them (before the completion resolves; SI-I1). No `Vec<u8>` of payload size exists in this sink; the whole-file buffer is the arena's, not the heap's.

**f.2 `ArrowIpcSink::write`.** For each batch: `(framing, bodies) = amoru_kernel::ipc::encode_framing(batch, page_bytes, base, &*alloc)` (contracts e.7; the framing is one page-rounded arena buffer holding the Schema message then the RecordBatch message, and `bodies` lists each Arrow buffer with its page-aligned offset relative to `base`). For the first record of a file, `base` is the file position after the magic and the sink writes the whole framing at `base` with `write_file(path, base, framing.view())`, measuring `schema_len` (the first message's length prefix plus padding) once. For every later record, `base = pos − schema_len` so that the RecordBatch message lands at the current position `pos`: the sink writes `framing.view().slice(schema_len, framing.len() − schema_len)` at `pos` (a short, buffered write; the framing is small) and the body offsets `encode_framing` computed remain page-aligned and consistent with the message's rewritten buffer offsets. Each body is written with `write_file(path, offset, BufferView::of_arrow(buf, alloc))` from the payload's own arena buffer at a page-aligned offset (direct IO); no copy; the payload is dropped when the last completion resolves. The footer (record batch blocks with offset, metadata length and body length; the schema; custom metadata with the sequence range) is written with `arrow::ipc`'s footer builder at roll, followed by its length and the trailing magic, from one small arena buffer. Files roll at `file_bytes`.

**f.3 `TensorSink::write`.** `Amb1` per-morsel: the header (contracts e.4) in one page-rounded arena buffer written with `write_file(path, 0, hdr.view())`, then `write_file(path, data_offset, BufferView::of_tensor(&tensor))` of the tensor bytes from the arena at the page-aligned data offset; run-mode: the tensor bytes at the running offset, header at `finish` (until then the file is `.tmp`). `SafeTensors`: the tensor bytes are written at the running offset as they arrive (unaligned by the spec, so buffered; the reactor counts it), the header JSON is written at `finish` when every offset is known, into the space reserved for it at the start of the file (the sink reserves the header's maximum size from the schema and pads with spaces, which the spec allows); no in-memory buffering of payload bytes.

**f.4 `ReorderBuffer::write(seq, payload)` and `skip(seq)`.** If `seq == next_expected`: forward to inner, advance, then drain any held consecutive sequences; else hold (`held_bytes += payload.bytes`); if `held_bytes > buffer_bytes`, set `stalled = true` (the morsel is still held; the bound is soft by one morsel, hard thereafter because the scheduler stops admitting). `stalled` clears when `held_bytes ≤ buffer_bytes / 2`. `skip(seq)`: forward `skip` to the inner sink; if `seq == next_expected`, advance and drain as after a write; if `seq > next_expected`, record it in a small sorted set so that when `next_expected` reaches it the buffer advances past it without waiting; if `seq < next_expected`, ignore. A `write` for a sequence number already skipped or below `next_expected` is `Sink("sequence out of range")`.

**f.5 Commit.** Object store: the reactor's `write_object` completion (a single `put`, or a completed multipart upload; the reactor aborts on failure, 06 f.4); local: write to `<name>.tmp` through `write_file`, then `fsync` and rename with `std::fs` on the completing thread (two syscalls of no payload size; the same rule the placement manifest follows, 09 f.12). `finish` writes `_SUCCESS` for Parquet after every file is committed (an empty `write_object`).

**f.9 `SinkHandle::wrap`.** `Ordered(ReorderBuffer::new(sink, buffer_bytes))` when `ordered || sink.requires_order()`, else `Plain(sink)`; `buffer_bytes` is `ordering.buffer_bytes`. The facade calls it once after constructing the sink; the scheduler receives the handle and never sees the inner sink. `is_stalled` on a `Plain` handle is `false`, so the scheduler's admission rule (SC-I3) reads one method for both cases.

**f.6 Backpressure.** Sinks do not throttle; they complete when the reactor completes. A slow store manifests as reactor completions taking longer, which the placement engine sees as a growing last queue and the controller as a sink-bound classification (S10 is the placement engine's and controller's to satisfy; the sink's obligation is SI-I1 and SI-I3).

**f.7 `committed_seq` and `checkpoint`.** Each file sink keeps, under its writer mutex, the list of committed files with their sequence ranges (e.5) and, for the file being written, the set of sequence numbers appended so far. `committed_seq()`: with an ordered inner stream (the sink is wrapped in `ReorderBuffer`, or the scheduler delivers in order) the ranges are contiguous and the answer is the last committed `seq_max`; in general, the sink keeps a small sorted set of sequence numbers above the last contiguous point that are either committed or declared skipped through `skip(seq)`, and advances the watermark through it (the set is bounded by the reorder distance, which `sink.concurrency` bounds, plus the skips, which the error budget bounds). A skipped sequence number therefore never holds the watermark back, and on resume it is replayed like any other uncommitted morsel (it may be skipped again). `checkpoint()` serialises e.5 under the same mutex; it does not force a roll: a file in progress stays out of the checkpoint, and its rows are replayed after a resume. A sink that wants a smaller replay rolls more often (`sink.file_bytes`), which is the user's trade to make, not the sink's.

**f.8 `resume(schema, state, committed_seq)`.** Parse e.5 (refuse an unknown `version` or a `kind` that does not match this sink); list the destination prefix; remove every file whose sequence range (from the footer metadata for Parquet and IPC, from the file name for AMB1) lies entirely above `committed_seq`, and every `.tmp` file and open multipart upload under the prefix (`resumed_files_removed`); refuse with `Resume` if a file's range straddles the watermark (it cannot happen with a correct checkpoint; it means the checkpoint and the store disagree, and the user should look); restore the committed list and `next_index`; open a writer for `part-{next_index}`; move to `Open`. A store that cannot list (a write-only credential) makes `resume` fail with `Resume("cannot list destination")`, which is the honest answer.

## g. Concurrency within the component

`write` may be called concurrently for different payloads (the scheduler's sink driver issues up to `sink.concurrency`); the Parquet writer is single-threaded by nature, so `ParquetSink` serialises appends with a mutex around the writer and lets encodes of different row groups overlap only at roll boundaries. `ArrowIpcSink` and per-morsel `TensorSink` are concurrent up to the reactor's file depth. `ReorderBuffer` holds a mutex across `write`'s and `skip`'s bookkeeping only, not across the inner `write`. Every reactor call a sink makes returns at once (RE-I6); the sink's `write` future awaits the completions, and the scheduler's sink drive is what polls it. `SinkHandle` adds no lock of its own.

## h. Behaviour

**Normal path.** `open(schema)`; writes arrive in completion order; Parquet rolls files; `finish` commits the last file and the marker; `SinkSummary` returned.

**Edge cases.** Zero writes then `finish`: Parquet writes one empty file with the schema and the marker; tensor sinks write a header-only file. A morsel larger than `file_bytes` (a single oversized row, SO-I9): the current file rolls first, then one file containing one row group, whose file buffer is sized at `2 × payload.bytes` for that file only; if the encoder still overflows it (a pathological encoding), the write fails with `Sink("morsel too large to encode")` naming the bytes, never with a truncated file. Schema drift between morsels (a kernel returned a different schema): `Sink` error naming the differing field; the scheduler terminates. Ordered sink with a permanently missing sequence (an error morsel with `skip` policy): the scheduler calls the contract's `skip(seq)` on the handle, which the reorder buffer implements (f.4) so it advances.

**Failures.** Store write failure after reactor retries: `Failed`; `finish` returns the error (the reactor already aborted the multipart upload, 06 f.4); committed files remain (their names listed in the error's context so the caller can report partial output); the rolled file's arena buffer is dropped at that point. Local disk full: same. Rename failure at commit: `Io`; the `.tmp` remains for inspection.

## i. Configuration

`ordering.required`, `ordering.buffer_bytes`, `sink.concurrency`, `sink.row_group_bytes`, `sink.file_bytes` (preamble section 5).

## j. Observability

`SinkStats`; `tracing`: `sink.roll` (info: file, bytes), `sink.commit` (info), `sink.stall` (warn, once per stall episode), `sink.encode` (trace: bytes, µs).

## k. Tests

Tests use the testkit's `FakeReactor` (contracts d.15: in-memory files keyed by path for `write_file`, `ops()` for every operation including `write_object`, `with_latency`, `fail_next(op, n)`) and `FakeAllocator` (`with_limit`, `pinned(bool)`, `page_bytes(n)`, `in_use`, `AllocStats`), and `FakeSink` (`requires_order(bool)`, `written()`, `skipped()`, `committed_seq()`) as the inner sink of a `ReorderBuffer` or `SinkHandle`. Round-trip tests that read files back through `ParquetSource` or `TensorSource` need component 7 and are tagged "(integration, closes in wave 3)"; a written IPC file is instead read back in the test with `arrow`'s `FileReader` and with `amoru_kernel::ipc::decode`, which need no other component.

**SI-T1 ownership_once.** After `write` resolves, `FakeAllocator::in_use(Host)` has dropped by the payload's bytes (for `ParquetSink`, net of the file buffer, which is accounted separately); the future resolved once; for `ArrowIpcSink` the `FakeReactor::ops()` entries for the write all have `t_resolve` before the drop. SI-I1.

**SI-T2 finish_once.** `write` after `finish` and double `finish` error. SI-I2.

**SI-T3 no_partial_final.** `FakeReactor::fail_next(WriteObject, 1)` on a rolled Parquet file: the sink enters `Failed`, `finish` returns the error naming the committed files, no `ops()` entry for the final name resolved successfully after the failure, and the rolled file's arena buffer is released; locally, `.tmp` files are removed by `finish`. SI-I3.

**SI-T4 encode_only_copy.** `payload_copies_total` unchanged across a run of `ParquetSink`; `encode_bytes` equals input bytes; every `write_object` in `ops()` has `src_tier` equal to the host tier (the encoded file came from the arena). `ArrowIpcSink` and `TensorSink`: `encode_bytes == 0`, and every `write_file` in `ops()` has a length equal to one of the payload's buffers or the framing. SI-I4.

**SI-T5 reorder.** Random arrival order of 1,000 sequences into `ReorderBuffer<dyn Sink>` over a `FakeSink`; `FakeSink::written()` is strictly increasing; `reorder_held_max ≤ buffer_bytes + one morsel`; stall flag raised and cleared correctly when a low sequence is delayed; `skip(7)` and `skip(23)` before their turn advance the buffer without waiting and appear in `FakeSink::skipped()`. SI-I5, f.4.

**SI-T6 device_rejected.** (reference host, E1; `cuda`, skipped and listed without a device) Device payload → `Sink("device payload")`. SI-I6.

**SI-T7 summary_exact.** Rows, bytes and file list match the generator's ledger. SI-I7.

**SI-T8 parquet_roundtrip.** (integration, closes in wave 3) Written files read back by `ParquetSource` equal the input batches; row-group and file sizes within 10% of targets. e.2.

**SI-T9 ipc_page_aligned.** Every buffer offset in the written IPC file is a multiple of `page_bytes`; `arrow`'s `FileReader` reads it back equal; `amoru_kernel::ipc::decode` over each record batch block read back into a page-rounded buffer yields arrays whose pointers lie inside that buffer (no copy, CT-T18's check); a two-record file has one Schema message. e.3, f.2.

**SI-T10 amb1_and_safetensors.** (integration, closes in wave 3 for the `TensorSource` read-back; the byte-level checks run alone) Both formats round-trip through `TensorSource`; the AMB1 header parses with the contracts reader; the safetensors header JSON is valid and its offsets match the written bytes. e.4.

**SI-T11 slow_store.** `FakeReactor::with_latency(100 ms)`; sink completions are slow but correct; `FakeReactor::in_flight()` never exceeds the scheduler-side `sink.concurrency` the test drives with; `ParquetSink` holds at most one open file buffer plus the in-flight rolled files. f.6.

**SI-T12 committed_seq_exact.** Ordered and unordered delivery of 1,000 sequences with rolls every 50 and sequences 7, 23 and 24 declared through `skip`; after each commit, `committed_seq()` equals the model's contiguous watermark (skips counted as committed) and never exceeds it; footer metadata ranges match the rows in each file. SI-I8.

**SI-T13 resume_removes_uncommitted.** Write 1,000 sequences, checkpoint at a random point, keep writing, then drop the sink without `finish`; a new sink `resume`s with that checkpoint and watermark; every file above the watermark and every `.tmp` is gone, `next_index` continues, replaying the sequences above the watermark and finishing yields files that read back equal to an uninterrupted run; a straddling file (constructed by hand) is refused. f.8, e.5, SI-I7 across resume.

**SI-T14 unresumable_says_so.** Run-mode `TensorSink` and `SafeTensors`: `checkpoint()` at startup returns `Ok(None)`, `resume` returns `Resume("sink does not support resume")` and `committed_seq() == None`; `ParquetSink`, `ArrowIpcSink` and per-morsel `TensorSink` return `Some` from `checkpoint()` at startup with an empty `committed` list; `ReorderBuffer` over `ParquetSink` delegates all three methods. d.1.

**SI-T15 sink_handle.** `SinkHandle::wrap` over a `FakeSink::requires_order(false)` with `ordered = false` is `Plain` with `is_stalled() == false` and `next_expected() == None`; with `ordered = true`, or over `requires_order(true)` with `ordered = false`, it is `Ordered`; every `Sink` method called on the handle reaches the fake (`written()`, `skipped()`, `finish_calls`, `resume_calls`); a `Plain` handle passes `skip` straight through. f.9, d.1.

**SI-T16 parquet_encodes_into_arena.** Over `FakeAllocator::with_limit(Host, 2 GiB)`: `open` allocates one file buffer (`allocations_total` grows by one), a roll issues exactly one `write_object` whose length equals the file's bytes and whose `src_tier` is the host tier, and the next file allocates the next buffer; with `with_limit(Host, 512 MiB)` and `file_bytes = 1 GiB`, `open` fails with `Alloc` naming the sink in the message (the facade's budget check, not a runtime surprise). f.1.

## l. Implementation notes for the agent

Decisions the PM took on 2026-09-22, on the component 8 agent's report, each because the document could not be followed as written:

The run id in a footer (e.2, e.3) has no route through the `Sink` trait, so `ParquetSink` and `ArrowIpcSink` carry an inherent `with_run_id(RunId)`, which is this component's own d.1 and therefore pre-approved; unset, the footer carries the nil id. The facade sets it.

`resume` (f.8) must list a destination and delete what is above the watermark. The reactor had neither, so the contracts gained `delete_object` and `abort_multipart` (contracts d.9) and a sink's constructor takes `Arc<dyn ObjectMetadata>` beside its reactor, whose `list_prefix` is the listing side. Until a sink is built with one, a non-local scheme returns `Resume("cannot list destination")`, which f.8 already calls the honest answer.

One piece of that resume is still missing and is known: nothing hands a sink the id of a multipart upload it started. `write_object` drives multipart internally through `put_multipart`, which never exposes an id, so a sink cannot record one in its checkpoint and cannot abandon its own interrupted upload after a crash; `abort_multipart` is usable today only by a caller that obtained an id some other way. The interface that closes it is the reactor reporting what it has in flight, so the checkpoint thread can record `(url, upload id)` pairs in the manifest and a resumed run can abort them; it is not built, because nothing resumes across a process yet. It is a wave 4 item, when the facade wires resume, and until then a sink resuming against an object store leaves its interrupted upload to the store's own lifecycle policy, which the operator's guide must say (PM, 2026-09-22, on the component 6 agent's report).

`encode_framing` refuses a `base_offset` that is not page-aligned (contracts e.7), so f.2's "the first record's base is the file position after the magic" is unreachable: the magic sits at 0 and the first record's base is the first page boundary. Every later record's base is `round_up(cursor, page)`.

The footer is written with `arrow::ipc::writer::FileWriter` over a template and its block entries rewritten in place, the technique `amoru_kernel::ipc` already uses, because `arrow` does not re-export `flatbuffers` and adding a crate to reach an interface is not a reason the dependency table accepts.

Arrow emits an all-ones validity bitmap of its own for a column with no nulls, which is not arena memory, so `BufferView::of_arrow` refuses it; the sink stages such a buffer in a small arena buffer and counts it as an encode copy only when its address is one of the batch's own buffers, so a genuinely foreign payload buffer is still reported (SI-I4).

A held morsel's `write` future does not resolve until its turn (f.4), because SI-I1 says the arena has the bytes back when it resolves; that also removes the problem that `skip` is synchronous and cannot forward held morsels itself.

SI-T16 says `open` fails "with `Alloc` naming the sink in the message"; `AmoruError::Alloc` has no message field, so the test asserts the variant and the sink logs its name.


Files: `src/lib.rs`, `src/parquet_sink.rs` (f.1, e.2, f.5, f.7, f.8; the `std::io::Write` over an arena buffer), `src/ipc_sink.rs` (f.2, e.3; records through `amoru_kernel::ipc::encode_framing`, the footer through `arrow::ipc`'s flatbuffer builders; no encoder of its own), `src/tensor_sink.rs` (f.3, e.4), `src/reorder.rs` (f.4), `src/handle.rs` (`SinkHandle`, f.9), `src/commit.rs` (f.5), `src/checkpoint.rs` (e.5, the shared watermark set of f.7, used by all three file sinks), `src/stats.rs`. No `unsafe`.

Verify before starting: whether the pinned `parquet` version's `ArrowWriter` accepts a caller-supplied `Write` without an internal `Vec` staging of whole pages (it buffers a column chunk's pages before flushing a row group; that buffering is the encoder's and is counted in `encode_bytes`, but it must not hold a second copy of the whole file); whether `AsyncArrowWriter` targeting `object_store` multipart directly would avoid the whole-file buffer (if so, and if it can write from arena memory, use it and record the change in f.1; if it copies through a `Vec`, do not).

Anti-patterns: no `Vec<u8>` of payload size anywhere in this crate; no `write` that copies an arena buffer for `ArrowIpcSink` or `TensorSink`; no commit of a file that has not been fully written; no second IPC encoder (the one in contracts e.7 is the one); no method on `ReorderBuffer` that the scheduler must call besides the contract's `Sink` methods and the two inherent readers `is_stalled` and `next_expected`.

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
| S17, D13 | SI-I8, e.5, f.7, f.8 | SI-T12, SI-T13, SI-T14 |
| contracts d.8 `skip`, 10 d.1 `SinkHandle` | f.4, f.9 | SI-T5, SI-T15 |
| contracts e.7 (one IPC encoder), d.3 `BufferView` | e.3, f.1, f.2, SI-I4 | SI-T4, SI-T9, SI-T16 |

## o. Deferred (post-v1)

None.
