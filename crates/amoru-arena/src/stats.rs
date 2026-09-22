//! What the arena reports about itself: the contract's `AllocStats` and the arena's own
//! `ArenaStats` for the run report (section j).

use amoru_kernel::AllocStats;

use crate::Inner;
use crate::classes::CLASS_COUNT;

/// The arena's own figures for the run report (section j). `AllocStats` (contracts d.3) is
/// the per-tier byte accounting; this is everything else the report names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ArenaStats {
    /// Slabs claimed per size class in the host region (f.3, f.5). In the small-budget
    /// mode these are the blocks the region was seeded with (e.1).
    pub slabs_by_class: [u32; CLASS_COUNT],
    /// The largest single host allocation still possible (d.1).
    pub largest_free: u64,
    /// Host bytes on class free lists: charged to a class, held by no buffer (f.5).
    pub stranded_bytes: u64,
    /// Releases that found nothing live at the address, summed over every region: a double
    /// release, or a release of a tier this arena never hands out (h).
    pub double_release: u64,
    /// True when the host region is backed by transparent huge pages (f.1).
    pub huge_pages_active: bool,
    /// True when the host region is page-locked, so the run's host tier is `PinnedHost`
    /// (AR-I6).
    pub pinned: bool,
}

impl Inner {
    /// The contract's per-tier accounting (AR-I5). `payload_copies_total` and
    /// `boundary_copies_total` are the copy counters other components own; the arena has no
    /// interface through which they can be raised and therefore always reports zero.
    pub(crate) fn alloc_stats(&self) -> AllocStats {
        let host = self.host.space.in_use();
        let mut device_in_use = [0u64; 8];
        for region in &self.devices {
            device_in_use[region.index] = region.space.in_use();
        }
        AllocStats {
            host_in_use: if self.pinned { 0 } else { host },
            pinned_in_use: if self.pinned { host } else { 0 },
            device_in_use,
            allocations_total: self.allocations.load(std::sync::atomic::Ordering::Relaxed),
            payload_copies_total: 0,
            boundary_copies_total: 0,
        }
    }

    /// The arena's own figures (section j); the slab, stranding and largest-free columns
    /// are the host region's, which is the region the report is about.
    pub(crate) fn arena_stats(&self) -> ArenaStats {
        let mut double_release = self.host.space.double_releases()
            + self.foreign.load(std::sync::atomic::Ordering::Relaxed);
        for region in &self.devices {
            double_release += region.space.double_releases();
        }
        ArenaStats {
            slabs_by_class: self.host.space.slabs_by_class(),
            largest_free: self.host.space.largest_free(),
            stranded_bytes: self.host.space.stranded_bytes(),
            double_release,
            huge_pages_active: self.huge_pages_active,
            pinned: self.pinned,
        }
    }
}
