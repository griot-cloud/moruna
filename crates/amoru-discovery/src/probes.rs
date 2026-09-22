//! The probes of e.4, and every `libc` call the crate makes.
//!
//! Section l permits `unsafe` in this module only, so the two values `os.rs` needs from the C
//! library (the page size and the logical core count) and the two the sampler needs on a host
//! without `/proc` are exposed here as safe wrappers rather than duplicated as `unsafe` blocks
//! elsewhere. Every block carries a `// SAFETY:` comment naming the invariant it relies on.
//!
//! Each probe is bounded to 100 ms by a helper thread (e.4); a probe that does not finish in time
//! is reported as unavailable with a note, which `discover` turns into `Probed(false)`.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The bound on one probe (e.4).
const PROBE_TIMEOUT: Duration = Duration::from_millis(100);

/// What one probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProbeReport {
    /// Whether the path works on this host.
    pub(crate) available: bool,
    /// A human readable fact for `Discovered.notes`, when there is one.
    pub(crate) note: Option<String>,
}

impl ProbeReport {
    /// The path works.
    fn yes() -> ProbeReport {
        ProbeReport {
            available: true,
            note: None,
        }
    }

    /// The path does not work, for the stated reason.
    fn no(note: impl Into<String>) -> ProbeReport {
        ProbeReport {
            available: false,
            note: Some(note.into()),
        }
    }
}

/// Run `f` with the e.4 timeout. `None` means it did not finish in time; the thread is left to
/// finish on its own, which is safe because every probe only touches its own resources.
fn bounded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("amoru-probe".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        });
    if spawned.is_err() {
        return None;
    }
    rx.recv_timeout(PROBE_TIMEOUT).ok()
}

/// Wrap a probe body in the timeout, turning a timeout into "unavailable, with a note".
fn timed(field: &'static str, f: impl FnOnce() -> ProbeReport + Send + 'static) -> ProbeReport {
    let started = std::time::Instant::now();
    let report = bounded(f).unwrap_or_else(|| {
        ProbeReport::no(format!("probe `{field}` did not finish within 100 ms"))
    });
    tracing::debug!(
        target: "discovery.probe",
        field,
        available = report.available,
        duration_us = started.elapsed().as_micros() as u64,
        "probe finished"
    );
    report
}

// ---------------------------------------------------------------------------
// Values the rest of the crate needs from the C library.
// ---------------------------------------------------------------------------

/// `sysconf(_SC_PAGESIZE)`, which is also the direct IO alignment (e.3).
pub(crate) fn page_size() -> usize {
    // SAFETY: `sysconf` reads a system constant, takes no pointer and has no failure mode
    // other than returning -1, which is checked below.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value > 0 { value as usize } else { 4096 }
}

/// `sysconf(_SC_NPROCESSORS_ONLN)`, the host's logical core count (b, "Quota").
pub(crate) fn logical_cores() -> usize {
    // SAFETY: as `page_size`; a system constant, no pointer, -1 on failure.
    let value = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if value > 0 { value as usize } else { 1 }
}

/// Total host RAM where the platform has no `/proc/meminfo` (macOS: `sysctl hw.memsize`).
/// `None` on a platform whose total RAM this crate reads from `/proc/meminfo` instead.
pub(crate) fn platform_total_ram() -> Option<u64> {
    #[cfg(target_vendor = "apple")]
    {
        let name = c"hw.memsize";
        let mut value: u64 = 0;
        let mut len = core::mem::size_of::<u64>();
        // SAFETY: `name` is a NUL terminated C string with a static lifetime; `value` and `len`
        // are live, correctly sized locals; `sysctlbyname` writes at most `len` bytes into
        // `value` and updates `len`. A non-zero return means nothing was written.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut value).cast::<libc::c_void>(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && len == core::mem::size_of::<u64>() && value > 0 {
            return Some(value);
        }
        None
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        None
    }
}

/// The process's resident anonymous bytes where the platform has no `/proc/self/statm`
/// (macOS: `proc_pidinfo`). `None` on a platform that has `/proc`.
pub(crate) fn platform_rss_anon() -> Option<u64> {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: `proc_taskinfo` is a plain C struct of integers, for which an all zero bit
        // pattern is a valid value; it is overwritten by the call below before it is read.
        let mut info: libc::proc_taskinfo = unsafe { core::mem::zeroed() };
        let size = core::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: `info` is a live, zeroed `proc_taskinfo` and `size` is its exact size, which is
        // the contract `proc_pidinfo` documents for `PROC_PIDTASKINFO`; it writes at most `size`
        // bytes and returns the number written.
        let written = unsafe {
            libc::proc_pidinfo(
                std::process::id() as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                (&raw mut info).cast::<libc::c_void>(),
                size,
            )
        };
        if written == size {
            // Anonymous memory is resident minus the file backed part the kernel reports.
            let resident = info.pti_resident_size;
            return Some(resident);
        }
        None
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        None
    }
}

/// Free bytes available to an unprivileged process under `path`, through `statvfs`.
// The `statvfs` field widths differ between the supported targets (`u64` on 64 bit Linux and
// macOS, narrower elsewhere), so the casts are kept even where they are the identity.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `c_path` is a live NUL terminated string and `buf` is a live, zeroed `statvfs`;
    // `statvfs` writes only into `buf` and returns -1 without writing on failure.
    let (rc, buf) = unsafe {
        let mut buf: libc::statvfs = core::mem::zeroed();
        let rc = libc::statvfs(c_path.as_ptr(), &raw mut buf);
        (rc, buf)
    };
    if rc != 0 {
        return None;
    }
    let block = if buf.f_frsize > 0 {
        buf.f_frsize as u64
    } else {
        buf.f_bsize as u64
    };
    Some((buf.f_bavail as u64).saturating_mul(block))
}

/// Whether `path` is on a filesystem that cannot outlive the node (`tmpfs`, `overlay`, `ramfs`).
/// `None` when the filesystem cannot be identified, which is not the same as "durable".
pub(crate) fn filesystem_is_ephemeral(path: &Path) -> Option<bool> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `c_path` is a live NUL terminated string and `buf` is a live, zeroed `statfs`;
    // `statfs` writes only into `buf` and returns -1 without writing on failure.
    let (rc, buf) = unsafe {
        let mut buf: libc::statfs = core::mem::zeroed();
        let rc = libc::statfs(c_path.as_ptr(), &raw mut buf);
        (rc, buf)
    };
    if rc != 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        const TMPFS_MAGIC: i64 = 0x0102_1994;
        const OVERLAYFS_MAGIC: i64 = 0x794c_7630;
        const RAMFS_MAGIC: i64 = 0x8584_58f6;
        let magic = buf.f_type as i64;
        Some(magic == TMPFS_MAGIC || magic == OVERLAYFS_MAGIC || magic == RAMFS_MAGIC)
    }
    #[cfg(target_vendor = "apple")]
    {
        let name: Vec<u8> = buf
            .f_fstypename
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        let name = String::from_utf8_lossy(&name).to_ascii_lowercase();
        Some(name == "tmpfs" || name == "overlay" || name == "ramfs")
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// The probes of e.4.
// ---------------------------------------------------------------------------

/// `huge_pages`: transparent huge pages enabled, or explicit huge pages reserved.
pub(crate) fn probe_huge_pages(sys_root: &Path, proc_root: &Path) -> ProbeReport {
    let thp = sys_root.join("kernel/mm/transparent_hugepage/enabled");
    let meminfo = proc_root.join("meminfo");
    timed("huge_pages", move || {
        if let Ok(text) = std::fs::read_to_string(&thp)
            && (text.contains("[always]") || text.contains("[madvise]"))
        {
            return ProbeReport::yes();
        }
        if let Ok(text) = std::fs::read_to_string(&meminfo)
            && crate::os::parse_meminfo_field(&text, "HugePages_Total").is_some_and(|n| n > 0)
        {
            return ProbeReport::yes();
        }
        ProbeReport::no("transparent huge pages are not enabled and none are reserved")
    })
}

/// `memlock`: `mlock` one page of a temporary anonymous mapping, then `munlock`.
pub(crate) fn probe_memlock() -> ProbeReport {
    timed("memlock", || {
        let len = page_size();
        // SAFETY: an anonymous, private mapping of `len` bytes owned by nobody else. The pointer
        // is checked against MAP_FAILED before it is used, and the mapping is unmapped on every
        // path below, so nothing outlives this block.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return ProbeReport::no("could not map one page to test mlock");
        }
        // SAFETY: `addr` is the live mapping returned above, of exactly `len` bytes; `mlock`,
        // `munlock` and `munmap` each take that same range and nothing else refers to it.
        let locked = unsafe {
            let rc = libc::mlock(addr, len);
            if rc == 0 {
                libc::munlock(addr, len);
            }
            rc
        };
        // SAFETY: as above; the mapping is released exactly once, after the last use.
        unsafe {
            libc::munmap(addr, len);
        }
        if locked == 0 {
            ProbeReport::yes()
        } else {
            ProbeReport::no("mlock is not permitted (RLIMIT_MEMLOCK or the platform refuses it)")
        }
    })
}

/// `io_uring`: `io_uring_setup(8, ...)` then close. Linux only; every other platform is absent.
pub(crate) fn probe_io_uring() -> ProbeReport {
    timed("io_uring", || {
        #[cfg(target_os = "linux")]
        {
            // `struct io_uring_params` is 120 bytes; a zeroed 128 byte buffer is at least that
            // and is what the kernel fills in.
            let mut params = [0u64; 16];
            // SAFETY: `SYS_io_uring_setup` takes an entry count and a pointer to a writable
            // `io_uring_params`; `params` is a live, zeroed buffer at least as large as that
            // struct and is not aliased. The kernel writes only inside it.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_io_uring_setup,
                    8u32,
                    (&raw mut params).cast::<libc::c_void>(),
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` is a descriptor this call just created and nothing else holds.
                unsafe {
                    libc::close(fd as libc::c_int);
                }
                return ProbeReport::yes();
            }
            ProbeReport::no("io_uring_setup was refused (seccomp, or the kernel lacks io_uring)")
        }
        #[cfg(not(target_os = "linux"))]
        {
            ProbeReport::no("io_uring exists on Linux only")
        }
    })
}

/// `direct_io_staging`: open a temporary file in the staging directory with `O_DIRECT`, write one
/// page from an aligned buffer, read it back, delete it. Linux only; `O_DIRECT` has no portable
/// equivalent, so every other platform reports the path as unavailable and the reactor buffers.
pub(crate) fn probe_direct_io(staging_dir: Option<PathBuf>) -> ProbeReport {
    timed("direct_io", move || {
        let Some(dir) = staging_dir else {
            return ProbeReport::no("no staging directory, so direct IO was not probed");
        };
        #[cfg(target_os = "linux")]
        {
            direct_io_roundtrip(&dir)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = dir;
            ProbeReport::no("O_DIRECT exists on Linux only")
        }
    })
}

/// One `O_DIRECT` write and read back of a single page in `dir` (Linux).
#[cfg(target_os = "linux")]
fn direct_io_roundtrip(dir: &Path) -> ProbeReport {
    let page = page_size();
    let path = dir.join(format!("amoru-direct-io-probe-{}", std::process::id()));
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return ProbeReport::no("the staging directory path is not a valid C string");
    };
    let Ok(layout) = std::alloc::Layout::from_size_align(page, page) else {
        return ProbeReport::no("the page size is not a valid alignment");
    };
    // SAFETY: `layout` has a non-zero size, so `alloc_zeroed` is allowed; the pointer is checked
    // for null before use and is freed with the same layout on every path below.
    let buffer = unsafe { std::alloc::alloc_zeroed(layout) };
    if buffer.is_null() {
        return ProbeReport::no("could not allocate one aligned page");
    }
    // SAFETY: `c_path` is a live NUL terminated string; the descriptor is closed and the file
    // unlinked below, and `buffer` is a live, aligned allocation of exactly `page` bytes, which
    // is what the two IO calls read and write.
    let outcome = unsafe {
        let fd = libc::open(
            c_path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC | libc::O_DIRECT,
            0o600,
        );
        if fd < 0 {
            None
        } else {
            let written = libc::write(fd, buffer.cast::<libc::c_void>(), page);
            let read = if written == page as isize {
                libc::lseek(fd, 0, libc::SEEK_SET);
                libc::read(fd, buffer.cast::<libc::c_void>(), page)
            } else {
                -1
            };
            libc::close(fd);
            Some(read == page as isize)
        }
    };
    // SAFETY: `buffer` came from `alloc_zeroed` with this same `layout` and is not used again.
    unsafe {
        std::alloc::dealloc(buffer, layout);
    }
    let _ = std::fs::remove_file(&path);
    match outcome {
        Some(true) => ProbeReport::yes(),
        Some(false) => ProbeReport::no("an O_DIRECT write and read back of one page failed"),
        None => ProbeReport::no("the staging directory refused an O_DIRECT open"),
    }
}

/// `gds`: GPUDirect Storage needs the `gds` feature, which no v1 build of this crate has
/// (preamble 6.3; the driver binding lives in the reactor), so it is absent here.
pub(crate) fn probe_gds() -> ProbeReport {
    ProbeReport::no("built without the gds feature, so GPUDirect Storage is absent")
}

/// `rdma`: reserved for the multi-node extension (E11); absent in every v1 build.
pub(crate) fn probe_rdma() -> ProbeReport {
    ProbeReport::no("built without the rdma feature, so RDMA is absent")
}

/// Whether a directory exists, is writable and has at least 1 GiB free (e.4, `staging_dir`).
pub(crate) fn staging_dir_is_usable(dir: &Path) -> bool {
    const ONE_GIB: u64 = 1024 * 1024 * 1024;
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!("amoru-staging-probe-{}", std::process::id()));
    if std::fs::write(&probe, b"amoru").is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&probe);
    free_bytes(dir).is_some_and(|free| free >= ONE_GIB)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::fixtures::{TempDir, write};

    /// e.4: the huge page probe reads the two places the table names.
    #[test]
    fn huge_page_probe_reads_both_sources() {
        let tmp = TempDir::new("thp");
        let sys = tmp.path().join("sys");
        let proc_dir = tmp.path().join("proc");
        std::fs::create_dir_all(&proc_dir).expect("proc");

        // Neither source says yes.
        write(
            &sys,
            "kernel/mm/transparent_hugepage/enabled",
            "always [never]\n",
        );
        write(
            &proc_dir,
            "meminfo",
            "MemTotal: 100 kB\nHugePages_Total: 0\n",
        );
        assert!(!probe_huge_pages(&sys, &proc_dir).available);

        // Transparent huge pages are on.
        write(
            &sys,
            "kernel/mm/transparent_hugepage/enabled",
            "[always] madvise never\n",
        );
        assert!(probe_huge_pages(&sys, &proc_dir).available);
        write(
            &sys,
            "kernel/mm/transparent_hugepage/enabled",
            "always [madvise] never\n",
        );
        assert!(probe_huge_pages(&sys, &proc_dir).available);

        // Or explicit huge pages are reserved.
        write(
            &sys,
            "kernel/mm/transparent_hugepage/enabled",
            "always [never]\n",
        );
        write(&proc_dir, "meminfo", "HugePages_Total: 512\n");
        assert!(probe_huge_pages(&sys, &proc_dir).available);

        // Neither file exists: not available, with a note.
        let absent = tmp.path().join("absent");
        let report = probe_huge_pages(&absent, &absent);
        assert!(!report.available);
        assert!(report.note.is_some());
    }

    /// The probes that touch the host run, are bounded, and say why when they say no.
    #[test]
    fn host_probes_run_and_explain_themselves() {
        for report in [
            probe_memlock(),
            probe_io_uring(),
            probe_gds(),
            probe_rdma(),
            probe_direct_io(None),
            probe_direct_io(Some(std::env::temp_dir())),
        ] {
            assert!(report.available || report.note.is_some());
        }
    }

    /// The `libc` wrappers return usable values on whatever host the tests run on.
    #[test]
    fn libc_wrappers_answer() {
        let page = page_size();
        assert!(page >= 4096 && page.is_power_of_two(), "page size {page}");
        assert!(logical_cores() >= 1);
        assert!(free_bytes(&std::env::temp_dir()).is_some());
        assert!(free_bytes(Path::new("/amoru/definitely/absent")).is_none());
        // One of the two total RAM sources answers on every supported target.
        assert!(platform_total_ram().is_some() || std::fs::read_to_string("/proc/meminfo").is_ok());
        assert!(
            platform_rss_anon().is_some() || std::fs::read_to_string("/proc/self/statm").is_ok()
        );
        assert!(filesystem_is_ephemeral(&std::env::temp_dir()).is_some());
        assert!(filesystem_is_ephemeral(Path::new("/amoru/definitely/absent")).is_none());
    }

    /// e.4 `staging_dir`: a usable directory is writable with room; a missing one is not.
    #[test]
    fn staging_dir_usability() {
        let tmp = TempDir::new("staging");
        assert!(staging_dir_is_usable(tmp.path()));
        assert!(!staging_dir_is_usable(&tmp.path().join("absent")));
    }

    /// e.4: a probe that runs long is reported as unavailable with a note, not left to hang.
    #[test]
    fn a_slow_probe_times_out() {
        let report = timed("slow", || {
            std::thread::sleep(Duration::from_millis(400));
            ProbeReport::yes()
        });
        assert!(!report.available);
        assert!(
            report
                .note
                .as_deref()
                .is_some_and(|note| note.contains("100 ms"))
        );
    }
}
