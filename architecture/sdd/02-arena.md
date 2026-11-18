# Moruna SDD 02: Memory arena (`moruna-arena`)

**Document type:** software design document, component 2 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/moruna-runtime-design.md` section 5.6a; decisions D10; criteria S1, S13, S14
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` (d.3 `Buffer`, `BufferView`, `Allocator`, `AllocStats`; d.12 `Limits`, `HostProfile`, `Guarantee`; e.1 one host tier per run)
**Component location:** `crates/moruna-arena`, Rust
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

**AR-I2. Budget is enforced at allocation.** `alloc` fails with `MorunaError::Alloc` rather than exceeding the tier's budget; the sum of live buffer bytes per tier, including internal fragmentation, never exceeds the region size. Upholds G-I1.

**AR-I3. The region is allocated once.** After `Arena::new` returns, no further `mmap`, `cudaMalloc`, `cudaHostAlloc` or `mlock` call is made for the run; a request that cannot be served from the region fails. Rationale: the flat memory footprint (architecture 5.6a) and the pod's cgroup accounting depend on this, and so does the reactor: registering the host tier with a NIC (feature `rdma`, 06 f.5) is one call only because the host region is one mapping and not many. The reservation is one region for that reason and must not be split into several mappings as an optimisation.

**AR-I4. Free is O(1) and never blocks a worker for longer than a class lock.** `release` puts the slot into the releasing thread's cache under that cache's own lock, or pushes it to a per-class free list under a per-class mutex (or lock-free stack; see l); a cache past its quantum puts slots back under their class locks first (f.3); it never touches the OS.

**AR-I5. Stats are exact.** In use and live are exact when read; the peak is exact to within threads × the refill quantum. `AllocStats.*_in_use` equals the sum of class sizes of live buffers plus live large allocations whenever it is read with no allocation or release in flight: the regions count a thread's cached slots (f.3) as in use from the refill that takes them to the flush that puts them back, and the arena subtracts what the caches hold when it reads. `allocations_total` is each thread's count, summed when read. The host peak (`host_peak`) moves where the regions' count moves, at a refill, a flush and an allocation no cache serves, so it is never below the most in use at once and above it by at most what the caches held then: threads × `REFILL_QUANTUM` (1 MiB a thread). (Amended 2026-11-18: the counters were shared words every allocation and release wrote.)

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
    pub register_rdma: bool,             // feature rdma only; E11, never true in v1
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
    /// The `ArenaStats` of section j, which `Allocator::stats` has no room for.
    /// Added 2026-09-22: section j named the struct and no method exposed it.
    pub fn arena_stats(&self) -> ArenaStats;
    /// The most host bytes in use at once since `reset_host_peak` (or since `new`), exact
    /// across every host region: one mark the regions share. Added 2026-10-29 for warm-up's
    /// peak (SDD 24 p.4, CM-I21).
    pub fn host_peak(&self) -> u64;
    /// Start a new host high-water mark at the bytes in use now.
    pub fn reset_host_peak(&self);
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

`moruna_kernel::{Allocator, Buffer, AllocStats, Tier, TierKind, DeviceId, Guarantee, MorunaError, ALIGNMENT}` and the `ArenaHandle` trait from `buffer.rs`:

```rust
pub trait ArenaHandle: Send + Sync { fn release(&self, ptr: *mut u8, len: usize, tier: Tier); }
```

With feature `cuda`: `cudarc::driver::{CudaDevice, CudaSlice, sys::cuMemHostRegister}` (or `cudaHostAlloc` for the pinned host region; see f.2).

## e. Data model, formats and state machines

### e.1 Region layout

```
host region (host_bytes, rounded DOWN to max(page.bytes, 64 KiB))
┌────────────────────────────────────────────────────────────────────┐
│ slabs (class-assigned, grow upward) │ free │ large allocs (grow downward) │
└────────────────────────────────────────────────────────────────────┘
```

The region is rounded down, never up: rounding `host_bytes` up to a 512 MiB boundary would reserve as much as 511 MiB above the host budget, and f.1 touches every page, so the process would sit above the ceiling before the first morsel was read, which G-I1 and AR-I2 forbid. The preamble wins over this document where they disagree (preamble section 9), so the region is `host_bytes` rounded down to a multiple of `max(page.bytes, 64 KiB)` (PM, 2026-09-22, on the component 2 agent's report). Slabs are claimed from the low end when a class's free list is empty; large allocations are carved from the high end. A slab is not a fixed 512 MiB: it is `slab_bytes(c) = min(512 MiB, max(class_size(c), region_bytes / 8))`, rounded down to a multiple of `class_size(c)`, and when the gap cannot serve that it shrinks to the largest multiple of `class_size(c)` the gap can serve, one slot being the floor. A fixed 512 MiB slab made the region's size a lie: a region of 1.34 GiB could give a slab to two classes and then refused an 80,000 byte allocation with 64 MiB in use, because the third class could not claim its 512 MiB while a gigabyte of the region sat free (PM, 2026-09-22, on the first end-to-end run). A region now serves any allocation that fits its free space, which is what AR-I2 and the budget arithmetic of 11 f.1 both assume. Above about 4 GiB the rule gives 512 MiB again, so nothing changes for a region large enough to have wanted it. The two meet at the `free` gap; when the gap cannot satisfy a slab claim or a large request, the allocation fails (AR-I2). A host region smaller than 512 MiB (budget below that) uses one slab equal to the whole region and serves every class from it by splitting (buddy-style) rather than by dedicated slabs; this is the small-budget mode and is selected when `host_bytes < 1 GiB`.

### e.2 Size classes

| Class | Size | Typical use |
|---|---|---|
| 0 | 64 KiB | trace and metadata buffers |
| 1..=6 | 128 KiB .. 4 MiB | small morsels, column chunks |
| 7..=10 | 8 MiB .. 64 MiB | default morsels |
| 11..=13 | 128 MiB .. 512 MiB | large morsels, staging segment buffers |
| large | > 512 MiB | rounded to page; bump from top |

A request of `n` bytes maps to class `ceil(log2(n)) - 16` clamped to 0..=13, else large. A class that can neither pop a slot nor claim a slab splits one down from a larger class (f.5). Internal fragmentation is bounded at 2× per buffer and is charged against the budget (AR-I2, AR-I5); the controller sees the charged size, not the requested one, through `AllocStats`.

### e.3 Buffer lifecycle

`Free` (on a class list) → `Live` (returned by `alloc`) → `Free` (on `release`). A `split_at` produces two `Live` buffers sharing one class slot: the arena records the split and releases the slot only when both halves have been released (a per-slot atomic counter kept in a side table indexed by slot address).

### e.4 Device region

Per device: one `cuMemAlloc` of `device_bytes`, the same slab and class structure over device addresses, no host mapping. Device buffers carry `device_ptr()` only.

## f. Algorithms and policies

**f.1 Reservation (host).** `mmap(NULL, size, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE)`; then `madvise(MADV_HUGEPAGE)` if `huge_pages != Absent`; then, if `host_tier == PinnedHost`: with `cuda`, `cuMemHostRegister(ptr, size, CU_MEMHOSTREGISTER_PORTABLE)`, else `mlock(ptr, size)`. `Present` guarantees turn a failure into `MorunaError::Config { name: "host_profile", msg }` (platform bug); `Probed(true)` turns a failure into a fallback (`Host` tier, `huge_pages_active = false`) recorded for the report (contracts d.12: a probed guarantee may fall back, a declared one may not); `Absent` and `Probed(false)` skip the step. `Unknown` never reaches the arena (DS-I6) and is a `Config` error if it does. The fallback from `PinnedHost` to `Host` happens inside `new`, before any buffer exists, so it never becomes a tier move: after `new` returns, `is_pinned()` is final for the run (AR-I6). Touch every page once (`memset` in 2 MiB strides) so the cgroup charge happens at start, not on first use; this is what makes the footprint flat from the first morsel. The region's size is therefore not the run's memory ceiling and never was: it is the ceiling less what the process already held before the arena existed, less the reserve, less the state the stateful kernels are expected to hold outside the arena. The facade computes it by that rule and samples the baseline before this call (12 f.1); the controller then takes this region's size as its whole allowance rather than subtracting the baseline a second time (11 f.1). An arena sized at the ceiling leaves the controller nothing, because f.1's touch makes the arena part of the baseline the moment it exists. With `MAP_NORESERVE` the touch is what commits memory; do the touch before `mlock`.

**f.1 Giving back what no buffer holds (RB4, 2026-11-14).** The touch charges the whole region at start, and the region `new` made never drains (it is what every zero-length buffer points into). So a ceiling that falls below it is met in place: `cap_host(Some(bytes))` makes the arena refuse any host allocation that would take what it has in use past `bytes` (an `Alloc` like any other the regions cannot serve; `available` and `host_capacity` answer to the cap), and `release_free()` gives every whole page no buffer holds back to the operating system, keeping the mapping: the top area's gap and freed large blocks under the top lock, and the free slots of every class of a page or more under that class's lock, the locks an allocation takes those bytes under, so no buffer can be handed out of a range while it is given back. Linux uses `MADV_DONTNEED`, which uncharges the cgroup at once; macOS `MADV_FREE_REUSABLE`, which leaves the footprint it enforces limits against; a page-locked region gives nothing back. A page given back faults in zeroed, and is charged, when a buffer is next carved from it. Buffers already out stay where they are and drain as the run moves on. `cap_host(None)` lifts the cap. The limits watcher is the one caller (12, MH 4.4). AR-T15.

**f.1 The touch is asked for, not assumed (RB4, 2026-11-14).** `ArenaConfig::pretouch`, off by default, decides whether `new` and `grow` touch every page. Off, a page is charged when a buffer first lands on it, and a region costs what the run puts in it. Touching a 6.9 GB region (a 24 GiB budget) put the better part of a second before the first morsel of every run on the benchmark VM, and charged gigabytes a run never used. The controller's model counts the whole region as resident whatever is touched (11 f.3), so a region that is not resident is a figure it errs above, never below. With `pretouch` on, the footprint is flat from the first morsel as before (AR-T11); off, `new` charges almost nothing and the bytes written are charged (AR-T16).

**f.2 Reservation (device).** `cuMemAlloc` of the device budget; on failure, retry once at 90% and report the reduced size; below that, `Config` error.

**f.3 alloc(n, tier).** Class `c` from e.2. Lock class `c`'s list for `tier`; pop; if empty, claim a slab of `slab_bytes(c)` (e.1) (lock the region's bump lock, check the gap, advance; when the gap is smaller than `slab_bytes(c)` the claim shrinks to the largest multiple of `class_size(c)` that fits, and fails only when not one slot fits); carve the slab into `slab / size` slots and push all but one; return the one. Update in-use atomics. Large: lock the bump lock, check the gap, carve from the top, record in the large table (sorted by address for coalescing on free).

**f.3 Each thread keeps a cache of free slots (2026-11-18).** Eight threads allocating at once queued on the class lists' locks, the region list's lock and the shared counters: a 64 KiB allocation and release took 34 ns on one thread and 1,092 to 1,295 ns a thread on eight. So each thread has a *lane* per arena, holding a cache of free slots of the classes up to `REFILL_QUANTUM` (1 MiB, so classes 0..=4) under a lock of its own, which no other thread takes except to flush it. A host allocation of a cached class takes a slot from the cache. When the cache has none of the class, it refills from the oldest region that is not draining and has one, taking up to half a quantum of slots under the class lock once, and never more than a quantum cached in all. A release puts the slot into the releasing thread's cache. When that would take the cache past a quantum, it first puts slots back on their class lists, other classes first, down to half. Every host buffer holds its thread's handle for its region, so a release needs no lookup and no region-list lock. A slot in a cache is free and on no list: no allocation, merge or `release_free` sees it until it is handed out or put back. Exactness is kept where it matters:
- A refusal first flushes every thread's cache and then decides, so nothing is refused while slots sit in a cache (AR-T18).
- A shrink flushes every cache of the regions it marks draining, under the region list's write lock, before it retires any, and a slot released into a draining region goes back to it.
- `largest_free`, `release_free` and `arena_stats` flush first, and a thread's caches are flushed when it ends.
- A cap (f.1) or a live side region (RB5) sends every allocation through the regions, which check the budget as a whole.

At most threads × `REFILL_QUANTUM` bytes sit in caches; `stranded_bytes` counts them.

The counters are the threads' too (AR-I5). A refill charges the region's count and the tier's watermark for all the slots it takes, and a flush releases them, so an allocation or a release through the cache writes no word another thread writes: in use is read as the regions' count less what the caches hold, and the allocation count is each thread's summed. Everything a thread writes per allocation and release (its lane, its handle for the region, its cache's entry for the region, with the slots held inline) is aligned to 128 bytes, the cache line of the machines Moruna runs on and the pair of lines x86 fetches together. Two threads' small heap objects on one line cost as much as a shared counter: aligning them took eight threads from 335 to 403 ns a pair to 112 to 119.

**f.3 A large request with room but no gap gets a side region (RB5, 2026-11-14).** A host request of at least 1 MiB that every region refuses, though the budget has room for its charge (`in use + charge <= capacity`, capped), is served from a *side region* of its own: mapped untouched, draining from birth so it takes nothing else, and unmapped when the buffer comes home. Before it is mapped, every free page of the regions goes back to the operating system (`release_free`), so the arena is resident in what is in use and a side region on top never takes the process past the regions' bytes the controller counts (11 f.3). While a side region is live, every host allocation is checked against the budget as a whole, so the regions together never hold more than their capacity wherever the bytes are; `available` counts a side region's bytes as used. A side region is not capacity: the watcher's arithmetic and its pacing do not count it (`regions_capacity`, `draining_bytes`). Not for a page-locked or pre-touched arena, whose regions must be prepared at reservation. Before, a 128 MiB sink buffer was refused with more than a gigabyte free in pieces, and Enrich over the whole dataset ended at 10.6 s. AR-T17.

**f.4 release.** Class buffers: into the releasing thread's cache when it keeps the class and the region is not draining (f.3), otherwise push to the class list. Large: mark free in the large table and coalesce with neighbours; if the freed block is at the current top, retreat the top pointer.

**f.5 Slab claiming policy.** A class never returns a slab to the bump area; a run whose class mix changes (small morsels early, large later) can strand slabs. Accepted (parent section 8): the controller's morsel targets change slowly, and the 2× bound already covers the mix. Stranding may not become a refusal, though: when a class's free list is empty and the gap cannot give it even one slot, it splits a free slot of a larger class down buddy-style, exactly as the small-budget mode does (e.1, f.3), so stranding costs a run fragmentation and never an allocation the region could serve. It strands upward only: a slab cut up for a small class cannot serve a large one. `largest_free` reports what the region can still serve so the controller can see stranding: the largest class size on a free list, and, from the gap, the gap itself when it is at least a slab's largest class and otherwise the largest class size the gap can hold.

**f.6 Registration for RDMA (feature `rdma`).** After pinning, `ibv_reg_mr` over the whole host region once; `rdma_region()` returns the lkey/rkey and address for the source to hand to a peer. This single call is possible only because f.1 makes one mapping (AR-I3); the reactor states the same dependency from its side (06 f.5).

**f.7 `contains` and `tier_of`.** Two comparisons against the host region's bounds, then one per device region; no lock, no allocation. `tier_of` for a pointer inside the host region returns `PinnedHost` when `is_pinned()` and `Host` otherwise, never the other. Used by `BufferView::of_arrow` and `Payload::table_with` (contracts d.3, d.4) to recover the tier of an Arrow buffer that lost its arena token, and by the adapters to skip the boundary copy (AD-I2).

## g. Concurrency within the component

Per-class mutex (one per class per tier), one bump lock per region, one lock per thread's cache (f.3), atomics for stats, each thread's own where it can be (AR-I5). Lock order within the component: the region list, then the registry of lanes, then a thread's cache, then a class lock, then the bump lock (a slab claim holds the class and bump locks, in that order). Nothing that holds a cache waits for the region list. No lock is held across any call outside the arena. Lock-free alternative for class lists (Treiber stack with tagged pointers) is permitted if the agent shows a benchmark on the reference host where the mutex version's `alloc`+`release` pair exceeds 200 ns at 16 threads; otherwise the mutex stays (preamble 4.2).

## h. Behaviour

**Normal path.** `Arena::new` at run start; sources and placement call `alloc` per morsel; kernels' outputs are copied into arena buffers by the adapters; `release` on drop; at run end the arena is dropped, which unregisters, unpins and unmaps once.

**Edge cases.** `alloc(0)`: returns a buffer with `len() == 0` and a non-null pointer, which is charged nothing and owns no class-0 slot. It cannot own one: a release carries the buffer's own length, so a zero-length release cannot retire a slot, and `split_at(0)` would otherwise hand the zero-length half a slot that the other half is still using. It counts in `allocations_total` (PM, 2026-09-22). `alloc` larger than the region: `Alloc` immediately. `split_at` on a large allocation: allowed; both halves release into the large table as separate blocks. Budget below 64 KiB: `Config` error at `new`. More than 8 devices: `Config` error (contracts `AllocStats` has 8 slots).

**Failures.** `mmap` failure: `Config` with errno. `mlock` failure under `Guarantee::Probed(true)`: fall back to `Host`, set `is_pinned = false`, log at warn; under `Present`: `Config`. `cuMemHostRegister` failure: same rule. Device `cuMemAlloc` failure: f.2. Double release (a bug): debug assertion in tests; in release builds the second release is ignored and counted in a `double_release` diagnostic counter surfaced in the report.

## i. Configuration

`morsel.alignment` (64), `page.bytes`, `arena.huge_pages`, `arena.pin`, `budget.host`, `budget.device` (preamble section 5).

## j. Observability

`AllocStats` (contracts) plus `ArenaStats { slabs_by_class: [u32; 14], largest_free: u64, stranded_bytes: u64, double_release: u64, huge_pages_active: bool, pinned: bool }` exposed for the run report. `tracing` events: `arena.reserved` (sizes, pinned, huge) at info; `arena.fallback` at warn with the reason; `arena.alloc_failed` at debug with bytes, tier, in-use.

## k. Tests

**AR-T1 alignment.** 10,000 random-size allocations across tiers (host only in CI); every pointer is a multiple of 64 and of `page_bytes`. AR-I1.

**AR-T2 budget_enforced.** Allocate until failure; sum of charged sizes ≤ region; the failure is `Alloc` with correct `in_use`. AR-I2.

**AR-T3 allocated_once.** A counting shim around `mmap`/`mlock`: exactly one `mmap` and at most one `mlock` per run. AR-I3. The shim is two atomic counters and an accessor, always compiled rather than behind an off-by-default feature, because a feature nothing enables means the gate never runs this test and AR-I3 goes unproved (PM, 2026-09-22).

**AR-T4 release_o1.** 1 M alloc/release pairs from 16 threads; p99 latency under 1 µs on the reference host (provisional elsewhere). AR-I4.

**AR-T5 stats_exact.** Random alloc/release sequence of up to 2 MiB with a model, so classes a cache keeps and some it does not; `AllocStats` equals the model at every step, and `host_peak` lies between the model's peak and that plus one quantum. Then four threads at once in lockstep rounds, one step each a round, each with its own model: after every round `AllocStats` equals the sum of the models, and `host_peak` lies between the most the models held together at a round's end and the most they can have held within a round plus four quanta. Without the charge a refill makes, both fail. AR-I5.

**AR-T6 pin_all_or_nothing.** With memlock forbidden (ulimit in test) and `memlock = Probed(true)`, `PinnedHost` requests fail with `Alloc { tier: PinnedHost }`, `Host` requests succeed and `is_pinned()` is false; with it allowed and `host_tier = PinnedHost`, `PinnedHost` requests succeed, `Host` requests fail with `Alloc { tier: Host }` and `is_pinned()` is true; with memlock forbidden and `memlock = Present`, `new` returns `Config`. AR-I6.

**AR-T7 device_no_host_ptr.** (feature `cuda`, skipped without a device and listed as skipped) `host_ptr()` is None; `as_ref` panics with "Device". AR-I7.

**AR-T8 split_at.** Split, release one half, allocate again: slot is not reused until both halves are released. e.3.

**AR-T9 large_coalesce.** Three adjacent large allocations, free middle then ends; `largest_free` returns the full gap. f.4.

**AR-T10 small_budget_mode.** Region of 256 MiB serves classes 0..=12 by splitting; class 13 fails with `Alloc`. e.1.

**AR-T13 region_serves_its_free_space.** A 1.5 GiB region takes a mixed workload across classes 0..=10 and the large path, in rotation, until an allocation genuinely cannot fit: every failure comes with `largest_free` below the request and `in_use` plus the free lists accounting for the region, never with a gigabyte free and a refused 80,000 bytes. Releasing everything makes the region serve the same workload again. e.1, f.3, AR-I2.

**AR-T14 the_host_peak_is_the_most_in_use_at_once_and_resets.** A freed spike is still the peak; a reset starts the mark at what is in use; a buffer only a grown region can hold charges the same mark. (Unit test in `src/lib.rs`, 2026-10-29.)

**AR-T15 a_capped_arena_gives_back_what_it_does_not_hold.** (`src/lib.rs`) With a buffer kept and one freed, `release_free` gives back at least the freed buffer; under a cap of what is in use plus 4 MiB an 8 MiB allocation is refused, `available` is the 4 MiB left, a 2 MiB one is served and writable; with the cap lifted a 16 MiB one is served. f.1, RB4.

**AR-T17 side_region.** (`tests/ar_arena_large.rs`) A 128 MiB region filled with 2 MiB buffers and every other one freed refuses 8 MiB from its regions, serves it from a side region, counts its bytes against `available`, refuses a 2 MiB request once the budget as a whole is spent though a hole remains, and serves it again once the side buffer is dropped. AR-T13 accepts a refusal for the budget as a whole while a side region is live. f.3, RB5.

**AR-T18 refusal_flushes_caches.** (`tests/ar_arena.rs`) Another thread fills a 4 MiB region with 512 KiB buffers and releases them, so a quantum of slots sits in its cache, and lives on. This thread is then served all eight slots, and the region is full after. Without the flush the seventh is refused: 512 KiB is below a side region's least, so nothing else serves it. f.3.

**AR-T16 lazy_footprint.** (`tests/ar_arena_large.rs`) With `pretouch` off, resident memory after `new` grows by under a tenth of a 256 MiB region, and writing a 64 MiB buffer charges at least 60 MiB. Linux, from `/proc/self/statm`. f.1, RB4.

**AR-T11 flat_footprint.** After `new`, RSS (from `/proc/self/statm`) is within 2% of the region size and does not grow during AR-T5. f.1.

**AR-T12 contains_and_tier_of.** For every live buffer, `contains(ptr)` is true for its first and last byte and false one byte past its end and for a heap pointer; `tier_of` equals the buffer's tier; on a pinned arena every host `tier_of` is `PinnedHost`, on an unpinned one `Host`; `BufferView::of_arrow` over `into_arrow_buffer` of an arena buffer and over a slice of it reports that tier. f.7, AR-I6.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/region.rs` (f.1, f.2, f.7, mmap and cuda), `src/classes.rs` (e.2, f.3, f.4), `src/large.rs` (large table and coalescing), `src/handle.rs` (`ArenaHandle` impl), `src/stats.rs`, `src/rdma.rs` (feature). `unsafe` permitted in `region.rs` (syscalls, cuda) and `classes.rs`/`large.rs` (pointer arithmetic), each with `// SAFETY:` citing AR-I1 or AR-I2.

Use `libc` for `mmap`, `madvise`, `mlock`; do not use `memmap2` here (no crate maps files; sources read into the arena through the reactor). Do not use `alloc::alloc` for arena memory. The process uses the platform allocator; its glibc tuning is set in `moruna-runtime`, not here (F8.9).

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
