# Amoru SDD 06: IO reactor (`amoru-reactor`)

**Document type:** software design document, component 6 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/amoru-runtime-design.md` sections 4.2 (threads), 5.2 (local direct IO), 5.6 (tier moves), 6 (hosting); criteria S14, S15; global invariants G-I2, G-I7
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.9 (`Reactor`, `Completion`, `CompletionSender`, `CopySrc`, `CopyDst`, `IoPaths`), d.3 (`Buffer`, `BufferView`, `Allocator`), d.2 (`Tier`, `SegmentRef`), d.12 (`HostProfile`, `Guarantee`), e.1 (tier transitions)
**Component location:** `crates/amoru-reactor`, Rust; features `uring`, `cuda`, `gds`
**Consumes:** contracts (1); the arena (2) as `Arc<dyn Allocator>` and discovery's (3) `HostProfile` as a value, neither as a crate. **Consumed by:** sources (7), sinks (8), placement (9)

**Decisions worth your eye:** (1) a tokio runtime is the reactor, with io_uring used for file IO through a dedicated submission thread rather than tokio-uring, so the same runtime serves object storage and files; (2) the reactor selects every path once at start from the host profile and never re-probes, and a fallback taken because a filesystem refused `O_DIRECT` at open sticks to that path for the run; (3) object-store reads land in the arena with one copy from the HTTP body, which is the ingress boundary, counted separately from payload copies; (4) every trait method enqueues and returns, so a worker may submit from inside a placement call, and the only blocking anywhere is `Completion::wait` in the scheduler's drives.

---

## a. Purpose and boundary

The reactor is where every byte enters and leaves the process and where every move between tiers is issued. It owns the threads that talk to storage and to accelerators, so that no worker ever blocks on IO. It implements the contract's `Reactor` trait (d.9): the file operations `read_file`, `read_file_opt` and `write_file`, the object operations `read_object` and `write_object`, `copy` between tiers, the segment registry (`register_segment`, `unregister_segment`), the path report and `shutdown`; every operation completes exactly once, and every call returns as soon as the operation is enqueued.

It owns: the tokio runtime and its threads; the io_uring ring and its submission thread; direct IO alignment handling; object-store clients and their concurrency limit; CUDA streams and events for copies; GPUDirect Storage handles; the segment registry and its descriptor cache; path selection and `IoPaths`.

It refuses to know: what a file contains; which morsel a buffer belongs to; when to read (sources and placement decide); budgets (the arena enforces them).

## b. Vocabulary

**Operation.** One `read_file`, `read_file_opt`, `write_file`, `read_object`, `write_object` or `copy` call; identified by an operation id for tracing. Its *kind* is which of the six it is.

**Sticky fallback.** A fallback decision remembered for the rest of the run for one path (a file), taken when the filesystem itself refuses the fast path at open time; as opposed to a per-operation fallback, which affects one operation only.

**Guaranteed path.** A path whose `HostProfile` field is `Present`: a failure on it is an error (G-I7). A path whose field is `Probed(true)` is *available*: a failure on it may fall back, once per operation, and is counted.

**Ring.** The io_uring instance, when the `uring` path is selected.

**Blocking pool.** tokio's `spawn_blocking` pool, used for `pread`/`pwrite` when the ring is not available and for CUDA synchronous calls that cannot be made asynchronous.

**Copy stream.** A CUDA stream dedicated to one direction (host to device, device to host) per device; copies are enqueued with an event; completion is polled by a reactor task.

**Ingress copy.** The copy from a network library's buffer (an HTTP body) into an arena buffer for object reads; unavoidable with today's HTTP stacks; counted as `ingress_bytes`, not as a payload copy.

## c. Invariants

**RE-I1. Exactly-once completion, and a failure loses nothing the caller still holds.** Every operation resolves its `Completion` exactly once. A read (`read_file`, `read_file_opt`, `read_object`, `copy` into a `CopyDst::Buffer`) resolves with the same `Buffer` it was handed, or with an error, in which case the buffer was dropped and its arena bytes released. A write (`write_file`, `write_object`, `copy` from a `CopySrc::View`) takes a `BufferView` and resolves with `()` or an error; either way the reactor drops only the view, and the bytes the view was over are still owned by the caller (contracts d.3), so a failed demotion or sink write can be retried or reported without a copy. No operation writes outside the destination's length or reads outside the view's.

**RE-I2. Workers never run reactor work, and never wait for it.** All reactor work runs on reactor threads or the blocking pool; `Completion::wait` is the only way a non-reactor thread blocks on an in-flight operation, and only the scheduler's source-drive loop and the sink driver may call it (contracts CT-I7). Submission itself never blocks (RE-I6), so a worker may issue an operation from inside `Placement::push` or `pop` (contracts d.9) and observe it through `Completion::then`.

**RE-I3. Paths are chosen once.** `IoPaths` is fixed by `Reactor::new` from the host profile; no operation changes the selected path mid-run. A failure on a guaranteed (`Present`) path is an error, not a fallback (G-I7). A failure on an available (`Probed(true)`) path is retried once through the fallback and counted in `fallbacks`; the fallback is per operation, except an `EINVAL` from an `O_DIRECT` open, which is sticky for that path (f.9), because the filesystem will refuse every later open the same way.

**RE-I4. Direct IO is used whenever legal.** A file operation whose buffer, offset and length are all multiples of `page_bytes` uses direct IO when the path is selected; otherwise buffered IO; the choice is recorded per operation in a counter pair (`direct_ops`, `buffered_ops`).

**RE-I5. Copies are DMA.** `copy` between the host tier and `Device` uses the device's copy engine (`cuMemcpyHtoDAsync` / `DtoHAsync`) on a copy stream, from the pinned host tier directly and from an unpinned one through the pinned bounce buffer (contracts e.1, a counted fallback); `Disk → Device` uses cuFile when `gds` is present. Disk and the host tier are joined by `read_file` and `write_file`, direct IO when aligned. The CPU touches no payload byte. Upholds G-I2, S14.

**RE-I6. Bounded concurrency, and submission never blocks.** At most `reactor.object_concurrency` object requests and at most `reactor.file_depth` file operations are in flight; further submissions queue in order. Every trait method returns after enqueueing; the permit for the limit is acquired on a reactor thread, never on the caller's (contracts d.9).

**RE-I7. Cancellation completes.** After `shutdown`, every in-flight operation resolves (with `Cancelled` or its natural result) within the longest single operation's duration; no `Completion` is left dangling; `shutdown` is idempotent and is the trait method of contracts d.9.

**RE-I8. A segment descriptor lives exactly as long as its registration.** `register_segment` opens and caches the descriptor once; `unregister_segment` closes it, so an unlinked segment's space is returned to the filesystem at unlink and not at process exit. A `copy` naming an unregistered segment is `Io { op: "copy", msg: "segment not registered" }`, never a fresh open.

## d. Interfaces

### d.1 Exposed

```rust
pub struct ReactorConfig {
    pub threads: usize,                       // reactor.threads
    pub object_concurrency: usize,            // reactor.object_concurrency
    pub file_depth: usize,                    // reactor.file_depth
    pub page_bytes: usize,
    pub profile: HostProfile,                 // all fields resolved (DS-I6): Present, Absent or Probed(_)
    pub devices: Vec<DeviceId>,
    pub object_store: ObjectStoreConfig,      // credentials, endpoints, region; from the surface
}

/// Everything the `object_store` builders need, for every backend a URL may name.
/// A backend whose field is `None` is unavailable: a URL for it is `Config { name:
/// "object_store" }` at the first operation. Values come from the surface's arguments
/// or, when a field is `None` there, from the environment the way the `object_store`
/// crate reads it (`AWS_*`, `GOOGLE_*`, `AZURE_*`); the reactor does not read the
/// environment itself.
#[derive(Clone, Debug, Default)]
pub struct ObjectStoreConfig {
    pub s3: Option<S3Config>,
    pub gcs: Option<GcsConfig>,
    pub azure: Option<AzureConfig>,
    /// Root for `file://` URLs; `None` means `file://` URLs are absolute paths.
    pub local_root: Option<std::path::PathBuf>,
    /// Permit plain-HTTP endpoints (MinIO in CI); false in every default.
    pub allow_http: bool,
}
#[derive(Clone, Debug, Default)]
pub struct S3Config {
    pub endpoint: Option<String>,             // custom endpoint (MinIO, R2); None = AWS
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub bucket: Option<String>,               // default bucket when the URL is a bare key; the URL's bucket wins
}
#[derive(Clone, Debug, Default)]
pub struct GcsConfig {
    pub service_account_path: Option<std::path::PathBuf>,
    pub service_account_json: Option<String>, // one of the two; both set is `Config`
    pub bucket: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct AzureConfig {
    pub account: Option<String>,
    pub access_key: Option<String>,
    pub container: Option<String>,
}

pub struct Reactor { /* private */ }
impl Reactor {
    /// Builds the runtime, selects the paths (e.2) and, when `alloc.is_pinned()` is
    /// false and a device exists, allocates the pinned bounce buffer. The allocator is
    /// the arena behind the contract; the reactor uses `alloc`, `page_bytes`,
    /// `contains`, `tier_of` and `is_pinned` and nothing arena-specific.
    pub fn new(cfg: ReactorConfig, alloc: std::sync::Arc<dyn Allocator>) -> Result<std::sync::Arc<Reactor>>;
    pub fn stats(&self) -> ReactorStats;
}

/// Contracts d.9 `ObjectMetadata`, for sources' `plan` (07 e.1): `head` and `list`
/// of `object_store` through f.3's client cache; both complete on a reactor thread
/// and enqueue like every other operation (f.8). the reactor fills
/// `amoru_kernel::ObjectMeta` from `object_store::ObjectMeta`; no `object_store` type
/// is re-exported.
impl amoru_kernel::ObjectMetadata for Reactor { /* contracts d.9 */ }
/// Contracts d.9, every method: `read_file`, `read_file_opt`, `write_file`,
/// `read_object`, `write_object`, `copy`, `register_segment`, `unregister_segment`,
/// `paths`, `shutdown`. Consumers hold `Arc<dyn Reactor>`; sources also hold
/// `Arc<dyn ObjectMetadata>`, which the facade makes from the same `Arc<Reactor>`.
impl amoru_kernel::Reactor for Reactor { /* contracts d.9 */ }

#[derive(Clone, Debug, Default)]
pub struct ReactorStats {
    pub direct_ops: u64, pub buffered_ops: u64, pub uring_ops: u64,
    pub object_reads: u64, pub object_bytes: u64, pub ingress_bytes: u64,
    pub copies_h2d: u64, pub copies_d2h: u64, pub copies_gds: u64, pub copy_bytes: u64,
    pub queued_max: u64, pub errors: u64, pub fallbacks: u64, pub sticky_fallbacks: u64, pub bounce_bytes: u64,
    pub segments_registered: u64, pub segments_open: u64,
}
```

`shutdown` is the trait method; there is no second inherent one. `read_file_opt(path, offset, dst, allow_short)` is the trait method of contracts d.9: with `allow_short == false` it behaves as `read_file` and returns `(dst, dst.len())`; with `true` a read that reaches end of file resolves with the bytes actually read, and the bytes of `dst` beyond that count are unspecified. `register_segment(segment, path)` opens the file once (`O_DIRECT` when direct IO is selected, buffered otherwise, per f.9) and records `(segment, path, fd)`; a second registration of the same number is `Io { op: "register_segment", msg: "already registered" }`. `unregister_segment(segment)` closes the descriptor and forgets the number; unregistering an unknown number is a no-op. Both are synchronous (one `open` or `close`, no IO of payload size) and are the only trait methods that do not enqueue.

### d.2 Consumed

`amoru_kernel::{Reactor as ReactorTrait, Completion, CompletionSender, CopySrc, CopyDst, IoPaths, Buffer, BufferView, Allocator, Tier, TierKind, SegmentRef, HostProfile, Guarantee, DeviceId, AmoruError}` (the arena arrives as `Arc<dyn Allocator>`; no `amoru_arena` dependency); `tokio` (`rt-multi-thread`, `sync::Semaphore`, `task::spawn_blocking`); `io-uring` (feature); `object_store` (`get_range`, `put`, `head`, `list`, multipart; the `aws`, `gcp`, `azure` features; `ObjectMeta` re-exported); `cudarc` (streams, events, memcpy async); `cufile` bindings (feature `gds`; the agent writes a minimal FFI over `libcufile`, no crate); `libc` (`open`, `pread`, `pwrite`, `posix_fadvise`).

## e. Data model, formats and state machines

### e.1 Operation state machine

`Submitted` (the caller's thread: the operation is placed on the runtime's queue and the call returns) → `Queued` (on a reactor thread, waiting for a permit) → `InFlight` → `Completed | Failed | Cancelled`. `Completion` observes only the terminal state, and a `then` callback registered on it runs on the reactor thread that reaches the terminal state. An operation on a guaranteed path that fails goes `Failed`; on an available (`Probed(true)`) path it goes `InFlight` again once via the fallback, then terminal. A sticky fallback (f.9) is decided before `InFlight` and is not a retry.

### e.2 Path selection

At `new`, from `profile` and the allocator. "Selected when" reads the field with `Guarantee::is_available()` (`Present` or `Probed(true)`); whether a failure may fall back is decided by `is_guaranteed()` per RE-I3, not here.

| Path | Selected when | Fallback (available paths only; on a guaranteed path a failure is an error) |
|---|---|---|
| io_uring | `io_uring.is_available()` and `cfg(feature = "uring")` | blocking pool `pread`/`pwrite` |
| direct IO | `direct_io_staging.is_available()` | buffered IO with `posix_fadvise(DONTNEED)` after each op; sticky per path on open-time `EINVAL` (f.9) |
| pinned copies | `alloc.is_pinned()` and `cuda` | staged through a 64 MiB pinned bounce buffer allocated from the arena at `new` (the arena is then unpinned, so the bounce buffer is the one page-locked region, allocated by `cuMemHostAlloc` outside the arena; the only allocation of payload size outside the arena in the process, and it is fixed at `new`) |
| GDS | `gds.is_available()` and `cfg(feature = "gds")` | `Disk → host tier → Device` two-step, issued by the placement engine (09 e.4), not inside `copy` |
| RDMA (reserved, not v1) | `rdma.is_available()` and `cfg(feature = "rdma")` | none: without the feature, `Remote` endpoints are `Unsupported("rdma")`, never emulated over TCP |

`IoPaths` records the five booleans; `rdma` is always false in a v1 build; `pinned` is `alloc.is_pinned()`.

### e.3 Alignment rule for direct IO

An operation is direct-eligible iff `buffer.host_ptr() % page_bytes == 0`, `offset % page_bytes == 0`, `len % page_bytes == 0` (for a write, the view's pointer and length). Sources and placement are responsible for issuing eligible operations; the reactor does not pad or split. A non-eligible operation on a host where direct IO is selected is executed buffered and counted in `buffered_ops`, and a `tracing` warn is emitted once per run per `(operation kind, path)`, because it indicates a caller bug rather than a host limitation (a non-page-multiple final piece of a staging record, 09 f.5, is the one expected case: for a registered segment path the buffered operation is counted but the warn is not emitted, because the segment format makes that piece deliberate).

### e.4 Segment registry

`RwLock<HashMap<u32, SegmentEntry { path: PathBuf, fd: OwnedFd, direct: bool, gds_handle: Option<CuFileHandle> }>>`. Filled by `register_segment`, emptied by `unregister_segment`; read by `copy` for a `Disk` endpoint (the `SegmentRef::segment` field is the key) and by `read_file`/`write_file` when the path equals a registered path (the cached descriptor is used instead of the per-path fd cache of f.1, so a segment has exactly one open descriptor, RE-I8).

## f. Algorithms and policies

**f.1 `read_file` and `read_file_opt` (uring).** Submit `IORING_OP_READ` on the path's fd (fds are opened per path once and cached, one fd per `(path, direct flag)`, except registered segments, which use the registry's descriptor, e.4; the cache is closed at `shutdown`); the submission thread batches submissions every 50 µs or 32 entries; a completion thread reaps CQEs and resolves the `CompletionSender`s. Short reads (fewer bytes than requested at end of file) are `Io { op: "read_file", msg: "short read" }` for `read_file` and for `read_file_opt(.., false)`; `read_file_opt(.., true)` resolves `(dst, n)` with the bytes read. Reads continue in a loop until `len` bytes or end of file (a single `pread` may return less on some filesystems without meaning end of file). `write_file` is the mirror: `IORING_OP_WRITE` from the view's pointer; a short write is an error and is retried from the written offset up to three times before `Io`.

**f.2 `read_file` (blocking pool).** `spawn_blocking(|| pread(fd, ptr, len, offset))` in a loop until `len` bytes or EOF; direct flag as in e.3; `write_file` uses `pwrite` the same way.

**f.3 `read_object`.** On a reactor thread, acquire a permit from the object semaphore; `object_store.get_range(path, offset..offset+len)`; stream the body into the buffer with a bounded number of chunks in flight; count `ingress_bytes`; release the permit. Retries are the object_store crate's (exponential backoff, 3 attempts) plus one reactor-level retry on connection reset; then `Io`. The client for a URL's scheme and bucket is built once from `ObjectStoreConfig` and cached; an unknown scheme or a backend with no configuration is `Config { name: "object_store" }`.

**f.4 `write_object(url, src: BufferView)`.** For `src.len() ≤ 64 MiB`, single `put` of a `Bytes` built over the view without copying (`Bytes::from_owner(view)`; the view's owner keeps the arena bytes alive); larger, multipart with 16 MiB parts (each part a `slice` of the view), at most 4 parts in flight; on failure abort the multipart upload and resolve the error; the view is dropped and the caller's bytes are untouched (RE-I1).

**f.5 `copy(src: CopySrc, dst: CopyDst)`.** The legal transitions are contracts e.1, the single authority; this table repeats its rows and adds only the mechanism inside the reactor. "Host tier" is `PinnedHost` when `alloc.is_pinned()` and `Host` otherwise; the two never coexist, so no row joins them.

| From → To | Mechanism in the reactor |
|---|---|
| host tier → Device(d) | `cuMemcpyHtoDAsync` on the device's H2D stream from the view's pointer when pinned; when unpinned, through the bounce buffer in 64 MiB pieces (`memcpy` into the bounce buffer on the blocking pool, then the same async copy; counted in `fallbacks` and `bounce_bytes`; `AllocStats.payload_copies_total` is not incremented because contracts e.1 classes the bounce as a counted fallback, and the report shows `bounce_bytes` beside it); record an event; a reactor task polls events every 100 µs and resolves |
| Device(d) → host tier | `cuMemcpyDtoHAsync` on the D2H stream into the destination buffer; bounce when unpinned; same completion |
| host tier → Disk | not a `copy`: the placement engine issues `write_file` on the segment path (09 f.5); a `copy(View(host), Disk(_))` resolves `Io { op: "copy", msg: "disk endpoint without gds" }` |
| Disk → host tier | not a `copy`: `read_file` of `[offset, offset + len)`; `copy(Disk(_), Buffer(host))` resolves the same error |
| Disk → Device(d) | `cuFileRead` from the registered segment's `gds_handle` into the device buffer when the GDS path is selected; otherwise the same error, and the placement engine does `read_file` into a host-tier buffer then `copy(View, Buffer(device))` (09 e.4) |
| Device(d) → Disk | `Io { op: "copy", msg: "gds write not used in v1" }`; the placement engine does `copy(View(device), Buffer(host))` then `write_file` |
| host tier → Remote(n) | reserved, feature `rdma`, not v1: one-sided `RDMA_WRITE` from the view into a region node `n` leased to this node, on a queue pair the reactor holds per peer; completion by the NIC's completion queue, polled by the same task that polls CUDA events; v1 returns `Unsupported("rdma")` |
| Remote(n) → host tier | reserved, feature `rdma`, not v1: one-sided `RDMA_READ` from `RemoteRef { addr, rkey, len }` on node `n` into the destination buffer; v1 returns `Unsupported("rdma")` |

Every pair contracts e.1 calls illegal (`Device ↔ Device`, `Remote ↔ Disk`, and a same-tier copy) resolves `Io { op: "copy", msg }` naming the pair before anything is submitted; a `Remote` endpoint of any kind is `Unsupported("rdma")` in a v1 build. A `dst` shorter than `src` is the same pre-submission error. Disk endpoints are `SegmentRef`s (contracts d.9) resolved through the registry (e.4); a `SegmentRef` whose `segment` is not registered is RE-I8's error. The reactor is the only component that ever holds a queue pair, a memory key or a lease: the placement engine asks for moves, the reactor knows peers. The arena's single reservation (AR-I3) is what makes registering the whole host tier with a NIC a one-time operation; that is the reason the reservation is one region and not many, and it is stated in both documents so the arena agent does not "optimise" it away.

The `rdma` rows and the `register_peer`/`lease` methods they will need are not in the contract's `Reactor` trait yet; they are added to it in the same pull request as the feature (E11), so that a v1 build carries the dispatch arms (CT-I11) but no dead API.

**f.6 Concurrency limits.** Two semaphores: object (`object_concurrency`) and file (`file_depth`). Copies are limited by the CUDA stream queue (unbounded issue, bounded by the arena's budget through in-flight reservations held by the placement engine).

**f.7 Shutdown.** Set a cancelled flag; drain the uring ring (`IORING_OP_ASYNC_CANCEL` for pending); wait for CUDA streams (`cuStreamSynchronize`) with a 5 s cap, after which completions resolve `Cancelled`; close the fd cache and every registered segment descriptor; stop the runtime. Idempotent: a second call returns at once. Trait methods called after `shutdown` resolve `Cancelled` without submitting.

**f.8 Submission.** Every trait method except the two registry calls does the same three things on the caller's thread: build the operation record with a `Completion::channel()`, push it onto the runtime's unbounded submission queue (a `tokio::sync::mpsc::unbounded_channel` per operation kind, drained by a reactor task), and return the `Completion`. Permit acquisition (f.6), path selection per operation (e.3), the registry read and every syscall happen on reactor threads. This is what lets a worker submit from inside `Placement::push` or `pop` while holding no lock that the reactor could contend on (contracts d.9; preamble 4.1 forbids a worker to wait, not to submit). The queue is unbounded because the caller cannot be made to wait; the bound on outstanding operations is the placement engine's reservations and the scheduler's read-ahead, and `queued_max` reports the high-water mark for the report.

**f.9 Fallback rules.** Two kinds. Per operation: an operation on an available (`Probed(true)`) path that fails is re-issued once through the row's fallback in e.2 (blocking pool for uring, buffered for direct, bounce for pinned, two-step for GDS by the caller), counted in `fallbacks`, and the `reactor.fallback` warn is emitted once per `(operation kind, path)`; a second failure resolves the error. Sticky per path: when opening a file with `O_DIRECT` returns `EINVAL` (the filesystem does not support direct IO on this path) and `direct_io_staging` is `Probed(true)`, the reactor records the path as buffered in the fd cache (or the registry entry), counts `sticky_fallbacks`, warns once for the path, and every later operation on that path opens and runs buffered without re-trying `O_DIRECT`; a per-operation failure never becomes sticky. On a guaranteed path (`Present`) neither kind applies: an open-time `EINVAL` is `Config { name: "host_profile", msg }` naming the path and the field, and an operation failure is `Io`. `paths()` never changes after `new` in either case; the sticky set is reported in `ReactorStats` and the run report, not in `IoPaths`.

## g. Concurrency within the component

`threads` tokio worker threads; one io_uring submission thread and one completion thread (uring path); one CUDA event-polling task per device; one drain task per submission queue (f.8). Semaphores are tokio semaphores, acquired only on reactor threads. The segment registry (e.4) is read on every disk op and written by the two registry calls, which may come from any thread; the read guard is dropped before submission. No lock is held across an await. Lock order: registry, then the fd cache; nothing else in this component locks, and neither lock is in the preamble's order because nothing outside the reactor is called while either is held. `Completion::then` callbacks run on the resolving reactor thread and are the placement engine's (short, non-blocking, contracts d.9); a callback that panics is caught, counted in `errors` and logged, so one bad callback cannot take a reactor thread down.

"Reactor threads never allocate outside the arena" (preamble 4.1) means allocations of payload size: the bounce buffer (e.2) and the per-operation records, `Bytes` handles, HTTP framing and flatbuffers are ordinary heap allocations and are permitted.

## h. Behaviour

**Normal path (source read).** Source calls `read_object(url, offset, buf)` for a Parquet row group's byte range; reactor acquires a permit, streams the body into `buf`, resolves; source decodes on the reactor thread pool (decode is the source's; see 07) or hands off.

**Normal path (promotion from disk to a device, no GDS).** Placement calls `read_file(segment_path, offset, host_buf)` and, in its `then` callback, `copy(CopySrc::View(host_buf.view()), CopyDst::Buffer(device_buf))`; both resolve in order on reactor threads; the second callback updates the morsel's tier (09 g). With GDS it is one `copy(CopySrc::Disk(seg), CopyDst::Buffer(device_buf))`.

**Normal path (demotion to disk).** Placement calls `write_file(segment_path, offset, view)` once per piece of the record (09 f.5) from inside `push` on a worker thread; each call returns at once (f.8); the completions' `then` callbacks run on reactor threads and the last one marks the entry `OnDisk`.

**Edge cases.** Zero-length operation: resolves immediately with the buffer (or `()`), on the caller's thread, and a `then` registered on it runs at once there. Offset beyond EOF: `Io` with "short read" (or `(dst, 0)` for `read_file_opt(.., true)`). Object range beyond object length: `Io` (object stores return 416). A `copy` whose `dst` is smaller than `src`: error before submission. `register_segment` for an already-registered number: error (segments are immutable). `unregister_segment` with operations in flight on that segment: the descriptor is closed after the last of them resolves (the entry is marked closing and the drain task closes it), so RE-I1 holds. A trait call after `shutdown`: `Cancelled` at once.

**Failures.** `EINVAL` from `O_DIRECT` open on a filesystem that does not support it: f.9 (sticky per path on an available path; `Config` on a guaranteed one). CUDA error on copy: resolve `Io { op: "copy" }` with the CUDA error string after the one per-operation fallback through the bounce buffer when the pinned path is available; the placement engine then re-issues the identical operation once (09 f.10) and fails the morsel on the second error. Object store 5xx after retries: `Io` with the status. A write failure of any kind: the error resolves, the view is dropped, the caller's bytes are intact (RE-I1). Ring submission queue full: the drain task waits for space before submitting the next record; the caller is never involved (`queued_max` stat).

## i. Configuration

`reactor.threads`, `reactor.object_concurrency`, `reactor.file_depth`, `page.bytes`. `ObjectStoreConfig` is not a table row: it is credentials and endpoints the surface passes through (preamble section 5 owns no secret).

## j. Observability

`ReactorStats`; `IoPaths` in the report; `tracing`: `reactor.paths` (info at start), `reactor.fallback` (warn, once per `(operation kind, path)`), `reactor.sticky_fallback` (warn, once per path), `reactor.misaligned` (warn, once per `(operation kind, path)`), `reactor.segment` (debug: register and unregister), `reactor.op` (trace level, id, kind, bytes, duration).

## k. Tests

Tests use the real reactor over `FakeAllocator` buffers (testkit, contracts d.15; knobs `pinned(bool)` and `page_bytes(n)`) and a temp directory; object-store tests use the `object_store` in-memory backend wrapped by a test-local `ObjectStore` implementation that counts in-flight requests and can fail a named multipart part (that wrapper lives in this crate's tests, not in the testkit). MinIO in CI is an integration job. A test that needs `cuda`, `gds` or the reference NVMe is tagged "(reference host, E1)" and, where it cannot run, listed as skipped with its id (the preamble's wave gate rule); the MinIO job is "(integration, closes in wave 2)".

**RE-T1 exactly_once.** 10,000 mixed operations; each `Completion` resolves once; reads resolve with the same buffer identity (pointer) and length; writes resolve `()` and the `Arc<Buffer>` behind each view has a strong count of 1 afterwards. RE-I1.

**RE-T2 no_worker_work.** Instrument thread ids; every operation's work, including permit acquisition and the path decision, runs on reactor or blocking-pool threads; the calling thread's time inside any trait method is under 20 µs at p99 with `file_depth = 1` and 1,000 queued operations (submission does not wait for permits). RE-I2, RE-I6, f.8.

**RE-T3 paths_fixed.** `IoPaths` after `new` equals the profile-derived expectation for all 16 combinations of the four probed paths (`io_uring`, `direct_io_staging`, `gds`, `memlock` through `FakeAllocator::pinned`, each `Probed(true)` or `Probed(false)`; `rdma` is false in every v1 build and asserted so); the same 16 with `Present` in place of `Probed(true)` select the same paths; injected failure mid-run on a `Present` path yields an error, on a `Probed(true)` path yields one fallback and one `fallbacks` count. RE-I3.

**RE-T4 direct_when_aligned.** Aligned ops count as `direct_ops`, misaligned as `buffered_ops` with one warn per `(kind, path)` (two misaligned reads on one path warn once; a read and a write on it warn twice); a misaligned final piece on a registered segment path counts and does not warn. RE-I4, e.3.

**RE-T5 copy_is_dma.** (reference host, E1; `cuda`, skipped and listed without a device) Host→device copy of 1 GiB from a pinned `FakeAllocator` view; worker CPU time under 1% of wall; bytes equal after round trip; with `pinned(false)` the same copy succeeds through the bounce buffer and counts `fallbacks` and `bounce_bytes`. RE-I5, S14.

**RE-T6 concurrency_bound.** 1,000 object reads with `object_concurrency = 4`; the test-local backend wrapper never sees more than 4 in flight; all 1,000 calls return before the first completes. RE-I6.

**RE-T7 shutdown_completes.** Issue 100 slow operations, `shutdown`; all completions resolve within 5 s; a second `shutdown` returns at once; a `read_file` after `shutdown` resolves `Cancelled`; every cached fd and registered descriptor is closed (`/proc/self/fd` count returns to the pre-test value). RE-I7.

**RE-T8 short_read.** Read past EOF errors for `read_file` and `read_file_opt(.., false)`; `read_file_opt(.., true)` returns the short length, and 0 at an offset past the end. f.1.

**RE-T9 multipart.** 200 MiB `write_object` from a view uses multipart with 16 MiB parts; failure of part 5 aborts the upload (the test-local wrapper asserts `abort_multipart` was called), the completion resolves the error, and the source bytes are unchanged and still owned by the test. f.4, RE-I1.

**RE-T10 uring_vs_blocking_equivalence.** Same operations through both paths produce identical bytes. e.2.

**RE-T11 bandwidth.** (reference host, E1; provisional elsewhere with the host named) Sequential direct reads of a 4 GiB file with `file_depth = 32` reach ≥ 80% of `fio` sequential read bandwidth on the same device. S4 (IO-bound), S15.

**RE-T12 sticky_fallback.** With `direct_io_staging = Probed(true)` and a path on a filesystem that refuses `O_DIRECT` (a tmpfs mount in the test container, or a shim that makes `open` return `EINVAL` for one path): the first operation falls back, `sticky_fallbacks == 1`, one `reactor.sticky_fallback` warn, and 100 later operations on that path open buffered with no further `O_DIRECT` attempt (counted by the shim) while operations on another path stay direct; with `Present`, the first operation is `Config { name: "host_profile" }`; a per-operation `EIO` injected on a direct read falls back once and the next operation on the path tries direct again. f.9, RE-I3.

**RE-T13 segment_registry.** `register_segment` twice with one number errors; `copy(Disk(seg), ..)` for an unregistered number is RE-I8's error; after `unregister_segment` and unlink, `df` on the temp filesystem shows the space returned (the descriptor is closed); `unregister_segment` with a read in flight closes after the read resolves with the right bytes. RE-I8, e.4.

**RE-T14 then_on_reactor_thread.** A `then` callback registered on each of the six operation kinds runs exactly once, on a reactor or blocking-pool thread (thread id check), before the next operation on the same path is submitted; a callback that panics is caught, counted in `errors`, and the reactor keeps serving. e.1, g.

**RE-T15 probed_versus_present.** A profile with `io_uring = Present` and a seccomp shim that fails `io_uring_setup` makes `new` return `Config`; with `Probed(true)` and the same shim, `new` succeeds with `io_uring == false` in `IoPaths` and a note; with `Probed(false)` the ring is never attempted. RE-I3, e.2.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/runtime.rs` (tokio setup, semaphores, submission queues and drain tasks (f.8), shutdown (f.7)), `src/paths.rs` (e.2, f.9), `src/file_uring.rs` (f.1, submission and completion threads), `src/file_blocking.rs` (f.2), `src/fdcache.rs` (per-path descriptors, sticky flags), `src/object.rs` (f.3, f.4, `ObjectStoreConfig` → `object_store` builders for s3, gcs, azure, local), `src/copy.rs` (f.5, cuda streams and events, bounce buffer), `src/gds.rs` (feature; minimal `libcufile` FFI: driver open, handle register, read, write), `src/segments.rs` (e.4 registry), `src/stats.rs`. `unsafe` permitted in `file_uring.rs`, `file_blocking.rs`, `copy.rs`, `gds.rs` with `// SAFETY:` citing RE-I1 (the bytes outlive the operation: the reactor holds the `Buffer` or the `BufferView` until completion).

Do not use `tokio-uring` (it wants its own runtime); do not use `tokio::fs` for payload reads (it copies through a `Vec`). Open files with `O_DIRECT` only when the direct IO path is selected and the path is not sticky-buffered.

Anti-patterns: no `Vec<u8>` intermediates for payload bytes; no blocking call on a tokio worker thread; no per-operation fd open; no silent buffered fallback on a `Present` host; no permit acquisition or syscall on the caller's thread inside a trait method (f.8); no second `shutdown` method beside the trait's.

Verify before starting: `io_uring` availability in the CI container (DS-T5's shim); `libcufile` presence on the reference GPU host (`ls /usr/local/cuda/lib64/libcufile.so`).

## m. Open items

None. (`reactor.file_depth` is in the preamble's table.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S14, G-I2 | RE-I5 | RE-T5 |
| G-I7 | RE-I3, RE-I4, f.9 | RE-T3, RE-T4, RE-T10, RE-T12, RE-T15 |
| S15, S4 (IO-bound) | RE-I6, f.1 | RE-T11 |
| preamble 4.1 (no worker IO), contracts d.9 (non-blocking submission) | RE-I2, RE-I6, f.8 | RE-T2, RE-T14 |
| preamble 4.3 (shutdown) | RE-I7 | RE-T7 |
| contracts d.3 (`BufferView`), RE-I1 (writes lose nothing) | RE-I1 | RE-T1, RE-T9 |
| contracts d.9 (`register_segment`), PL-I8 | RE-I8, e.4 | RE-T13 |
| contracts e.1 (tier transitions) | f.5 | RE-T5, PL-T11 |

## o. Deferred (post-v1)

None.
