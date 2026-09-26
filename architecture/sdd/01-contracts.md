# Moruna SDD 01: Contracts crate (`moruna-kernel`)

**Document type:** software design document, component 1 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted)
**Parent:** `architecture/moruna-runtime-design.md` sections 5.1 to 5.4, 5.9; decisions D9, D10; criteria S7, S9, S13
**Preamble:** `00-preamble.md` (read first)
**Component location:** `crates/moruna-kernel`, Rust 2024 edition
**Consumes:** nothing internal; `arrow`, `dlpark`, `thiserror`, `blake3` **Consumed by:** every other component

---

## a. Purpose and boundary

The contracts crate is the set of types and interfaces that cross component boundaries, in one place, with one owner, frozen before any other component is built. It has almost no behaviour of its own: the exceptions are the pointer conversions between a table column and a tensor, byte accounting for payloads, the kernel fingerprint, and the definitions of the two file formats that more than one component reads and writes (the Moruna aligned binary format and the trace record schema). Everything else in it is a signature.

It owns: `Morsel`, `Payload`, `Tier`, `MorselFeatures`, `Origin`, `Split`; the traits `Source`, `Kernel`, `Sink`, `Allocator`, `Reactor`, `Placement`, `Knobs`, `TraceSink`; the value types those traits exchange (`PayloadSpec`, `KernelKind`, `KernelHints`, `Limits`, `HostProfile`, `TierBudgets`, `PlacementStats`, `Knob`, `TraceRecord`, `SinkSummary`, `Completion`); the error type `MorunaError`; the constants `ALIGNMENT` and the aligned binary format `MRB1`.

It re-exports the two dependencies that appear in its public signatures, `pub use arrow;` and `pub use dlpark;`, so that a crate depending on `moruna-kernel` alone can name a `RecordBatch` or a DLPack capsule without adding a dependency of its own and without risking a second, incompatible version of either: the testkit (d.15) depends on `moruna-kernel` only and could not otherwise be written, and a kernel author's crate gets the same guarantee, which is what S7 rests on (added 2026-09-22 on the F0.3 agent's report). It refuses to know: how any trait is implemented; how the process is threaded (preamble section 4 governs implementers, not this crate); anything about Python beyond what `dlpark` and Arrow's C Data Interface already are; any policy (sizing, admission, eviction). It has no dependency on tokio, cudarc, pyo3, parquet or object_store; a kernel author's crate depending on `moruna-kernel` must not pull any of those in.

## b. Vocabulary

Beyond the preamble's global vocabulary:

**Resident.** A payload whose bytes are addressable in the tier its tag names. A `Tier::Disk` payload is by definition not resident.

**Item size.** Bytes per element of a tensor dtype (`f32` is 4).

**Element count.** Product of a tensor's shape dimensions; a zero-dimensional tensor has element count 1.

**Contiguous.** A tensor whose strides equal the row-major strides for its shape, or whose strides are absent. Only contiguous tensors are produced by conversions; a non-contiguous tensor may be carried but never converted to a column.

**Numeric column.** An Arrow array of primitive type `Int8..Int64`, `UInt8..UInt64`, `Float16`, `Float32`, `Float64`, or a `FixedSizeList` of one of those with a fixed list size, with zero nulls.

**Fingerprint.** A 32-byte BLAKE3 digest over a kernel's stable identity string and its configuration bytes.

**Completion.** A future-like handle for a reactor operation that resolves exactly once to the buffer the operation was given, or to an error.

## c. Invariants

**CT-I1. Two payload variants.** `Payload` has exactly two variants, `Table` and `Tensor`; adding a third is a breaking change to this crate and to every component. Upholds G-I6. Rationale: the portability guarantee (S7) and every adapter depend on the match being total.

**CT-I2. A payload always carries its tier, and the tier is truthful.** Constructing a `Payload` with a tier its bytes are not in is impossible through the public API: `Payload::table(batch)` and `Payload::tensor(t)` infer the tier from the buffer's provenance (arena metadata) and constructors that take an explicit tier are `unsafe` with the caller asserting residency. Rationale: G-I3 is meaningless if tags can lie.

**CT-I3. `Morsel::bytes` is exact for tensors and Arrow-accounted for tables.** For a tensor, `bytes == element_count × item_size`. For a table, `bytes == batch.get_array_memory_size()`. It is recomputed whenever the payload is replaced. Rationale: every budget in the system is denominated in this number.

**CT-I4. Conversions are pointer operations.** `Payload::as_tensor` and `Payload::as_column` allocate nothing of payload size; they either return a view over the same bytes or an error. Upholds G-I2, S13. Rationale: this is the zero-copy claim of the whole design.

**CT-I5. Conversions fail at plan time.** `PayloadSpec::check(&SourceSchema) -> Result<()>` rejects, before any morsel is read, every schema whose columns cannot cross to the requested payload kind; `as_tensor` on a column that passed `check` cannot fail for a type reason. Rationale: a run must not die on morsel 40,000 for a reason knowable at morsel 0.

**CT-I6. Every trait object is `Send + Sync`.** Every trait in this crate has `Send + Sync` as a supertrait, and every value type exchanged across a trait is `Send`. Rationale: role-free workers (D1) hand any object to any thread.

**CT-I7. No blocking in the contract.** No trait method in this crate is documented as blocking except `Placement::pop_blocking`; asynchronous operations return `Completion` or a boxed future. Rationale: a worker must never block on IO by accident (preamble 4.1).

**CT-I8. The trace schema is stable and hashed.** `TraceRecord::SCHEMA_HASH` is a compile-time constant computed from the field list; a test asserts its value, so a schema change is a deliberate edit of the test. Rationale: the run report and the learned sizer are pure functions of this schema (G-I4).

**CT-I9. Alignment constant.** `ALIGNMENT == 64`, and every buffer or file offset this crate defines a layout for is a multiple of it. Rationale: cache-line and SIMD alignment, and the aligned binary format's DMA readiness.

**CT-I10. Errors carry the morsel.** Every error variant that can occur while processing a morsel carries `seq`, `stage` and, where known, `features` and the measured footprint, so a diagnostic can be produced without a debugger (G-I8).

**CT-I11. Reserved variants are matched, never wildcarded.** `Tier::Remote`, `NodeId` values other than `LOCAL_NODE`, `Locality::Local` and `StagingCodec` exist in v1 so that the multi-node extension adds behaviour without changing a signature. Every `match` on `Tier` in every crate has an explicit `Remote` arm that returns `MorunaError::Unsupported("rdma")` (or handles it, once the `rdma` feature exists); no `_ =>` arm covers it. Rationale: the aim is one framework that runs at one node and at many without rework; a wildcard arm is where the rework would hide.

**CT-I12. A morsel is recomputable from its origin.** `Source::read` is deterministic for a given `(split, rows)` while the input is unchanged, and a morsel at stage `s` equals `kernels[1..=s]` applied in order to that read. `Origin` therefore names the morsel completely. Q0 eviction (D5) and run resume (placement e.5) both rely on this and on nothing else; neither replicates bytes. Rationale: recovery by lineage costs nothing on the normal path, recovery by replication costs a copy of everything.

## d. Interfaces

All code below is normative: names, shapes and doc comments are binding; the implementer may add private helpers and derive macros. `Result<T>` is `core::result::Result<T, MorunaError>` throughout.

### d.1 Identifiers and constants

```rust
/// Cache-line alignment for every buffer and file layout in Moruna.
pub const ALIGNMENT: usize = 64;

/// Stage index in the linear chain. Stage 0 is the source's output.
pub type StageId = u16;
/// Monotonic morsel sequence number assigned by the source.
pub type Seq = u64;
/// Source split identifier, unique within a run.
pub type SplitId = u32;
/// Accelerator index as enumerated by discovery.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct DeviceId(pub u8);
/// Index of a node in a run. A single-node run has exactly one node, `LOCAL_NODE`.
/// Reserved for the multi-node extension (architecture section 11); every v1
/// value is `LOCAL_NODE` and every v1 `match` on it handles the general case
/// explicitly (CT-I11).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub struct NodeId(pub u16);
pub const LOCAL_NODE: NodeId = NodeId(0);
/// Identity of one run; 16 random bytes, printed as 32 lowercase hex characters.
/// Names the staging directory and the run manifest (placement e.3, e.5).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct RunId(pub [u8; 16]);
```

### d.2 Tier

```rust
/// Where a payload's bytes physically are.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Tier {
    /// Accelerator memory on the given device.
    Device(DeviceId),
    /// Page-locked host memory; a valid DMA source and target.
    PinnedHost,
    /// Ordinary host memory.
    Host,
    /// A staging segment on local disk; the payload has no resident bytes.
    Disk(SegmentRef),
    /// Registered memory on another node of the same run, reachable by one-sided
    /// RDMA through the reactor. Reserved: no v1 component produces this variant,
    /// and every v1 component that matches on `Tier` handles it by returning
    /// `MorunaError::Unsupported("rdma")` rather than by a wildcard arm (CT-I11).
    Remote(NodeId, RemoteRef),
}

/// Location of a payload in another node's registered memory. Fields are those
/// a one-sided RDMA read needs and nothing else; the memory key is opaque here.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct RemoteRef {
    /// Virtual address on the owning node.
    pub addr: u64,
    /// Remote memory key as registered with the owning node's NIC.
    pub rkey: u32,
    /// Byte length.
    pub len: u64,
}

/// Location of a demoted payload inside a staging segment.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SegmentRef {
    /// Segment file number within the run's staging directory.
    pub segment: u32,
    /// Byte offset of the payload's first byte; a multiple of the page size.
    pub offset: u64,
    /// Byte length of the payload as written.
    pub len: u64,
}

/// A tier without its payload (no device id, no segment, no remote ref); what a
/// water mark, a knob or a per-tier array is keyed by. `Tier::kind()` maps to it and
/// `TierKind::index()` equals `Tier::index()` for the corresponding tier.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum TierKind { Device, PinnedHost, Host, Disk, Remote }

/// How a staging segment's records are encoded. `Raw` is the payload's in-memory
/// layout written as is (page-aligned Arrow IPC or MRB1), moved by DMA with no CPU
/// in the path (PL-I4). Reserved for a compressed variant (a Vortex-encoded record,
/// architecture 5.6) that trades CPU on the demotion path for disk bytes when the
/// controller decides the run is disk-bound with idle cores; every v1 `match`
/// handles the enum explicitly (CT-I11) and the segment record header carries
/// the codec byte, so adding the variant changes no layout.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub enum StagingCodec { #[default] Raw }

impl Tier {
    /// True for Device, PinnedHost and Host; false for Disk and Remote.
    pub fn is_resident(&self) -> bool;
    /// Ordering used by the placement engine: Device > PinnedHost > Host > Remote > Disk.
    pub fn rank(&self) -> u8;
    /// Index into per-tier arrays: Device = 0, PinnedHost = 1, Host = 2, Disk = 3, Remote = 4.
    pub fn index(&self) -> usize;
    pub fn kind(&self) -> TierKind;
}
impl TierKind { pub fn index(&self) -> usize; }
/// Length of every per-tier array in the contract (`bytes_by_tier`, water marks, reservations).
pub const TIER_COUNT: usize = 5;
```

### d.3 Buffers and the allocator

```rust
/// The arena token every `Buffer` carries: the object that receives the bytes back when the
/// buffer drops. Defined here so the arena crate implements it without a circular dependency
/// (section l). `release` is called once per `Buffer`, so twice per allocation after a
/// `split_at`, once per half with that half's own pointer and length.
pub trait ArenaHandle: Send + Sync {
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier);
}

/// An owned, aligned region in one tier, returned to its arena on drop.
/// Deref to `[u8]` is provided only for Host and PinnedHost buffers;
/// a Device buffer exposes `device_ptr()` and panics on `as_ref()`.
pub struct Buffer { /* private: ptr, len, tier, arena token */ }

impl Buffer {
    /// Wrap a region the arena owns; the arena crate and a test allocator are the only callers.
    /// SAFETY: `ptr` is valid for `len` bytes in `tier` until `arena.release(ptr, len, tier)`,
    /// which this buffer calls exactly once on drop (CT-I2, the tier tag is truthful).
    pub unsafe fn from_raw(ptr: *mut u8, len: usize, tier: Tier, arena: std::sync::Arc<dyn ArenaHandle>) -> Buffer;
    /// The token this buffer releases to.
    pub fn arena(&self) -> &std::sync::Arc<dyn ArenaHandle>;
    pub fn len(&self) -> usize;
    pub fn tier(&self) -> Tier;
    /// Host-visible pointer; `None` for Device buffers.
    pub fn host_ptr(&self) -> Option<*mut u8>;
    /// Device pointer as an integer; `None` for host tiers.
    pub fn device_ptr(&self) -> Option<u64>;
    /// Split off a prefix; both halves keep the arena token and are freed independently.
    pub fn split_at(self, mid: usize) -> (Buffer, Buffer);
    /// Zero-copy conversion to an Arrow buffer whose deallocation releases to the arena
    /// (host tiers only; Device → error). The arena token survives this conversion and
    /// every Arrow slice of the result (slices share the allocation), so `Payload::table`
    /// on a batch decoded over such a buffer infers the tier correctly (e.2).
    pub fn into_arrow_buffer(self) -> Result<arrow::buffer::Buffer>;
    /// A shareable read-only view of this buffer for use as a DMA source (d.9). The
    /// view keeps the buffer alive; the buffer is not consumed.
    pub fn view(self: &std::sync::Arc<Buffer>) -> BufferView;
}

/// A read-only, `Send + 'static` view over bytes that a reactor operation may read
/// after the call returns (a DMA source). It owns a reference to whatever keeps the
/// bytes alive, so a failed operation loses nothing (RE-I1 drops the view, not the
/// bytes). All constructors are safe; the `unsafe` is inside this crate (l).
pub struct BufferView { /* private: ptr, len, tier, owner: Arc<dyn Any + Send + Sync> */ }
impl BufferView {
    pub fn len(&self) -> usize;
    pub fn tier(&self) -> Tier;
    pub fn host_ptr(&self) -> Option<*const u8>;
    pub fn device_ptr(&self) -> Option<u64>;
    /// Over an Arrow buffer whose allocation the allocator owns (`contains`); the tier
    /// is the allocator's for that region. `Staging("not an arena buffer")` otherwise.
    pub fn of_arrow(buf: &arrow::buffer::Buffer, alloc: &dyn Allocator) -> Result<BufferView>;
    /// Over the whole contiguous byte range of a tensor; tier = the tensor's.
    pub fn of_tensor(t: &std::sync::Arc<ManagedTensor>) -> Result<BufferView>;
    /// A sub-range of this view (for writing one page-aligned piece of a record).
    pub fn slice(&self, offset: usize, len: usize) -> BufferView;
}

/// Allocation statistics the arena maintains; read by the controller and by CT-T tests.
#[derive(Copy, Clone, Default, Debug)]
pub struct AllocStats {
    pub host_in_use: u64,
    pub pinned_in_use: u64,
    pub device_in_use: [u64; 8],
    pub allocations_total: u64,
    pub payload_copies_total: u64,   // incremented by any component that copies payload bytes with the CPU; must stay 0 outside sources and sinks
    pub boundary_copies_total: u64,  // adapters' one-time copy of a kernel's non-arena host output into the arena (05-adapters AD-I2)
}

pub trait Allocator: Send + Sync {
    /// Allocate `bytes` in `tier`, aligned to ALIGNMENT and to the page size when
    /// `bytes >= page size`. Fails with `MorunaError::Alloc` if the tier's budget
    /// would be exceeded; never blocks.
    fn alloc(&self, bytes: usize, tier: Tier) -> Result<Buffer>;
    /// The page size discovery reported for this host.
    fn page_bytes(&self) -> usize;
    fn stats(&self) -> AllocStats;
    /// True if `ptr` lies inside a region this allocator owns (used by adapters to skip the boundary copy).
    fn contains(&self, ptr: *const u8) -> bool { let _ = ptr; false }
    /// The tier of the region containing `ptr`, when `contains(ptr)`.
    fn tier_of(&self, ptr: *const u8) -> Option<Tier> { let _ = ptr; None }
    /// Record that a component copied `bytes` of payload with the CPU, which only a
    /// source decoding a non-layout-preserving format or a sink encoding to one may do
    /// (G-I2), and that an adapter copied `bytes` of a kernel's non-arena output into
    /// the arena once at the boundary (05 AD-I2). Without these the two counters in
    /// `AllocStats` have no writer: the arena owns the struct and no other method can
    /// raise them (added 2026-09-22 on the component 2 agent's report). Default: no-op,
    /// so a fake that does not count is still a valid allocator.
    fn note_payload_copy(&self, bytes: u64) { let _ = bytes; }
    fn note_boundary_copy(&self, bytes: u64) { let _ = bytes; }
    /// True when this allocator's host tier is page-locked. A run has exactly one host
    /// tier: `PinnedHost` when true, `Host` when false; the two never coexist in one
    /// process and no move between them exists (e.1).
    fn is_pinned(&self) -> bool;
}
```

### d.4 Payload, tensor wrapper, conversions

```rust
/// Element type of a tensor. Maps one-to-one to DLPack dtype codes and to the
/// Arrow primitive types listed in section e.3.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum DType { I8, I16, I32, I64, U8, U16, U32, U64, F16, BF16, F32, F64, Bool }

impl DType { pub fn item_size(&self) -> usize; }

/// A DLPack-backed tensor with ownership. Wraps `dlpark::versioned::Dlpack`, which is
/// `ManagedBox<DLManagedTensorVersioned>`, the DLPack 1.x versioned struct; `dlpark` 0.8.0 has
/// no type named `ManagedTensor`, and the versioned struct is what section l asked the agent to
/// verify, so the alias `pub type Dlpack = dlpark::versioned::Dlpack` is the exported name and
/// `into_dlpack` and `from_dlpack` speak it. The wrapper adds tier tracking and Moruna's
/// contiguity checks, and `from_dlpack` rejects a capsule whose major version is not DLPack's.
pub struct ManagedTensor { /* private */ }

impl ManagedTensor {
    pub fn dtype(&self) -> DType;
    pub fn shape(&self) -> &[i64];
    /// `None` means contiguous row-major.
    pub fn strides(&self) -> Option<&[i64]>;
    pub fn is_contiguous(&self) -> bool;
    pub fn element_count(&self) -> u64;
    pub fn byte_len(&self) -> u64;
    /// Raw data pointer plus byte offset, as DLPack defines them.
    pub fn data_ptr(&self) -> (*mut u8, u64);
    /// Export as a DLPack capsule the caller owns; consumes self.
    pub fn into_dlpack(self) -> Dlpack;
    /// Import from DLPack; the tier is read from the DLPack device field. A capsule whose
    /// major version is not `dlpark::ffi::DLPACK_MAJOR_VERSION` is `Convert`, never accepted.
    pub fn from_dlpack(t: Dlpack) -> Result<Self>;
    /// Wrap the bytes of an arena buffer, starting at `byte_offset`, as a contiguous
    /// row-major tensor of `dtype` and `shape`; the buffer is owned by the tensor and
    /// released when it drops. Checks `byte_offset + element_count × item_size ≤ len`
    /// and alignment; safe. Tier = the buffer's. This is how a staged tensor record
    /// comes back from disk (placement f.6) without `unsafe` outside this crate.
    pub fn from_buffer(buf: Buffer, byte_offset: u64, dtype: DType, shape: Vec<i64>) -> Result<Self>;
}

/// The data inside a morsel.
pub enum Payload {
    Table(arrow::record_batch::RecordBatch, Tier),
    Tensor(ManagedTensor, Tier),
}

/// What a kernel or sink wants to receive.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PayloadKind { Table, Tensor, Either }
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum TierPref { Host, Device, Any }
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct PayloadSpec { pub kind: PayloadKind, pub tier: TierPref }

/// Schema of a source's output or a kernel's output.
#[derive(Clone, Debug)]
pub enum SourceSchema {
    Table(arrow::datatypes::SchemaRef),
    Tensor { dtype: DType, shape: Vec<i64> },   // shape[0] == -1 means "batch dimension, variable"
}
impl SourceSchema {
    /// BLAKE3 over the Arrow IPC schema message bytes (Table) or over
    /// `"tensor:" || dtype code || shape as i64 LE` (Tensor). Keys the profile store (RC e.3).
    pub fn hash(&self) -> [u8; 32];
}

impl PayloadSpec {
    /// Plan-time check (CT-I5): Ok if every payload conforming to `schema` can be
    /// delivered as `self.kind`. For `Tensor` on a table schema, every projected
    /// column must be a numeric column and all must share one dtype.
    pub fn check(&self, schema: &SourceSchema) -> Result<()>;
}

impl Payload {
    /// Infer the tier from the batch's buffers (arena metadata); error if buffers
    /// are not arena-owned and not host memory.
    pub fn table(batch: RecordBatch) -> Result<Payload>;
    /// As `table`, but infers the tier through `alloc.tier_of` on each buffer's pointer
    /// rather than the arena token; for batches whose buffers were built over a
    /// foreign pointer that happens to lie in the arena (adapters AD-I2).
    pub fn table_with(batch: RecordBatch, alloc: &dyn Allocator) -> Result<Payload>;
    pub fn tensor(t: ManagedTensor) -> Result<Payload>;
    /// Caller asserts residency in `tier`. SAFETY: bytes must be addressable in `tier`.
    pub unsafe fn table_in(batch: RecordBatch, tier: Tier) -> Payload;
    pub unsafe fn tensor_in(t: ManagedTensor, tier: Tier) -> Payload;

    pub fn tier(&self) -> Tier;
    pub fn kind(&self) -> PayloadKind;
    /// CT-I3 accounting.
    pub fn bytes(&self) -> u64;
    pub fn rows(&self) -> u64;   // batch.num_rows() or shape[0] (1 for a 0-d tensor)

    /// Zero-copy view of a numeric column (or all columns of one dtype when
    /// `column` is None, as a 2-D tensor rows × columns) as a tensor. CT-I4.
    /// Errors: NotNumeric, HasNulls, MixedDTypes, NotContiguous.
    pub fn as_tensor(&self, column: Option<&str>) -> Result<ManagedTensor>;
    /// Zero-copy view of a contiguous tensor as one Arrow column named `name`:
    /// 1-D becomes a primitive array, 2-D becomes FixedSizeList(width). CT-I4.
    pub fn as_column(&self, name: &str) -> Result<arrow::array::ArrayRef>;
    /// Replace or append column `name` in a Table payload with `array` (same length).
    pub fn with_column(self, name: &str, array: arrow::array::ArrayRef) -> Result<Payload>;
}
```

### d.5 Morsel

```rust
/// Where a morsel's bytes came from. The lineage of every morsel in the run: a
/// morsel at stage `s` is exactly `kernels[1..=s]` applied to
/// `source.read(split, row_start..row_end)`, so any morsel can be re-obtained
/// from its origin (CT-I12). This is what Q0 eviction and run resume both rely on.
#[derive(Clone, Debug)]
pub struct Origin {
    pub split: SplitId,
    pub row_start: u64,   // inclusive, within the split
    pub row_end: u64,     // exclusive
    /// The node that read the split. `LOCAL_NODE` in every v1 run.
    pub node: NodeId,
}

/// Features the controller and the trace consume. Table fields are None for tensors and vice versa.
#[derive(Clone, Debug, Default)]
pub struct MorselFeatures {
    pub rows: u64,
    pub bytes: u64,
    pub column_bytes: Vec<u64>,
    pub mean_string_len: Option<f32>,
    pub null_ratio: Option<f32>,
    pub shape: Option<Vec<i64>>,
    pub dtype: Option<DType>,
}

pub struct Morsel {
    pub seq: Seq,
    pub stage: StageId,
    pub payload: Payload,
    pub bytes: u64,            // == payload.bytes(); kept for lock-free accounting reads
    pub origin: Origin,
    pub features: MorselFeatures,
}

impl Morsel {
    pub fn new(seq: Seq, stage: StageId, payload: Payload, origin: Origin) -> Morsel; // computes bytes and features
    /// Replace the payload (a kernel produced a new one); recomputes bytes and features, advances stage by one.
    pub fn with_output(self, payload: Payload) -> Morsel;
}
```

### d.6 Source

```rust
#[derive(Clone, Debug)]
pub struct Split {
    pub id: SplitId,
    pub rows: u64,
    pub uncompressed_bytes: u64,      // exact from metadata or estimated (see `estimated`)
    pub estimated: bool,
    pub column_bytes: Vec<u64>,       // per projected column; empty for tensors
    pub null_counts: Vec<Option<u64>>,
    pub sub_splittable: bool,         // can `read` take a RowRange narrower than the split?
}

#[derive(Copy, Clone, Debug)]
pub struct RowRange { pub start: u64, pub end: u64 }

pub trait Source: Send + Sync {
    fn schema(&self) -> SourceSchema;
    /// All splits, in delivery order, before any read. Called once per run; on a
    /// resumed run the result must equal the plan recorded in the manifest
    /// (checked by a digest over split ids and row counts, placement e.5; a
    /// mismatch is `Resume`).
    fn plan(&self) -> Result<Vec<Split>>;
    /// Read a split (or a row range of it) into buffers from `alloc` in `tier`,
    /// returning a resident payload. Runs on the reactor; must not block a worker.
    /// Deterministic for a given `(split, rows)` for the lifetime of the input
    /// (CT-I12): the same call returns the same rows in the same order, when
    /// `repeatable()` is true.
    /// The allocator is borrowed for the whole life of the future, not just the call.
    /// With the elided lifetime the future could not capture `alloc`, so a source that
    /// learns its sizes only after decoding (Parquet does: the footer gives bytes, not
    /// the arrow layout) had to do its whole read synchronously and return a resolved
    /// future, which loses exactly the concurrency `readahead.splits` exists to buy
    /// (E10, component 7, decided by the PM 2026-09-22).
    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Payload>>;
    /// True when `plan` and `read` satisfy CT-I12. A source that pulls from a
    /// one-shot iterator returns false; the runtime then disables Q0 eviction
    /// (stages Q0 instead) and refuses `resume` for the run. Default true, which
    /// every file-backed source satisfies by construction.
    fn repeatable(&self) -> bool { true }
}
```

### d.7 Kernel

```rust
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct Fingerprint(pub [u8; 32]);

pub enum KernelKind {
    Stateless,
    Stateful { max_instances: core::num::NonZeroUsize },
}

/// How a kernel's instances come back when a run is resumed from its manifest.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum ResumePolicy {
    /// A fresh `init` is enough: the state does not depend on which morsels were
    /// seen (a loaded model, a compiled expression). The default.
    #[default] Reinit,
    /// The state depends on morsels seen; `KernelState::checkpoint` returns it and
    /// `Kernel::restore` rebuilds it. The scheduler checkpoints every instance at
    /// each manifest write.
    Checkpoint,
    /// The kernel cannot be resumed; a resume attempt fails with `Resume` naming the stage.
    Forbid,
}

/// How a Python kernel's invocations run; reported per stage in the run report.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum GilState { FreeThreaded, Serialised }

#[derive(Clone, Debug, Default)]
pub struct KernelHints {
    pub expected_amplification: Option<f64>,
    pub uses_device_memory: bool,
    pub releases_gil: Option<bool>,
    pub preferred_rows: Option<u64>,
    pub resume: ResumePolicy,
    /// Bytes one instance's state is expected to hold (a model's weights); seeds the
    /// controller's state term before the first `footprint` is observed (RC f.3).
    pub state_bytes: Option<u64>,
}

/// Per-instance state a stateful kernel keeps between `apply` calls.
pub trait KernelState: Send {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any;
    /// Serialise the state for the run manifest. Called only when the kernel's
    /// `ResumePolicy` is `Checkpoint`; the default returns `Ok(None)`, which the
    /// scheduler treats as "nothing to save" for `Reinit` kernels and as an error
    /// for `Checkpoint` kernels (a `Checkpoint` kernel must return `Some`).
    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> { Ok(None) }
    /// Bytes this instance's state holds right now (a loaded model, an accumulator).
    /// `None` means unknown, and the default is `None`. Read by the scheduler after
    /// every `apply` into `TraceRecord::state_bytes`, so the controller can see
    /// state that grows with morsels seen and budget for it (RC f.3); a kernel
    /// whose state accumulates should implement it, because the linear model
    /// `a_k × bytes_in` does not describe it and the alternative is a breach.
    fn footprint(&self) -> Option<u64> { None }
}
/// Unit state for stateless kernels.
pub struct NoState;

pub struct InitCtx {
    pub instance: usize,             // 0..max_instances
    pub device: Option<DeviceId>,    // assigned device for uses_device_memory kernels
    pub alloc: std::sync::Arc<dyn Allocator>,
}

pub trait Kernel: Send + Sync + 'static {
    fn fingerprint(&self) -> Fingerprint;
    fn kind(&self) -> KernelKind;
    fn hints(&self) -> KernelHints { KernelHints::default() }
    /// What this kernel wants delivered.
    fn accepts(&self) -> PayloadSpec;
    /// Output schema for the given input schema; errors are plan-time errors.
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema>;
    /// Once per instance, on the worker that will own the instance.
    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>>;
    /// Rebuild an instance from bytes `KernelState::checkpoint` produced. Called
    /// instead of `init` on resume, only for `ResumePolicy::Checkpoint` kernels.
    /// The default refuses; a kernel that declares `Checkpoint` must override it.
    fn restore(&self, ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>> {
        let _ = (ctx, state);
        Err(MorunaError::Resume("kernel declares Checkpoint but does not implement restore".into()))
    }
    /// Synchronous; may take seconds; must not spawn threads that outlive the call;
    /// safe to call concurrently on different `state`s. Returns a resident payload.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload>;
}
```

### d.8 Sink

```rust
#[derive(Clone, Debug, Default)]
pub struct SinkSummary { pub rows: u64, pub bytes: u64, pub files: Vec<String> }

pub trait Sink: Send + Sync {
    fn open(&mut self, schema: &SourceSchema) -> Result<()>;
    fn accepts(&self) -> PayloadSpec;
    fn requires_order(&self) -> bool { false }
    /// Takes ownership; runs on the reactor; completes when the bytes are handed
    /// to the store's client (not necessarily committed; see `committed_seq`).
    /// `seq` is the morsel's sequence number, which the sink records with the
    /// output it lands in so a resume can identify uncommitted output.
    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>>;
    /// Exactly once, after the last `write` completed.
    fn finish(&mut self) -> Result<SinkSummary>;

    // Resume support. A sink that leaves the defaults in place is not resumable:
    // `Scheduler::apply_resume_point` calls `Sink::resume` first, and its `Resume` names the sink.

    /// Highest `seq` such that every morsel with a sequence number at or below
    /// it is committed (visible to a reader and safe against process loss) or was
    /// declared skipped through `skip`. `None` means nothing is committed yet, or
    /// the sink does not track commits.
    fn committed_seq(&self) -> Option<Seq> { None }
    /// The scheduler will never write `seq` (error policy `skip`); the sink counts
    /// it as committed for the watermark. Default: nothing, which is correct for
    /// a sink that does not track commits.
    fn skip(&self, seq: Seq) { let _ = seq; }
    /// Opaque sink state for the run manifest (for a file sink: committed file
    /// names and the next file index). Called at every manifest write, and once at
    /// startup: a resumable sink returns `Some` even before its first write (an
    /// empty file list), so `checkpoint()? == None` at startup is how the scheduler
    /// detects a non-resumable sink (SC f.11).
    fn checkpoint(&self) -> Result<Option<Vec<u8>>> { Ok(None) }
    /// Called instead of `open` on resume. The sink must discard any output that
    /// holds a sequence number above `committed_seq` (an uncommitted file, a
    /// multipart upload) and continue numbering after the checkpointed state.
    fn resume(&mut self, schema: &SourceSchema, state: &[u8], committed_seq: Option<Seq>) -> Result<()> {
        let _ = (schema, state, committed_seq);
        Err(MorunaError::Resume("sink does not support resume".into()))
    }
}
```

### d.9 Reactor

```rust
/// Resolves exactly once. `Completion<Buffer>` returns the same buffer the operation was given.
/// Implemented in this crate over `std` only (a `Mutex` + `Condvar` slot with a stored waker),
/// so that the reactor, the fakes and this crate agree on one type.
pub struct Completion<T> { /* private */ }
pub struct CompletionSender<T> { /* private */ }
impl<T: Send + 'static> Completion<T> {
    /// A linked pair. The reactor (or a fake) keeps the sender and resolves it once.
    pub fn channel() -> (CompletionSender<T>, Completion<T>);
    /// Blocking wait; only the scheduler's source and sink drives may call it (CT-I7, RE-I2).
    pub fn wait(self) -> Result<T>;
    /// Run `f` on the thread that resolves the completion, at resolution (or at once if
    /// already resolved). This is how the placement engine observes move completions
    /// without a thread of its own (placement g); `f` must be short and must not block.
    pub fn then(self, f: Box<dyn FnOnce(Result<T>) + Send + 'static>);
}
impl<T> core::future::Future for Completion<T> { type Output = Result<T>; /* ... */ }
impl<T> CompletionSender<T> { pub fn resolve(self, r: Result<T>); }

/// One end of a `copy`. A `Disk` endpoint names a registered staging segment range and
/// is legal only on the GDS rows of the reactor's copy table; everywhere else disk is
/// reached through `read_file` and `write_file`.
pub enum CopySrc { View(BufferView), Disk(SegmentRef) }
pub enum CopyDst { Buffer(Buffer), Disk(SegmentRef) }

pub trait Reactor: Send + Sync {
    /// Read `dst.len()` bytes from `path` at `offset` into `dst`. Direct IO when the
    /// buffer, offset and length are page-aligned and the host allows; buffered otherwise.
    /// Exactly `dst.len()` bytes or `Io`; see `read_file_opt` for short reads.
    fn read_file(&self, path: &std::path::Path, offset: u64, dst: Buffer) -> Completion<Buffer>;
    /// As `read_file`, but a read that ends at end-of-file returns the bytes read.
    fn read_file_opt(&self, path: &std::path::Path, offset: u64, dst: Buffer, allow_short: bool) -> Completion<(Buffer, usize)>;
    /// Write `src.len()` bytes at `offset`. The view keeps the bytes alive; on error the
    /// caller still holds them.
    fn write_file(&self, path: &std::path::Path, offset: u64, src: BufferView) -> Completion<()>;
    /// Ranged object read (S3-compatible, GCS, Azure, file://) into `dst`.
    fn read_object(&self, url: &str, offset: u64, dst: Buffer) -> Completion<Buffer>;
    fn write_object(&self, url: &str, src: BufferView) -> Completion<()>;
    /// DMA between tiers per the reactor's copy table (06 f.5): PinnedHost<->Device via
    /// the copy engine; Disk<->Device via GDS when present. Returns the destination
    /// buffer when the destination is a buffer. Never a CPU copy (G-I2).
    fn copy(&self, src: CopySrc, dst: CopyDst) -> Completion<Option<Buffer>>;
    /// Name a staging segment file so `SegmentRef`s can be resolved by `copy`'s Disk
    /// endpoints and so the reactor can cache its descriptor; `unregister_segment`
    /// closes the descriptor so an unlinked file's space is actually released.
    fn register_segment(&self, segment: u32, path: &std::path::Path) -> Result<()>;
    fn unregister_segment(&self, segment: u32);
    /// Remove an object or a local file. Resume needs it: a sink that is resumed must
    /// discard output above `committed_seq`, which means deleting the files it wrote
    /// and did not commit, and without this the resume path was complete only for a
    /// local prefix, where `std::fs` could be used directly (E10, component 8, decided
    /// by the PM 2026-09-22). Deleting what is not there is `Ok(())`, because a resume
    /// that runs twice must not fail the second time.
    fn delete_object(&self, url: &str) -> Completion<()>;
    /// Abandon an in-flight multipart upload so its parts are not billed forever. Same
    /// reason: a killed run leaves them, and only the reactor knows the upload id.
    fn abort_multipart(&self, url: &str, upload_id: &str) -> Completion<()>;
    /// Which direct paths this reactor selected at start (for the run report).
    fn paths(&self) -> IoPaths;
    /// Cancel what can be cancelled; every outstanding completion resolves within the
    /// longest single operation's duration (RE-I7). Idempotent.
    fn shutdown(&self);
}

/// Object-store metadata for sources' `plan`. Implemented by the reactor; a source
/// holds `Arc<dyn ObjectMetadata>` beside `Arc<dyn Reactor>` (the facade passes the
/// same reactor for both), so a test can supply metadata without a runtime.
pub trait ObjectMetadata: Send + Sync {
    fn head_object(&self, url: &str) -> Completion<ObjectMeta>;
    fn list_prefix(&self, url: &str) -> Completion<Vec<ObjectMeta>>;
}
#[derive(Clone, Debug)]
pub struct ObjectMeta { pub url: String, pub size: u64, pub last_modified_ns: Option<u64>, pub e_tag: Option<String> }

#[derive(Clone, Debug, Default)]
pub struct IoPaths { pub direct_io: bool, pub io_uring: bool, pub gds: bool, pub pinned: bool, pub rdma: bool /* always false without feature rdma */ }
```

Submission never blocks the caller: every method returns after enqueueing, and any concurrency limit is waited for on a reactor thread (RE-I6). A worker may therefore issue a reactor operation from inside `Placement::push` or `pop` (placement g) without violating preamble 4.1; what it may not do is `wait`.

### d.10 Placement

```rust
#[derive(Clone, Debug, Default)]
pub struct TierBudgets { pub device: [u64; 8], pub pinned_host: u64, pub host: u64, pub disk: u64 }

#[derive(Clone, Debug, Default)]
pub struct QueueStats {
    pub stage: StageId,
    pub bytes_by_tier: [u64; TIER_COUNT],  // indexed by Tier::index: Device(sum), PinnedHost, Host, Disk, Remote
    pub count: u64,
    pub misses: u64,                 // pops that waited on a move
    pub miss_wait_us: u64,
    pub demotions: u64,
    pub promotions: u64,
}
#[derive(Clone, Debug, Default)]
pub struct PlacementStats { pub queues: Vec<QueueStats>, pub in_flight_bytes: u64 }

/// Which node's memory a pop may be satisfied from. Reserved for the multi-node
/// extension; v1 callers pass `Any` and, with one node, the two are equivalent.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum Locality {
    /// Only entries resident on the calling node.
    Local,
    /// Any node; the engine may issue a remote move to satisfy the pop.
    #[default] Any,
}

/// Pieces of a checkpoint the placement engine does not own but records in the
/// manifest on behalf of the scheduler (placement e.5).
#[derive(Clone, Debug, Default)]
pub struct CheckpointExtras {
    /// `(stage, instance, bytes)` from `KernelState::checkpoint` for `Checkpoint` kernels.
    pub kernel_states: Vec<(StageId, usize, Vec<u8>)>,
    /// From `Sink::checkpoint`.
    pub sink_state: Option<Vec<u8>>,
    /// From `Sink::committed_seq` at the moment of the checkpoint.
    pub committed_seq: Option<Seq>,
    /// Where the source drive is: the index into the plan and the next row within
    /// that split, plus the next sequence number to assign.
    pub source_cursor: SourceCursor,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct SourceCursor { pub split_index: u32, pub row_offset: u64, pub next_seq: Seq }

/// What `Placement::restore` hands back so the scheduler can continue the run.
#[derive(Clone, Debug, Default)]
pub struct ResumePoint {
    pub extras: CheckpointExtras,
    /// Morsels the manifest knew about that have no disk copy and are not
    /// committed: the scheduler re-reads each from its origin and pushes it to
    /// Q0 with its original `seq` before restarting the source drive.
    pub to_recompute: Vec<(Seq, Origin)>,
}

pub trait Placement: Send + Sync {
    /// Never blocks. Ok(()) even if the queue is above high water; the caller
    /// consults `is_full` for admission.
    fn push(&self, stage: StageId, morsel: Morsel) -> Result<()>;
    /// Returns the head if it is resident in a tier satisfying `want` and `locality`;
    /// `Ok(None)` if the queue is empty or the head is not yet resident. Never blocks.
    fn pop(&self, stage: StageId, want: PayloadSpec, locality: Locality) -> Result<Option<Morsel>>;
    /// Blocks until the head is resident in a tier satisfying `want` and `locality`,
    /// or the queue is closed (returns Ok(None)). The only blocking call in the contract (CT-I7).
    /// The second element is the microseconds the caller waited on a move (0 when none);
    /// the scheduler writes it to the trace as `placement_miss_wait_us` (PL-I9).
    fn pop_blocking(&self, stage: StageId, want: PayloadSpec, locality: Locality) -> Result<Option<(Morsel, u64)>>;
    /// True if the head of `stage` is resident in a tier satisfying `want` (non-consuming;
    /// the scheduler's pick uses it so a worker never pops what it cannot run).
    fn peek_resident(&self, stage: StageId, want: PayloadSpec, locality: Locality) -> bool;
    /// Entries in state `Evicted` for `stage`, oldest first; the scheduler re-reads each
    /// from its origin and pushes the replacement with `replace`.
    fn evicted(&self, stage: StageId) -> Vec<(Seq, Origin)>;
    /// Replace an `Evicted` entry's bytes (same `seq`) in its original position.
    fn replace(&self, stage: StageId, morsel: Morsel) -> Result<()>;
    /// Cancel every in-flight move, stop planning, release reservations (preamble 4.3). Idempotent.
    fn shutdown(&self);
    /// The sink has committed every morsel with a sequence number at or below `seq`.
    /// Lets the engine forget the lineage of committed morsels (placement f.11).
    fn set_committed(&self, seq: Seq);
    /// Write the run manifest atomically to the staging directory (placement e.5).
    /// Returns the manifest path. An engine without a staging directory returns
    /// `Err(Resume("no staging directory"))`.
    fn checkpoint(&self, extras: &CheckpointExtras) -> Result<std::path::PathBuf>;
    /// Rebuild queues from a manifest written by `checkpoint`: entries with a disk
    /// copy come back `OnDisk`; the rest are listed in `ResumePoint::to_recompute`.
    /// Must be called before any `push`. Validates the manifest against the plan
    /// and kernel fingerprints it is given.
    fn restore(&self, manifest: &std::path::Path, plan: &[Split], fingerprints: &[Fingerprint]) -> Result<ResumePoint>;
    fn is_full(&self, stage: StageId) -> bool;
    /// Declare the consumer of a queue so promotion targets the right tier.
    fn set_consumer(&self, stage: StageId, want: PayloadSpec);
    fn set_budgets(&self, budgets: TierBudgets);
    fn set_water(&self, stage: StageId, tier: TierKind, low: u64, high: u64);
    fn set_staging(&self, stage: StageId, enabled: bool);
    fn set_promotion_window(&self, stage: StageId, morsels: u16);
    /// No more pushes will arrive for this stage; pops drain then return None.
    fn close(&self, stage: StageId);
    fn stats(&self) -> PlacementStats;
}
```

### d.11 Knobs (scheduler side)

```rust
#[derive(Clone, Debug)]
pub enum Knob {
    MorselTarget { stage: StageId, bytes: u64 },
    ActiveWorkers(u16),
    ReadAhead(u16),
    StagingTrigger { stage: StageId, on: bool },
    HighWater { stage: StageId, tier: TierKind, bytes: u64 },
    PromotionWindow { stage: StageId, morsels: u16 },
}

/// Implemented by the scheduler; the controller is the only caller (G-I5). The
/// scheduler forwards `StagingTrigger`, `HighWater` and `PromotionWindow` to the
/// placement engine (`set_staging`, `set_water(low = bytes / 2, high = bytes)`,
/// `set_promotion_window`) and keeps the others; the controller never calls a
/// placement setter except `set_budgets`. Values outside the preamble's ranges are
/// clamped by the scheduler and counted in `SchedulerStats::knob_clamps`.
pub trait Knobs: Send + Sync {
    fn set(&self, knob: Knob);
    fn snapshot(&self) -> KnobSnapshot;
    /// End the run with a diagnostic (the controller's third breach, state growth,
    /// sampler failure). The scheduler enters `Terminating` as for a kernel error.
    fn terminate(&self, diagnostic: MorunaError);
}

#[derive(Clone, Debug, Default)]
pub struct KnobSnapshot {
    pub morsel_target: Vec<(StageId, u64)>,
    pub active_workers: u16,
    pub read_ahead: u16,
    pub staging: Vec<(StageId, bool)>,
    pub high_water: Vec<(StageId, TierKind, u64)>,
    pub promotion_window: Vec<(StageId, u16)>,
}

/// Live scheduler counters the controller classifies bottlenecks from (RC f.6).
/// Implemented by the scheduler; read by the controller each tick.
pub trait StatsSource: Send + Sync { fn scheduler_stats(&self) -> SchedulerStats; }

#[derive(Clone, Debug, Default)]
pub struct StageStats { pub stage: StageId, pub tasks: u64, pub busy_ns: u64, pub errors: u32, pub skipped: u32, pub instances_live: u16 }

#[derive(Clone, Debug, Default)]
pub struct SchedulerStats {
    pub per_stage: Vec<StageStats>,
    pub workers_active: u16, pub workers_busy: u16,
    pub reads_in_flight: u16, pub writes_in_flight: u16, pub sink_concurrency: u16,
    pub source_exhausted: bool, pub seq_issued: Seq,
    pub committed_seq: Option<Seq>, pub checkpoints: u64, pub last_checkpoint_us: u64, pub resumed: bool, pub recomputed: u64,
    pub knob_clamps: u64,
}

/// The probe protocol (RC f.2) as the controller sees it. Implemented by the scheduler.
/// For stage 1 the scheduler issues one read of about `bytes` from the source cursor
/// (advancing it, so the morsel has the next `seq`); for stage k > 1 it pops the head of
/// Q(k−1), which is the previous stage's probe output. Runs on one worker with the rest
/// parked; the output is pushed downstream as normal, nothing is wasted.
pub trait Prober: Send + Sync { fn probe(&self, stage: StageId, bytes: u64) -> Result<ProbeResult>; }

#[derive(Clone, Debug)]
pub struct ProbeResult { pub bytes_in: u64, pub rows_in: u64, pub peak_delta: u64, pub dev_peak_delta: u64, pub wall_ns: u64, pub cpu_ns: u64 }

/// What happens after a kernel error (preamble `errors.policy`).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ErrorPolicy { Terminate, Skip, Budget(u32) }

/// Which decision function sizes morsels (preamble `sizer`).
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum SizerKind { #[default] Rule, Learned }

/// A clonable cancel flag set by the surface; polled by the scheduler's drives and
/// workers between tasks.
#[derive(Clone, Default, Debug)]
pub struct CancelToken { /* private: Arc<AtomicBool> */ }
impl CancelToken { pub fn new() -> Self; pub fn cancel(&self); pub fn is_cancelled(&self) -> bool; }

/// A hook the facade installs so the controller sees every trace record as it is
/// emitted (RC `on_record`); the scheduler calls it after `TraceSink::record`.
pub type RecordHook = std::sync::Arc<dyn Fn(&TraceRecord) + Send + Sync>;
```

### d.12 Limits and host profile (discovery side)

```rust
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum LimitSource { Cgroup, Os, Explicit }

#[derive(Clone, Debug)]
pub struct Device { pub id: DeviceId, pub total_bytes: u64, pub free_bytes: u64, pub name: String }

#[derive(Clone, Debug)]
pub struct Limits {
    pub memory_ceiling: u64,
    pub memory_kill: Option<u64>,
    pub cpu_quota: f64,
    pub page_bytes: usize,
    pub devices: Vec<Device>,
    pub source: LimitSource,
}

/// Guarantees a platform declares; `Unknown` means probe. Discovery replaces every
/// `Unknown` with `Probed(bool)`, so a consumer can tell a declared guarantee (which
/// must hold: a failure is an error, G-I7) from a probed one (which may fall back).
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum Guarantee { #[default] Unknown, Present, Absent, Probed(bool) }
impl Guarantee {
    /// Present or Probed(true).
    pub fn is_available(&self) -> bool;
    /// Present only.
    pub fn is_guaranteed(&self) -> bool;
}

#[derive(Clone, Debug, Default)]
pub struct HostProfile {
    pub huge_pages: Guarantee,
    pub memlock: Guarantee,
    pub io_uring: Guarantee,
    pub direct_io_staging: Guarantee,
    pub gds: Guarantee,
    pub rdma: Guarantee,
    pub staging_dir: Option<std::path::PathBuf>,
    /// `Present`: `staging_dir` outlives the process and the node (a persistent
    /// volume, a detachable disk), so a manifest written there can be resumed from
    /// another node. `Absent`: local only (resume works after a process restart on
    /// the same node, if the directory survived). `Unknown` is treated as `Absent`:
    /// durability across nodes is a fact about the platform that cannot be probed,
    /// only declared. Discovery does refuse a tmpfs or overlay staging directory
    /// declared `Present` (03 e.4, `Config` error).
    pub durable_staging: Guarantee,
}

/// Live resource sampling. Implemented by discovery; one instance per run, shared by
/// the controller (its tick) and the scheduler (before and after every `apply`).
/// Cheap: a few file reads or syscalls; interior mutability for its caches.
pub trait Sampler: Send + Sync {
    fn sample(&self) -> Sample;
    /// Reset the kernel's peak counter (cgroup v2 `memory.peak` is writable on
    /// kernels ≥ 6.x; otherwise the sampler tracks its own running peak and resets
    /// that), so a probe measures its own peak (RC f.2).
    fn reset_peak(&self);
}

/// One sample of live resource state; produced by discovery's sampler.
#[derive(Copy, Clone, Debug, Default)]
pub struct Sample {
    pub anon_bytes: u64,
    pub file_bytes: u64,
    pub peak_anon_bytes: u64,
    pub throttled_us: u64,
    pub device_used: [u64; 8],
    pub at_ns: u64,
}
```

### d.13 Trace

```rust
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Outcome { Ok, Error, Skipped, Probe }

/// One row per morsel per stage. Field order is the schema; see section e.5.
#[derive(Clone, Debug)]
pub struct TraceRecord {
    pub seq: Seq, pub stage: StageId, pub worker: u16,
    pub instance: u16,        // stateful instance index; u16::MAX for stateless
    pub t_start_ns: u64, pub t_end_ns: u64,
    pub rows_in: u64, pub bytes_in: u64, pub rows_out: u64, pub bytes_out: u64,
    pub tier_in: u8, pub tier_out: u8,
    pub feat_mean_string_len: f32, pub feat_null_ratio: f32, pub feat_column_bytes: Vec<u64>,
    pub knob_morsel_target: u64, pub knob_active_workers: u16, pub knob_read_ahead: u16,
    pub mem_anon_before: u64, pub mem_anon_peak: u64, pub dev_mem_peak: u64,
    pub cpu_time_us: u64, pub throttled_delta_us: u64,
    pub q_bytes_before: Vec<u64>, pub q_bytes_after: Vec<u64>,
    pub staging_bytes_delta: i64, pub placement_miss_wait_us: u64,
    pub state_bytes: u64,     // KernelState::footprint after apply; 0 when None or stateless
    pub sizer: u8,            // 0 rule, 1 learned
    pub outcome: Outcome, pub error: Option<String>,
}

impl TraceRecord {
    pub fn arrow_schema() -> arrow::datatypes::SchemaRef;
    /// The canonical field list "name:type,..." in schema order; the string the hash is over.
    pub const SCHEMA_FIELDS: &'static str;
    /// BLAKE3 of `SCHEMA_FIELDS` (CT-I8), pinned as a literal because BLAKE3 cannot be evaluated
    /// in a constant expression (blake3 1.8.7 has no const hashing). CT-T9 recomputes the digest
    /// from `SCHEMA_FIELDS` and asserts this constant, so a schema change that does not update
    /// the literal fails the test rather than passing silently, which is what CT-I8 asks.
    pub const SCHEMA_HASH: [u8; 32];
    /// The digest recomputed from `SCHEMA_FIELDS` at run time; equal to `SCHEMA_HASH`.
    pub fn schema_hash() -> [u8; 32];
}

pub trait TraceSink: Send + Sync {
    /// Bounded, non-blocking beyond a channel push; drops nothing (backpressure is on the writer, not the caller).
    fn record(&self, r: TraceRecord);
    fn flush(&self) -> Result<()>;
}

/// Read-side of the trace for the controller; implemented by the trace writer.
pub trait TraceTail: Send + Sync {
    /// The last `n` records of `stage`, oldest first, from the in-memory chunks.
    fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord>;
}
```

### d.14 Errors

```rust
#[derive(thiserror::Error, Debug)]
pub enum MorunaError {
    #[error("plan: {0}")] Plan(String),
    #[error("source split {split}: {msg}")] Source { split: SplitId, msg: String },
    #[error("kernel stage {stage} morsel {seq}: {msg}")] Kernel { stage: StageId, seq: Seq, msg: String, features: Option<MorselFeatures> },
    #[error("sink: {0}")] Sink(String),
    #[error("alloc {bytes} bytes in {tier:?}: budget {budget} in use {in_use}")] Alloc { bytes: u64, tier: Tier, budget: u64, in_use: u64 },
    #[error("io {op} {target}: {msg}")] Io { op: &'static str, target: String, msg: String },
    #[error("budget: morsel {seq} stage {stage} footprint {footprint} exceeds budget {budget}")] Budget { seq: Seq, stage: StageId, footprint: u64, budget: u64, features: MorselFeatures },
    #[error("staging: {0}")] Staging(String),
    #[error("convert: {0}")] Convert(ConvertError),
    #[error("config {name}: {msg}")] Config { name: &'static str, msg: String },
    #[error("cancelled")] Cancelled,
    /// A manifest could not be written, read, validated or applied; the message
    /// names the manifest path and the first mismatch.
    #[error("resume: {0}")] Resume(String),
    /// A reserved path (`rdma`, `remote`) was reached in a build that does not
    /// implement it. Always a bug or a misconfiguration, never a runtime condition.
    #[error("unsupported: {0}")] Unsupported(&'static str),
}

#[derive(thiserror::Error, Debug)]
pub enum ConvertError {
    #[error("column {0} is not numeric")] NotNumeric(String),
    #[error("column {0} has nulls")] HasNulls(String),
    #[error("columns have mixed dtypes")] MixedDTypes,
    #[error("tensor is not contiguous")] NotContiguous,
    #[error("tensor rank {0} not convertible (need 1 or 2)")] Rank(usize),
    /// A DLPack capsule whose major version this build does not implement. Added 2026-09-22
    /// (issue #13) because d.4 requires `from_dlpack` to refuse a foreign version and no
    /// variant said so; `Plan` was wrong (this is not a plan-time schema fact) and
    /// `NotNumeric` was a misuse of a column error for a capsule header.
    #[error("dlpack major version {found} is not {expected}")] Version { found: u32, expected: u32 },
}
```

### d.15 Test fakes (`moruna-testkit`)

The testkit is built by this component's agent in wave 0 (preamble 6.4) because every later component's tests depend on it and CT-T13 requires it. It implements every trait above with the knobs below; a component SDD's test names a fake and a knob from this list and nothing else.

| Fake | Implements | Knobs (builder methods) | Observables |
|---|---|---|---|
| `FakeAllocator` | `Allocator` | `with_limit(tier, bytes)`, `pinned(bool)`, `page_bytes(n)`, `fail_next(n)` | `allocations_total`, `in_use(tier)`, `payload_copies()` and `boundary_copies()` (what `note_payload_copy` and `note_boundary_copy` were told, added 2026-09-22: `stats()` reported a hardcoded zero and the counters had no observable, so no test could prove the one decode copy G-I2 allows), `AllocStats`; buffers are real heap allocations tagged with the requested tier so `Payload::table` tier inference, `into_arrow_buffer` and `BufferView::of_arrow` work |
| `FakeReactor` | `Reactor` | `with_latency(Duration)`, `fail_next(op: OpKind, n)`, `cancel_on_shutdown(bool)`, in-memory files keyed by path (`read_file`/`write_file` copy to and from a `Vec<u8>` per path) | `ops() -> Vec<OpRecord { kind, path_or_url, offset, len, src_tier, dst_tier, t_submit, t_resolve }>`, `in_flight()`, `shutdown_calls`, `paths()` returns whatever `with_paths(IoPaths)` set, `file(path)` and `object(url)` return the bytes the fake holds (the object side added 2026-09-22, so a sink's writes can be read back). The fake holds files in memory, so a component whose commit is an `fsync` and a rename through `std::fs` cannot observe it here and says so in its own tests |
| `FakePlacement` | `Placement` | `with_pressure(stage, evict_after_bytes)` (entries beyond the byte count on a stage-0 queue become `Evicted`), `with_delay(Duration)` (a pop of a fresh entry waits, counted as a miss), `with_manifest_store()` (in-memory manifests keyed by path so `checkpoint`/`restore` round-trip across engine instances) | `pushed(stage) -> Vec<Seq>`, `popped(stage)`, `committed()`, `manifests_written()`, `budgets_set() -> Vec<TierBudgets>` (every `set_budgets` argument in call order), `shutdown_calls` |
| `FakeSource` | `Source` | `splits(n, rows_each, bytes_each)`, `schema(SourceSchema)`, `sub_splittable(bool)`, `repeatable(bool)`, `fail_split(id)` | `reads() -> Vec<(SplitId, Option<RowRange>)>`; deterministic content (row i of split s has value `s * 1_000_000 + i`) |
| `FakeSink` | `Sink` | `commit_every(n)` (commits in blocks of n sequence numbers, so `committed_seq` advances in steps), `resumable(bool)`, `fail_at(seq)`, `latency(Duration)`, `requires_order(bool)` | `written() -> Vec<Seq>`, `skipped()`, `committed_seq()`, `open_calls`, `resume_calls`, `finish_calls`, `shutdown_calls` |
| `FakeKernel` | `Kernel` | `amplification(f64)` (allocates `a × bytes_in` from the global allocator during `apply`, freed on return), `latency(Duration)`, `stateful(instances, state_bytes)`, `resume(ResumePolicy)`, `fail_on(applies)`, `panic_on(applies)`, `grow_state_by(bytes)` per apply | `applies() -> Vec<(usize /* apply index */, usize /* instance */, std::thread::ThreadId)>`, `init_calls`, `restore_calls`, `checkpoint_calls` |
| `FakeSampler` | `Sampler` | `scripted(Vec<Sample>)` (returns the sequence, then repeats the last), `live()` (reads the real process) | `samples_taken`, `peak_resets` |
| `FakeTrace` | `TraceSink`, `TraceTail` | `capacity(n)` | `records() -> Vec<TraceRecord>`, `flush_calls`, `finish_calls` |
| `FakeKnobs` | `Knobs`, `StatsSource`, `Prober` | `stats(SchedulerStats)`, `probe_result(stage, ProbeResult)` | `writes() -> Vec<Knob>`, `terminated() -> Option<MorunaError>` |

`fail_on` and `panic_on` count applies, not sequence numbers, and `applies()` reports the apply index rather than a `Seq`, because `Kernel::apply` receives a `Payload` and no morsel: a kernel never learns its position in the run. That is deliberate and not a gap to close. A kernel that knew its sequence number could not be the same code inside a Polars expression or a DataFusion function, which S7 requires, and nothing in the runtime needs it: the scheduler holds the seq, attaches it to `MorunaError::Kernel` when an apply fails (CT-I10) and writes it to the trace, so every per-morsel assertion a component test wants is available from the scheduler's own output. A test that wants "morsel 7 fails" therefore asserts that the trace records an `Error` or `Skipped` outcome for whichever sequence numbers failed and that the sink's `skipped()` equals exactly those, which is the property that matters, rather than naming a number the fake cannot see (decided by the PM 2026-09-22 on the F0.3 agent's report, issue #12; SC-T7, SC-T14 and PY-T2 are worded that way).

Every fake records a `shutdown_calls` counter wherever the trait it implements has a `shutdown` method, so a test can assert that shutdown ran exactly once. `Sink` and `TraceSink` have no `shutdown` in the contract, so `FakeSink::shutdown_calls` and `FakeTrace::finish_calls` count the fakes' own inherent `shutdown()` and `finish()`, which the facade calls; a test naming them is asserting about the facade's lifecycle (preamble 4.3), not about a trait method.

Also in the testkit: the data generator and the benchmark kernels of preamble 6.5 are not here; they belong to the `bench` agent (wave 1). `moruna-testkit` depends on `moruna-kernel` only.

## e. Data model, formats and state machines

### e.1 Payload tier state machine

A payload's tier changes only through the placement engine (promotion or demotion) or a kernel producing a new payload. A run has exactly one host tier, `PinnedHost` when `Allocator::is_pinned()` and `Host` otherwise (AR pins all or nothing); "host tier" below means whichever it is, and no move between `Host` and `PinnedHost` exists. This table is the single authority; the reactor's copy table (06 f.5) and the placement move table (09 e.4) cite it and add nothing.

| From | To | Mechanism | Legal when |
|---|---|---|---|
| host tier | Device(d) | copy engine (`cuMemcpyHtoDAsync`) when pinned; through the reactor's pinned bounce buffer when not (a fallback, counted) | `cuda` |
| Device(d) | host tier | copy engine (`cuMemcpyDtoHAsync`); bounce when unpinned | `cuda` |
| host tier | Disk | `write_file`, direct IO when page-aligned (alignment, not pinning, is what direct IO needs); buffered otherwise | always |
| Disk | host tier | `read_file`, same rule | always |
| Disk | Device(d) | `copy(Disk, Buffer)` by GDS; else Disk → host tier → Device | `gds`, else two-step |
| Device(d) | Disk | Device → host tier → Disk (GDS write not used in v1) | always |
| host tier | Remote(n) | reserved, `rdma` | never in v1 (`Unsupported`) |
| Remote(n) | host tier | reserved, `rdma` | never in v1 (`Unsupported`) |

Illegal in every build: `Host ↔ PinnedHost` (they do not coexist), `Remote ↔ Disk` (the owning node moves its own bytes), `Device ↔ Device` across devices in v1. Attempting an illegal transition is `MorunaError::Staging` and is a bug in the placement engine, not a runtime condition.

### e.2 Buffer provenance

Every `Buffer` carries an arena token; `Payload::table` reads the token from the batch's first buffer to infer the tier (all buffers of one batch must share a tier, checked, `Plan` error otherwise). A batch whose buffers are not arena-owned (a kernel allocated with the global allocator) is accepted with tier `Host` and counted in `AllocStats.payload_copies_total` only if a component later copies it into the arena; the adapters SDD decides when that copy happens (at the kernel boundary, once).

### e.3 Type mapping for conversions

| Arrow type | DType | Notes |
|---|---|---|
| Int8/16/32/64 | I8/I16/I32/I64 | |
| UInt8/16/32/64 | U8/U16/U32/U64 | |
| Float16 | F16 | |
| Float32 / Float64 | F32 / F64 | |
| Boolean | (not convertible) | Arrow bitmap vs one byte per value; `NotNumeric` |
| FixedSizeList(T, n), T above | as T, 2-D shape [rows, n] | requires zero nulls at both levels |
| anything else | (not convertible) | `NotNumeric` |

`as_tensor(None)` on a table with k numeric columns of one dtype and no nulls returns a 2-D tensor of shape `[rows, k]` **only if** the columns are already adjacent in one buffer, which Arrow does not guarantee; otherwise it returns `NotContiguous` and the caller (adapters) requests a single column or a `FixedSizeList`. This is stated so no implementer "helpfully" copies.

### e.4 Moruna aligned binary format (`MRB1`)

Used by `TensorSink`, `TensorSource`, and staging segments for tensor payloads. Little-endian throughout.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 4 | magic | ASCII `MRB1` |
| 4 | 2 | version | 1 |
| 6 | 1 | dtype | `DType` code: I8=0, I16=1, I32=2, I64=3, U8=4, U16=5, U32=6, U64=7, F16=8, BF16=9, F32=10, F64=11, Bool=12 |
| 7 | 1 | ndim | 0..=8 |
| 8 | 8 × ndim | shape | i64 each |
| 8 + 8·ndim | 8 | data_offset | absolute byte offset of the payload; the smallest multiple of `page_bytes` (4096 default) that is ≥ header end |
| data_offset | element_count × item_size | payload | row-major, contiguous |
| after payload | to next 64 | padding | zeros |

A reader validates magic, version, ndim, that `data_offset` is a multiple of 4096 and ≥ the header end, and that the file length ≥ `data_offset + payload_len`; any failure is `MorunaError::Io { op: "mrb1" }`. An unknown version is rejected, not skipped.

### e.5 Trace record Arrow schema

Field order and types are exactly as in `TraceRecord` (d.13): unsigned integers as `UInt64`/`UInt16`/`UInt8`, floats as `Float32`, vectors as `List<UInt64>`, `outcome` as `UInt8` (Ok=0, Error=1, Skipped=2, Probe=3), `error` as nullable `Utf8`. `SCHEMA_HASH` is BLAKE3 over `"seq:u64,stage:u16,..."` in that order; test CT-T9 pins the value after the agent computes it once.

### e.6 Fingerprint

`Fingerprint::compute(identity: &str, config: &[u8]) -> Fingerprint` is BLAKE3 over `identity.len() as u64 LE || identity || config`. For a Rust kernel, `identity` is the crate name, version and type path; for a Python kernel, the adapters SDD defines it (qualified name plus source hash). Two kernels with equal fingerprints are assumed to have equal amplification behaviour; that is a profile-store assumption, not a correctness one.

### e.7 Page-aligned Arrow IPC record encoding

Used by staging segments (09 e.3) and by `ArrowIpcSink` (08 e.3); defined here, as `MRB1` is, because two components read and write it. It is the Arrow IPC stream framing with one deviation the format permits: every body buffer of a record batch is placed at a multiple of `page_bytes` rather than of 8, so each can be written from and read into an arena buffer by direct IO or GDS without a copy. One record:

| Piece | Content | Placement |
|---|---|---|
| framing | the IPC `Schema` message and the `RecordBatch` message (flatbuffers), continuation marker and lengths as in the IPC stream format, with each `Buffer` entry's offset rewritten to the page-aligned body layout below | one arena buffer, page-rounded, zero-padded; under 64 KiB in practice, `Staging` if over 1 MiB |
| body | each Arrow buffer of the batch, in schema order, each starting at the next page boundary after the previous | written from the batch's own buffers through `BufferView::of_arrow`; no copy |
| tail | zero padding to the next page boundary | not written; implied by the next record's offset |

`moruna_kernel::ipc` provides `encode_framing(batch, page_bytes, base_offset, alloc: &dyn Allocator) -> Result<(Buffer /* framing */, Vec<(usize /* body offset */, arrow::buffer::Buffer)>)>` (allocating only the framing buffer from `alloc`) and `decode(buf: arrow::buffer::Buffer, page_bytes) -> Result<RecordBatch>`, whose arrays point into `buf` (arrow's IPC reader over an aligned buffer; verified by pointer comparison in CT-T18). A reader validates the continuation marker, message lengths and that every body offset is a page multiple; any failure is `Io { op: "ipc" }`.

## f. Algorithms and policies

**f.1 Byte accounting.** `Payload::bytes`: for `Table`, `batch.get_array_memory_size() as u64`; for `Tensor`, `element_count() * item_size()` ignoring strides (a non-contiguous view is charged as if contiguous; conservative). `Morsel::new` and `with_output` call it once and cache.

**f.2 Feature extraction.** `MorselFeatures::from_payload`: rows; bytes; per-column `get_array_memory_size` for tables; `mean_string_len` as total string bytes over string value count across all `Utf8`/`LargeUtf8` columns (None if there are none); `null_ratio` as total nulls over total cells; for tensors, `shape` and `dtype`. Cost must be O(columns), never O(rows): string totals come from the offsets buffer's last value, not by iterating.

**f.3 `as_tensor(Some(col))`.** Locate the column; check e.3; check null count == 0; build a `ManagedTensor` over the array's values buffer with `data_ptr = buffer.as_ptr() + offset*item_size`, shape `[len]` (or `[len, n]` for `FixedSizeList`), no strides, tier = payload tier, and a deleter that holds a clone of the `ArrayRef` so the bytes outlive the view. No allocation of payload size (CT-I4).

**f.4 `as_column(name)`.** Require contiguous, rank 1 or 2, dtype in e.3 and not `BF16`/`Bool`; build an Arrow `Buffer` from the tensor's pointer with a custom deallocation that holds the `ManagedTensor`; rank 1 gives a primitive array, rank 2 gives `FixedSizeList(width = shape[1])`. No allocation of payload size.

**f.5 `PayloadSpec::check`.** `Table` on `Table`: Ok. `Tensor` on `Tensor`: Ok. `Either`: Ok. `Tensor` on `Table`: every column must satisfy e.3 with nullability declared false in the schema (a nullable field fails at plan time even if it happens to contain no nulls; that is deliberate). `Table` on `Tensor`: Ok only if rank ≤ 2 and dtype convertible.

**f.6 `Tier::rank` and `Tier::index`.** Rank (promotion order): Device = 4, PinnedHost = 3, Host = 2, Remote = 1, Disk = 0; a remote copy outranks a disk copy because an RDMA read is faster than an NVMe read, and the placement engine promotes from the highest-ranked copy it holds. Index (array position, stable for the trace and stats): Device = 0, PinnedHost = 1, Host = 2, Disk = 3, Remote = 4. The two orders differ on purpose: rank is a policy and may change; index is a schema and may not (CT-I8).

## g. Concurrency within the component

The crate has no threads. All types are `Send`; `Payload`, `Morsel`, `Buffer`, `ManagedTensor` are `Send` but not `Sync` (single owner at a time; the scheduler moves them between threads). Trait objects are `Send + Sync`. `Completion<T>` is `Send`. `ManagedTensor`'s deleter runs on whichever thread drops it; implementers of adapters must make Python deleters attach to the interpreter first (that is the adapters SDD's concern, noted here so the wrapper exposes `set_deleter_hook`).

## h. Behaviour

**Normal path.** A source builds `Payload::table(batch)` from arena buffers; `Morsel::new` computes bytes and features; the placement engine tags tiers as it moves bytes; a kernel receives the morsel, calls `as_tensor` if it wants a tensor view, produces a payload, returns it; `with_output` recomputes; the sink consumes.

**Edge cases.** Empty batch (0 rows): valid; bytes may be non-zero (buffers exist); features have `rows = 0`. Zero-dimensional tensor: element count 1. A `FixedSizeList` with list size 0: `NotNumeric`. A batch with mixed-tier buffers: `Plan` error at construction. `as_tensor` on `Tier::Disk`: `Convert(NotContiguous)` is wrong; it is `Staging("payload not resident")`. `split_at` at 0 or at len: allowed, one half is empty.

**Failures.** All errors are values; nothing in this crate panics on input. `Buffer::as_ref` on a device buffer panics because it is a programming error, not an input condition, and the panic message names the tier. Rust has no per-value trait implementation, so `AsRef<[u8]>` and `Deref` are implemented for every `Buffer` and both panic on a `Device` buffer rather than being absent for it; section l's "no `impl Deref` on device buffers" means no device buffer may be dereferenced, not that the implementation can be withheld from the type (E10, resolved by the PM 2026-09-22).

## i. Configuration

Rows owned here: `morsel.alignment` (compile constant `ALIGNMENT`), `page.bytes` (exposed through `Allocator::page_bytes`, set by discovery). No runtime configuration.

## j. Observability

None emitted. This crate defines `TraceRecord` and `AllocStats`; it emits nothing.

## k. Tests

Unit tests in `crates/moruna-kernel/tests/`, named `ct_tN_*`.

**CT-T1 payload_variants.** A `match` on `Payload` with two arms compiles with no wildcard (a compile-time assertion via a helper function). Proves CT-I1.

**CT-T2 tier_inference.** `Payload::table` on a batch built from `FakeAllocator` buffers tagged `PinnedHost` yields `Tier::PinnedHost`; mixed-tier buffers yield `Plan`. Proves CT-I2.

**CT-T3 bytes_accounting.** For a generated batch, `bytes == get_array_memory_size`; for tensors of each dtype and shapes `[0]`, `[]`, `[3,4]`, `bytes == count × item_size`. Proves CT-I3.

**CT-T4 as_tensor_zero_copy.** For each numeric type in e.3, `as_tensor` returns a tensor whose `data_ptr` equals the array's values pointer and `FakeAllocator.allocations_total` is unchanged. Proves CT-I4.

**CT-T5 as_column_zero_copy.** The reverse of T4 for rank 1 and rank 2; pointer equality; round trip `as_column(as_tensor(x)) == x` by value. Proves CT-I4.

**CT-T6 conversion_errors.** Nullable column → `HasNulls` at run time and `check` → `Plan` at plan time; Boolean → `NotNumeric`; strided tensor → `NotContiguous`; rank 3 → `Rank(3)`. Proves CT-I5.

**CT-T7 send_sync.** Static assertions that every trait object is `Send + Sync` and every value type is `Send`. Proves CT-I6.

**CT-T8 amb1_roundtrip.** Write every dtype and ndim 0..8 through a reference writer in the test, read back, byte-equal; corrupt magic, version, data_offset alignment, truncated payload each rejected with `Io { op: "mrb1" }`. Proves e.4.

**CT-T9 trace_schema_hash.** `TraceRecord::SCHEMA_HASH` equals the pinned constant; `arrow_schema()` field names and types match d.13 in order. Proves CT-I8.

**CT-T10 fingerprint_stability.** Same inputs → same fingerprint across processes (golden value); one byte of config change → different fingerprint. Proves e.6.

**CT-T11 features_cost.** (reference host, E1, for the timing; the structural half runs anywhere) `MorselFeatures::from_payload` on a 10 M-row string batch runs in under 1 ms on the reference host, provisional with the host name elsewhere; on every host the test also proves the O(columns) property structurally: the batch is built over a values buffer whose bytes are never read (a `FakeAllocator` buffer left uninitialised is fine) and the string total equals the offsets buffer's last value. Proves f.2.

**CT-T12 no_runtime_deps.** `cargo tree -p moruna-kernel` contains none of tokio, cudarc, pyo3, parquet, object_store. Proves the boundary in section a and S7.

**CT-T13 fakes_compile.** `moruna-testkit` implements every trait in d.3 to d.13 with the knobs in d.15, and its tests exercise every method and every knob once. Proves the contract is implementable.

**CT-T20 arena_token_not_leaked.** `split_at` and `into_arrow_buffer` consume a `Buffer` through `ManuallyDrop`, so each must move the arena token out rather than clone beside a field nothing will drop: after both halves of a split drop, and after the Arrow buffer drops, `Arc::strong_count` of the arena handle is what it was before. Rationale: the token is what keeps the arena's region mapped, and one leaked per morsel left a 1 GiB region resident for the life of the process, so a second `Runtime::run` was given no budget at all (PM, 2026-09-22, on the first end-to-end run; d.3, 12 f.1).

**CT-T14 reserved_variants_matched.** A `match` on `Tier` with five named arms, and a `match` on `StagingCodec` with one named arm, compile with no wildcard (the same helper technique as CT-T1); `Tier::Remote(..).is_resident() == false`; `rank` and `index` return the f.6 values; `LOCAL_NODE == NodeId::default()`. A repository-level lint (`tools/lint/no_tier_wildcard.sh`, added by this component) greps every crate for `match` expressions on a `Tier` or a `StagingCodec` with a `_ =>` arm and fails CI on a hit. Proves CT-I11.

**CT-T16 completion_channel.** `Completion::channel`; `resolve` on another thread wakes a `wait`, a `.await`, and a `then` callback, each exactly once; `then` registered after resolution runs at once; a dropped sender resolves with `Cancelled`. Proves d.9.

**CT-T17 buffer_view.** `BufferView::of_arrow` over an arrow buffer sliced from `into_arrow_buffer` reports the arena's tier and pointer; over a heap buffer returns `Staging`; `of_tensor` matches `data_ptr`; dropping the view while the source lives changes nothing; dropping the source while the view lives keeps the bytes valid (owner held). Proves d.3.

**CT-T18 ipc_page_aligned.** `encode_framing` then `decode` over a page-rounded copy round-trips 20 generated batches (all e.3 types plus strings and lists); every body offset is a page multiple; decoded arrays' data pointers lie inside the input buffer (no copy); a corrupted body offset is `Io { op: "ipc" }`. Proves e.7.

**CT-T19 tensor_from_buffer.** `ManagedTensor::from_buffer` over an `MRB1` body: `data_ptr == buf.host_ptr() + offset`, shape and dtype as given; a short buffer or misaligned offset is `Convert`/`Io`, not a panic. Proves d.4.

**CT-T15 resume_defaults.** A kernel with the default `restore` returns `Resume`; a `KernelState` with the default `checkpoint` returns `Ok(None)`; a sink with the default `resume` returns `Resume` and `committed_seq() == None`; `ResumePolicy::default() == Reinit`. Proves the resume defaults are refusals, not silent successes (l, anti-patterns).

## l. Implementation notes for the agent

Files: `src/lib.rs` (re-exports), `src/ids.rs` (d.1), `src/tier.rs` (d.2, f.6), `src/buffer.rs` (d.3; the `Buffer` type's arena token is an `Arc<dyn ArenaHandle>` trait object defined here with `fn release(&self, ptr, len, tier)` so the arena crate can implement it without a circular dependency), `src/view.rs` (d.3 `BufferView`), `src/payload.rs` (d.4, f.1, f.3, f.4, f.5), `src/tensor.rs` (the `dlpark` wrapper, `from_buffer`), `src/morsel.rs` (d.5, f.2), `src/source.rs`, `src/kernel.rs`, `src/sink.rs`, `src/reactor.rs` (d.9 traits, `ObjectMetadata`, `ObjectMeta` and endpoints), `src/completion.rs` (d.9 `Completion`, std only), `src/placement.rs`, `src/knobs.rs` (d.11 including `StatsSource`, `Prober`, `CancelToken`, `RecordHook`), `src/limits.rs` (d.12 including `Sampler`), `src/trace.rs` (d.13, e.5, `TraceTail`), `src/mrb1.rs` (e.4, reader and writer over `&[u8]`/`&mut [u8]` only; no IO), `src/ipc.rs` (e.7; `arrow` with the `ipc` feature), `src/error.rs`, `src/fingerprint.rs`. The testkit (d.15) is a sibling crate `crates/moruna-testkit` built in the same pull request.

`unsafe` is permitted only in `tensor.rs` (DLPack pointer handling, `from_buffer`), `payload.rs` (building Arrow buffers over foreign pointers and the `*_in` constructors), `view.rs` (constructing a view over an owner's bytes) and `buffer.rs` (pointer arithmetic in `split_at`); test code in any crate may use `unsafe` to construct a state a test needs (E9 exempts tests); each block carries `// SAFETY:` naming the invariant (CT-I2 or the DLPack contract).

`BoxFuture<'a, T>` is `core::pin::Pin<Box<dyn core::future::Future<Output = T> + Send + 'a>>`, defined in `lib.rs`; do not depend on `futures`.

Anti-patterns: no `Vec<u8>` copies of payload bytes anywhere in this crate; no `impl Deref<Target=[u8]> for Buffer` on device buffers; no default implementations that silently do less than the trait documents (a default may only delegate or return a constant); no `pub` fields on `Buffer` or `ManagedTensor`.

Pin dependency versions (preamble 6.2) and record them in the preamble's table in the same pull request. Verify `dlpark` supports DLPack 1.0's versioned `DLManagedTensorVersioned`; if it does not, wrap the unversioned struct and record the limitation in section m of this document as an escalation.

Environment facts to verify before starting: `cargo --version` ≥ the 2024-edition minimum; `blake3` crate available (used for fingerprint and schema hash; add to preamble 6.2 if not present there: it is, as `blake3`, purpose "hashing", used by 1, 9 and 11).

## m. Open items

None. (Four E10 items the wave 0 agent raised, issues #5 to #8, were resolved by the PM on 2026-09-22 and are written into d.3, d.4, d.13 and h above: `SCHEMA_FIELDS` beside a pinned `SCHEMA_HASH` with `schema_hash()` to recompute it, because BLAKE3 has no const evaluation; `dlpark::versioned::Dlpack` as the DLPack type, because `dlpark` 0.8.0 has no `ManagedTensor`; `ArenaHandle` and `unsafe Buffer::from_raw`, without which component 2 cannot construct a buffer at all; and `AsRef`/`Deref` implemented for every buffer and panicking on `Device`, because Rust has no per-value implementation. E1 and E2 in the preamble cover reference hardware and version pinning. The additions requested by components 5 and 9, `AllocStats.boundary_copies_total`, `Allocator::contains` and `Buffer::into_arrow_buffer`, are already in d.3. The multi-node reservations, `NodeId`, `RunId`, `Tier::Remote`, `RemoteRef`, `Locality`, `TIER_COUNT`, the staging reservation `StagingCodec`, the state seam `KernelState::footprint` with `TraceRecord::state_bytes`, and the resume seams, `ResumePolicy`, `KernelState::checkpoint`, `Kernel::restore`, `Sink::{committed_seq, checkpoint, resume}`, `Placement::{set_committed, checkpoint, restore}`, `CheckpointExtras`, `SourceCursor`, `ResumePoint`, `HostProfile::durable_staging`, `MorunaError::{Resume, Unsupported}`, are in d.1 to d.14 by decision of the architecture document's sections 10 and 11; they are not open.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| D9 | CT-I1, CT-I6 | CT-T1, CT-T7 |
| D10 | CT-I2, e.1 | CT-T2 |
| S7 | section a boundary | CT-T12, CT-T13 |
| S9 | CT-I8, e.5 | CT-T9 |
| S13 | CT-I4, CT-I5 | CT-T4, CT-T5, CT-T6 |
| S16, D12 | CT-I11 | CT-T14 |
| S17, D13, D5 | CT-I12, resume defaults | CT-T15 |
| G-I2 | CT-I4 | CT-T4, CT-T5 |
| G-I6 | CT-I1 | CT-T1 |
| G-I8 | CT-I10 | (exercised by RC and PL tests) |

## o. Deferred (post-v1)

None.
