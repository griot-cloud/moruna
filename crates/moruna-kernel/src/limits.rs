//! Limits, the host profile and the sampler (contracts d.12).

use std::path::PathBuf;

use crate::ids::DeviceId;

/// Where a limit came from.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum LimitSource {
    /// A cgroup v2 file.
    Cgroup,
    /// The operating system (total RAM, CPU count).
    Os,
    /// The user said so.
    Explicit,
}

/// One accelerator discovery found.
#[derive(Clone, Debug)]
pub struct Device {
    /// Index as discovery enumerated it.
    pub id: DeviceId,
    /// Total device memory.
    pub total_bytes: u64,
    /// Free device memory at discovery.
    pub free_bytes: u64,
    /// The device's name as the driver reports it.
    pub name: String,
}

/// What the host allows this process.
#[derive(Clone, Debug)]
pub struct Limits {
    /// The host memory ceiling.
    pub memory_ceiling: u64,
    /// The level at which the host would kill the process, where it is knowable.
    pub memory_kill: Option<u64>,
    /// CPUs available, as a fraction.
    pub cpu_quota: f64,
    /// The page size, which is also the direct IO alignment.
    pub page_bytes: usize,
    /// Every accelerator.
    pub devices: Vec<Device>,
    /// Where these figures came from.
    pub source: LimitSource,
}

/// Guarantees a platform declares; `Unknown` means probe. Discovery replaces every
/// `Unknown` with `Probed(bool)`, so a consumer can tell a declared guarantee (which
/// must hold: a failure is an error, G-I7) from a probed one (which may fall back).
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum Guarantee {
    /// Not declared; discovery probes and replaces this with `Probed`.
    #[default]
    Unknown,
    /// Declared present: a failure on this path is a platform bug.
    Present,
    /// Declared absent.
    Absent,
    /// Probed, with the result.
    Probed(bool),
}

impl Guarantee {
    /// Present or Probed(true).
    pub fn is_available(&self) -> bool {
        match self {
            Guarantee::Present => true,
            Guarantee::Probed(available) => *available,
            Guarantee::Unknown | Guarantee::Absent => false,
        }
    }

    /// Present only.
    pub fn is_guaranteed(&self) -> bool {
        matches!(self, Guarantee::Present)
    }
}

/// What the platform declares about the host.
#[derive(Clone, Debug, Default)]
pub struct HostProfile {
    /// Huge pages available to back the arena.
    pub huge_pages: Guarantee,
    /// `mlock` permitted, so the arena can be page-locked.
    pub memlock: Guarantee,
    /// io_uring permitted.
    pub io_uring: Guarantee,
    /// The staging directory accepts `O_DIRECT`.
    pub direct_io_staging: Guarantee,
    /// GPUDirect Storage present.
    pub gds: Guarantee,
    /// RDMA present (post-v1).
    pub rdma: Guarantee,
    /// Where staging segments go.
    pub staging_dir: Option<PathBuf>,
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
    /// One sample of live resource state.
    fn sample(&self) -> Sample;
    /// Reset the kernel's peak counter (cgroup v2 `memory.peak` is writable on
    /// kernels 6.x and later; otherwise the sampler tracks its own running peak and resets
    /// that), so a probe measures its own peak (RC f.2).
    fn reset_peak(&self);
}

/// One sample of live resource state; produced by discovery's sampler.
#[derive(Copy, Clone, Debug, Default)]
pub struct Sample {
    /// Anonymous host bytes of the process.
    pub anon_bytes: u64,
    /// Page-cache bytes attributed to the process.
    pub file_bytes: u64,
    /// The peak anonymous host bytes since the last `reset_peak`.
    pub peak_anon_bytes: u64,
    /// Microseconds the process was CPU-throttled, cumulative.
    pub throttled_us: u64,
    /// Device bytes in use per device.
    pub device_used: [u64; 8],
    /// When the sample was taken, nanoseconds since the epoch.
    pub at_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// d.12 and G-I7: a declared guarantee must hold, so a failure on it is a platform bug; a
    /// probed one may fall back. `is_available` and `is_guaranteed` are how a consumer tells
    /// the two apart, and `Unknown` is treated as absent until discovery probes it.
    #[test]
    fn declared_and_probed_guarantees_differ() {
        assert!(Guarantee::Present.is_available());
        assert!(Guarantee::Present.is_guaranteed());
        assert!(Guarantee::Probed(true).is_available());
        assert!(!Guarantee::Probed(true).is_guaranteed());
        assert!(!Guarantee::Probed(false).is_available());
        assert!(!Guarantee::Absent.is_available());
        assert!(!Guarantee::Absent.is_guaranteed());
        assert!(!Guarantee::Unknown.is_available());
        assert!(!Guarantee::Unknown.is_guaranteed());
        assert_eq!(Guarantee::default(), Guarantee::Unknown);

        // A profile that declares nothing is all `Unknown`, which discovery replaces.
        let profile = HostProfile::default();
        assert!(!profile.durable_staging.is_available());
        assert!(profile.staging_dir.is_none());
        assert!(!profile.gds.is_available());
        assert!(!profile.rdma.is_available());
    }
}
