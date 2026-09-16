# Amoru SDD 07: Sources (`amoru-sources`)

**Document type:** software design document, component 7 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` section 5.2; decision D3; criteria S12, S13; global invariant G-I2 (decode exception)
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.6 (`Source`, `Split`, `RowRange`), d.4 (`SourceSchema`, `Payload`), e.4 (`AMB1`)
**Component location:** `crates/amoru-sources`, Rust
**Consumes:** contracts (1), arena (2), reactor (6). **Consumed by:** scheduler (10, drives reads), placement (9, receives Q0 morsels), python surface (12, constructs)

**Decisions worth your eye:** (1) Parquet decoding produces Arrow buffers from the parquet crate's own allocations and is then moved into the arena with one copy, counted as decode; the alternative, a custom Arrow allocator through the parquet crate, is not supported upstream and is recorded as a later optimisation; (2) tensor files are memory-mapped and handed out by pointer when aligned, copied into the arena when not, and the report says which; (3) a `PyIteratorSource` exists for users with data that is neither, with no look-ahead and probe-only sizing.

---

## a. Purpose and boundary

A source turns a dataset into splits with metadata, then reads splits into resident payloads on request. Three implementations: `ParquetSource` (object store or local, footer-driven look-ahead, projection, row-range sub-splitting), `TensorSource` (safetensors, NumPy `.npy`, `AMB1`; memory-mapped, sliced along the leading dimension), and `PyIteratorSource` (a Python iterator of `pyarrow.RecordBatch` or DLPack objects; no metadata). Reads run on the reactor; sources never block a worker.

It owns: split planning and metadata extraction; decoding Parquet into Arrow; mapping tensor files; sub-splitting and the row-range contract; the decode boundary copy into the arena.

It refuses to know: how many splits to read ahead (scheduler's `read_ahead` knob); what morsel size to target (the scheduler passes a target when it asks for a read); where the payload goes next (placement).

## b. Vocabulary

**Footer.** Parquet file metadata at the end of the file: row groups, per-column chunk metadata (uncompressed and compressed sizes, null counts, statistics).

**Row group.** Parquet's unit of independent decoding; the default split.

**Projection.** The subset of columns the run reads; set at construction; applied at plan time so `column_bytes` reflect only projected columns.

**Decode copy.** The one CPU copy of decoded Arrow buffers into arena memory, permitted by G-I2's decode exception, counted in `decode_bytes`.

**Mapped payload.** A tensor payload whose data pointer lies inside a memory-mapped file rather than the arena; tier `Host`; not DMA-eligible until promoted by a copy into the arena.

## c. Invariants

**SO-I1. Plan before read.** `plan` completes and returns every split before the first `read` is issued; a `read` for a split id not in the plan is an error. (Contract in preamble 1.3 row 7.)

**SO-I2. Split metadata is what the file says.** For Parquet, `rows`, `uncompressed_bytes`, `column_bytes` and `null_counts` come from the footer for the projected columns with `estimated = false`; for tensors from the header, exact; for iterators, `estimated = true` from the first batch.

**SO-I3. A read returns a resident payload in the requested tier.** `read(split, rows, alloc, tier)` returns a payload whose buffers are arena-owned in `tier` (`Host` or `PinnedHost`; a `Device` request is a `Plan` error at construction for v1), except a mapped tensor payload, which is `Host` and marked `mapped = true` in the source's stats.

**SO-I4. Sub-splitting is exact.** `read(split, Some(range))` returns exactly `range.end - range.start` rows from the split's rows in order; consecutive ranges concatenate to the whole split.

**SO-I5. Decode is the only CPU copy.** Per morsel, payload bytes are copied by the CPU at most once, from decoder output into the arena; counted in `decode_bytes`; `AllocStats.payload_copies_total` is not incremented (the decode counter is separate and is the permitted exception). Upholds G-I2.

**SO-I6. Sources are stateless across reads.** Two reads of the same split and range return equal payloads; no read depends on a prior read (the scheduler may issue them in any order within read-ahead).

**SO-I7. Zero rows is a valid split.** A Parquet row group or tensor with zero rows plans as a split with `rows = 0` and reads as an empty payload.

**SO-I8. Reads are repeatable.** For `ParquetSource` and `TensorSource`, `plan()` called twice on unchanged inputs returns equal splits, and `read(split, rows)` called twice returns equal payloads (CT-I12). `PyIteratorSource` cannot promise this: it plans one split per pulled batch and cannot re-pull, so it reports `sub_splittable = false` and `repeatable() == false`, and a run over it is not resumable and Q0 eviction is disabled for it (the placement engine's `set_staging(0, true)` is forced, so its morsels are written rather than dropped; the facade notes "iterator source: no resume, Q0 staged"). Rationale: recovery and Q0 eviction both re-read; a source that cannot must say so rather than return different rows.

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
    pub fn new(cfg: ParquetSourceConfig, reactor: std::sync::Arc<Reactor>) -> Result<ParquetSource>;   // reads footers here (plan cache), so `plan` is cheap
    pub fn stats(&self) -> SourceStats;
}
impl Source for ParquetSource { /* contracts d.6 */ }

pub struct TensorSource { /* private */ }
pub struct TensorSourceConfig {
    pub paths: Vec<std::path::PathBuf>,     // local files only in v1 (mmap); object URLs are a Plan error
    pub tensors: Option<Vec<String>>,       // names within a safetensors file; None = all, in file order
    pub slice_rows_hint: Option<u64>,
}
impl TensorSource { pub fn new(cfg: TensorSourceConfig) -> Result<TensorSource>; pub fn stats(&self) -> SourceStats; }
impl Source for TensorSource { /* contracts d.6 */ }

// feature "python"
pub struct PyIteratorSource { /* private */ }
impl PyIteratorSource { pub fn new(iter: pyo3::Py<pyo3::PyAny>, schema: SourceSchema) -> Result<PyIteratorSource>; }
impl Source for PyIteratorSource { /* plan returns one Split per pulled batch, pulled lazily: see f.6 */ }

#[derive(Clone, Debug, Default)]
pub struct SourceStats {
    pub splits: u64, pub bytes_planned: u64, pub reads: u64, pub decode_bytes: u64,
    pub mapped_payloads: u64, pub copied_payloads: u64, pub footer_reads: u64, pub groups_skipped: u64,
}

/// Row-group statistics predicate; only min/max pruning in v1.
pub enum RowFilter { Gt(String, ScalarValue), Lt(String, ScalarValue), Eq(String, ScalarValue) }
```

The scheduler calls `read` with a `RowRange` it computes from the controller's morsel target and the split's `uncompressed_bytes / rows`; the source does not size.

### d.2 Consumed

`amoru_kernel::{Source, Split, RowRange, SourceSchema, Payload, Allocator, Buffer, Tier, DType, AmoruError, amb1}`; `amoru_reactor::Reactor` (`read_object`, `read_file`, `head_object`, `list_prefix`); `parquet` (`ParquetMetaDataReader`, `ParquetRecordBatchStreamBuilder` with a custom `AsyncFileReader` over the reactor, `ProjectionMask`, `RowSelection`); `safetensors` (header parse); `memmap2`; `arrow`.

## e. Data model, formats and state machines

### e.1 Parquet plan

```
for each url (after listing prefixes):
  head_object → size
  read_object(size-8-footer_len .. size)  // footer length from the last 8 bytes; one or two ranged reads
  parse metadata; for each row group rg:
    Split { id, rows: rg.num_rows, uncompressed_bytes: Σ projected column chunks' uncompressed_size,
            column_bytes: per projected chunk, null_counts: per chunk stats (None if absent),
            sub_splittable: true, estimated: false }
    skip rg if any RowFilter proves no row can match (min/max)
```

Split ids are assigned in file order then row-group order. The plan is cached in the source; `plan()` returns a clone.

### e.2 Parquet read

Given `(split, rows)`: build a `ParquetRecordBatchStreamBuilder` over an `AsyncFileReader` whose `get_bytes(range)` calls `reactor.read_object` into an arena buffer of the range's length (page-rounded up, direct-eligible) and returns the bytes view; set projection; set `RowSelection` to the row range when given; set the batch size to the whole range so exactly one batch is produced; drive the stream to its single batch; perform the decode copy of each column buffer into arena buffers of `tier`; assemble the `RecordBatch`; return `Payload::table`. Compressed page bytes are released as soon as the decoder consumes them.

### e.3 Tensor plan

For each path: detect format by extension and magic (`.safetensors` header, `.npy` magic `\x93NUMPY`, `AMB1` magic); parse the header; for each tensor (or the single tensor), one `Split` with `rows = shape[0]` (1 for rank 0), `uncompressed_bytes = element_count × item_size`, `sub_splittable = rank ≥ 1`, `column_bytes = []`, `estimated = false`. Record for each split its file offset and whether the data offset is 64-byte aligned.

### e.4 Tensor read

Memory-map the file once (per source, `memmap2::Mmap`, `MADV_SEQUENTIAL`); for `(split, rows)`, compute the byte range for rows `[start, end)` along dimension 0 (`row_bytes = product(shape[1..]) × item_size`); if the range's start pointer is 64-byte aligned, build a `ManagedTensor` over the mapping (deleter holds the `Arc<Mmap>`), tier `Host`, `mapped_payloads += 1`; else, or when `tier == PinnedHost`, allocate from the arena and copy (`copied_payloads += 1`, `decode_bytes += len`). On a host with direct IO and `tier == PinnedHost`, prefer `reactor.read_file` of the page-aligned enclosing range into an arena buffer and slice (this is the DMA path; the mmap path is the no-hardware path).

### e.5 Split state

Splits are immutable after plan; the source keeps no per-split state (SO-I6).

## f. Algorithms and policies

**f.1 Footer reads.** Two ranged reads per file (last 64 KiB speculatively, then the rest of the footer if longer), issued for all files with `object_concurrency` parallelism; `footer_reads` counts them.

**f.2 Row-group pruning.** For each `RowFilter`, if the column's min/max statistics exist and prove the predicate false for the whole group, skip; missing statistics never skip.

**f.3 Range read size.** Column chunks of a row group are read as one contiguous byte range from the first projected chunk's offset to the last projected chunk's end when the gap between projected chunks is under 1 MiB; otherwise one range per chunk. Ranges are rounded outward to page boundaries for direct-eligibility.

**f.4 Decode copy.** For each output column, `alloc(buffer_len, tier)` for each of the array's buffers (values, offsets, validity), `memcpy`, rebuild the array with `ArrayData::builder` over the arena buffers. Dictionary-encoded columns are unpacked to plain arrays before the copy (the dictionary is not carried; it would be a third buffer family and complicates the tensor crossing).

**f.5 Estimated bytes for iterators.** `PyIteratorSource::plan` pulls the first batch, records its bytes and rows, and returns one `Split` with `estimated = true`; subsequent `read` calls pull the next batch regardless of the split argument (the scheduler treats an iterator source as a stream: it calls `read` with the same split id until `read` returns `None`-equivalent, represented as a zero-row payload followed by `AmoruError::Source { msg: "exhausted" }`). This is the one source that does not obey SO-I6, and it is documented in the surface as "no look-ahead, probe-only sizing".

**f.6 Coalescing.** When a split's `uncompressed_bytes` is below the scheduler's target, the scheduler may pass a `RowRange` spanning the whole split and issue several splits; the source does not merge splits (each read is one payload). The placement engine's queue is where small morsels accumulate; the controller raises `morsel.min_bytes` in effect by asking the scheduler for larger ranges, not by asking the source to merge.

## g. Concurrency within the component

`read` is called from the scheduler's source-drive loop and runs on reactor threads (decode included; decode is CPU-heavy, so the reactor runs it on the blocking pool to keep its async threads free). Sources are `Send + Sync`; the Parquet plan cache is immutable after `new`; the mmap `Arc` is shared. No locks after construction.

## h. Behaviour

**Normal path (Parquet on MinIO).** `new` lists the prefix, reads 40 footers in parallel, plans 400 splits; the scheduler issues `read` for splits 0..read_ahead with ranges sized to the target; each read fetches the row group's byte range, decodes on the blocking pool, copies into the arena, returns; placement receives Q0 morsels.

**Normal path (safetensors).** `new` parses headers; splits per tensor; reads slice rows along dim 0; aligned data is mapped and handed out by pointer.

**Edge cases.** Row group with zero rows (SO-I7). Projection naming a missing column: `Plan` error at `new` naming the column and the file. Files with different schemas under one prefix: `Plan` error unless the projected columns have identical types in all files. A tensor file whose data offset is unaligned (typical safetensors): copied path, `copied_payloads` counts it, report notes "safetensors data unaligned; copied into arena (X GiB)". A `RowRange` outside the split: `Source` error. Nested Parquet types (struct, list): decoded and copied as-is; they are tables, not convertible to tensors (contracts e.3).

**Failures.** Footer parse failure: `Source` naming the file. Object read failure after the reactor's retries: `Source` with the split id and byte range. Decoder error (corrupt page): `Source` with row group and column. mmap failure: `Io`.

## i. Configuration

`readahead.splits` (consumed by the scheduler, not here), `reactor.object_concurrency` (footer parallelism). No rows owned.

## j. Observability

`SourceStats`; `tracing`: `source.plan` (info: files, splits, bytes, skipped), `source.unaligned_tensor` (warn once per file), `source.read` (trace: split, range, bytes, decode µs).

## k. Tests

Uses the testkit generator for Parquet and tensor files and the in-memory object store; MinIO in CI.

**SO-T1 plan_before_read.** `read` with an unplanned id errors; every planned id reads. SO-I1.

**SO-T2 metadata_exact.** Generated Parquet with known row counts, sizes and nulls; splits match the generator's ledger for the projection. SO-I2.

**SO-T3 resident_tier.** Reads into `Host` and `PinnedHost` (fake pinned) yield arena-owned buffers in that tier (checked via `Allocator::contains`). SO-I3.

**SO-T4 subsplit_exact.** Ranges `[0,10)`, `[10,25)`, `[25,rows)` concatenate to the full row group; random ranges match a pandas-free reference (the generator's rows). SO-I4.

**SO-T5 one_decode_copy.** `decode_bytes` equals payload bytes per read; `payload_copies_total` unchanged. SO-I5.

**SO-T6 stateless.** Same split twice → equal batches; reversed read order → same results. SO-I6.

**SO-T7 zero_rows.** Zero-row row group and zero-length tensor plan and read. SO-I7.

**SO-T8 pruning.** Row groups whose statistics exclude the filter are skipped; groups without statistics are read. f.2.

**SO-T9 tensor_mapped_vs_copied.** Aligned `AMB1`: mapped, pointer inside the mmap; safetensors with unaligned data: copied, pointer inside the arena. e.4.

**SO-T10 iterator_source.** A Python generator of five batches yields five morsels then exhaustion; `estimated` is true. f.5.

**SO-T11 schema_mismatch.** Two files with a column type difference in the projection: `Plan` error names both files. h.

**SO-T12 bandwidth.** (reference host) Parquet from local NVMe with the identity kernel reaches ≥ 80% of the reactor's RE-T11 figure after decode (decode cost reported separately). S4, S12.

**SO-T13 repeatable_reads.** For `ParquetSource` and `TensorSource`: two `plan()` calls are equal; for 50 random `(split, rows)` pairs, two `read` calls are byte-equal; for `PyIteratorSource`, `sub_splittable == false` and `repeatable() == false`. SO-I8.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/parquet/{mod.rs, plan.rs (e.1, f.1, f.2), read.rs (e.2, f.3), decode_copy.rs (f.4), reader.rs (AsyncFileReader over the reactor)}`, `src/tensor/{mod.rs, plan.rs (e.3), read.rs (e.4), safetensors.rs, npy.rs}`, `src/py_iter.rs` (feature python), `src/stats.rs`. `unsafe` permitted in `tensor/read.rs` (mmap pointer to `ManagedTensor`) with `// SAFETY:` citing that the `Arc<Mmap>` held by the deleter outlives the view.

The `AsyncFileReader` implementation must request page-aligned ranges and hand the parquet crate a `Bytes` view over the arena buffer without copying (use `Bytes::from_owner` over the `Buffer`); the decode copy happens after decoding, not before.

Anti-patterns: no `read_to_end` of a whole file; no reading unprojected columns; no dictionary arrays in output; no merging of splits inside the source.

Verify before starting: parquet crate `RowSelection` behaviour for selections that begin mid-page (it must skip, not decode, the excluded rows; check with a 1 M-row group and a 10-row range that the decode time is small); `Bytes::from_owner` availability in the pinned `bytes` version.

## m. Open items

None.

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| CT-I12, S17 | SO-I8 | SO-T13 |
| D3 (look-ahead) | SO-I2 | SO-T2 |
| G-I2 (decode exception) | SO-I5 | SO-T5 |
| S13 (mapped tensors) | e.4 | SO-T9 |
| S12, S4 | f.3, e.2 | SO-T12 |
| 5.2 sub-splitting | SO-I4 | SO-T4 |
