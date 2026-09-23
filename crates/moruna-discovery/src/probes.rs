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
        .name("moruna-probe".to_string())
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

/// `TASK_VM_INFO` (`mach/task_info.h`), the `task_info` flavour that separates the anonymous
/// part of a process's memory from the file backed part.
#[cfg(target_vendor = "apple")]
const TASK_VM_INFO: libc::task_flavor_t = 22;

/// The prefix of `task_vm_info` through `phys_footprint`, which `mach/task_info.h` calls "rev1" and whose
/// `TASK_VM_INFO_REV1_COUNT` is exactly the 38 `natural_t` words this struct is long.
/// `task_info` fills the first `count` words and reports back how many it filled, so declaring
/// the prefix the caller reads, and then checking the reported count, is how one asks for a
/// stable subset of a flavour the kernel keeps extending.
#[cfg(target_vendor = "apple")]
#[repr(C)]
#[derive(Clone, Copy)]
struct TaskVmInfoRev1 {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    /// Anonymous ("internal") resident bytes.
    internal: u64,
    internal_peak: u64,
    /// File backed ("external") resident bytes: mapped files and loaded images.
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    /// Anonymous bytes the compressor holds; still charged to the process.
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    /// The ledger macOS charges the process for and enforces its limits against.
    phys_footprint: u64,
}

/// The process's `(anonymous, file backed)` resident bytes where the platform has no
/// `/proc/self/statm` (macOS). `None` on a platform that has `/proc`.
///
/// The anonymous figure is `phys_footprint`: the ledger macOS charges a process for, enforces
/// its own per-process memory limits against, and shows as "Memory" in Activity Monitor. It is
/// the DS-I4 quantity on this platform: it counts the process's own dirty pages, including the
/// anonymous pages the compressor has taken (the analogue of `unevictable`: still owed by the
/// process, not reclaimable by dropping a cache), and it excludes the file backed pages a
/// mapped Parquet file or a loaded dylib contributes. Those are `external`, returned here as
/// the file backed half, which is what `/proc/self/statm`'s `shared` field gives on Linux.
///
/// The three candidate routes were measured against the same process at the same moment, with
/// 256 MiB of touched anonymous memory and then a 512 MiB file mapped and fully read:
/// `proc_pidinfo(PROC_PIDTASKINFO)`'s `pti_resident_size` and `mach_task_basic_info`'s
/// `resident_size` returned byte-identical values that rose by the whole 537 MB of mapped file,
/// while `phys_footprint` rose by 0.3 MB (page tables) and `external` took the 537 MB. Both
/// resident routes therefore report the quantity DS-I4 forbids, and neither is used; the older
/// `proc_pid_rusage` ledger is kept only as a fallback, since it reports the same
/// `phys_footprint` to the byte but cannot report the file backed half.
pub(crate) fn platform_anon_and_file() -> Option<(u64, u64)> {
    #[cfg(target_vendor = "apple")]
    {
        const WORDS: libc::mach_msg_type_number_t = (core::mem::size_of::<TaskVmInfoRev1>()
            / core::mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        // SAFETY: `TaskVmInfoRev1` is a `repr(C)` struct of integers, for which an all zero bit
        // pattern is a valid value; it is overwritten by the call below before it is read.
        let mut info: TaskVmInfoRev1 = unsafe { core::mem::zeroed() };
        let mut count = WORDS;
        // SAFETY: `mach_task_self_` is a port name the dynamic loader initialises before `main`
        // and never writes again, so this is a plain load of an initialised `mach_port_t`.
        // `libc` deprecates it in favour of the `mach2` crate; section l makes this module the
        // one place the crate calls `libc`, and a second FFI crate for one port name is a worse
        // trade than reading the static the `mach_task_self()` wrapper reads.
        #[allow(deprecated)]
        let task = unsafe { libc::mach_task_self_ };
        // SAFETY: `info` is a live, zeroed `TaskVmInfoRev1` and `count` is its exact length in
        // `natural_t` words, which is the contract `task_info` documents for `TASK_VM_INFO`: it
        // writes at most `count` words into the buffer and sets `count` to the number written.
        let kr = unsafe {
            libc::task_info(
                task,
                TASK_VM_INFO,
                (&raw mut info).cast::<libc::integer_t>(),
                &raw mut count,
            )
        };
        // A kernel that filled fewer words than asked did not reach `phys_footprint`, which
        // would leave the zero this struct was created with; that is a failure, not a zero.
        if kr == 0 && count >= WORDS {
            return Some((info.phys_footprint, info.external));
        }
        platform_phys_footprint_rusage().map(|anon| (anon, 0))
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        None
    }
}

/// `phys_footprint` through the older `proc_pid_rusage` ledger, for a kernel whose `task_info`
/// does not reach the field. The same number, without the file backed half.
#[cfg(target_vendor = "apple")]
fn platform_phys_footprint_rusage() -> Option<u64> {
    // SAFETY: `rusage_info_v4` is a plain C struct of integers, for which an all zero bit
    // pattern is a valid value; it is overwritten by the call below before it is read.
    let mut info: libc::rusage_info_v4 = unsafe { core::mem::zeroed() };
    // SAFETY: `info` is a live, zeroed `rusage_info_v4`, which is the struct
    // `proc_pid_rusage` documents for `RUSAGE_INFO_V4`; it writes only into that buffer and
    // returns non-zero without writing on failure.
    let rc = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            (&raw mut info).cast::<libc::rusage_info_t>(),
        )
    };
    if rc == 0 {
        return Some(info.ri_phys_footprint);
    }
    None
}

/// What the sampler is measuring on a host where the platform answers instead of `/proc`, for
/// `Discovered::notes`, so a run report measured on a developer's macOS host says which
/// quantity its memory figures are (DS-I4). `None` where `/proc` answers and the note would be
/// noise.
pub(crate) fn platform_memory_note() -> Option<String> {
    platform_anon_and_file()?;
    Some(
        "memory is measured through mach on this host: anon_bytes is the process's \
         phys_footprint, the ledger macOS enforces its own limits against, which counts \
         anonymous and compressed pages and excludes mapped files and loaded images (DS-I4)"
            .to_string(),
    )
}

/// Map `bytes` of `file` read-only, read one byte of every page so those pages become resident,
/// call `observe` while the mapping is live, then unmap. Test-only: it exists so DS-T13 can make
/// a large amount of file backed memory resident without an `unsafe` block outside this module
/// (section l). `None` when the mapping could not be made.
#[cfg(test)]
pub(crate) fn with_mapped_file<R>(
    file: &std::fs::File,
    bytes: usize,
    observe: impl FnOnce() -> R,
) -> Option<R> {
    use std::os::fd::AsRawFd;
    // SAFETY: a null `addr` asks the kernel to choose the address; `bytes` is the length of the
    // mapping and `file` is a live, readable file at least that long. The call writes nothing
    // and returns `MAP_FAILED` on failure.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return None;
    }
    let mut sum: u64 = 0;
    let mut offset = 0usize;
    while offset < bytes {
        // SAFETY: `offset < bytes`, and the mapping is `bytes` long and readable, so this reads
        // one byte inside it.
        let byte = unsafe { *addr.cast::<u8>().add(offset) };
        sum = sum.wrapping_add(u64::from(byte));
        offset += page_size();
    }
    // The sum is never used; without this the reads are dead code and the pages never fault in.
    std::hint::black_box(sum);
    let observed = observe();
    // SAFETY: `addr` and `bytes` are exactly the mapping made above, which is not used again.
    unsafe { libc::munmap(addr, bytes) };
    Some(observed)
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
// `statfs::f_type` is `i64` on 64 bit Linux and narrower elsewhere, so the cast is kept even
// where it is the identity.
#[allow(clippy::unnecessary_cast)]
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
    timed("huge_pages", move || huge_pages_decision(&thp, &meminfo))
}

/// The `huge_pages` decision itself, without the timeout around it. The tests assert this, so the
/// rule the e.4 table states is proved independently of how fast a host runs the probe.
fn huge_pages_decision(thp: &Path, meminfo: &Path) -> ProbeReport {
    if let Ok(text) = std::fs::read_to_string(thp)
        && (text.contains("[always]") || text.contains("[madvise]"))
    {
        return ProbeReport::yes();
    }
    if let Ok(text) = std::fs::read_to_string(meminfo)
        && crate::os::parse_meminfo_field(&text, "HugePages_Total").is_some_and(|n| n > 0)
    {
        return ProbeReport::yes();
    }
    ProbeReport::no("transparent huge pages are not enabled and none are reserved")
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
    let path = dir.join(format!("moruna-direct-io-probe-{}", unique_tag()));
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

/// A name no other probe in this process, and no other process, is using at the same moment.
/// Several tests probe one directory at once, and so will several runs on one host.
fn unique_tag() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Whether a directory exists, is writable and has at least 1 GiB free (e.4, `staging_dir`).
pub(crate) fn staging_dir_is_usable(dir: &Path) -> bool {
    const ONE_GIB: u64 = 1024 * 1024 * 1024;
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!("moruna-staging-probe-{}", unique_tag()));
    if std::fs::write(&probe, b"moruna").is_err() {
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
        let thp = sys.join("kernel/mm/transparent_hugepage/enabled");
        let meminfo = proc_dir.join("meminfo");

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
        assert!(!huge_pages_decision(&thp, &meminfo).available);

        // Transparent huge pages are on.
        write(
            &sys,
            "kernel/mm/transparent_hugepage/enabled",
            "[always] madvise never\n",
        );
        assert!(huge_pages_decision(&thp, &meminfo).available);
        write(
            &sys,
            "kernel/mm/transparent_hugepage/enabled",
            "always [madvise] never\n",
        );
        assert!(huge_pages_decision(&thp, &meminfo).available);

        // Or explicit huge pages are reserved.
        write(
            &sys,
            "kernel/mm/transparent_hugepage/enabled",
            "always [never]\n",
        );
        write(&proc_dir, "meminfo", "HugePages_Total: 512\n");
        assert!(huge_pages_decision(&thp, &meminfo).available);

        // Neither file exists: not available, with a note.
        let absent = tmp.path().join("absent");
        let report = huge_pages_decision(&absent.join("thp"), &absent.join("meminfo"));
        assert!(!report.available);
        assert!(report.note.is_some());

        // And through the bounded wrapper, which answers either way.
        let report = probe_huge_pages(&sys, &proc_dir);
        assert!(report.available || report.note.is_some());
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
        assert!(free_bytes(Path::new("/moruna/definitely/absent")).is_none());
        // One of the two total RAM sources answers on every supported target.
        assert!(platform_total_ram().is_some() || std::fs::read_to_string("/proc/meminfo").is_ok());
        assert!(
            platform_anon_and_file().is_some()
                || std::fs::read_to_string("/proc/self/statm").is_ok()
        );
        // The note exists exactly where the platform path is the one that answers.
        assert_eq!(
            platform_memory_note().is_some(),
            platform_anon_and_file().is_some()
        );
        assert!(filesystem_is_ephemeral(&std::env::temp_dir()).is_some());
        assert!(filesystem_is_ephemeral(Path::new("/moruna/definitely/absent")).is_none());
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
