//! Moruna component 2, the memory arena: the only source of payload memory in the process.
//!
//! Design: `architecture/sdd/02-arena.md`. At start the arena reserves the host budget as
//! one contiguous, aligned region, backs it with huge pages when the host offers them and
//! page-locks it when the run's host tier is `PinnedHost`, and then hands out `Buffer`s
//! from it for the rest of the run (AR-I3). On a device host it does the same per device.
//! It is why the budget is exact: bytes not in the arena are not morsel bytes.
//!
//! The arena implements the contract's `Allocator` (contracts d.3) and the `ArenaHandle`
//! every `Buffer` releases to; consumers reach it through `Arc<dyn Allocator>`.
//!
//! The host side is a list of regions. A run whose budget never moves has one, made at
//! `new`, exactly as before. When the machine grows under the run, the facade's watcher calls
//! [`Arena::grow`], which maps and touches one more region; when it shrinks, [`Arena::shrink`]
//! marks the newest grown regions *draining*: they take no new allocation, and each is unmapped
//! when its last buffer comes home. Neither method is on the contract's
//! `Allocator`, so no consumer can call them: they are the facade's alone (MH 4.4).
//!
//! On granted memory ([`Arena::with_backing`]) the budget is the arena's from the start, as a
//! mapping's address space is, and its memory is made as the run first needs it: a small
//! region at start, then steps of what is in use (at most [`GRANTED_STEP_MAX`]), each made
//! ahead of need on a thread of its own, or when an allocation finds the regions full. A
//! mapping's pages come the same way, when a buffer first lands on them (RB4).

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible function here returns the contract's `MorunaError` (contracts d.14), whose
// size is fixed by that crate and is above clippy's 128 byte threshold. The arena may not
// box it: the error type crosses every component boundary and is the contract's to change.
#![allow(clippy::result_large_err)]

mod classes;
mod handle;
mod index;
mod lanes;
mod large;
pub mod region;
mod stats;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use moruna_kernel::{
    AllocStats, Allocator, Buffer, DeviceId, Guarantee, MorunaError, Result, Tier, TierKind,
};

use crate::classes::{GRANULE, SMALL_BUDGET_BYTES, Space, Watermark};
use crate::index::RangeIndex;
use crate::region::Mapping;

pub use crate::lanes::REFILL_QUANTUM;
pub use crate::region::{Granted, HostBacking};
pub use crate::stats::ArenaStats;

/// What a `Buffer` holds to find its way home (contracts d.3).
type Handle = Arc<dyn moruna_kernel::ArenaHandle>;

/// How the facade builds the arena (d.1). Every value comes from discovery or the
/// controller: the arena decides nothing, it reserves what it is told to reserve.
#[derive(Clone, Debug)]
pub struct ArenaConfig {
    /// Host region size, the host budget (preamble section 5, `budget.host`).
    pub host_bytes: u64,
    /// The run's one host tier, `Host` or `PinnedHost`, from `Discovered.host_tier`
    /// (03 d.1). The arena will not construct both (AR-I6, contracts e.1).
    pub host_tier: TierKind,
    /// Device regions to reserve, in bytes per device (`budget.device`).
    pub device_bytes: Vec<(DeviceId, u64)>,
    /// Page size for this host, from `Limits` (`page.bytes`).
    pub page_bytes: usize,
    /// From `HostProfile`; `Present`, `Absent` or `Probed(_)`, never `Unknown` (DS-I6).
    pub huge_pages: Guarantee,
    /// From `HostProfile`; the same rule.
    pub memlock: Guarantee,
    /// Register the host region with the NIC (feature `rdma`). Reserved for the multi-node
    /// extension: `Arena::new` refuses a run that asks for it with `Unsupported("rdma")`
    /// (E11, CT-I11).
    pub register_rdma: bool,
    /// Touch every page of the host region at `new` and `grow`, so the whole region is charged
    /// at start and the footprint is flat from the first morsel (f.1, AR-T11). Off by default
    /// (RB4, 2026-11-14): a page is charged when a buffer first lands on it. Touching a 6.9 GB
    /// region took the better part of a second before the first morsel on the benchmark VM, and
    /// charged gigabytes a run never used; the controller's model already counts the whole
    /// region as resident (11 f.3), so a region that is not is only a figure it errs above.
    pub pretouch: bool,
}

impl Default for ArenaConfig {
    fn default() -> ArenaConfig {
        ArenaConfig {
            host_bytes: 0,
            host_tier: TierKind::Host,
            device_bytes: Vec::new(),
            page_bytes: 4096,
            huge_pages: Guarantee::Absent,
            memlock: Guarantee::Absent,
            register_rdma: false,
            pretouch: false,
        }
    }
}

/// The size a grown region is rounded up to: the huge page, so a region is backed by whole
/// huge pages when the host has them and a small change of the budget is never a region.
pub const REGION_ROUNDING: u64 = 2 * 1024 * 1024;

/// The region a granted arena makes at start, when its budget is larger.
pub const FIRST_GRANTED_REGION: u64 = 32 * 1024 * 1024;

/// The most a granted arena adds in one step for allocations a region holds several of
/// (stream C, 2026-11-17). A step is what is in use, at least [`GRANTED_STEP_MIN`] and at most
/// this, so what the arena has made stays within a step or two of what the run holds,
/// whatever its budget. An allocation larger than a step gets a region of its own size.
pub const GRANTED_STEP_MAX: u64 = 64 * 1024 * 1024;

/// The least a granted arena adds in one step.
pub const GRANTED_STEP_MIN: u64 = 16 * 1024 * 1024;

/// When the free bytes of the regions a granted arena has made fall below this, it makes its
/// next step ahead of need, on a thread of its own: the kernel zeroes a region's pages as it
/// makes it, which a worker that found the regions full would otherwise wait for.
pub const GRANTED_AHEAD_BELOW: u64 = 32 * 1024 * 1024;

/// One host region: the region `new` reserved, or one `grow` added.
struct HostRegion {
    space: Space,
    /// Held so the mapping is released exactly once, when the region is retired or the arena
    /// drops. The `Space` addresses this mapping, so it is dropped after it.
    _mapping: Mapping,
    /// Set by `shrink`: the region takes no new allocation and is unmapped when its last buffer
    /// is released.
    draining: AtomicBool,
    /// True for the region `new` made, and for each region a granted arena made out of its
    /// budget. None drains: the run's first region is what every zero-length buffer points
    /// into, and the budget arithmetic of 11 f.1 sized them all.
    base: bool,
    /// True for a side region (RB5, [`Inner::alloc_side`]): one large buffer the other regions
    /// had room for by charge but not in one piece. Draining from birth, so it takes nothing
    /// else, and unmapped when that buffer comes home; not capacity, so neither the watcher's
    /// arithmetic nor its pacing counts it.
    side: bool,
}

/// The smallest request a side region is made for: below it a refusal is the arena being full,
/// not fragmented, and a region per request would cost a mapping for each.
const SIDE_MIN: u64 = 1024 * 1024;

/// One reserved device region and the size-class allocator over it.
struct RegionAlloc {
    space: Space,
    /// Held so the mapping is released exactly once, when the arena drops (AR-I3). The
    /// `Space` addresses this mapping, so it is dropped after it.
    _mapping: Mapping,
    /// Device index for `AllocStats::device_in_use`; 0 for the host region.
    index: usize,
}

/// The most host regions the lookup index holds; with more, `tier_of` asks the region list
/// under its lock. A run's regions are its first, its budget's steps of 16 MiB or more, what it
/// grew by, and its live side regions: a few hundred at most on a node.
const HOST_INDEX_REGIONS: usize = 4096;

/// The host regions held for writing ([`Inner::write_host`]): when it is let go, the lookup
/// index is published from the list first, while the write lock still keeps every reader of
/// the list and every allocation out.
struct HostWriter<'a> {
    guard: RwLockWriteGuard<'a, Vec<Arc<HostRegion>>>,
    index: &'a RangeIndex,
}

impl std::ops::Deref for HostWriter<'_> {
    type Target = Vec<Arc<HostRegion>>;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl std::ops::DerefMut for HostWriter<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl Drop for HostWriter<'_> {
    fn drop(&mut self) {
        self.index
            .publish(self.guard.iter().map(|region| region.space.range()));
    }
}

/// Everything a `Buffer` needs to find its way home. `Buffer`s hold an `Arc` of this, so
/// the regions outlive every buffer cut from them.
struct Inner {
    /// The host regions, oldest first. Read-locked by an allocation a thread's cache cannot
    /// serve (`lanes`), and write-locked only by `grow`, `shrink`, a new region of the budget and
    /// the release that retires a drained region.
    host: RwLock<Vec<Arc<HostRegion>>>,
    /// The host regions' bounds for `tier_of`, which reads them with no lock; published from
    /// `host` each time its write lock is let go (`write_host`).
    host_index: RangeIndex,
    /// Every host region's charges together, and their high-water mark.
    host_watermark: Arc<Watermark>,
    devices: Vec<RegionAlloc>,
    host_tier: Tier,
    page_bytes: usize,
    pinned: bool,
    huge_pages_active: bool,
    allocations: AtomicU64,
    foreign: AtomicU64,
    /// Payload bytes a source decoded or a sink encoded with the CPU, and bytes an
    /// adapter copied once at the kernel boundary. The arena owns `AllocStats`, so it
    /// owns the counters; the components that do the copying report them through
    /// `Allocator::note_payload_copy` and `note_boundary_copy` (contracts d.3).
    payload_copies: AtomicU64,
    boundary_copies: AtomicU64,
    /// What `grow` needs to prepare a region the way `new` prepared the first.
    align: u64,
    huge_pages: Guarantee,
    memlock: Guarantee,
    pretouch: bool,
    /// Regions retired after draining, and the bytes they held (section j).
    retired_regions: AtomicU64,
    retired_bytes: AtomicU64,
    /// The most host bytes the arena may have in use, set when the ceiling falls below what its
    /// regions hold and the first region cannot drain (RB4); `u64::MAX` when uncapped.
    host_cap: AtomicU64,
    /// Side regions mapped and not yet retired (RB5): while any is, every host allocation is
    /// checked against the budget as a whole.
    side_live: AtomicU64,
    /// Side regions made over the run.
    side_made: AtomicU64,
    /// Double releases counted by regions that have since been retired.
    retired_double_release: AtomicU64,
    /// Where host regions come from when the process does not map them itself (the data OS's
    /// granted memory objects); `None` maps them with `mmap`.
    backing: Option<Arc<dyn HostBacking>>,
    /// Host bytes of the budget not yet made into regions: granted memory only, and zero once
    /// the run has needed all of it. Changed only under the host write lock, so a reader that
    /// holds the read lock sees it and the regions agree.
    uncommitted: AtomicU64,
    /// The host budget a granted arena makes its regions out of; zero for a mapped arena.
    granted_budget: u64,
    /// Held while a region is made out of `uncommitted`, so two allocations that both find the
    /// regions full make one region, not two, and a region made ahead is not made twice.
    committing: Mutex<()>,
    /// The thread making the next region ahead of need, while one is.
    ahead: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Regions made ahead of need over the run.
    ahead_made: AtomicU64,
    /// The arena's identity, by which a thread finds its lane ([`lanes`]).
    id: u64,
    /// Each thread's lane, the handle its buffers hold ([`lanes::Lane`]).
    lanes: lanes::Registry,
}

/// The memory arena (d.1).
pub struct Arena {
    inner: Arc<Inner>,
}

impl Arena {
    /// Reserve and prepare every region (f.1, f.2). Fails if any region cannot be created
    /// at the requested size and does not partially succeed: a failure after the host
    /// region exists releases it before returning (AR-I3).
    pub fn new(cfg: ArenaConfig) -> Result<Arc<Arena>> {
        Arena::build(cfg, None)
    }

    /// Reserve the host budget on memory `backing` grants, and every device region as
    /// [`Arena::new`] does. The budget is the arena's capacity from the start, but only its
    /// first region, [`FIRST_GRANTED_REGION`] at most, is made now. Each further region is a
    /// step of what is in use, between [`GRANTED_STEP_MIN`] and [`GRANTED_STEP_MAX`] (an
    /// allocation larger than a step gets a region of its own size), made ahead of need on a
    /// thread of its own when the free bytes of the regions fall below [`GRANTED_AHEAD_BELOW`],
    /// or when an allocation finds them full, until the budget is made. The budget sizes
    /// nothing: what is made stays within a step and the bytes kept ahead of what the run
    /// holds, and a run that holds a few hundred megabytes of a 20 GiB budget makes a few
    /// hundred megabytes, without a worker waiting for the kernel to zero them. The regions are the
    /// run's fixed budget: none drains, and no page of them is given back, so a reused page is
    /// never zeroed again (`architecture/data-os.md` C4). `pretouch` and `memlock` do not
    /// apply: the backing's pages are present and resident when it gives them.
    pub fn with_backing(cfg: ArenaConfig, backing: Arc<dyn HostBacking>) -> Result<Arc<Arena>> {
        Arena::build(cfg, Some(backing))
    }

    fn build(cfg: ArenaConfig, backing: Option<Arc<dyn HostBacking>>) -> Result<Arc<Arena>> {
        if cfg.register_rdma {
            return Err(MorunaError::Unsupported("rdma"));
        }
        if cfg.page_bytes == 0 || !cfg.page_bytes.is_power_of_two() {
            return Err(MorunaError::Config {
                name: "page.bytes",
                msg: format!("{} is not a power of two", cfg.page_bytes),
            });
        }
        let host_tier = match cfg.host_tier {
            TierKind::Host => Tier::Host,
            TierKind::PinnedHost => Tier::PinnedHost,
            TierKind::Device | TierKind::Disk | TierKind::Remote => {
                return Err(MorunaError::Config {
                    name: "arena.pin",
                    msg: format!(
                        "host_tier is {:?}; a run's host tier is Host or PinnedHost (contracts e.1)",
                        cfg.host_tier
                    ),
                });
            }
        };
        if cfg.host_bytes < GRANULE {
            return Err(MorunaError::Config {
                name: "budget.host",
                msg: format!(
                    "host budget {} is below the smallest size class ({GRANULE} bytes)",
                    cfg.host_bytes
                ),
            });
        }
        if cfg.device_bytes.len() > 8 {
            return Err(MorunaError::Config {
                name: "budget.device",
                msg: format!(
                    "{} devices; AllocStats accounts for at most 8 (contracts d.3)",
                    cfg.device_bytes.len()
                ),
            });
        }
        let align = (cfg.page_bytes as u64).max(GRANULE);
        let host_bytes = region::align_down(cfg.host_bytes, align);
        if host_bytes < GRANULE {
            return Err(MorunaError::Config {
                name: "budget.host",
                msg: format!(
                    "host budget {} leaves nothing once rounded down to {align} bytes",
                    cfg.host_bytes
                ),
            });
        }
        let mut uncommitted = 0;
        let (hosts, pinned) = match &backing {
            None => {
                let host = region::new_host(
                    host_bytes,
                    align,
                    host_tier == Tier::PinnedHost,
                    cfg.huge_pages,
                    cfg.memlock,
                    cfg.pretouch,
                )?;
                let pinned = host.pinned;
                (vec![host], pinned)
            }
            // Granted memory is resident: a run whose tier is page-locked has it so. One region
            // now; the rest of the budget as the run needs it (`Inner::alloc_committed`).
            Some(backing) => {
                let first = host_bytes
                    .min(FIRST_GRANTED_REGION)
                    .min(region::granted_chunk(backing.as_ref(), align));
                let hosts = granted(backing.as_ref(), first, align)?;
                let made: u64 = hosts.iter().map(|h| h.mapping.bytes()).sum();
                uncommitted = host_bytes - made;
                (hosts, host_tier == Tier::PinnedHost)
            }
        };
        let huge_pages_active = hosts.iter().all(|h| h.huge_pages_active);
        let host_tier = if pinned { Tier::PinnedHost } else { Tier::Host };
        let host_watermark = Arc::new(Watermark::default());
        let host_allocs: Vec<Arc<HostRegion>> = hosts
            .into_iter()
            .map(|host| {
                Arc::new(HostRegion {
                    space: Space::new(
                        host.mapping.base(),
                        host.mapping.bytes(),
                        host_tier,
                        host.mapping.bytes() < SMALL_BUDGET_BYTES,
                        Arc::clone(&host_watermark),
                    ),
                    _mapping: host.mapping,
                    draining: AtomicBool::new(false),
                    base: true,
                    side: false,
                })
            })
            .collect();
        let devices = Self::devices(&cfg, align)?;
        let granted_budget = if backing.is_some() { host_bytes } else { 0 };
        tracing::info!(
            target: "arena.reserved",
            host_bytes,
            pinned,
            huge_pages = huge_pages_active,
            regions = host_allocs.len(),
            granted = backing.is_some(),
            uncommitted,
            devices = devices.len(),
            "arena reserved"
        );
        let host_index = RangeIndex::new(HOST_INDEX_REGIONS);
        host_index.publish(host_allocs.iter().map(|r| r.space.range()));
        Ok(Arc::new(Arena {
            inner: Arc::new(Inner {
                host: RwLock::new(host_allocs),
                host_index,
                host_watermark,
                devices,
                host_tier,
                page_bytes: cfg.page_bytes,
                pinned,
                huge_pages_active,
                allocations: AtomicU64::new(0),
                foreign: AtomicU64::new(0),
                payload_copies: AtomicU64::new(0),
                boundary_copies: AtomicU64::new(0),
                align,
                huge_pages: cfg.huge_pages,
                memlock: cfg.memlock,
                pretouch: cfg.pretouch,
                retired_regions: AtomicU64::new(0),
                retired_bytes: AtomicU64::new(0),
                host_cap: AtomicU64::new(u64::MAX),
                side_live: AtomicU64::new(0),
                side_made: AtomicU64::new(0),
                retired_double_release: AtomicU64::new(0),
                backing,
                uncommitted: AtomicU64::new(uncommitted),
                granted_budget,
                committing: Mutex::new(()),
                ahead: Mutex::new(None),
                ahead_made: AtomicU64::new(0),
                id: lanes::next_id(),
                lanes: lanes::Registry::default(),
            }),
        }))
    }

    /// Add `bytes` of host memory as one new region: rounded up to
    /// [`REGION_ROUNDING`], mapped, advised and touched exactly as `new` prepared the first
    /// region, and page-locked when the arena is (AR-I6: pinning stays all or nothing, so a
    /// grown region that cannot be locked is refused, not added unlocked). Returns the bytes
    /// added.
    ///
    /// The facade's watcher is the only caller: the method is not on the contract's
    /// `Allocator`, so no consumer reaches it. The caller bounds the total by
    /// `budget.elastic.memory_max_bytes`; the arena reserves what it is told to (a).
    pub fn grow(&self, bytes: u64) -> Result<u64> {
        let inner = &self.inner;
        let bytes = region::align_up(bytes, REGION_ROUNDING.max(inner.align));
        if bytes < GRANULE {
            return Ok(0);
        }
        if let Some(backing) = &inner.backing {
            let mut added = 0;
            for host in granted(backing.as_ref(), bytes, inner.align)? {
                added += host.mapping.bytes();
                inner.write_host().push(Arc::new(HostRegion {
                    space: Space::new(
                        host.mapping.base(),
                        host.mapping.bytes(),
                        inner.host_tier,
                        host.mapping.bytes() < SMALL_BUDGET_BYTES,
                        Arc::clone(&inner.host_watermark),
                    ),
                    _mapping: host.mapping,
                    draining: AtomicBool::new(false),
                    base: false,
                    side: false,
                }));
            }
            tracing::info!(target: "arena.grow", bytes = added, "arena grew by granted regions");
            return Ok(added);
        }
        let host = region::new_host(
            bytes,
            inner.align,
            inner.pinned,
            inner.huge_pages,
            inner.memlock,
            inner.pretouch,
        )?;
        if host.pinned != inner.pinned {
            // The mapping drops here and is released: the grown region could not be locked, and a
            // run has one host tier (AR-I6).
            return Err(MorunaError::Config {
                name: "arena.pin",
                msg: format!(
                    "a grown region of {bytes} bytes could not be page-locked, and the arena is;                      the region is not added (AR-I6)"
                ),
            });
        }
        let added = host.mapping.bytes();
        let grown = Arc::new(HostRegion {
            space: Space::new(
                host.mapping.base(),
                added,
                inner.host_tier,
                added < SMALL_BUDGET_BYTES,
                Arc::clone(&inner.host_watermark),
            ),
            _mapping: host.mapping,
            draining: AtomicBool::new(false),
            base: false,
            side: false,
        });
        inner.write_host().push(grown);
        tracing::info!(target: "arena.grow", bytes = added, "arena grew by a region");
        Ok(added)
    }

    /// Give back at least `bytes` of host memory: mark the newest grown regions
    /// draining until the bytes marked reach `bytes` or no grown region is left. A draining
    /// region takes no new allocation and is unmapped when its last buffer is released;
    /// one that is already empty is unmapped here. Returns the bytes marked, which may exceed
    /// `bytes` by part of one region and may fall short of it: the region `new` made never
    /// drains, so what is below it is the controller's to give back by holding less.
    ///
    /// Shrink is eventual: the caller learns when it is complete from
    /// [`Arena::draining_bytes`] reaching zero. The facade's watcher is the only caller.
    pub fn shrink(&self, bytes: u64) -> u64 {
        let mut marked = 0u64;
        let mut host = self.inner.write_host();
        for region in host.iter().rev() {
            if marked >= bytes {
                break;
            }
            if region.base || region.draining.load(Ordering::SeqCst) {
                continue;
            }
            // Under the write lock no allocation is in flight, so once the flag is set nothing
            // new can land in the region.
            region.draining.store(true, Ordering::SeqCst);
            marked += region.space.bytes();
        }
        // Every thread's cache puts back what it holds of a draining region and lets go of it,
        // so the region holds only what buffers hold and is retired when they come home.
        self.inner
            .lanes
            .flush(&|region| region.draining.load(Ordering::SeqCst), true);
        self.inner.retire_drained(&mut host);
        drop(host);
        if marked > 0 {
            tracing::info!(target: "arena.shrink", bytes = marked, "arena regions draining");
        }
        marked
    }

    /// Host bytes in regions that are draining and not yet unmapped. Zero when every
    /// shrink is complete.
    pub fn draining_bytes(&self) -> u64 {
        self.inner
            .read_host()
            .iter()
            .filter(|r| !r.side && r.draining.load(Ordering::SeqCst))
            .map(|r| r.space.bytes())
            .sum()
    }

    /// Host bytes the arena can hand out: every region that is not draining. The facade's
    /// budget arithmetic compares its target with this and with [`Arena::region_bytes`], which
    /// counts the draining regions too, because they are still resident.
    pub fn host_capacity(&self) -> u64 {
        self.regions_capacity()
            .min(self.inner.host_cap.load(Ordering::SeqCst))
    }

    /// Host bytes in the regions that are not draining, whatever the cap.
    pub fn regions_capacity(&self) -> u64 {
        self.inner.live_capacity(&self.inner.read_host())
    }

    /// Side regions made over the run (RB5): large buffers the regions had room for by charge
    /// but not in one piece.
    pub fn side_regions(&self) -> u64 {
        self.inner.side_made.load(Ordering::Relaxed)
    }

    /// Host bytes in use in side regions now (RB5).
    pub fn side_in_use(&self) -> u64 {
        self.inner
            .read_host()
            .iter()
            .filter(|r| r.side)
            .map(|r| r.space.in_use())
            .sum()
    }

    /// Hand out at most `cap` host bytes at once, or lift the cap with `None` (RB4). What the
    /// limits watcher sets when the ceiling falls below the first region, which never drains:
    /// buffers already out stay where they are, and an allocation that would take the arena
    /// past the cap is refused like one the regions cannot serve.
    pub fn cap_host(&self, cap: Option<u64>) {
        self.inner
            .host_cap
            .store(cap.unwrap_or(u64::MAX), Ordering::SeqCst);
    }

    /// Give every whole page of the host regions that no buffer holds back to the operating
    /// system, keeping the mappings (RB4). The next use of a page faults it in and charges it
    /// then. Nothing is given back from a page-locked region. Returns the bytes given back.
    pub fn release_free(&self) -> u64 {
        // Granted memory is the run's for the whole run (C4): nothing goes back.
        if self.inner.pinned || self.inner.backing.is_some() {
            return 0;
        }
        // What the threads' caches hold is free too.
        self.inner.lanes.flush(&|_| true, false);
        let page = self.inner.page_bytes as u64;
        let released: u64 = self
            .inner
            .read_host()
            .iter()
            .filter(|r| !r.draining.load(Ordering::SeqCst))
            .map(|r| r.space.release_free(page))
            .sum();
        if released > 0 {
            tracing::info!(target: "arena.release", bytes = released, "free pages given back");
        }
        released
    }

    #[cfg(feature = "cuda")]
    fn devices(cfg: &ArenaConfig, align: u64) -> Result<Vec<RegionAlloc>> {
        let mut out = Vec::with_capacity(cfg.device_bytes.len());
        for (id, bytes) in &cfg.device_bytes {
            if usize::from(id.0) >= 8 {
                return Err(MorunaError::Config {
                    name: "budget.device",
                    msg: format!(
                        "device id {}; AllocStats accounts for ids 0..8 (contracts d.3)",
                        id.0
                    ),
                });
            }
            let want = region::align_down(*bytes, align);
            if want < GRANULE {
                return Err(MorunaError::Config {
                    name: "budget.device",
                    msg: format!("device {} budget {bytes} is below one size class", id.0),
                });
            }
            let (mapping, got) = region::new_device(*id, want)?;
            out.push(RegionAlloc {
                space: Space::new(
                    mapping.base(),
                    region::align_down(got, align),
                    Tier::Device(*id),
                    got < SMALL_BUDGET_BYTES,
                    Arc::default(),
                ),
                _mapping: mapping,
                index: usize::from(id.0),
            });
        }
        Ok(out)
    }

    #[cfg(not(feature = "cuda"))]
    fn devices(cfg: &ArenaConfig, _align: u64) -> Result<Vec<RegionAlloc>> {
        if cfg.device_bytes.is_empty() {
            return Ok(Vec::new());
        }
        Err(MorunaError::Config {
            name: "budget.device",
            msg: "device regions need the `cuda` feature; without it Tier::Device is \
                  unconstructible (preamble 6.2)"
                .into(),
        })
    }

    /// True when the host regions are memory a [`HostBacking`] granted.
    pub fn granted(&self) -> bool {
        self.inner.backing.is_some()
    }

    /// True when the host region is backed by transparent huge pages (d.1, f.1).
    pub fn huge_pages_active(&self) -> bool {
        self.inner.huge_pages_active
    }

    /// Bytes reserved for `tier`; zero for a tier this arena has no region for (d.1). For the
    /// host tier, every region still mapped, draining ones included, because they are resident,
    /// and the budget a granted arena has yet to make into regions, because it is the arena's.
    pub fn region_bytes(&self, tier: Tier) -> u64 {
        if self.inner.is_host(tier) {
            let host = self.inner.read_host();
            let mapped: u64 = host.iter().map(|r| r.space.bytes()).sum();
            return mapped + self.inner.uncommitted.load(Ordering::SeqCst);
        }
        match self.inner.space_of(tier) {
            Some(space) => space.bytes(),
            None => 0,
        }
    }

    /// The most host bytes the arena has had in use at once since [`Arena::reset_host_peak`]
    /// (or since it was made), exact: every charge raises the mark as it is made, whichever
    /// region it is in. What warm-up's peak of a unit reads (SDD 24 p.4).
    pub fn host_peak(&self) -> u64 {
        self.inner.host_watermark.peak()
    }

    /// Start a new host high-water mark at the bytes in use now.
    pub fn reset_host_peak(&self) {
        self.inner.host_watermark.reset();
    }

    /// Largest single allocation currently possible in `tier`, for controller diagnostics
    /// (d.1, f.5). A draining region serves nothing, so it is not counted; the next region a
    /// granted arena would make is. The threads' caches are flushed first, as a refusal flushes
    /// them, so their slots count.
    pub fn largest_free(&self, tier: Tier) -> u64 {
        if self.inner.is_host(tier) {
            self.inner.lanes.flush(&|_| true, false);
            let host = self.inner.read_host();
            return host
                .iter()
                .filter(|r| !r.draining.load(Ordering::SeqCst))
                .map(|r| r.space.largest_free())
                .max()
                .unwrap_or(0)
                .max(self.inner.next_region());
        }
        match self.inner.space_of(tier) {
            Some(space) => space.largest_free(),
            None => 0,
        }
    }

    /// The arena's own figures for the run report (section j), with the threads' caches
    /// flushed first, so the largest free allocation counts their slots.
    pub fn arena_stats(&self) -> ArenaStats {
        self.inner.lanes.flush(&|_| true, false);
        self.inner.arena_stats()
    }

    /// Host bytes of the budget a granted arena has not yet made into regions: they are the
    /// arena's, and made when an allocation first needs them. Zero for an arena that maps its
    /// own regions, whose pages come when a buffer first lands on them instead.
    pub fn uncommitted_bytes(&self) -> u64 {
        self.inner.uncommitted.load(Ordering::SeqCst)
    }

    /// Regions a granted arena made ahead of need over the run (`GRANTED_AHEAD_BELOW`).
    pub fn regions_made_ahead(&self) -> u64 {
        self.inner.ahead_made.load(Ordering::Relaxed)
    }

    /// Waits until no region is being made ahead of need, so what the arena has made is
    /// settled: before reading its figures, and before a run's report.
    pub fn settle(&self) {
        let ahead = self
            .inner
            .ahead
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(thread) = ahead {
            let _ = thread.join();
        }
    }
}

/// The lanes go with the arena: each then lives only as long as a buffer that holds it, and
/// the arena's memory with them ([`lanes`]).
impl Drop for Arena {
    fn drop(&mut self) {
        self.inner.lanes.clear();
    }
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("host_tier", &self.inner.host_tier)
            .field("host_bytes", &self.region_bytes(self.inner.host_tier))
            .field("devices", &self.inner.devices.len())
            .field("pinned", &self.inner.pinned)
            .field("huge_pages_active", &self.inner.huge_pages_active)
            .finish()
    }
}

/// Host regions of `bytes` (a multiple of `align`) on memory `backing` grants, each at most its
/// largest region.
fn granted(backing: &dyn HostBacking, bytes: u64, align: u64) -> Result<Vec<region::HostRegion>> {
    let chunk = region::granted_chunk(backing, align);
    if chunk < GRANULE {
        return Err(MorunaError::Config {
            name: "budget.host",
            msg: format!(
                "the granted memory's largest region of {} bytes holds no size class once \
                 aligned to {align} bytes",
                backing.region_max()
            ),
        });
    }
    let mut out = Vec::new();
    let mut left = bytes;
    while left >= GRANULE {
        let asked = left.min(chunk);
        let host = region::new_granted(backing, asked, align)?;
        // A misaligned grant may hold an alignment less than asked: what it holds is counted,
        // and the shortfall is not chased with a grant of its own.
        let short = host.mapping.bytes() < asked;
        left = left.saturating_sub(host.mapping.bytes());
        out.push(host);
        if short {
            break;
        }
    }
    Ok(out)
}

impl Inner {
    /// True when `tier` is this run's host tier (AR-I6): the one tier the host regions serve.
    fn is_host(&self, tier: Tier) -> bool {
        matches!(tier, Tier::Host | Tier::PinnedHost) && tier == self.host_tier
    }

    /// The device region that serves `tier`, or `None` when this arena has none. Host tiers are
    /// served by the region list, not by this.
    fn space_of(&self, tier: Tier) -> Option<&Space> {
        match tier {
            Tier::Device(id) => self.device(id),
            Tier::Host | Tier::PinnedHost => None,
            Tier::Disk(_) => None,
            Tier::Remote(_, _) => None,
        }
    }

    /// The region for one device.
    fn device(&self, id: DeviceId) -> Option<&Space> {
        self.devices
            .iter()
            .find(|r| r.index == usize::from(id.0))
            .map(|r| &r.space)
    }

    /// The host regions, for reading.
    fn read_host(&self) -> RwLockReadGuard<'_, Vec<Arc<HostRegion>>> {
        self.host.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The host regions, for `grow`, `shrink` and retiring. When the guard is let go, the
    /// lookup index is published from the list before its lock is.
    fn write_host(&self) -> HostWriter<'_> {
        HostWriter {
            guard: self.host.write().unwrap_or_else(|e| e.into_inner()),
            index: &self.host_index,
        }
    }

    /// Unmap every draining region that holds no live buffer. Called with the write
    /// lock held, so no allocation or release is inside a region while it is removed.
    fn retire_drained(&self, host: &mut Vec<Arc<HostRegion>>) {
        host.retain(|region| {
            let empty = region.draining.load(Ordering::SeqCst) && region.space.in_use() == 0;
            if empty && region.side {
                self.side_live.fetch_sub(1, Ordering::SeqCst);
                self.retired_double_release
                    .fetch_add(region.space.double_releases(), Ordering::Relaxed);
                return false;
            }
            if empty {
                self.retired_regions.fetch_add(1, Ordering::Relaxed);
                self.retired_bytes
                    .fetch_add(region.space.bytes(), Ordering::Relaxed);
                self.retired_double_release
                    .fetch_add(region.space.double_releases(), Ordering::Relaxed);
                tracing::info!(
                    target: "arena.retired",
                    bytes = region.space.bytes(),
                    "a drained region was unmapped"
                );
            }
            !empty
        });
    }

    /// The tier of the region holding `ptr` (f.7). The host regions are asked through their
    /// index, with no lock, and through their list only when the index holds too few.
    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        let in_host = match self.host_index.contains(ptr) {
            Some(inside) => inside,
            None => self.read_host().iter().any(|r| r.space.contains(ptr)),
        };
        if in_host {
            return Some(self.host_tier);
        }
        self.devices
            .iter()
            .find(|r| r.space.contains(ptr))
            .map(|r| r.space.tier())
    }

    /// Serve a host allocation (f.3): from this thread's cache when it keeps the class, and
    /// otherwise from the regions. A refusal first flushes every thread's cache and asks the
    /// regions again, so nothing is refused while slots sit in a cache; then a granted arena makes
    /// a region of its budget, and a large request may get a side region (RB5).
    fn alloc_host_buffer(
        self: &Arc<Self>,
        bytes: u64,
        lane: Option<&lanes::Lane>,
    ) -> core::result::Result<(*mut u8, Handle), (u64, u64)> {
        // Read before the regions are, so a region made while they are searched is found.
        let seen = self.uncommitted.load(Ordering::SeqCst);
        // A cap or a live side region has every allocation checked against the budget as a
        // whole (RB4, RB5), which the regions' path does.
        if let (Some(lane), Some(class)) = (lane, lanes::cached_class(bytes))
            && self.host_cap.load(Ordering::SeqCst) == u64::MAX
            && self.side_live.load(Ordering::SeqCst) == 0
            && let Some((ptr, handle)) = self.alloc_cached(lane, class, bytes)
        {
            return Ok((ptr, handle));
        }
        self.alloc_host(bytes, lane)
            .or_else(|_| {
                self.lanes.flush(&|_| true, false);
                self.alloc_host(bytes, lane)
            })
            .or_else(|refused| self.alloc_committed(bytes, seen, lane).ok_or(refused))
            .or_else(|refused| self.alloc_side(bytes, lane).ok_or(refused))
    }

    /// The handle a buffer cut from `region` holds: this thread's for the region, or the arena
    /// itself for a thread with no lane.
    fn handle_in(self: &Arc<Self>, region: &Arc<HostRegion>, lane: Option<&lanes::Lane>) -> Handle {
        match lane {
            Some(lane) => lane.handle_for(region),
            None => Arc::clone(self) as Handle,
        }
    }

    /// Serve a host allocation from the oldest region that is not draining and can hold it
    /// (f.3). Oldest first, so the regions a shrink would pick are the last to fill.
    fn alloc_host(
        self: &Arc<Self>,
        bytes: u64,
        lane: Option<&lanes::Lane>,
    ) -> core::result::Result<(*mut u8, Handle), (u64, u64)> {
        let host = self.read_host();
        let cap = self.host_cap.load(Ordering::SeqCst);
        if cap != u64::MAX {
            let in_use: u64 = host.iter().map(|r| r.space.in_use()).sum();
            if in_use.saturating_add(bytes) > cap {
                return Err((cap, in_use));
            }
        }
        // A side region's bytes are outside the regions' capacity, so while one is live the
        // budget is checked as a whole: the regions together never hold more than their
        // capacity, whichever of them the bytes are in (RB5).
        if self.side_live.load(Ordering::SeqCst) > 0 {
            let in_use: u64 = host.iter().map(|r| r.space.in_use()).sum();
            let limit = self.live_capacity(&host).min(cap);
            if in_use.saturating_add(crate::classes::charge_of(bytes)) > limit {
                return Err((limit, in_use));
            }
        }
        for region in host.iter() {
            if region.draining.load(Ordering::SeqCst) {
                continue;
            }
            if let Ok((ptr, _charged)) = region.space.alloc(bytes) {
                return Ok((ptr, self.handle_in(region, lane)));
            }
        }
        let budget = host
            .iter()
            .filter(|r| !r.draining.load(Ordering::SeqCst))
            .map(|r| r.space.bytes())
            .sum::<u64>()
            + self.uncommitted.load(Ordering::SeqCst);
        let in_use = host.iter().map(|r| r.space.in_use()).sum();
        Err((budget, in_use))
    }

    /// Host bytes in the regions that serve allocations, neither draining nor side regions,
    /// and in the budget a granted arena has yet to make into regions.
    fn live_capacity(&self, host: &[Arc<HostRegion>]) -> u64 {
        host.iter()
            .filter(|r| !r.side && !r.draining.load(Ordering::SeqCst))
            .map(|r| r.space.bytes())
            .sum::<u64>()
            + self.uncommitted.load(Ordering::SeqCst)
    }

    /// The largest region a granted arena could still make out of its budget, and so the
    /// largest allocation one more region could serve; zero when there is none to make.
    fn next_region(&self) -> u64 {
        match &self.backing {
            Some(backing) => self
                .uncommitted
                .load(Ordering::SeqCst)
                .min(region::granted_chunk(backing.as_ref(), self.align)),
            None => 0,
        }
    }

    /// The bytes of the region a granted arena makes for an allocation charged `charge` when
    /// `in_use` bytes are held: a step of what is in use, at least [`GRANTED_STEP_MIN`] and at
    /// most [`GRANTED_STEP_MAX`], or `charge` when that is larger; at most the backing's
    /// largest and what is left of the budget. Never a share of the budget: what is made
    /// follows what the run holds.
    fn region_for(&self, charge: u64, in_use: u64) -> u64 {
        let step = region::align_up(in_use.clamp(GRANTED_STEP_MIN, GRANTED_STEP_MAX), self.align);
        step.max(region::align_up(charge, self.align))
            .min(self.next_region())
    }

    /// The bytes of the budget's regions a granted arena has made.
    fn made(&self) -> u64 {
        self.granted_budget
            .saturating_sub(self.uncommitted.load(Ordering::SeqCst))
    }

    /// `made` as a region of the budget, not yet among the arena's regions.
    fn budget_region(&self, made: region::HostRegion) -> Arc<HostRegion> {
        let usable = made.mapping.bytes();
        Arc::new(HostRegion {
            space: Space::new(
                made.mapping.base(),
                usable,
                self.host_tier,
                usable < SMALL_BUDGET_BYTES,
                Arc::clone(&self.host_watermark),
            ),
            _mapping: made.mapping,
            draining: AtomicBool::new(false),
            base: true,
            side: false,
        })
    }

    /// Puts `region`, made of the budget, among the budget's regions: after the others and
    /// before any grown one, so the regions a shrink would pick are still the last to fill.
    /// Called with `committing` held. Its usable bytes leave what is left to make.
    fn add_budget_region(&self, region: Arc<HostRegion>, asked: u64) {
        let usable = region.space.bytes();
        {
            let mut host = self.write_host();
            let at = host.iter().position(|r| !r.base).unwrap_or(host.len());
            host.insert(at, region);
            self.uncommitted
                .fetch_sub(usable.min(asked), Ordering::SeqCst);
        }
        tracing::info!(
            target: "arena.commit",
            bytes = usable,
            uncommitted = self.uncommitted.load(Ordering::SeqCst),
            "the arena made a region of its budget"
        );
    }

    /// After a host allocation: when the free bytes of the regions made fall below
    /// [`GRANTED_AHEAD_BELOW`] and the budget has more to make, the next step is made ahead of
    /// need on a thread of its own, one at a time. Two loads and a comparison when it is not.
    fn consider_ahead(self: &Arc<Self>) {
        if self.backing.is_none() || self.uncommitted.load(Ordering::Relaxed) == 0 {
            return;
        }
        let made = self.made();
        if made.saturating_sub(self.host_watermark.live()) >= GRANTED_AHEAD_BELOW
            || made >= self.host_cap.load(Ordering::Relaxed)
        {
            return;
        }
        let mut ahead = self.ahead.lock().unwrap_or_else(|e| e.into_inner());
        if ahead.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        if let Some(done) = ahead.take() {
            let _ = done.join();
        }
        let inner = Arc::downgrade(self);
        *ahead = std::thread::Builder::new()
            .name("moruna-arena-ahead".into())
            .spawn(move || {
                if let Some(inner) = inner.upgrade() {
                    inner.make_ahead();
                }
            })
            .ok();
    }

    /// Makes the next step of a granted arena's budget, if the free bytes of its regions are
    /// still below [`GRANTED_AHEAD_BELOW`] once no other region is being made.
    fn make_ahead(&self) {
        let Some(backing) = self.backing.as_ref() else {
            return;
        };
        let _committing = self.committing.lock().unwrap_or_else(|e| e.into_inner());
        let live = self.host_watermark.live();
        let made = self.made();
        // Nothing past a cap the watcher set (RB4): a region it would never let be used.
        let room = self.host_cap.load(Ordering::SeqCst).saturating_sub(made);
        if made.saturating_sub(live) >= GRANTED_AHEAD_BELOW || room < GRANULE {
            return;
        }
        let take = region::align_down(self.region_for(0, live).min(room), self.align);
        if take < GRANULE {
            return;
        }
        match region::new_granted(backing.as_ref(), take, self.align) {
            Ok(made) => {
                self.add_budget_region(self.budget_region(made), take);
                self.ahead_made.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(target: "arena.commit", bytes = take, error = %e, "the backing refused a region made ahead");
            }
        }
    }

    /// A host allocation the regions made so far cannot serve, served from one more region made
    /// now out of the budget a granted arena has not yet made (`Arena::with_backing`). The
    /// region is sized by `region_for`, and serves every later allocation too: none drains, and the budget's arithmetic is unchanged, because the
    /// budget counted it from the start. `None` when there is nothing left to make, the budget
    /// has no room for `bytes`, or one more region would not hold them in one piece (a side
    /// region may, `alloc_side`). `seen` is what was left to make before the caller's
    /// `alloc_host`: when it was nothing, that call saw every region there will be.
    fn alloc_committed(
        self: &Arc<Self>,
        bytes: u64,
        seen: u64,
        lane: Option<&lanes::Lane>,
    ) -> Option<(*mut u8, Handle)> {
        let backing = self.backing.as_ref()?;
        if seen == 0 {
            return None;
        }
        // One region at a time. An allocation that waited here finds the region the one before
        // it made, and makes none of its own.
        let _committing = self.committing.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(served) = self.alloc_host(bytes, lane) {
            return Some(served);
        }
        let charge = crate::classes::charge_of(bytes);
        let in_use = {
            let host = self.read_host();
            let in_use: u64 = host.iter().map(|r| r.space.in_use()).sum();
            let limit = self
                .live_capacity(&host)
                .min(self.host_cap.load(Ordering::SeqCst));
            if in_use.saturating_add(charge) > limit {
                return None;
            }
            in_use
        };
        let take = self.region_for(charge, in_use);
        if take < GRANULE || charge > take {
            return None;
        }
        let made = match region::new_granted(backing.as_ref(), take, self.align) {
            Ok(made) => made,
            Err(e) => {
                tracing::warn!(target: "arena.commit", bytes = take, error = %e, "the backing refused a region of the budget");
                return None;
            }
        };
        let region = self.budget_region(made);
        // Served before the region is seen, so the allocation that made it is the one it serves.
        let served = region
            .space
            .alloc(bytes)
            .ok()
            .map(|(ptr, _)| (ptr, self.handle_in(&region, lane)));
        self.add_budget_region(region, take);
        served
    }

    /// RB5 (2026-11-14): a large host request the regions have room for by charge but not in
    /// one piece is served from a side region of its own, mapped untouched (so it costs only the
    /// pages written), draining from birth (so it takes nothing else) and unmapped when the
    /// buffer comes home. Its charge counts against the budget like any other, and while it is
    /// live every allocation is checked against the budget as a whole (`alloc_host`). Before, a
    /// 128 MiB sink buffer was refused with a gigabyte free in pieces and the run ended (Enrich
    /// over the whole dataset). `None` when the arena is page-locked or touched at reservation
    /// (a side region would have to be prepared the same way), the request is small, or the
    /// budget has no room for it.
    fn alloc_side(
        self: &Arc<Self>,
        bytes: u64,
        lane: Option<&lanes::Lane>,
    ) -> Option<(*mut u8, Handle)> {
        if self.pinned || bytes < SIDE_MIN || (self.pretouch && self.backing.is_none()) {
            return None;
        }
        let charge = crate::classes::charge_of(bytes);
        let mut host = self.write_host();
        let in_use: u64 = host.iter().map(|r| r.space.in_use()).sum();
        let limit = self
            .live_capacity(&host)
            .min(self.host_cap.load(Ordering::SeqCst));
        if in_use.saturating_add(charge) > limit {
            return None;
        }
        // The pages the regions hold free go back to the operating system first: freed buffers
        // leave their pages resident until something lands on them again, and a side region on
        // top of those would put the process past what the controller counts the arena as
        // (11 f.3: the regions' bytes). After this the arena is resident in what is in use.
        let page = self.page_bytes as u64;
        if self.backing.is_none() {
            for region in host.iter().filter(|r| !r.draining.load(Ordering::SeqCst)) {
                region.space.release_free(page);
            }
        }
        let len = region::align_up(charge, self.align.max(GRANULE));
        let mapped = match &self.backing {
            // A granted side region is one more grant, made only if one region holds it.
            Some(backing) => {
                if len > region::granted_chunk(backing.as_ref(), self.align) {
                    return None;
                }
                region::new_granted(backing.as_ref(), len, self.align).ok()?
            }
            None => region::new_host(len, self.align, false, self.huge_pages, self.memlock, false)
                .ok()?,
        };
        let side = Arc::new(HostRegion {
            space: Space::new(
                mapped.mapping.base(),
                mapped.mapping.bytes(),
                self.host_tier,
                crate::classes::class_for(bytes).is_some(),
                Arc::clone(&self.host_watermark),
            ),
            _mapping: mapped.mapping,
            draining: AtomicBool::new(true),
            base: false,
            side: true,
        });
        let (ptr, _) = side.space.alloc(bytes).ok()?;
        // Draining from birth, so the handle is the buffer's own and no cache keeps the region.
        let handle = self.handle_in(&side, lane);
        host.push(side);
        self.side_live.fetch_add(1, Ordering::SeqCst);
        self.side_made.fetch_add(1, Ordering::Relaxed);
        tracing::info!(target: "arena.side", bytes, charge, "a large buffer got a side region");
        Some((ptr, handle))
    }
}

/// Contracts d.3. The arena overrides every method, including the three with defaults
/// (d.1).
impl Allocator for Arena {
    fn available(&self, tier: Tier) -> Option<u64> {
        if self.inner.is_host(tier) {
            let host = self.inner.read_host();
            let live = host.iter().filter(|r| !r.draining.load(Ordering::SeqCst));
            let (bytes, in_use) = live.fold((0u64, 0u64), |(b, u), r| {
                (b + r.space.bytes(), u + r.space.in_use())
            });
            // A side region's bytes count against the budget too (RB5).
            let side: u64 = host
                .iter()
                .filter(|r| r.side)
                .map(|r| r.space.in_use())
                .sum();
            let bytes = (bytes + self.inner.uncommitted.load(Ordering::SeqCst))
                .min(self.inner.host_cap.load(Ordering::SeqCst));
            return Some(bytes.saturating_sub(in_use.saturating_add(side)));
        }
        self.inner
            .space_of(tier)
            .map(|space| space.bytes().saturating_sub(space.in_use()))
    }

    fn note_payload_copy(&self, bytes: u64) {
        self.inner
            .payload_copies
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    fn note_boundary_copy(&self, bytes: u64) {
        self.inner
            .boundary_copies
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }
    fn alloc(&self, bytes: usize, tier: Tier) -> Result<Buffer> {
        if let Tier::Remote(_, _) = tier {
            // Reserved for the multi-node extension; no v1 component produces it (CT-I11).
            return Err(MorunaError::Unsupported("rdma"));
        }
        let inner = &self.inner;
        // This thread's lane: its cache of free slots, and the handles its buffers hold, words no
        // other thread writes unless it releases a buffer this one allocated.
        let lane = inner.lane();
        let served = if inner.is_host(tier) {
            inner
                .alloc_host_buffer(bytes as u64, lane.as_deref())
                .map(|(ptr, handle)| (ptr, inner.host_tier, handle))
        } else {
            match inner.space_of(tier) {
                Some(space) => match space.alloc(bytes as u64) {
                    Ok((ptr, _charged)) => {
                        let handle: Handle = match lane {
                            Some(lane) => lane,
                            None => Arc::clone(inner) as Handle,
                        };
                        Ok((ptr, space.tier(), handle))
                    }
                    Err(_) => Err((space.bytes(), space.in_use())),
                },
                None => {
                    return Err(MorunaError::Alloc {
                        bytes: bytes as u64,
                        tier,
                        budget: 0,
                        in_use: 0,
                    });
                }
            }
        };
        match served {
            Ok((ptr, served_tier, handle)) => {
                inner.allocations.fetch_add(1, Ordering::Relaxed);
                if inner.is_host(tier) {
                    inner.consider_ahead();
                }
                // SAFETY: `ptr` is `bytes` bytes inside a region this arena reserved for
                // `tier`; that region stays mapped until it holds no live buffer (AR-I3),
                // the allocator will not hand the same bytes out again until they are released,
                // and the buffer releases them to this same arena exactly once on drop (CT-I2).
                Ok(unsafe { Buffer::from_raw(ptr, bytes, served_tier, handle) })
            }
            Err((budget, in_use)) => {
                tracing::debug!(target: "arena.alloc_failed", bytes, tier = ?tier, in_use, "allocation refused");
                Err(MorunaError::Alloc {
                    bytes: bytes as u64,
                    tier,
                    budget,
                    in_use,
                })
            }
        }
    }

    fn page_bytes(&self) -> usize {
        self.inner.page_bytes
    }

    fn stats(&self) -> AllocStats {
        self.inner.alloc_stats()
    }

    fn contains(&self, ptr: *const u8) -> bool {
        self.inner.tier_of(ptr).is_some()
    }

    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        self.inner.tier_of(ptr)
    }

    fn is_pinned(&self) -> bool {
        self.inner.pinned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(host_bytes: u64) -> ArenaConfig {
        ArenaConfig {
            host_bytes,
            page_bytes: 4096,
            huge_pages: Guarantee::Probed(false),
            memlock: Guarantee::Absent,
            ..ArenaConfig::default()
        }
    }

    #[test]
    fn rdma_registration_is_reserved() {
        let mut c = cfg(1 << 20);
        c.register_rdma = true;
        assert!(matches!(
            Arena::new(c),
            Err(MorunaError::Unsupported("rdma"))
        ));
    }

    #[test]
    fn configuration_is_validated() {
        let mut c = cfg(1 << 20);
        c.page_bytes = 3;
        assert!(
            matches!(Arena::new(c), Err(MorunaError::Config { name, .. }) if name == "page.bytes")
        );

        let mut c = cfg(1 << 20);
        c.host_tier = TierKind::Device;
        assert!(
            matches!(Arena::new(c), Err(MorunaError::Config { name, .. }) if name == "arena.pin")
        );

        assert!(
            matches!(Arena::new(cfg(1024)), Err(MorunaError::Config { name, .. }) if name == "budget.host")
        );

        let mut c = cfg(1 << 20);
        c.page_bytes = 1 << 21;
        c.host_bytes = (1 << 21) - 1;
        assert!(
            matches!(Arena::new(c), Err(MorunaError::Config { name, .. }) if name == "budget.host")
        );

        let mut c = cfg(1 << 20);
        c.device_bytes = (0..9).map(|i| (DeviceId(i), 1 << 20)).collect();
        assert!(
            matches!(Arena::new(c), Err(MorunaError::Config { name, .. }) if name == "budget.device")
        );

        let mut c = cfg(1 << 20);
        c.device_bytes = vec![(DeviceId(0), 1 << 20)];
        let r = Arena::new(c);
        #[cfg(not(feature = "cuda"))]
        assert!(
            matches!(r, Err(MorunaError::Config { name, .. }) if name == "budget.device"),
            "device regions need the cuda feature"
        );
        #[cfg(feature = "cuda")]
        let _ = r;
    }

    /// AR-T15 (RB4). A capped arena refuses what would take it past the cap and serves what fits;
    /// the pages no buffer holds are given back, and a buffer later carved from them is whole
    /// and writable; a lifted cap serves the region again.
    #[test]
    fn a_capped_arena_gives_back_what_it_does_not_hold() {
        let arena = Arena::new(cfg(64 << 20)).expect("arena");
        let kept = arena.alloc(8 << 20, Tier::Host).expect("kept");
        let freed = arena.alloc(8 << 20, Tier::Host).expect("freed");
        drop(freed);
        let released = arena.release_free();
        assert!(
            released >= 8 << 20,
            "at least the freed buffer goes back: {released}"
        );
        let in_use = arena.stats().host_in_use;
        arena.cap_host(Some(in_use + (4 << 20)));
        assert!(arena.host_capacity() <= in_use + (4 << 20));
        assert!(
            matches!(
                arena.alloc(8 << 20, Tier::Host),
                Err(MorunaError::Alloc { .. })
            ),
            "past the cap is refused"
        );
        assert_eq!(
            arena.available(Tier::Host),
            Some(4 << 20),
            "what is left under the cap"
        );
        let mut fits = arena.alloc(2 << 20, Tier::Host).expect("under the cap");
        fits.fill(7);
        assert!(
            fits.iter().all(|b| *b == 7),
            "a given-back page is writable"
        );
        arena.cap_host(None);
        let mut again = arena
            .alloc(16 << 20, Tier::Host)
            .expect("the cap is lifted");
        again.fill(1);
        drop((kept, fits, again));
    }

    #[test]
    fn the_host_peak_is_the_most_in_use_at_once_and_resets() {
        let arena = Arena::new(cfg(64 << 20)).expect("arena");
        assert_eq!(arena.host_peak(), 0);
        let held = arena.alloc(1 << 20, Tier::Host).expect("held");
        let spike = arena.alloc(8 << 20, Tier::Host).expect("spike");
        let at_spike = arena.stats().host_in_use;
        drop(spike);
        assert!(arena.stats().host_in_use < at_spike);
        assert_eq!(
            arena.host_peak(),
            at_spike,
            "a freed spike is still the peak"
        );
        arena.reset_host_peak();
        assert_eq!(arena.host_peak(), arena.stats().host_in_use);
        let after_reset = arena.host_peak();
        let small = arena.alloc(2 << 20, Tier::Host).expect("small");
        let at_small = arena.stats().host_in_use;
        drop(small);
        assert_eq!(arena.host_peak(), at_small);
        assert!(at_small > after_reset && at_small < at_spike);
        drop(held);

        // A region added by `grow` charges the same mark: a buffer the first region cannot hold.
        let arena = Arena::new(cfg(4 << 20)).expect("arena");
        arena.grow(32 << 20).expect("grown");
        let grown = arena
            .alloc(20 << 20, Tier::Host)
            .expect("only the grown region holds it");
        let charged = arena.stats().host_in_use;
        drop(grown);
        assert!(charged >= 20 << 20);
        assert_eq!(arena.host_peak(), charged);
    }

    #[test]
    fn a_host_arena_serves_its_own_tier_only() {
        let arena = Arena::new(cfg(4 << 20)).expect("arena");
        assert!(!arena.is_pinned());
        assert_eq!(arena.page_bytes(), 4096);
        assert_eq!(arena.region_bytes(Tier::Host), 4 << 20);
        assert_eq!(arena.region_bytes(Tier::PinnedHost), 0);
        assert_eq!(arena.region_bytes(Tier::Device(DeviceId(0))), 0);
        assert_eq!(arena.largest_free(Tier::PinnedHost), 0);
        assert!(!arena.huge_pages_active());

        let b = arena.alloc(1024, Tier::Host).expect("host buffer");
        assert_eq!(b.len(), 1024);
        assert_eq!(b.tier(), Tier::Host);
        assert_eq!(arena.stats().host_in_use, GRANULE);
        assert_eq!(arena.stats().pinned_in_use, 0);
        assert_eq!(arena.stats().allocations_total, 1);

        let e = arena.alloc(1024, Tier::PinnedHost).expect_err("wrong tier");
        assert!(matches!(e, MorunaError::Alloc { tier, .. } if tier == Tier::PinnedHost));
        let seg = moruna_kernel::SegmentRef {
            segment: 0,
            offset: 0,
            len: 1,
        };
        let e = arena
            .alloc(1024, Tier::Disk(seg))
            .expect_err("no disk region");
        assert!(matches!(e, MorunaError::Alloc { tier, .. } if tier == Tier::Disk(seg)));
        let rr = moruna_kernel::RemoteRef {
            addr: 0,
            rkey: 0,
            len: 1,
        };
        let e = arena
            .alloc(1024, Tier::Remote(moruna_kernel::LOCAL_NODE, rr))
            .expect_err("reserved");
        assert!(matches!(e, MorunaError::Unsupported("rdma")));

        drop(b);
        assert_eq!(arena.stats().host_in_use, 0);
    }

    #[test]
    fn a_release_the_arena_did_not_hand_out_is_counted() {
        let arena = Arena::new(cfg(4 << 20)).expect("arena");
        let mut heap = [0u8; 8];
        let seg = moruna_kernel::SegmentRef {
            segment: 0,
            offset: 0,
            len: 1,
        };
        let rr = moruna_kernel::RemoteRef {
            addr: 0,
            rkey: 0,
            len: 1,
        };
        let handle: &dyn moruna_kernel::ArenaHandle = arena.inner.as_ref();
        handle.release(heap.as_mut_ptr(), 8, Tier::Host);
        handle.release(heap.as_mut_ptr(), 8, Tier::PinnedHost);
        handle.release(heap.as_mut_ptr(), 8, Tier::Device(DeviceId(3)));
        handle.release(heap.as_mut_ptr(), 8, Tier::Disk(seg));
        handle.release(
            heap.as_mut_ptr(),
            8,
            Tier::Remote(moruna_kernel::LOCAL_NODE, rr),
        );
        assert_eq!(arena.arena_stats().double_release, 5);
    }
}
