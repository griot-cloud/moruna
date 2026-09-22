//! Region reservation and release (f.1, f.2) and the bounds `contains` tests (f.7).
//!
//! One mapping per tier for the life of the run (AR-I3): `mmap` once, `madvise` once,
//! touch every page once so the cgroup charge happens at start, then page-lock once when
//! the run's host tier is `PinnedHost`. Nothing here is called again after `Arena::new`.
//!
//! `unsafe` is permitted in this module for those calls (section l); every block cites the
//! invariant it relies on.

use std::sync::atomic::{AtomicU64, Ordering};

use amoru_kernel::{AmoruError, Guarantee, Result};

/// `mmap` calls made through this module since the process started.
static MMAP_CALLS: AtomicU64 = AtomicU64::new(0);
/// `mlock` (or `cuMemHostRegister`) calls made through this module since the process started.
static MLOCK_CALLS: AtomicU64 = AtomicU64::new(0);

/// Counting shim for AR-T3: the reservation syscalls this module has made.
///
/// The feature is on by default so that AR-T3 runs in the `cargo test --workspace` gate
/// the preamble's section 6.7 requires; it adds two atomic counters and this accessor and
/// nothing else.
#[cfg(feature = "test-shim")]
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub struct SyscallCounts {
    /// `mmap` calls.
    pub mmap: u64,
    /// `mlock` or `cuMemHostRegister` calls.
    pub mlock: u64,
}

/// Read the counting shim (AR-T3).
#[cfg(feature = "test-shim")]
pub fn syscall_counts() -> SyscallCounts {
    SyscallCounts {
        mmap: MMAP_CALLS.load(Ordering::Relaxed),
        mlock: MLOCK_CALLS.load(Ordering::Relaxed),
    }
}

/// What backs a region, and what has to be undone when it is released.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Backing {
    /// An anonymous private mapping; `pinned` when the whole region is page-locked.
    Host { pinned: bool },
    /// A device allocation (feature `cuda`); the base is a device address.
    #[cfg(feature = "cuda")]
    Device,
}

/// One contiguous region: the raw mapping, the aligned usable range inside it, and what
/// backs it. Dropped exactly once, at the end of the run.
pub(crate) struct Mapping {
    raw: *mut u8,
    raw_len: usize,
    base: *mut u8,
    bytes: u64,
    backing: Backing,
}

// SAFETY: a `Mapping` owns its region exclusively (AR-I3: it is created once and released
// once), and no `&self` method writes through `base`; the allocator above it serialises
// every write to the region's contents through its own locks.
unsafe impl Send for Mapping {}
// SAFETY: see the `Send` impl.
unsafe impl Sync for Mapping {}

/// The outcome of preparing the host region: the mapping plus what actually happened, which
/// the report needs (`huge_pages_active`, `pinned`) because a `Probed` guarantee may fall
/// back (G-I7, f.1).
pub(crate) struct HostRegion {
    pub(crate) mapping: Mapping,
    pub(crate) huge_pages_active: bool,
    pub(crate) pinned: bool,
}

impl Mapping {
    /// First byte of the usable region.
    pub(crate) fn base(&self) -> *mut u8 {
        self.base
    }

    /// Usable bytes.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        match self.backing {
            Backing::Host { pinned } => {
                if pinned {
                    // SAFETY: `base` is the range this mapping locked in `new_host`, and it
                    // is still mapped; unlocking a locked range cannot fail destructively.
                    unsafe { unlock(self.base, self.bytes as usize) };
                }
                // SAFETY: `raw`/`raw_len` is exactly the range `mmap` returned (AR-I3: one
                // reservation, so one release), and no buffer over it can outlive this
                // mapping: every `Buffer` holds an `Arc` of the arena that owns it.
                unsafe {
                    libc::munmap(self.raw.cast(), self.raw_len);
                }
            }
            #[cfg(feature = "cuda")]
            Backing::Device => {
                // SAFETY: `base` is the device address `cuMemAlloc_v2` returned in
                // `new_device` and has not been freed.
                unsafe {
                    let _ = cudarc::driver::sys::cuMemFree_v2(self.base as usize as u64);
                }
            }
        }
    }
}

/// Round `value` down to a multiple of `align` (a power of two).
pub(crate) fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

/// Round `value` up to a multiple of `align` (a power of two); saturating.
pub(crate) fn align_up(value: u64, align: u64) -> u64 {
    value.saturating_add(align - 1) & !(align - 1)
}

/// Reserve and prepare the host region (f.1): one `mmap`, one `madvise`, the page touch,
/// then one `mlock` when the run's host tier is `PinnedHost`.
///
/// `bytes` is the usable size and is already a multiple of `align`; `align` is the larger of
/// the page size and the 64 KiB granule, so every size class is aligned as AR-I1 requires.
pub(crate) fn new_host(
    bytes: u64,
    align: u64,
    want_pinned: bool,
    huge_pages: Guarantee,
    memlock: Guarantee,
) -> Result<HostRegion> {
    let raw_len = usize::try_from(bytes.saturating_add(align)).map_err(|_| AmoruError::Config {
        name: "budget.host",
        msg: format!("host region of {bytes} bytes does not fit this address space"),
    })?;
    #[cfg(target_os = "linux")]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
    // MAP_NORESERVE is a Linux flag; on macOS an anonymous private mapping is already
    // lazily committed, and the page touch below is what charges it either way (f.1).
    #[cfg(not(target_os = "linux"))]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
    MMAP_CALLS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: a fresh anonymous mapping of `raw_len` bytes; the kernel chooses the address
    // and the region is owned by this `Mapping` until it drops (AR-I3).
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            raw_len,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            -1,
            0,
        )
    };
    if raw == libc::MAP_FAILED {
        let err = std::io::Error::last_os_error();
        return Err(AmoruError::Config {
            name: "budget.host",
            msg: format!("mmap of {raw_len} bytes failed: {err}"),
        });
    }
    let raw = raw.cast::<u8>();
    let base_addr = align_up(raw as usize as u64, align);
    // SAFETY: `base_addr - raw` is at most `align - 1` and the mapping is `raw_len ==
    // bytes + align` long, so the aligned base plus `bytes` stays inside it (AR-I1).
    let base = unsafe { raw.add((base_addr - raw as usize as u64) as usize) };
    let mut mapping = Mapping {
        raw,
        raw_len,
        base,
        bytes,
        backing: Backing::Host { pinned: false },
    };

    let huge_pages_active = apply_huge_pages(&mapping, huge_pages)?;
    touch(&mapping, if huge_pages_active { 2 << 20 } else { align });
    let pinned = if want_pinned {
        apply_memlock(&mapping, memlock)?
    } else {
        false
    };
    mapping.backing = Backing::Host { pinned };
    Ok(HostRegion {
        mapping,
        huge_pages_active,
        pinned,
    })
}

/// `madvise(MADV_HUGEPAGE)` under the guarantee rules of f.1: a `Present` guarantee that
/// fails is a platform bug (`Config`), a `Probed(true)` guarantee that fails falls back.
fn apply_huge_pages(m: &Mapping, huge_pages: Guarantee) -> Result<bool> {
    match huge_pages {
        Guarantee::Unknown => Err(AmoruError::Config {
            name: "host_profile",
            msg: "huge_pages is Unknown at the arena; discovery resolves every guarantee \
                  before the arena is built (DS-I6)"
                .into(),
        }),
        Guarantee::Absent | Guarantee::Probed(false) => Ok(false),
        Guarantee::Present | Guarantee::Probed(true) => {
            let err = madvise_hugepage(m);
            match (err, huge_pages) {
                (None, _) => Ok(true),
                (Some(msg), Guarantee::Present) => Err(AmoruError::Config {
                    name: "host_profile",
                    msg: format!("huge_pages is declared Present but madvise failed: {msg}"),
                }),
                (Some(msg), _) => {
                    tracing::warn!(target: "arena.fallback", reason = %msg, "huge pages unavailable; continuing without them");
                    Ok(false)
                }
            }
        }
    }
}

/// `madvise(MADV_HUGEPAGE)`; `None` on success, the reason on failure. The flag is Linux
/// only, so every other host reports it as unavailable and the guarantee rules above decide
/// what that means.
#[cfg(target_os = "linux")]
fn madvise_hugepage(m: &Mapping) -> Option<String> {
    // SAFETY: `base`/`bytes` is the usable range of a live mapping; `madvise` only advises.
    let rc = unsafe { libc::madvise(m.base.cast(), m.bytes as usize, libc::MADV_HUGEPAGE) };
    if rc == 0 {
        None
    } else {
        Some(std::io::Error::last_os_error().to_string())
    }
}

/// See the Linux version above.
#[cfg(not(target_os = "linux"))]
fn madvise_hugepage(_m: &Mapping) -> Option<String> {
    Some("MADV_HUGEPAGE is a Linux facility; this host has none".into())
}

/// Touch every page of the region once so the cgroup charge happens at start and the
/// footprint is flat from the first morsel (f.1, AR-T11). The stride is the page size, or
/// 2 MiB when transparent huge pages backed the region.
fn touch(m: &Mapping, stride: u64) {
    let stride = stride.max(1) as usize;
    let len = m.bytes as usize;
    let mut off = 0usize;
    while off < len {
        let chunk = stride.min(len - off);
        // SAFETY: `off + chunk <= bytes`, so the write stays inside the mapping, which is
        // readable and writable and not yet shared with any other thread.
        unsafe { std::ptr::write_bytes(m.base.add(off), 0, chunk) };
        off += chunk;
    }
}

/// Page-lock the whole region (f.1) under the guarantee rules: `Present` that fails is a
/// platform bug, `Probed(true)` that fails falls back to the `Host` tier (AR-I6).
fn apply_memlock(m: &Mapping, memlock: Guarantee) -> Result<bool> {
    match memlock {
        Guarantee::Unknown => Err(AmoruError::Config {
            name: "host_profile",
            msg: "memlock is Unknown at the arena; discovery resolves every guarantee before \
                  the arena is built (DS-I6)"
                .into(),
        }),
        Guarantee::Absent | Guarantee::Probed(false) => Err(AmoruError::Config {
            name: "arena.pin",
            msg: "host_tier is PinnedHost but memlock is not available; discovery chooses \
                  PinnedHost only when it is (03 d.1)"
                .into(),
        }),
        Guarantee::Present | Guarantee::Probed(true) => {
            MLOCK_CALLS.fetch_add(1, Ordering::Relaxed);
            // SAFETY: `base`/`bytes` is the usable range of a live mapping; `mlock` only
            // changes its residency.
            let rc = unsafe { libc::mlock(m.base.cast(), m.bytes as usize) };
            if rc == 0 {
                return Ok(true);
            }
            let err = std::io::Error::last_os_error();
            if memlock == Guarantee::Present {
                return Err(AmoruError::Config {
                    name: "host_profile",
                    msg: format!("memlock is declared Present but mlock failed: {err}"),
                });
            }
            tracing::warn!(target: "arena.fallback", reason = %err, "memlock refused; the run's host tier is Host");
            Ok(false)
        }
    }
}

/// `munlock`, used only when a locked region drops.
///
/// # Safety
/// `ptr`/`len` must be a range this process locked and has not unlocked.
unsafe fn unlock(ptr: *mut u8, len: usize) {
    // SAFETY: the caller guarantees the range was locked by this process and is still mapped.
    unsafe {
        libc::munlock(ptr.cast(), len);
    }
}

/// Reserve one device region (f.2): `cuMemAlloc` of the device budget, one retry at 90%,
/// then a `Config` error.
#[cfg(feature = "cuda")]
pub(crate) fn new_device(id: amoru_kernel::DeviceId, bytes: u64) -> Result<(Mapping, u64)> {
    use cudarc::driver::sys as cu;

    // SAFETY: `cuInit` is idempotent and takes no pointers.
    let rc = unsafe { cu::cuInit(0) };
    if rc != cu::CUresult::CUDA_SUCCESS {
        return Err(AmoruError::Config {
            name: "budget.device",
            msg: format!("cuInit failed for device {}: {rc:?}", id.0),
        });
    }
    let mut dev: cu::CUdevice = 0;
    // SAFETY: `dev` is a live local the driver writes once.
    let rc = unsafe { cu::cuDeviceGet(&mut dev, i32::from(id.0)) };
    if rc != cu::CUresult::CUDA_SUCCESS {
        return Err(AmoruError::Config {
            name: "budget.device",
            msg: format!("cuDeviceGet failed for device {}: {rc:?}", id.0),
        });
    }
    let mut ctx: cu::CUcontext = std::ptr::null_mut();
    // SAFETY: `ctx` is a live local the driver writes once; the primary context is
    // reference counted and released when the process ends.
    let rc = unsafe { cu::cuDevicePrimaryCtxRetain(&mut ctx, dev) };
    if rc != cu::CUresult::CUDA_SUCCESS {
        return Err(AmoruError::Config {
            name: "budget.device",
            msg: format!(
                "cuDevicePrimaryCtxRetain failed for device {}: {rc:?}",
                id.0
            ),
        });
    }
    // SAFETY: `ctx` is the primary context just retained for this device.
    let rc = unsafe { cu::cuCtxSetCurrent(ctx) };
    if rc != cu::CUresult::CUDA_SUCCESS {
        return Err(AmoruError::Config {
            name: "budget.device",
            msg: format!("cuCtxSetCurrent failed for device {}: {rc:?}", id.0),
        });
    }
    for (attempt, want) in [bytes, bytes / 10 * 9].into_iter().enumerate() {
        let mut dptr: cu::CUdeviceptr = 0;
        // SAFETY: `dptr` is a live local the driver writes once; `want` is the request.
        let rc = unsafe { cu::cuMemAlloc_v2(&mut dptr, want as usize) };
        if rc == cu::CUresult::CUDA_SUCCESS {
            if attempt == 1 {
                tracing::warn!(target: "arena.fallback", device = id.0, bytes = want, "device budget reduced to 90%");
            }
            let mapping = Mapping {
                raw: std::ptr::null_mut(),
                raw_len: 0,
                base: dptr as usize as *mut u8,
                bytes: want,
                backing: Backing::Device,
            };
            return Ok((mapping, want));
        }
    }
    Err(AmoruError::Config {
        name: "budget.device",
        msg: format!("cuMemAlloc of {bytes} bytes on device {} failed", id.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_helpers() {
        assert_eq!(align_up(0, 64), 0);
        assert_eq!(align_up(1, 64), 64);
        assert_eq!(align_up(64, 64), 64);
        assert_eq!(align_down(65, 64), 64);
        assert_eq!(align_down(63, 64), 0);
    }

    #[test]
    fn an_unknown_guarantee_is_a_config_error() {
        let r = new_host(
            1 << 20,
            1 << 16,
            false,
            Guarantee::Unknown,
            Guarantee::Absent,
        );
        assert!(matches!(r, Err(AmoruError::Config { name, .. }) if name == "host_profile"));
        let r = new_host(
            1 << 20,
            1 << 16,
            true,
            Guarantee::Absent,
            Guarantee::Unknown,
        );
        assert!(matches!(r, Err(AmoruError::Config { name, .. }) if name == "host_profile"));
    }

    #[test]
    fn pinning_without_memlock_is_a_config_error() {
        let r = new_host(
            1 << 20,
            1 << 16,
            true,
            Guarantee::Absent,
            Guarantee::Probed(false),
        );
        assert!(matches!(r, Err(AmoruError::Config { name, .. }) if name == "arena.pin"));
    }

    #[test]
    fn a_declared_huge_page_guarantee_that_fails_is_a_config_error() {
        // On a host with no `MADV_HUGEPAGE` the declared guarantee is a platform bug (f.1);
        // where the call does work the region is simply created, which is the other half of
        // the same rule.
        let r = new_host(
            1 << 20,
            1 << 16,
            false,
            Guarantee::Present,
            Guarantee::Absent,
        );
        match r {
            Err(AmoruError::Config { name, .. }) => assert_eq!(name, "host_profile"),
            Ok(region) => assert!(region.huge_pages_active),
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_region_is_aligned_touched_and_released() {
        let before = {
            #[cfg(feature = "test-shim")]
            {
                syscall_counts().mmap
            }
            #[cfg(not(feature = "test-shim"))]
            {
                0
            }
        };
        let region = new_host(
            4 << 20,
            1 << 16,
            false,
            Guarantee::Probed(false),
            Guarantee::Absent,
        )
        .expect("a 4 MiB region");
        assert_eq!(region.mapping.base() as usize % (1 << 16), 0);
        assert_eq!(region.mapping.bytes(), 4 << 20);
        assert!(!region.huge_pages_active);
        assert!(!region.pinned);
        // The touch in `new_host` wrote every page, so every byte of the region is
        // addressable and zero.
        // SAFETY: test-only; the whole usable range of a live mapping.
        let bytes = unsafe { std::slice::from_raw_parts(region.mapping.base(), 4 << 20) };
        assert!(bytes.iter().all(|b| *b == 0));
        // The counter is process wide and the unit tests run in parallel, so this only
        // proves the reservation went through the shim; AR-T3 owns the "exactly one" half
        // in a test binary of its own.
        #[cfg(feature = "test-shim")]
        assert!(syscall_counts().mmap > before);
        let _ = before;
    }
}
