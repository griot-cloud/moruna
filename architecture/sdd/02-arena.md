# Amoru SDD 02: Memory arena (`amoru-arena`)

**Document type:** software design document, component 2 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/amoru-runtime-design.md` section 5.6a; decisions D10; criteria S1, S13, S14
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` (d.3 `Buffer`, `BufferView`, `Allocator`, `AllocStats`; d.12 `Limits`, `HostProfile`, `Guarantee`; e.1 one host tier per run)
**Component location:** `crates/amoru-arena`, Rust
**Consumes:** contracts (1). Feature `cuda` adds `cudarc`. **Consumed by:** reactor (6), sources (7), sinks (8), placement (9), adapters (5)

**Decisions worth your eye:** (1) size-class allocation with powers of two from 64 KiB and a large-object path above 512 MiB, accepting up to 2× internal fragmentation in exchange for O(1) alloc and free; (2) transparent huge pages by `madvise` rather than hugetlbfs, so the arena works without a boot-time reservation and gets faster when one exists; (3) the host arena is pinned only when a device is present and memlock allows, never on a CPU-only host.

---

## a. Purpose and boundary

The arena is the only source of payload memory in the process. At start it reserves the host budget as one contiguous, aligned region, backs it with huge pages when the host offers them, page-locks it when an accelerator is present and the host allows, registers it with a NIC when the `rdma` feature is on, and then hands out `Buffer`s from it for the rest of the run. On a GPU host it does the same per device with a device arena sized to the device budget. It is why the budget is exact: bytes not in the arena are not morsel bytes.

It owns: reservation and release of the arena regions; the size-class allocator over them; per-tier accounting (`AllocStats`); the `ArenaHandle` that `Buffer` drops call; page-size and alignment guarantees; pinning and registration.

It refuses to know: what a buffer contains; which tier a caller *should* ask for (placement decides); whether an allocation failure should shrink a morsel (controller decides); anything about files or devices beyond allocating their memory.

## b. Vocabulary

**Region.** One contiguous mapping per tier: the host region (`Host` or `PinnedHost`, never both: a host is one or the other for the run, and contracts e.1 calls whichever it is "the host tier") and one device region per device.

**Size class.** A power-of-two allocation size from 64 KiB (class 0) to 512 MiB (class 13); requests round up to the next class.

**Large allocation.** A request above 512 MiB, served from the region's top-down bump area, rounded to the page size, coalesced on free.

**Slab.** A 512 MiB-aligned chunk of a region assigned to one size class on demand; slabs are never returned to the bump area during a run.

**Token.** The `ArenaHandle` trait object a `Buffer` carries; its `release` returns the buffer to the class free list.

## c. Invariants

**AR-I1. Every buffer is aligned.** Returned pointers are multiples of `ALIGNMENT`, and multiples of `page_bytes` when the class size is ≥ `page_bytes` (true for every class, since class 0 is 64 KiB). Proves the contract in preamble 1.3 row 2.

**AR-I2. Budget is enforced at allocation.** `alloc` fails with `AmoruError::Alloc` rather than exceeding the tier's budget; the sum of live buffer bytes per tier, including internal fragmentation, never exceeds the region size. Upholds G-I1.

**AR-I3. The region is allocated once.** After `Arena::new` returns, no further `mmap`, `cudaMalloc`, `cudaHostAlloc` or `mlock` call is made for the run; a request that cannot be served from the region fails. Rationale: the flat memory footprint (architecture 5.6a) and the pod's cgroup accounting depend on this, and so does the reactor: registering the host tier with a NIC (feature `rdma`, 06 f.5) is one call only because the host region is one mapping and not many. The reservation is one region for that reason and must not be split into several mappings as an optimisation.

**AR-I4. Free is O(1) and never blocks a worker for longer than a class lock.** `release` pushes to a per-class free list under a per-class mutex (or lock-free stack; see l); it never touches the OS.

**AR-I5. Stats are exact.** `AllocStats.*_in_use` equals the sum of class sizes of live buffers plus live large allocations, updated atomically in `alloc` and `release`.

**AR-I6. Pinning is all-or-nothing and reported.** Either the whole host region is page-locked and `Tier::PinnedHost` requests succeed while `Tier::Host` requests return `Alloc` with `tier: Host`, or none of it is and the reverse holds; the state is fixed at `new` and exposed by `Allocator::is_pinned()`. A run therefore has exactly one host tier (contracts e.1): every host buffer the arena hands out carries that tier, every other component derives "the host tier" from `is_pinned()`, and no move between `Host` and `PinnedHost` exists anywhere. Upholds G-I7.

**AR-I7. Device buffers never expose a host pointer.** `Buffer::host_ptr()` is `None` for device buffers; `as_ref` panics with the tier in the message (contracts h).

## d. Interfaces

### d.1 Exposed

```rust
pub struct ArenaConfig {
    pub host_bytes: u64,                 // host region size (the host budget from the controller)
    pub host_tier: TierKind,             // Host or PinnedHost, from `Discovered.host_tier` (03 d.1); the arena will not construct both
    pub device_bytes: Vec<(DeviceId, u64)>,
    pub page_bytes: usize,               // from Limits
    pub huge_pages: Guarantee,           // from HostProfile; Present, Absent or Probed(_) (DS-I6), never Unknown
    pub memlock: Guarantee,
    pub register_rdma: bool,             // feature rdma only
}

pub struct Arena { /* private */ }

impl Arena {
    /// Reserves and prepares every region. Fails if any region cannot be created at
    /// the requested size; does not partially succeed (AR-I3).
    pub fn new(cfg: ArenaConfig) -> Result<std::sync::Arc<Arena>>;
    pub fn huge_pages_active(&self) -> bool;
    pub fn region_bytes(&self, tier: Tier) -> u64;
    /// Largest single allocation currently possible in `tier` (for controller diagnostics).
    pub fn largest_free(&self, tier: Tier) -> u64;
    /// Registered memory region handle for RDMA peers (feature rdma); None otherwise.
    pub fn rdma_region(&self) -> Option<RdmaRegion>;
}

/// Contracts d.3. The arena overrides every method, including the three with
/// defaults: `contains(ptr)` is a range check against the host and device regions;
/// `tier_of(ptr)` returns the region's tier (`Host` or `PinnedHost` for the host
/// region, `Device(d)` for a device region) when `contains(ptr)`, else `None`;
/// `is_pinned()` is the all-or-nothing state fixed at `new` (AR-I6). Consumers
/// reach the arena through `Arc<dyn Allocator>`; the inherent methods above are for
/// the facade and the report only.
impl Allocator for Arena {
    fn alloc(&self, bytes: usize, tier: Tier) -> Result<Buffer>;
    fn page_bytes(&self) -> usize;
    fn stats(&self) -> AllocStats;
    fn contains(&self, ptr: *const u8) -> bool;
    fn tier_of(&self, ptr: *const u8) -> Option<Tier>;
    fn is_pinned(&self) -> bool;
}
```

`alloc(bytes, Host)` on a pinned arena and `alloc(bytes, PinnedHost)` on an unpinned one fail with `Alloc` naming the requested tier: the host region has one tier and a caller that wants "the host tier" asks `is_pinned()` first (contracts e.1). `Buffer::view` (contracts d.3) needs nothing from the arena: the view holds the `Arc<Buffer>`, and the arena token inside it is what `BufferView::of_arrow(buf, alloc)` resolves through `contains` and `tier_of`.

### d.2 Consumed

`amoru_kernel::{Allocator, Buffer, AllocStats, Tier, TierKind, DeviceId, Guarantee, AmoruError, ALIGNMENT}` and the `ArenaHandle` trait from `buffer.rs`:

```rust
pub trait ArenaHandle: Send + Sync { fn release(&self, ptr: *mut u8, len: usize, tier: Tier); }
```

With feature `cuda`: `cudarc::driver::{CudaDevice, CudaSlice, sys::cuMemHostRegister}` (or `cudaHostAlloc` for the pinned host region; see f.2).

## e. Data model, formats and state machines

### e.1 Region layout

```
host region (host_bytes, aligned to 512 MiB up)
┌────────────────────────────────────────────────────────────────────┐
│ slabs (class-assigned, grow upward) │ free │ large allocs (grow downward) │
└────────────────────────────────────────────────────────────────────┘
```

Slabs are 512 MiB each and are claimed from the low end when a class's free list is empty; large allocations are carved from the high end. The two meet at the `free` gap; when the gap cannot satisfy a slab claim or a large request, the allocation fails (AR-I2). A host region smaller than 512 MiB (budget below that) uses one slab equal to the whole region and serves every class from it by splitting (buddy-style) rather than by dedicated slabs; this is the small-budget mode and is selected when `host_bytes < 1 GiB`.

### e.2 Size classes

| Class | Size | Typical use |
|---|---|---|
| 0 | 64 KiB | trace and metadata buffers |
| 1..=6 | 128 KiB .. 4 MiB | small morsels, column chunks |
| 7..=10 | 8 MiB .. 64 MiB | default morsels |
| 11..=13 | 128 MiB .. 512 MiB | large morsels, staging segment buffers |
| large | > 512 MiB | rounded to page; bump from top |

A request of `n` bytes maps to class `ceil(log2(n)) - 16` clamped to 0..=13, else large. Internal fragmentation is bounded at 2× per buffer and is charged against the budget (AR-I2, AR-I5); the controller sees the charged size, not the requested one, through `AllocStats`.

### e.3 Buffer lifecycle

`Free` (on a class list) → `Live` (returned by `alloc`) → `Free` (on `release`). A `split_at` produces two `Live` buffers sharing one class slot: the arena records the split and releases the slot only when both halves have been released (a per-slot atomic counter kept in a side table indexed by slot address).

### e.4 Device region

Per device: one `cuMemAlloc` of `device_bytes`, the same slab and class structure over device addresses, no host mapping. Device buffers carry `device_ptr()` only.

## f. Algorithms and policies

**f.1 Reservation (host).** `mmap(NULL, size, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE)`; then `madvise(MADV_HUGEPAGE)` if `huge_pages != Absent`; then, if `host_tier == PinnedHost`: with `cuda`, `cuMemHostRegister(ptr, size, CU_MEMHOSTREGISTER_PORTABLE)`, else `mlock(ptr, size)`. `Present` guarantees turn a failure into `AmoruError::Config { name: "host_profile", msg }` (platform bug); `Probed(true)` turns a failure into a fallback (`Host` tier, `huge_pages_active = false`) recorded for the report (contracts d.12: a probed guarantee may fall back, a declared one may not); `Absent` and `Probed(false)` skip the step. `Unknown` never reaches the arena (DS-I6) and is a `Config` error if it does. The fallback from `PinnedHost` to `Host` happens inside `new`, before any buffer exists, so it never becomes a tier move: after `new` returns, `is_pinned()` is final for the run (AR-I6). Touch every page once (`memset` in 2 MiB strides) so the cgroup charge happens at start, not on first use; this is what makes the footprint flat from the first morsel. With `MAP_NORESERVE` the touch is what commits memory; do the touch before `mlock`.

**f.2 Reservation (device).** `cuMemAlloc` of the device budget; on failure, retry once at 90% and report the reduced size; below that, `Config` error.

**f.3 alloc(n, tier).** Class `c` from e.2. Lock class `c`'s list for `tier`; pop; if empty, claim a slab (lock the region's bump lock, check the gap, advance); carve the slab into `512 MiB / size` slots and push all but one; return the one. Update in-use atomics. Large: lock the bump lock, check the gap, carve from the top, record in the large table (sorted by address for coalescing on free).

**f.4 release.** Class buffers: push to the class list. Large: mark free in the large table and coalesce with neighbours; if the freed block is at the current top, retreat the top pointer.

**f.5 Slab claiming policy.** A class never returns a slab; a run whose class mix changes (small morsels early, large later) can strand slabs. Accepted (parent section 8): the controller's morsel targets change slowly, and the 2× bound already covers the mix. `largest_free` reports the gap so the controller can see stranding.

**f.6 Registration for RDMA (feature `rdma`).** After pinning, `ibv_reg_mr` over the whole host region once; `rdma_region()` returns the lkey/rkey and address for the source to hand to a peer. This single call is possible only because f.1 makes one mapping (AR-I3); the reactor states the same dependency from its side (06 f.5).

**f.7 `contains` and `tier_of`.** Two comparisons against the host region's bounds, then one per device region; no lock, no allocation. `tier_of` for a pointer inside the host region returns `PinnedHost` when `is_pinned()` and `Host` otherwise, never the other. Used by `BufferView::of_arrow` and `Payload::table_with` (contracts d.3, d.4) to recover the tier of an Arrow buffer that lost its arena token, and by the adapters to skip the boundary copy (AD-I2).

## g. Concurrency within the component

Per-class mutex (one per class per tier), one bump lock per region, atomics for stats. Lock order within the component: class lock before bump lock (a slab claim holds both, in that order). No lock is held across any call outside the arena. Lock-free alternative for class lists (Treiber stack with tagged pointers) is permitted if the agent shows a benchmark on the reference host where the mutex version's `alloc`+`release` pair exceeds 200 ns at 16 threads; otherwise the mutex stays (preamble 4.2).

## h. Behaviour

**Normal path.** `Arena::new` at run start; sources and placement call `alloc` per morsel; kernels' outputs are copied into arena buffers by the adapters; `release` on drop; at run end the arena is dropped, which unregisters, unpins and unmaps once.

**Edge cases.** `alloc(0)`: returns a class-0 buffer with `len() == 0` (never a null pointer). `alloc` larger than the region: `Alloc` immediately. `split_at` on a large allocation: allowed; both halves release into the large table as separate blocks. Budget below 64 KiB: `Config` error at `new`. More than 8 devices: `Config` error (contracts `AllocStats` has 8 slots).

**Failures.** `mmap` failure: `Config` with errno. `mlock` failure under `Guarantee::Probed(true)`: fall back to `Host`, set `is_pinned = false`, log at warn; under `Present`: `Config`. `cuMemHostRegister` failure: same rule. Device `cuMemAlloc` failure: f.2. Double release (a bug): debug assertion in tests; in release builds the second release is ignored and counted in a `double_release` diagnostic counter surfaced in the report.

## i. Configuration

`morsel.alignment` (64), `page.bytes`, `arena.huge_pages`, `arena.pin`, `budget.host`, `budget.device` (preamble section 5).

## j. Observability

`AllocStats` (contracts) plus `ArenaStats { slabs_by_class: [u32; 14], largest_free: u64, stranded_bytes: u64, double_release: u64, huge_pages_active: bool, pinned: bool }` exposed for the run report. `tracing` events: `arena.reserved` (sizes, pinned, huge) at info; `arena.fallback` at warn with the reason; `arena.alloc_failed` at debug with bytes, tier, in-use.

## k. Tests

**AR-T1 alignment.** 10,000 random-size allocations across tiers (host only in CI); every pointer is a multiple of 64 and of `page_bytes`. AR-I1.

**AR-T2 budget_enforced.** Allocate until failure; sum of charged sizes ≤ region; the failure is `Alloc` with correct `in_use`. AR-I2.

**AR-T3 allocated_once.** `strace`-style test using a counting shim around `mmap`/`mlock` (feature `test-shim`): exactly one `mmap` and at most one `mlock` per run. AR-I3.

**AR-T4 release_o1.** 1 M alloc/release pairs from 16 threads; p99 latency under 1 µs on the reference host (provisional elsewhere). AR-I4.

**AR-T5 stats_exact.** Random alloc/release sequence with a model; `AllocStats` equals the model at every step. AR-I5.

**AR-T6 pin_all_or_nothing.** With memlock forbidden (ulimit in test) and `memlock = Probed(true)`, `PinnedHost` requests fail with `Alloc { tier: PinnedHost }`, `Host` requests succeed and `is_pinned()` is false; with it allowed and `host_tier = PinnedHost`, `PinnedHost` requests succeed, `Host` requests fail with `Alloc { tier: Host }` and `is_pinned()` is true; with memlock forbidden and `memlock = Present`, `new` returns `Config`. AR-I6.

**AR-T7 device_no_host_ptr.** (feature `cuda`, skipped without a device and listed as skipped) `host_ptr()` is None; `as_ref` panics with "Device". AR-I7.

**AR-T8 split_at.** Split, release one half, allocate again: slot is not reused until both halves are released. e.3.

**AR-T9 large_coalesce.** Three adjacent large allocations, free middle then ends; `largest_free` returns the full gap. f.4.

**AR-T10 small_budget_mode.** Region of 256 MiB serves classes 0..=12 by splitting; class 13 fails with `Alloc`. e.1.

**AR-T11 flat_footprint.** After `new`, RSS (from `/proc/self/statm`) is within 2% of the region size and does not grow during AR-T5. f.1.

**AR-T12 contains_and_tier_of.** For every live buffer, `contains(ptr)` is true for its first and last byte and false one byte past its end and for a heap pointer; `tier_of` equals the buffer's tier; on a pinned arena every host `tier_of` is `PinnedHost`, on an unpinned one `Host`; `BufferView::of_arrow` over `into_arrow_buffer` of an arena buffer and over a slice of it reports that tier. f.7, AR-I6.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/region.rs` (f.1, f.2, f.7, mmap and cuda), `src/classes.rs` (e.2, f.3, f.4), `src/large.rs` (large table and coalescing), `src/handle.rs` (`ArenaHandle` impl), `src/stats.rs`, `src/rdma.rs` (feature). `unsafe` permitted in `region.rs` (syscalls, cuda) and `classes.rs`/`large.rs` (pointer arithmetic), each with `// SAFETY:` citing AR-I1 or AR-I2.

Use `libc` for `mmap`, `madvise`, `mlock`; do not use `memmap2` here (no crate maps files; sources read into the arena through the reactor). Do not use `alloc::alloc` for arena memory. The global allocator for the process is `mimalloc`, set in `amoru-runtime`, not here.

Anti-patterns: no per-allocation syscalls; no `Vec<u8>` behind `Buffer`; no growth of the region after `new`; no silent fallback under a `Present` guarantee.

Verify before starting: `MADV_HUGEPAGE` availability on the reference host (`cat /sys/kernel/mm/transparent_hugepage/enabled`); `ulimit -l` in the test container; `cudarc` version pinned in the preamble supports `cuMemHostRegister`.

## m. Open items

None.

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S1, G-I1 | AR-I2, AR-I5 | AR-T2, AR-T5 |
| S13 | AR-I1 | AR-T1 |
| S14 | AR-I6 | AR-T6, AR-T7 |
| G-I7 | AR-I6 | AR-T6 |
| D10, 5.6a flat footprint | AR-I3 | AR-T3, AR-T11 |
| contracts e.1 (one host tier), d.3 `tier_of` | AR-I6, f.7 | AR-T6, AR-T12 |

## o. Deferred (post-v1)

None.
