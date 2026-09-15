# Amoru SDD 06: IO reactor (`amoru-reactor`)

**Document type:** software design document, component 6 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` sections 4.2 (threads), 5.2 (local direct IO), 5.6 (tier moves), 6 (hosting); criteria S14, S15; global invariants G-I2, G-I7
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.9 (`Reactor`, `Completion`, `IoPaths`), d.3 (`Buffer`), d.2 (`Tier`)
**Component location:** `crates/amoru-reactor`, Rust; features `uring`, `cuda`, `gds`
**Consumes:** arena (2), discovery (3). **Consumed by:** sources (7), sinks (8), placement (9)

**Decisions worth your eye:** (1) a tokio runtime is the reactor, with io_uring used for file IO through a dedicated submission thread rather than tokio-uring, so the same runtime serves object storage and files; (2) the reactor selects every path once at start from the host profile and never re-probes; (3) object-store reads land in the arena with one copy from the HTTP body, which is the ingress boundary, counted separately from payload copies.

---

## a. Purpose and boundary

The reactor is where every byte enters and leaves the process and where every move between tiers is issued. It owns the threads that talk to storage and to accelerators, so that no worker ever blocks on IO. It exposes six operations (read and write to a file, read and write to an object, copy between tiers, and the path report) and completes each exactly once.

It owns: the tokio runtime and its threads; the io_uring ring and its submission thread; direct IO alignment handling; object-store clients and their concurrency limit; CUDA streams and events for copies; GPUDirect Storage handles; path selection and `IoPaths`.

It refuses to know: what a file contains; which morsel a buffer belongs to; when to read (sources and placement decide); budgets (the arena enforces them).

## b. Vocabulary

**Operation.** One `read_file`, `write_file`, `read_object`, `write_object` or `copy` call; identified by an operation id for tracing.

**Ring.** The io_uring instance, when the `uring` path is selected.

**Blocking pool.** tokio's `spawn_blocking` pool, used for `pread`/`pwrite` when the ring is not available and for CUDA synchronous calls that cannot be made asynchronous.

**Copy stream.** A CUDA stream dedicated to one direction (host to device, device to host) per device; copies are enqueued with an event; completion is polled by a reactor task.

**Ingress copy.** The copy from a network library's buffer (an HTTP body) into an arena buffer for object reads; unavoidable with today's HTTP stacks; counted as `ingress_bytes`, not as a payload copy.

## c. Invariants

**RE-I1. Exactly-once completion into the given buffer.** Every operation resolves its `Completion` exactly once, with the same `Buffer` it was handed (or an error carrying nothing, in which case the buffer was dropped and its arena bytes released). No operation writes outside the buffer's length.

**RE-I2. Workers never run reactor work.** All reactor work runs on reactor threads or the blocking pool; `Completion::wait` is the only way a non-reactor thread interacts with an in-flight operation, and only the scheduler's source-drive loop and the sink driver may call it (contracts CT-I7).

**RE-I3. Paths are chosen once.** `IoPaths` is fixed by `Reactor::new` from the host profile; no operation changes the path mid-run; a failure on a `Present`-guaranteed path is an error, not a fallback (G-I7); a failure on a probed path is retried once through the fallback and reported.

**RE-I4. Direct IO is used whenever legal.** A file operation whose buffer, offset and length are all multiples of `page_bytes` uses direct IO when the path is selected; otherwise buffered IO; the choice is recorded per operation in a counter pair (`direct_ops`, `buffered_ops`).

**RE-I5. Copies are DMA.** `copy` between `PinnedHost` and `Device` uses the device's copy engine (`cuMemcpyHtoDAsync` / `DtoHAsync`) on a copy stream; between `Disk` and `PinnedHost` it is a direct-IO file operation; `Disk → Device` uses cuFile when `gds` is present. The CPU touches no payload byte. Upholds G-I2, S14.

**RE-I6. Bounded concurrency.** At most `reactor.object_concurrency` object requests and at most `reactor.file_depth` file operations are in flight; further submissions queue in order.

**RE-I7. Cancellation completes.** After `shutdown`, every in-flight operation resolves (with `Cancelled` or its natural result) within the longest single operation's duration; no `Completion` is left dangling.

## d. Interfaces

### d.1 Exposed

```rust
pub struct ReactorConfig {
    pub threads: usize,                       // reactor.threads
    pub object_concurrency: usize,            // reactor.object_concurrency
    pub file_depth: usize,                    // reactor.file_depth
    pub page_bytes: usize,
    pub profile: HostProfile,                 // all fields resolved (DS-I6)
    pub devices: Vec<DeviceId>,
    pub object_store_config: ObjectStoreConfig,   // credentials, endpoints, region; from the surface
}

pub struct Reactor { /* private */ }
impl Reactor {
    pub fn new(cfg: ReactorConfig, arena: std::sync::Arc<Arena>) -> Result<std::sync::Arc<Reactor>>;
    /// RE-I7. Idempotent.
    pub fn shutdown(&self);
    pub fn stats(&self) -> ReactorStats;
    /// Object-store metadata helpers used by sources' `plan` (not in the contract because only sources use them).
    pub fn head_object(&self, url: &str) -> Completion<ObjectMeta>;
    pub fn list_prefix(&self, url: &str) -> Completion<Vec<ObjectMeta>>;
}
impl Reactor for Reactor { /* contracts d.9 */ }

#[derive(Clone, Debug, Default)]
pub struct ReactorStats {
    pub direct_ops: u64, pub buffered_ops: u64, pub uring_ops: u64,
    pub object_reads: u64, pub object_bytes: u64, pub ingress_bytes: u64,
    pub copies_h2d: u64, pub copies_d2h: u64, pub copies_gds: u64, pub copy_bytes: u64,
    pub queued_max: u64, pub errors: u64, pub fallbacks: u64,
}
```

### d.2 Consumed

`amoru_arena::Arena` (for `contains` and page size), `amoru_kernel::{Reactor as ReactorTrait, Completion, IoPaths, Buffer, Tier, HostProfile, Guarantee, DeviceId, AmoruError}`; `tokio` (`rt-multi-thread`, `sync::oneshot`, `task::spawn_blocking`); `io-uring` (feature); `object_store` (`get_range`, `put`, `head`, `list`, multipart); `cudarc` (streams, events, memcpy async); `cufile` bindings (feature `gds`; the agent writes a minimal FFI over `libcufile`).

## e. Data model, formats and state machines

### e.1 Operation state machine

`Submitted` → `Queued` (concurrency limit reached) → `InFlight` → `Completed | Failed | Cancelled`. `Completion` observes only the terminal state. An operation on a `Present`-guaranteed path that fails goes `Failed`; on a probed path it goes `InFlight` again once via the fallback, then terminal.

### e.2 Path selection

At `new`, from `profile`:

| Path | Selected when | Fallback |
|---|---|---|
| io_uring | `io_uring == Present` and `cfg(feature = "uring")` | blocking pool `pread`/`pwrite` |
| direct IO | `direct_io_staging == Present` | buffered IO with `posix_fadvise(DONTNEED)` after each op |
| pinned copies | `arena.is_pinned()` and `cuda` | staged through a 64 MiB pinned bounce buffer allocated from the arena at `new` |
| GDS | `gds == Present` and `cfg(feature = "gds")` | `Disk → PinnedHost → Device` two-step |

`IoPaths` records the four booleans.

### e.3 Alignment rule for direct IO

An operation is direct-eligible iff `buffer.host_ptr() % page_bytes == 0`, `offset % page_bytes == 0`, `len % page_bytes == 0`. Sources and placement are responsible for issuing eligible operations; the reactor does not pad or split. A non-eligible operation on a `direct_io == Present` host is executed buffered and counted in `buffered_ops`, and a `tracing` warn is emitted once per run per caller site, because it indicates a caller bug rather than a host limitation.

## f. Algorithms and policies

**f.1 `read_file` (uring).** Submit `IORING_OP_READ` with `O_DIRECT` fd (fds are opened per path once and cached, `O_DIRECT` when direct-eligible files are expected; one fd per (path, direct flag)); the submission thread batches submissions every 50 µs or 32 entries; a completion thread reaps CQEs and resolves oneshots. Short reads (fewer bytes than requested at end of file) are an error unless the caller passed `allow_short` (a flag on an extended method `read_file_opt`; the trait method is strict).

**f.2 `read_file` (blocking pool).** `spawn_blocking(|| pread(fd, ptr, len, offset))` in a loop until `len` bytes or EOF; direct flag as in e.3.

**f.3 `read_object`.** Acquire a permit from the object semaphore; `object_store.get_range(path, offset..offset+len)`; stream the body into the buffer with a bounded number of chunks in flight; count `ingress_bytes`; release the permit. Retries are the object_store crate's (exponential backoff, 3 attempts) plus one reactor-level retry on connection reset; then `Io`.

**f.4 `write_object`.** For `src.len() ≤ 64 MiB`, single `put`; larger, multipart with 16 MiB parts, at most 4 parts in flight; on failure abort the multipart upload.

**f.5 `copy`.** Dispatch on `(src.tier(), dst.tier())`:

| From → To | Mechanism |
|---|---|
| PinnedHost → Device | `cuMemcpyHtoDAsync` on the device's H2D stream; record event; a reactor task polls events every 100 µs and resolves |
| Device → PinnedHost | `cuMemcpyDtoHAsync` on the D2H stream; same |
| Host → Device, Device → Host | through the bounce buffer in 64 MiB pieces (the fallback path when the arena is unpinned); counted in `fallbacks` |
| Disk → PinnedHost | `read_file` of the segment range |
| PinnedHost → Disk | `write_file` |
| Disk → Device | `cuFileRead` (gds) else two-step |
| Device → Disk | `cuFileWrite` (gds) else two-step |
| same tier | error `Io { op: "copy", msg: "same tier" }` |

Disk endpoints are expressed by the caller as a `Buffer` in `Tier::Disk(SegmentRef)`; the reactor maps the segment number to the segment file path through a registry the placement engine fills (`register_segment(segment: u32, path: PathBuf)`, an exposed method not in the contract).

**f.6 Concurrency limits.** Two semaphores: object (`object_concurrency`) and file (`file_depth`). Copies are limited by the CUDA stream queue (unbounded issue, bounded by the arena's budget through in-flight reservations held by the placement engine).

**f.7 Shutdown.** Set a cancelled flag; drain the uring ring (`IORING_OP_ASYNC_CANCEL` for pending); wait for CUDA streams (`cuStreamSynchronize`) with a 5 s cap, after which completions resolve `Cancelled`; stop the runtime.

## g. Concurrency within the component

`threads` tokio worker threads; one io_uring submission thread and one completion thread (uring path); one CUDA event-polling task per device. Semaphores are tokio semaphores. The segment registry is an `RwLock<HashMap<u32, PathBuf>>` read on every disk op. No lock is held across an await except the registry read, which is dropped before submission. Lock order: registry only; nothing else in this component locks.

## h. Behaviour

**Normal path (source read).** Source calls `read_object(url, offset, buf)` for a Parquet row group's byte range; reactor acquires a permit, streams the body into `buf`, resolves; source decodes on the reactor thread pool (decode is the source's; see 07) or hands off.

**Normal path (promotion).** Placement calls `copy(&disk_buf, pinned_buf)` then `copy(&pinned_buf, device_buf)`; both resolve in order; placement updates the morsel's tier.

**Edge cases.** Zero-length operation: resolves immediately with the buffer. Offset beyond EOF: `Io` with "short read". Object range beyond object length: `Io` (object stores return 416). A `copy` whose `dst` is smaller than `src`: error before submission. `register_segment` for an already-registered number: error (segments are immutable).

**Failures.** `EINVAL` from `O_DIRECT` open on a filesystem that does not support it: on a probed host, fall back to buffered for that path and count a fallback; on a `Present` host, `Config`. CUDA error on copy: resolve `Io { op: "copy" }` with the CUDA error string; the placement engine decides (it retries once through the bounce path, then fails the morsel). Object store 5xx after retries: `Io` with the status. Ring submission queue full: back-pressure the submitter (queue in memory, `queued_max` stat).

## i. Configuration

`reactor.threads`, `reactor.object_concurrency`, `reactor.file_depth`, `page.bytes`.

## j. Observability

`ReactorStats`; `IoPaths` in the report; `tracing`: `reactor.paths` (info at start), `reactor.fallback` (warn, once per path), `reactor.misaligned` (warn, once per caller site), `reactor.op` (trace level, id, kind, bytes, duration).

## k. Tests

Tests use `FakeAllocator` buffers and a temp directory; object-store tests use the `object_store` in-memory backend and, in CI, MinIO.

**RE-T1 exactly_once.** 10,000 mixed operations; each `Completion` resolves once with the same buffer identity (pointer) and length. RE-I1.

**RE-T2 no_worker_work.** Instrument thread ids; every operation's work runs on reactor or blocking-pool threads. RE-I2.

**RE-T3 paths_fixed.** `IoPaths` after `new` equals the profile-derived expectation for all 16 profile combinations; injected failure mid-run on a `Present` path yields an error, on a probed path yields one fallback. RE-I3.

**RE-T4 direct_when_aligned.** Aligned ops count as `direct_ops`, misaligned as `buffered_ops` with one warn. RE-I4.

**RE-T5 copy_is_dma.** (cuda, skippable) Host→device copy of 1 GiB; worker CPU time under 1% of wall; bytes equal after round trip. RE-I5, S14.

**RE-T6 concurrency_bound.** 1,000 object reads with `object_concurrency = 4`; in-flight never exceeds 4 (fake backend counts). RE-I6.

**RE-T7 shutdown_completes.** Issue 100 slow operations, `shutdown`; all completions resolve within 5 s. RE-I7.

**RE-T8 short_read.** Read past EOF errors; `read_file_opt(allow_short)` returns the short length. f.1.

**RE-T9 multipart.** 200 MiB `write_object` uses multipart with 16 MiB parts; failure of part 5 aborts the upload (fake backend asserts abort called). f.4.

**RE-T10 uring_vs_blocking_equivalence.** Same operations through both paths produce identical bytes. e.2.

**RE-T11 bandwidth.** (reference host, provisional elsewhere) Sequential direct reads of a 4 GiB file with `file_depth = 32` reach ≥ 80% of `fio` sequential read bandwidth on the same device. S4 (IO-bound), S15.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/runtime.rs` (tokio setup, semaphores, shutdown), `src/paths.rs` (e.2), `src/file_uring.rs` (f.1, submission and completion threads), `src/file_blocking.rs` (f.2), `src/object.rs` (f.3, f.4, `ObjectStoreConfig` → `object_store` builders for s3, gcs, azure, local), `src/copy.rs` (f.5, cuda streams and events), `src/gds.rs` (feature; minimal `libcufile` FFI: driver open, handle register, read, write), `src/segments.rs` (registry), `src/stats.rs`. `unsafe` permitted in `file_uring.rs`, `copy.rs`, `gds.rs` with `// SAFETY:` citing RE-I1 (buffer outlives the operation: the reactor holds the `Buffer` until completion).

Do not use `tokio-uring` (it wants its own runtime); do not use `tokio::fs` for payload reads (it copies through a `Vec`). Open files with `O_DIRECT` only when the profile says direct IO is present.

Anti-patterns: no `Vec<u8>` intermediates for payload bytes; no blocking call on a tokio worker thread; no per-operation fd open; no silent buffered fallback on a `Present` host.

Verify before starting: `io_uring` availability in the CI container (DS-T5's shim); `libcufile` presence on the reference GPU host (`ls /usr/local/cuda/lib64/libcufile.so`).

## m. Open items

None. (`reactor.file_depth` is in the preamble's table.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S14, G-I2 | RE-I5 | RE-T5 |
| G-I7 | RE-I3, RE-I4 | RE-T3, RE-T4, RE-T10 |
| S15, S4 (IO-bound) | RE-I6, f.1 | RE-T11 |
| preamble 4.1 (no worker IO) | RE-I2 | RE-T2 |
| preamble 4.3 (shutdown) | RE-I7 | RE-T7 |
