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

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible function here returns the contract's `MorunaError` (contracts d.14), whose
// size is fixed by that crate and is above clippy's 128 byte threshold. The arena may not
// box it: the error type crosses every component boundary and is the contract's to change.
#![allow(clippy::result_large_err)]

mod classes;
mod handle;
mod large;
pub mod region;
mod stats;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_kernel::{
    AllocStats, Allocator, MorunaError, Buffer, DeviceId, Guarantee, Result, Tier, TierKind,
};

use crate::classes::{GRANULE, SMALL_BUDGET_BYTES, Space};
use crate::region::Mapping;

pub use crate::stats::ArenaStats;

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
        }
    }
}

/// One reserved region and the size-class allocator over it.
struct RegionAlloc {
    space: Space,
    /// Held so the mapping is released exactly once, when the arena drops (AR-I3). The
    /// `Space` addresses this mapping, so it is dropped after it.
    _mapping: Mapping,
    /// Device index for `AllocStats::device_in_use`; 0 for the host region.
    index: usize,
}

/// Everything a `Buffer` needs to find its way home. `Buffer`s hold an `Arc` of this, so
/// the regions outlive every buffer cut from them.
struct Inner {
    host: RegionAlloc,
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
        let host = region::new_host(
            host_bytes,
            align,
            host_tier == Tier::PinnedHost,
            cfg.huge_pages,
            cfg.memlock,
        )?;
        let pinned = host.pinned;
        let host_tier = if pinned { Tier::PinnedHost } else { Tier::Host };
        let host_alloc = RegionAlloc {
            space: Space::new(
                host.mapping.base(),
                host.mapping.bytes(),
                host_tier,
                host_bytes < SMALL_BUDGET_BYTES,
            ),
            _mapping: host.mapping,
            index: 0,
        };
        let devices = Self::devices(&cfg, align)?;
        tracing::info!(
            target: "arena.reserved",
            host_bytes,
            pinned,
            huge_pages = host.huge_pages_active,
            devices = devices.len(),
            "arena reserved"
        );
        Ok(Arc::new(Arena {
            inner: Arc::new(Inner {
                host: host_alloc,
                devices,
                host_tier,
                page_bytes: cfg.page_bytes,
                pinned,
                huge_pages_active: host.huge_pages_active,
                allocations: AtomicU64::new(0),
                foreign: AtomicU64::new(0),
                payload_copies: AtomicU64::new(0),
                boundary_copies: AtomicU64::new(0),
            }),
        }))
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

    /// True when the host region is backed by transparent huge pages (d.1, f.1).
    pub fn huge_pages_active(&self) -> bool {
        self.inner.huge_pages_active
    }

    /// Bytes reserved for `tier`; zero for a tier this arena has no region for (d.1).
    pub fn region_bytes(&self, tier: Tier) -> u64 {
        match self.inner.space_of(tier) {
            Some(space) => space.bytes(),
            None => 0,
        }
    }

    /// Largest single allocation currently possible in `tier`, for controller diagnostics
    /// (d.1, f.5).
    pub fn largest_free(&self, tier: Tier) -> u64 {
        match self.inner.space_of(tier) {
            Some(space) => space.largest_free(),
            None => 0,
        }
    }

    /// The arena's own figures for the run report (section j).
    pub fn arena_stats(&self) -> ArenaStats {
        self.inner.arena_stats()
    }
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("host_tier", &self.inner.host_tier)
            .field("host_bytes", &self.inner.host.space.bytes())
            .field("devices", &self.inner.devices.len())
            .field("pinned", &self.inner.pinned)
            .field("huge_pages_active", &self.inner.huge_pages_active)
            .finish()
    }
}

impl Inner {
    /// The region that serves `tier`, or `None` when this arena has none. The host region
    /// serves exactly one of `Host` and `PinnedHost` for the whole run (AR-I6).
    fn space_of(&self, tier: Tier) -> Option<&Space> {
        match tier {
            Tier::Host | Tier::PinnedHost => {
                if tier == self.host_tier {
                    Some(&self.host.space)
                } else {
                    None
                }
            }
            Tier::Device(id) => self.device(id),
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

    /// The tier of the region holding `ptr` (f.7).
    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        if self.host.space.contains(ptr) {
            return Some(self.host_tier);
        }
        self.devices
            .iter()
            .find(|r| r.space.contains(ptr))
            .map(|r| r.space.tier())
    }
}

/// Contracts d.3. The arena overrides every method, including the three with defaults
/// (d.1).
impl Allocator for Arena {
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
        let Some(space) = inner.space_of(tier) else {
            return Err(MorunaError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: 0,
                in_use: 0,
            });
        };
        match space.alloc(bytes as u64) {
            Ok((ptr, _charged)) => {
                inner.allocations.fetch_add(1, Ordering::Relaxed);
                let handle: Arc<dyn moruna_kernel::ArenaHandle> = Arc::clone(inner) as Arc<_>;
                // SAFETY: `ptr` is `bytes` bytes inside the region this arena reserved for
                // `tier` and holds for the life of the run (AR-I3), the allocator will not
                // hand the same bytes out again until they are released, and the buffer
                // releases them to this same arena exactly once on drop (CT-I2).
                Ok(unsafe { Buffer::from_raw(ptr, bytes, space.tier(), handle) })
            }
            Err(_) => {
                let in_use = space.in_use();
                tracing::debug!(target: "arena.alloc_failed", bytes, tier = ?tier, in_use, "allocation refused");
                Err(MorunaError::Alloc {
                    bytes: bytes as u64,
                    tier,
                    budget: space.bytes(),
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
