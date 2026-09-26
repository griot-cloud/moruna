//! What the arena reports about itself: the contract's `AllocStats` and the arena's own
//! `ArenaStats` for the run report (section j).

use moruna_kernel::AllocStats;

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
    /// Host regions mapped now: one for a run whose budget never moved, more after a `grow`
    ///.
    pub host_regions: u32,
    /// Host bytes in draining regions not yet unmapped.
    pub draining_bytes: u64,
    /// Regions a shrink retired over the run, and the bytes they gave back.
    pub retired_regions: u64,
    /// The bytes those retired regions held.
    pub retired_bytes: u64,
}

impl Inner {
    /// The contract's per-tier accounting (AR-I5). `payload_copies_total` and
    /// `boundary_copies_total` are the copy counters other components own; the arena has no
    /// interface through which they can be raised and therefore always reports zero.
    pub(crate) fn alloc_stats(&self) -> AllocStats {
        let host: u64 = self.read_host().iter().map(|r| r.space.in_use()).sum();
        let mut device_in_use = [0u64; 8];
        for region in &self.devices {
            device_in_use[region.index] = region.space.in_use();
        }
        AllocStats {
            host_in_use: if self.pinned { 0 } else { host },
            pinned_in_use: if self.pinned { host } else { 0 },
            device_in_use,
            allocations_total: self.allocations.load(std::sync::atomic::Ordering::Relaxed),
            payload_copies_total: self
                .payload_copies
                .load(std::sync::atomic::Ordering::Relaxed),
            boundary_copies_total: self
                .boundary_copies
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// The arena's own figures (section j); the slab, stranding and largest-free columns are the
    /// host regions', summed (largest-free is the largest of any region still serving), which is
    /// the memory the report is about.
    pub(crate) fn arena_stats(&self) -> ArenaStats {
        use std::sync::atomic::Ordering::Relaxed;
        let host = self.read_host();
        let mut double_release =
            self.foreign.load(Relaxed) + self.retired_double_release.load(Relaxed);
        let mut slabs_by_class = [0u32; CLASS_COUNT];
        let mut largest_free = 0u64;
        let mut stranded_bytes = 0u64;
        let mut draining_bytes = 0u64;
        for region in host.iter() {
            double_release += region.space.double_releases();
            for (total, count) in slabs_by_class.iter_mut().zip(region.space.slabs_by_class()) {
                *total = total.saturating_add(count);
            }
            stranded_bytes += region.space.stranded_bytes();
            if region.draining.load(std::sync::atomic::Ordering::SeqCst) {
                draining_bytes += region.space.bytes();
            } else {
                largest_free = largest_free.max(region.space.largest_free());
            }
        }
        for region in &self.devices {
            double_release += region.space.double_releases();
        }
        ArenaStats {
            slabs_by_class,
            largest_free,
            stranded_bytes,
            double_release,
            huge_pages_active: self.huge_pages_active,
            pinned: self.pinned,
            host_regions: u32::try_from(host.len()).unwrap_or(u32::MAX),
            draining_bytes,
            retired_regions: self.retired_regions.load(Relaxed),
            retired_bytes: self.retired_bytes.load(Relaxed),
        }
    }
}
