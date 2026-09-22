# Amoru SDD 07: Sources (`amoru-sources`)

**Document type:** software design document, component 7 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/amoru-runtime-design.md` section 5.2; decision D3; criteria S12, S13; global invariant G-I2 (decode exception)
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.6 (`Source`, `Split`, `RowRange`), d.4 (`SourceSchema`, `Payload`, `ManagedTensor::from_buffer`), d.3 (`Buffer`, `Allocator`), d.9 (`Reactor`), e.4 (`AMB1`)
**Component location:** `crates/amoru-sources`, Rust; feature `python` for `PyIteratorSource`
**Consumes:** contracts (1); the reactor (6) as `Arc<dyn Reactor>` and as `Arc<dyn ObjectMetadata>` (both contracts d.9); the arena (2) only through the `&dyn Allocator` that `read` receives. **Consumed by:** scheduler (10, drives reads), placement (9, receives Q0 morsels), python surface (12, constructs)

**Decisions worth your eye:** (1) Parquet decoding produces Arrow buffers from the parquet crate's own allocations and is then moved into the arena with one copy, counted as decode; the alternative, a custom Arrow allocator through the parquet crate, is not supported upstream and is recorded as a later optimisation; (2) tensor files are read by the reactor into arena buffers, page-aligned enclosing range and all, and the tensor is a safe view at a byte offset into that buffer (`ManagedTensor::from_buffer`), so an unaligned safetensors data section costs no copy and no `unsafe`, and every payload a source returns is arena-owned; (3) a `PyIteratorSource` exists for users with data that is neither, with no look-ahead and probe-only sizing, pulled on the reactor's blocking pool under interpreter attachment.

---

## a. Purpose and boundary

A source turns a dataset into splits with metadata, then reads splits into resident payloads on request. Three implementations: `ParquetSource` (object store or local, footer-driven look-ahead, projection, row-range sub-splitting), `TensorSource` (safetensors, NumPy `.npy`, `AMB1`; read into the arena and sliced along the leading dimension), and `PyIteratorSource` (a Python iterator of `pyarrow.RecordBatch` or DLPack objects; no metadata). Reads run on the reactor and land in arena buffers of the requested tier; sources never block a worker.

It owns: split planning and metadata extraction; decoding Parquet into Arrow; reading tensor files; sub-splitting and the row-range contract; the decode boundary copy into the arena.

It refuses to know: how many splits to read ahead (scheduler's `read_ahead` knob); what morsel size to target (the scheduler passes a target when it asks for a read); where the payload goes next (placement).

## b. Vocabulary

**Footer.** Parquet file metadata at the end of the file: row groups, per-column chunk metadata (uncompressed and compressed sizes, null counts, statistics).

**Row group.** Parquet's unit of independent decoding; the default split.

**Projection.** The subset of columns the run reads; set at construction; applied at plan time so `column_bytes` reflect only projected columns.

**Decode copy.** The one CPU copy of decoded Arrow buffers into arena memory, permitted by G-I2's decode exception, counted in `decode_bytes`.

**Enclosing range.** For a tensor read, the smallest page-aligned byte range of the file that contains the requested rows' bytes; what the reactor reads, so the operation is direct-eligible (06 e.3); the tensor is a view at an offset inside it.

**Host tier.** The run's one host tier, `PinnedHost` when `alloc.is_pinned()` and `Host` otherwise (contracts e.1); the tier a source is asked for on a host-only chain.

## c. Invariants

**SO-I1. Plan before read.** `plan` completes and returns every split before the first `read` is issued; a `read` for a split id not in the plan is an error. (Contract in preamble 1.3 row 7.)

**SO-I2. Split metadata is what the file says.** For Parquet, `rows`, `uncompressed_bytes`, `column_bytes` and `null_counts` come from the footer for the projected columns with `estimated = false`; for tensors from the header, exact; for iterators, `estimated = true` from the first batch.

**SO-I3. A read returns a resident payload in the requested tier, in arena buffers.** `read(split, rows, alloc, tier)` returns a payload every buffer of which was allocated from `alloc` in `tier` (`alloc.contains` is true for each), so `Payload::table` and `Payload::tensor` infer the tier from the arena token and the budget sees every payload byte (G-I1). `tier` is the run's host tier; asking for the other host tier is the arena's `Alloc` error passed through, and a `Device` request is a `Plan` error at construction for v1. No source returns memory it did not get from `alloc`: not a mapping, not a Python object's buffer, not a decoder's.

**SO-I4. Sub-splitting is exact.** `read(split, Some(range))` returns exactly `range.end - range.start` rows from the split's rows in order; consecutive ranges concatenate to the whole split.

**SO-I5. Decode is the only CPU copy.** Per morsel, payload bytes are copied by the CPU at most once, from decoder output into the arena (Parquet) or from a Python-owned batch into the arena (the iterator source, whose "decoder" is the interpreter); counted in `decode_bytes`; `AllocStats.payload_copies_total` is not incremented (the decode counter is separate and is the permitted exception). A tensor read copies nothing: the reactor's DMA lands the bytes in the arena and the tensor is a view. Upholds G-I2.

**SO-I6. Sources are stateless across reads.** Two reads of the same split and range return equal payloads; no read depends on a prior read (the scheduler may issue them in any order within read-ahead).

**SO-I7. Zero rows is a valid split.** A Parquet row group or tensor with zero rows plans as a split with `rows = 0` and reads as an empty payload.

**SO-I8. Reads are repeatable.** For `ParquetSource` and `TensorSource`, `plan()` called twice on unchanged inputs returns equal splits, and `read(split, rows)` called twice returns equal payloads (CT-I12). `PyIteratorSource` cannot promise this: it plans one split per pulled batch and cannot re-pull, so it reports `sub_splittable = false` and `repeatable() == false`, and a run over it is not resumable and Q0 eviction is disabled for it (the facade calls the placement engine's `set_staging(0, true)` before the first push, so its morsels are written rather than dropped, PL-I6; the facade notes "iterator source: no resume, Q0 staged"). Rationale: recovery and Q0 eviction both re-read; a source that cannot must say so rather than return different rows.

**SO-I9. A row is never split.** `read` returns whole rows: a single row (a long text row, a wide tensor row) larger than the scheduler's morsel maximum is returned at its natural size as a one-row payload, never truncated and never refused for being large; the scheduler decides what to do with an oversized morsel (SC-T17), the source only reports the size truthfully in `Payload::bytes`. Rationale: architecture section 7's "single row larger than the maximum morsel" case must degrade to one large morsel, not to an error at morsel 40,000.

## d. Interfaces

### d.1 Exposed

```rust
pub struct ParquetSource { /* private */ }
pub struct ParquetSourceConfig {
    pub urls: Vec<String>,                  // files or prefixes; prefixes are listed at plan time
    pub columns: Option<Vec<String>>,       // projection; None = all
    pub filters: Vec<RowFilter>,            // optional predicate pushdown on row-group statistics (skip groups only; no row filtering in v1)
    pub batch_rows_hint: Option<u64>,       // the scheduler passes the target per read; this is a fallback
}
impl ParquetSource {
    /// Reads footers here (plan cache), so `plan` is cheap. `meta` is the reactor's
    /// `ObjectMetadata` (contracts d.9) for object URLs; local paths use `std::fs::metadata`
    /// and `read_dir` for size and listing and `reactor.read_file` for bytes.
    pub fn new(cfg: ParquetSourceConfig, reactor: std::sync::Arc<dyn Reactor>, meta: std::sync::Arc<dyn ObjectMetadata>) -> Result<ParquetSource>;
    pub fn stats(&self) -> SourceStats;
}
impl Source for ParquetSource { /* contracts d.6 */ }

pub struct TensorSource { /* private */ }
pub struct TensorSourceConfig {
    pub paths: Vec<std::path::PathBuf>,     // local files only in v1; object URLs are a Plan error
    pub tensors: Option<Vec<String>>,       // names within a safetensors file; None = all, in file order
    pub slice_rows_hint: Option<u64>,
}
impl TensorSource {
    /// Parses every header here through `reactor.read_file_opt(.., allow_short = true)`.
    pub fn new(cfg: TensorSourceConfig, reactor: std::sync::Arc<dyn Reactor>) -> Result<TensorSource>;
    pub fn stats(&self) -> SourceStats;
}
impl Source for TensorSource { /* contracts d.6 */ }

// feature "python"; lives in this crate (not in amoru-adapters) so every source is
// constructed the same way by the facade and the scheduler sees one crate.
pub struct PyIteratorSource { /* private */ }
impl PyIteratorSource {
    /// `iter` yields `pyarrow.RecordBatch` objects (Table schema) or objects with
    /// `__dlpack__` (Tensor schema). The reactor is taken like every other source's;
    /// in v1 the iterator source issues no reactor operation (its pull runs on the
    /// blocking pool of the runtime that polls `read`, which is the reactor's, f.5).
    pub fn new(iter: pyo3::Py<pyo3::PyAny>, schema: SourceSchema, reactor: std::sync::Arc<dyn Reactor>) -> Result<PyIteratorSource>;
    pub fn stats(&self) -> SourceStats;
}
impl Source for PyIteratorSource { /* plan returns one Split per pulled batch, pulled lazily: see f.5; repeatable() == false */ }

#[derive(Clone, Debug, Default)]
pub struct SourceStats {
    pub splits: u64, pub bytes_planned: u64, pub reads: u64, pub decode_bytes: u64,
    pub tensor_direct_reads: u64, pub tensor_buffered_reads: u64, pub footer_reads: u64, pub groups_skipped: u64,
    pub oversized_rows: u64,                // one-row payloads above the range's target (SO-I9)
}

/// Row-group statistics predicate; only min/max pruning in v1.
pub enum RowFilter { Gt(String, ScalarValue), Lt(String, ScalarValue), Eq(String, ScalarValue) }
```

The scheduler calls `read` with a `RowRange` it computes from the controller's morsel target and the split's `uncompressed_bytes / rows`; the source does not size. Every constructor takes `Arc<dyn Reactor>`, never the concrete `amoru_reactor::Reactor`, so a test builds a source over the testkit's `FakeReactor` and the facade over the real one.

### d.2 Consumed

`amoru_kernel::{Source, Split, RowRange, SourceSchema, Payload, ManagedTensor, Allocator, Buffer, BufferView, Reactor, Completion, ObjectMetadata, ObjectMeta, Tier, DType, AmoruError, amb1}` (`ObjectMetadata` and `ObjectMeta` are contracts d.9; `head_object`, `list_prefix`; no `amoru_reactor` dependency); `parquet` (`ParquetMetaDataReader`, `ParquetRecordBatchStreamBuilder` with a custom `AsyncFileReader` over the reactor, `ProjectionMask`, `RowSelection`); `safetensors` (header parse); `arrow`; `bytes` (`Bytes::from_owner`); with `python`, `pyo3` and `arrow`'s C Data Interface (`FFI_ArrowArray`, `FFI_ArrowSchema`).

## e. Data model, formats and state machines

### e.1 Parquet plan

```
for each url (after listing prefixes through `list_prefix`, or `read_dir` for a local directory):
  head_object → size                      // std::fs::metadata for a local path
  read_object(size-8-footer_len .. size)  // read_file for a local path; footer length from the last 8 bytes; one or two ranged reads
  parse metadata; for each row group rg:
    Split { id, rows: rg.num_rows, uncompressed_bytes: Σ projected column chunks' uncompressed_size,
            column_bytes: per projected chunk, null_counts: per chunk stats (None if absent),
            sub_splittable: true, estimated: false }
    skip rg if any RowFilter proves no row can match (min/max)
```

Split ids are assigned in file order then row-group order. The plan is cached in the source; `plan()` returns a clone.

### e.2 Parquet read

Given `(split, rows)`: build a `ParquetRecordBatchStreamBuilder` over an `AsyncFileReader` whose `get_bytes(range)` calls `reactor.read_object` (or `read_file` for a local path) into an arena buffer of the range's length (page-rounded up, direct-eligible) allocated from the `alloc` the read received, and returns a `Bytes` over it without copying; set projection; set `RowSelection` to the row range when given; set the batch size to the whole range so exactly one batch is produced; drive the stream to its single batch; perform the decode copy of each column buffer into arena buffers of `tier`; assemble the `RecordBatch`; return `Payload::table`. Compressed page bytes are released as soon as the decoder consumes them. A range of one row whose bytes exceed any target is decoded and returned as is (SO-I9, `oversized_rows += 1`).

### e.3 Tensor plan

For each path: detect format by extension and magic (`.safetensors` header, `.npy` magic `\x93NUMPY`, `AMB1` magic), reading the first 64 KiB with `read_file_opt(path, 0, buf, true)` and the rest of a longer safetensors header with a second read; parse the header; for each tensor (or the single tensor), one `Split` with `rows = shape[0]` (1 for rank 0), `uncompressed_bytes = element_count × item_size`, `sub_splittable = rank ≥ 1`, `column_bytes = []`, `estimated = false`. Record for each split its absolute data offset in the file and its dtype and shape.

### e.4 Tensor read

For `(split, rows)`, compute the byte range for rows `[start, end)` along dimension 0 (`row_bytes = product(shape[1..]) × item_size`, `lo = data_offset + start × row_bytes`, `hi = lo + (end − start) × row_bytes`); round it outward to the enclosing range `[page_floor(lo), page_ceil(hi))`, clipped to the file length; `alloc(enclosing_len, tier)`; `reactor.read_file_opt(path, page_floor(lo), buf, allow_short = true)` (short only when the file ends inside the last page; the short count must still cover `hi`, else `Source`); on completion, `ManagedTensor::from_buffer(buf, lo − page_floor(lo), dtype, [end − start, shape[1..]])` (contracts d.4, safe; the tensor owns the buffer, tier = `tier`); return `Payload::tensor`. The read is direct when the reactor's direct path is selected (the buffer, offset and length are page multiples by construction, except a short final page, which is buffered and counted by the reactor), so the bytes land by DMA; `tensor_direct_reads` or `tensor_buffered_reads` is incremented from `reactor.paths()` and the range's shape. No copy, no mapping, no `unsafe` in this crate: an unaligned safetensors data section only changes `byte_offset`. The over-read is at most two pages per read, charged to the budget like the payload (it is inside the same buffer). `.npy` files with Fortran order are a `Plan` error at `new` (the tensor would be non-contiguous, contracts b).

### e.5 Split state

Splits are immutable after plan; the source keeps no per-split state (SO-I6).

## f. Algorithms and policies

**f.1 Footer reads.** Two ranged reads per file (last 64 KiB speculatively, then the rest of the footer if longer), issued for all files with `object_concurrency` parallelism; `footer_reads` counts them.

**f.2 Row-group pruning.** For each `RowFilter`, if the column's min/max statistics exist and prove the predicate false for the whole group, skip; missing statistics never skip.

**f.3 Range read size.** Column chunks of a row group are read as one contiguous byte range from the first projected chunk's offset to the last projected chunk's end when the gap between projected chunks is under 1 MiB; otherwise one range per chunk. Ranges are rounded outward to page boundaries for direct-eligibility.

**f.4 Decode copy.** For each output column, `alloc(buffer_len, tier)` for each of the array's buffers (values, offsets, validity), `memcpy`, rebuild the array with `ArrayData::builder` over the arena buffers. Dictionary-encoded columns are unpacked to plain arrays before the copy (the dictionary is not carried; it would be a third buffer family and complicates the tensor crossing).

**f.5 Estimated bytes for iterators, and the pull.** `PyIteratorSource::plan` pulls the first batch, records its bytes and rows, and returns one `Split` with `estimated = true`, `sub_splittable = false`; subsequent `read` calls pull the next batch regardless of the split argument (the scheduler treats an iterator source as a stream: it calls `read` with the same split id until `read` returns `None`-equivalent, represented as a zero-row payload followed by `AmoruError::Source { msg: "exhausted" }`). This is the one source that does not obey SO-I6, and it is documented in the surface as "no look-ahead, probe-only sizing". The pull itself: the `read` future, polled on the reactor's runtime (the scheduler's source drive is a reactor task, SC f.1), calls `tokio::task::spawn_blocking` so the reactor's async threads are never held by Python; on the blocking thread it attaches to the interpreter (`Python::attach`, correct under both free-threaded and GIL builds; the adapters SDD owns the detection), calls `next(iter)`, and for a `pyarrow.RecordBatch` exports it through the Arrow C Data Interface, allocates arena buffers of `tier` for every buffer of every column, copies (the decode copy, SO-I5, `decode_bytes`), releases the Python objects, detaches, and returns `Payload::table`; for a DLPack object it imports the capsule, copies the contiguous bytes into one arena buffer and returns `ManagedTensor::from_buffer` over it. A Python exception is `Source { split, msg: "<type>: <message>" }`; `StopIteration` is exhaustion. The pull holds the attachment for the duration of `next` and the export only, never across the arena allocation's failure path (an `Alloc` error is returned after detaching). The facade forces `set_staging(0, true)` on the placement engine for this source (SO-I8), so its morsels are written, never evicted.

**f.6 Coalescing.** When a split's `uncompressed_bytes` is below the scheduler's target, the scheduler may pass a `RowRange` spanning the whole split and issue several splits; the source does not merge splits (each read is one payload). The placement engine's queue is where small morsels accumulate; the controller raises `morsel.min_bytes` in effect by asking the scheduler for larger ranges, not by asking the source to merge. In the other direction the source never shrinks below one row: a `RowRange` of one row is honoured whatever its bytes (SO-I9).

## g. Concurrency within the component

`read` is called from the scheduler's source-drive loop and runs on reactor threads (decode included; decode is CPU-heavy, so the source runs it on the blocking pool through `spawn_blocking`, which is legal because the future is polled inside the reactor's runtime, and the Python pull of f.5 goes the same way). The reactor operations a read issues never block the polling thread (RE-I6); the future awaits their `Completion`s. Sources are `Send + Sync`; the Parquet plan cache is immutable after `new`; the iterator source holds its `Py<PyAny>` behind a mutex taken only on the blocking thread. No other locks after construction.

## h. Behaviour

**Normal path (Parquet on MinIO).** `new` lists the prefix through `list_prefix`, reads 40 footers in parallel, plans 400 splits; the scheduler issues `read` for splits 0..read_ahead with ranges sized to the target; each read fetches the row group's byte range into an arena buffer, decodes on the blocking pool, copies into arena buffers of `tier`, returns; placement receives Q0 morsels.

**Normal path (safetensors).** `new` parses headers; splits per tensor; a read computes the rows' enclosing range, the reactor lands it in an arena buffer by direct IO, and the payload is a `from_buffer` view at the data offset; no copy whatever the alignment of the file.

**Edge cases.** Row group with zero rows (SO-I7). Projection naming a missing column: `Plan` error at `new` naming the column and the file. Files with different schemas under one prefix: `Plan` error unless the projected columns have identical types in all files. A tensor file whose data offset is unaligned (typical safetensors): the same path with a non-zero `byte_offset`; nothing to report. A `RowRange` outside the split: `Source` error. Nested Parquet types (struct, list): decoded and copied as-is; they are tables, not convertible to tensors (contracts e.3). A single row larger than the scheduler's target (a 300 MiB text row, a 1 GiB tensor row): returned as a one-row payload (SO-I9); the source logs `source.oversized_row` once per split. A request for the wrong host tier (`PinnedHost` on an unpinned arena): the arena's `Alloc` error is returned as is, with the split named in a `Source` wrapper.

**Failures.** Footer parse failure: `Source` naming the file. Object read failure after the reactor's retries: `Source` with the split id and byte range. Decoder error (corrupt page): `Source` with row group and column. A tensor file shorter than its header claims: `Source` naming the file and the expected length (the short read of e.4 detects it).

## i. Configuration

`readahead.splits` (consumed by the scheduler, not here), `reactor.object_concurrency` (footer parallelism). No rows owned.

## j. Observability

`SourceStats`; `tracing`: `source.plan` (info: files, splits, bytes, skipped), `source.oversized_row` (warn once per split: split, row, bytes), `source.read` (trace: split, range, bytes, decode µs, direct or buffered).

## k. Tests

Files come from the bench generator (preamble 6.5) written to a temp directory in the test's setup. Tests over local paths use the testkit's `FakeReactor` (contracts d.15: in-memory files keyed by path, so the test writes the generated file bytes into the fake through `write_file` and reads them back through `read_file`/`read_file_opt`; `with_latency`, `fail_next(op, n)` for the failure cases) and `FakeAllocator` (`pinned(bool)`, `with_limit`, `page_bytes(n)`, `fail_next(n)`); tests over object URLs use the real reactor with `ObjectStoreConfig::local_root` and are tagged "(integration, closes in wave 3)"; the MinIO variants are the CI job. Python tests need an interpreter with pyarrow and are tagged "(integration, closes in wave 3)" as well, because they run under the `python` feature only.

**SO-T1 plan_before_read.** `read` with an unplanned id errors; every planned id reads. SO-I1.

**SO-T2 metadata_exact.** Generated Parquet with known row counts, sizes and nulls; splits match the generator's ledger for the projection. SO-I2.

**SO-T3 resident_tier.** With `FakeAllocator::pinned(false)`, a read into `Host` yields buffers for which `contains` is true and `tier_of` is `Host` on every buffer of every column (or the tensor's buffer), and a read into `PinnedHost` fails with `Alloc { tier: PinnedHost }`; with `pinned(true)` the reverse; `FakeAllocator::in_use(tier)` grew by exactly the payload's buffers. For each of the three sources. SO-I3.

**SO-T4 subsplit_exact.** Ranges `[0,10)`, `[10,25)`, `[25,rows)` concatenate to the full row group; random ranges match a pandas-free reference (the generator's rows). SO-I4.

**SO-T5 one_decode_copy.** Parquet: `decode_bytes` equals payload bytes per read; `payload_copies_total` unchanged. Tensor: `decode_bytes == 0` and `FakeAllocator::allocations_total` grew by exactly one per read. SO-I5.

**SO-T6 stateless.** Same split twice → equal batches; reversed read order → same results. SO-I6.

**SO-T7 zero_rows.** Zero-row row group and zero-length tensor plan and read. SO-I7.

**SO-T8 pruning.** Row groups whose statistics exclude the filter are skipped; groups without statistics are read. f.2.

**SO-T9 tensor_view_in_arena.** Aligned `AMB1` and safetensors with an unaligned data section: for 50 random row ranges each, the tensor's `data_ptr` lies inside the one arena buffer of the read at `lo − page_floor(lo)`, `FakeReactor::ops()` shows one `read_file` whose offset and length are page multiples (except at the file's last page), the values equal the generator's, and `payload_copies_total` and `decode_bytes` are unchanged; a file truncated below its header's claim gives `Source`. e.4.

**SO-T10 iterator_source.** (integration, closes in wave 3; `python` feature) A Python generator of five `pyarrow.RecordBatch`es yields five morsels then exhaustion; `estimated` is true; every buffer of every morsel is arena-owned (`contains`); the pull ran on a thread that is not the polling thread; a generator that raises gives `Source` with the exception's type in the message; a DLPack generator gives tensors through `from_buffer`. f.5.

**SO-T11 schema_mismatch.** Two files with a column type difference in the projection: `Plan` error names both files. h.

**SO-T12 bandwidth.** (reference host, E1; provisional elsewhere with the host named) Parquet from local NVMe with the identity kernel reaches ≥ 80% of the reactor's RE-T11 figure after decode (decode cost reported separately). S4, S12.

**SO-T13 repeatable_reads.** For `ParquetSource` and `TensorSource`: two `plan()` calls are equal; for 50 random `(split, rows)` pairs, two `read` calls are byte-equal; for `PyIteratorSource`, `sub_splittable == false` and `repeatable() == false`. SO-I8.

**SO-T14 oversized_row_natural_size.** A generated Parquet file with one 300 MiB string row among small rows, and an `AMB1` tensor whose single row is 256 MiB: `read(split, Some(that row))` returns a one-row payload of the natural size, `Payload::bytes` reports it, `oversized_rows == 1`, one `source.oversized_row` warn; with `FakeAllocator::with_limit(Host, 128 MiB)` the same read fails with the arena's `Alloc` (not a truncated payload) and the error names the split. SO-I9; the scheduler's side is SC-T17.

**SO-T15 read_failure_paths.** `FakeReactor::fail_next(ReadFile, 1)`: the read resolves `Source` naming the split and range, the arena buffer is released (`in_use` back to baseline), and the next read of the same range succeeds byte-equal; `FakeAllocator::fail_next(1)`: `Alloc` before any reactor operation is issued (`ops()` unchanged). h, SO-I6.

**SO-T16 local_paths_use_read_file.** `ParquetSource` over plain paths and `file://` URLs issues only `read_file`/`read_file_opt` on `FakeReactor` (`ops()` has no object operations) and never calls `ObjectMetadata` (a test-local implementation that panics is passed as `meta`); over an `s3://` URL it calls `head_object` and `read_object` (integration, closes in wave 3). d.1, e.1.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/parquet/{mod.rs, plan.rs (e.1, f.1, f.2), read.rs (e.2, f.3), decode_copy.rs (f.4), reader.rs (AsyncFileReader over `Arc<dyn Reactor>`, local and object paths)}`, `src/tensor/{mod.rs, plan.rs (e.3), read.rs (e.4), safetensors.rs, npy.rs}`, `src/py_iter.rs` (feature python, f.5), `src/stats.rs`. No `unsafe` in this crate: the tensor view is `ManagedTensor::from_buffer` (contracts d.4) and the Arrow C Data Interface import in `py_iter.rs` goes through `arrow`'s safe `from_ffi`, and the copy into the arena follows it. No crate maps files: every read lands in the arena through the reactor (e.4), so `memmap2` is not a dependency of this crate.

The `AsyncFileReader` implementation must request page-aligned ranges and hand the parquet crate a `Bytes` view over the arena buffer without copying (use `Bytes::from_owner` over the `Buffer`); the decode copy happens after decoding, not before. The reactor calls it makes are `read_object`, `read_file` and `read_file_opt` from contracts d.9 and `head_object`/`list_prefix` from `ObjectMetadata` (contracts d.9); nothing else of the reactor is named here.

Anti-patterns: no `read_to_end` of a whole file; no reading unprojected columns; no dictionary arrays in output; no merging of splits inside the source; no payload memory from anywhere but `alloc` (SO-I3); no holding the interpreter attachment across an arena allocation.

Verify before starting: parquet crate `RowSelection` behaviour for selections that begin mid-page (it must skip, not decode, the excluded rows; check with a 1 M-row group and a 10-row range that the decode time is small); `Bytes::from_owner` availability in the pinned `bytes` version; `ManagedTensor::from_buffer`'s alignment rule for a `byte_offset` that is a multiple of `item_size` but not of 64 (safetensors data starts 8-byte aligned).

## m. Open items

None. (The Vortex source that used to sit here is SO-O1 in section o.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| CT-I12, S17 | SO-I8 | SO-T13 |
| D3 (look-ahead) | SO-I2 | SO-T2 |
| G-I2 (decode exception) | SO-I5 | SO-T5 |
| S13 (aligned tensors, zero copy) | e.4 | SO-T9 |
| S12, S4 | f.3, e.2 | SO-T12 |
| 5.2 sub-splitting | SO-I4 | SO-T4 |
| G-I1, contracts e.1 (one host tier) | SO-I3 | SO-T3 |
| architecture 7 (single row larger than the maximum morsel) | SO-I9 | SO-T14 |
| contracts d.9 (`Arc<dyn Reactor>`, `ObjectMetadata`) | d.1, e.1 | SO-T15, SO-T16 |
| f.5 (iterator pull under attachment), PL-I6 | SO-I8 | SO-T10 |

## o. Deferred (post-v1)

**SO-O1. `VortexSource` (Phase 7).** A fourth source over the Vortex file format (compressed Arrow arrays with lightweight cascading encodings; lazy layouts bound to a segment source; zone-map statistics every 8k rows by default). Two properties matter to this runtime and are the reason to build it: `plan` gets per-zone statistics at 8k-row granularity rather than per-row-group, so look-ahead features are finer and sub-splitting to a morsel target is exact; and Vortex's segment source abstraction lets reads land in arena buffers directly, so columns in canonical (Arrow-layout) encodings arrive with no decode copy, removing for those columns the exception G-I2 grants Parquet. Compressed columns are canonicalised at read (the decode copy, as for Parquet), unless the consumer accepts Vortex arrays, which no v1 kernel does. Tensors are out of scope (Vortex has no tensor payload; AMB1 stays). The `vortex` crate enters the dependency table (preamble 6.2) in the pull request that builds this source, under E2, not before. Architecture 5.6 records the further step where Vortex on local disk serves as Q0's staging tier.
