//! The operating system fallback: `/proc/meminfo`, `/proc/self/status`, `sysconf` (e.3, f.2).
//!
//! Both readers take the `/proc` root as a parameter, so the Linux path is exercised against a
//! fixture directory on any host. Where `/proc` does not exist (macOS, which is where the team
//! develops) the same questions are answered through `probes.rs` instead, so `discover` and the
//! sampler have an answer on every supported target (preamble 6.2, targets).

use std::path::Path;

use crate::probes;

/// The page size, which is also the direct IO alignment (e.3).
pub(crate) fn page_bytes() -> usize {
    probes::page_size()
}

/// The host's logical core count, the CPU quota when no cgroup sets one (b, "Quota").
pub(crate) fn logical_cores() -> f64 {
    probes::logical_cores() as f64
}

/// Total host RAM: `/proc/meminfo` `MemTotal` where there is a `/proc`, the platform's own
/// interface otherwise.
pub(crate) fn total_ram_bytes(proc_root: &Path) -> Option<u64> {
    if let Ok(text) = std::fs::read_to_string(proc_root.join("meminfo"))
        && let Some(kib) = parse_meminfo_field(&text, "MemTotal")
    {
        return Some(kib.saturating_mul(1024));
    }
    probes::platform_total_ram()
}

/// One `"<key>: <value> kB"` line of `/proc/meminfo`, as the number it carries.
pub(crate) fn parse_meminfo_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, rest) = line.split_once(':')?;
        if name.trim() != key {
            return None;
        }
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
}

/// The process's own anonymous and file backed resident bytes (f.2), which the sampler reads
/// outside a cgroup and inside one whose limit is not the ceiling (DS-I4): from
/// `/proc/self/status` where there is a `/proc`, from the platform otherwise. `None` when neither
/// answers, which leaves the sampler repeating its last sample.
///
/// The anonymous half is the DS-I4 quantity on both paths: `RssAnon` from `status` (the kernel's
/// own count of the process's resident anonymous pages, every thread's heap and every arena
/// region included), and `phys_footprint` from mach, never plain resident size (see
/// `probes::platform_anon_and_file`). Never `VmRSS`, which adds `RssFile` and `RssShmem`: the
/// mapped text of the binary and its libraries, and every file the process has mapped, are
/// pages the kernel can drop and the process never allocated.
pub(crate) fn process_memory(proc_root: &Path, _page: usize) -> Option<(u64, u64)> {
    if let Ok(text) = std::fs::read_to_string(proc_root.join("self/status"))
        && let Some(pair) = parse_status(&text)
    {
        return Some(pair);
    }
    probes::platform_anon_and_file()
}

/// `/proc/self/status`: anonymous memory is `RssAnon`, and the file backed part is `RssFile`
/// plus `RssShmem`, all in kB. Shared memory goes with the file half because a cgroup's
/// `memory.stat` counts it under `file` and not `anon`, so the two paths agree on what a
/// process's anonymous memory is; a swapped page is in neither, as in `memory.stat`.
pub(crate) fn parse_status(text: &str) -> Option<(u64, u64)> {
    let anon = parse_meminfo_field(text, "RssAnon")?;
    let file = parse_meminfo_field(text, "RssFile").unwrap_or(0)
        + parse_meminfo_field(text, "RssShmem").unwrap_or(0);
    Some((anon.saturating_mul(1024), file.saturating_mul(1024)))
}

#[cfg(test)]
pub(crate) const STATUS: &str = "Name:\tmoruna\nVmHWM:\t 4000 kB\nVmRSS:\t 2000 kB\n\
RssAnon:\t 1600 kB\nRssFile:\t 300 kB\nRssShmem:\t 100 kB\nThreads:\t4\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::fixtures::{TempDir, write};

    /// e.3: `MemTotal` is read from a `/proc/meminfo` body, whatever the host.
    #[test]
    fn reads_meminfo_total() {
        let tmp = TempDir::new("meminfo");
        let proc_dir = tmp.path().join("proc");
        write(
            &proc_dir,
            "meminfo",
            "MemTotal:       16384000 kB\nMemFree:  100 kB\nHugePages_Total:  0\n",
        );
        assert_eq!(total_ram_bytes(&proc_dir), Some(16_384_000 * 1024));
        assert_eq!(
            parse_meminfo_field("MemTotal: 12 kB\n", "MemTotal"),
            Some(12)
        );
        assert_eq!(parse_meminfo_field("MemTotal: 12 kB\n", "MemFree"), None);
        assert_eq!(parse_meminfo_field("no colon here\n", "MemTotal"), None);
        assert_eq!(parse_meminfo_field("MemTotal: lots kB\n", "MemTotal"), None);
    }

    /// With no `/proc` (this development host) the platform interface answers instead.
    #[test]
    fn falls_back_to_the_platform_for_total_ram() {
        // The real host answers on every supported target: `/proc/meminfo` where there is one,
        // the platform's own interface where there is not.
        assert!(total_ram_bytes(Path::new("/proc")).is_some_and(|bytes| bytes > 0));

        // Pointed at a root with no `meminfo`, only a platform with its own interface answers.
        let tmp = TempDir::new("no-proc");
        let absent = tmp.path().join("absent");
        assert_eq!(total_ram_bytes(&absent), probes::platform_total_ram());
        if cfg!(target_vendor = "apple") {
            assert!(
                total_ram_bytes(&absent).is_some_and(|bytes| bytes > 0),
                "macOS reports hw.memsize"
            );
        }
    }

    /// f.2: `status` gives anonymous and file backed bytes; the platform answers where it does
    /// not.
    #[test]
    fn reads_process_memory() {
        let tmp = TempDir::new("status");
        let proc_dir = tmp.path().join("proc");
        write(&proc_dir, "self/status", STATUS);
        assert_eq!(
            process_memory(&proc_dir, 4096),
            Some((1600 * 1024, (300 + 100) * 1024))
        );
        assert_eq!(parse_status(STATUS), Some((1_638_400, 409_600)));
        assert_eq!(
            parse_status("RssAnon: 8 kB\n"),
            Some((8192, 0)),
            "no file backed lines is none of them"
        );
        assert_eq!(
            parse_status("VmRSS: 4194312 kB\nRssAnon: 8 kB\nRssFile: 4194304 kB\n"),
            Some((8192, 4 << 30)),
            "four gibibytes of mapped binary are file backed, never anonymous (DS-I4)"
        );
        assert_eq!(parse_status("VmRSS: 8 kB\n"), None);
        assert_eq!(parse_status("RssAnon: many kB\n"), None);
        assert_eq!(parse_status(""), None);

        // The real host answers on every supported target.
        assert!(process_memory(Path::new("/proc"), page_bytes()).is_some_and(|(anon, _)| anon > 0));

        // Pointed at a root with no `self/status`, only a platform with its own interface
        // answers. The figure itself moves between calls, so only its presence is asserted.
        let absent = tmp.path().join("absent");
        let fallback = process_memory(&absent, 4096);
        if cfg!(target_vendor = "apple") {
            assert!(
                fallback.is_some_and(|(anon, file)| anon > 0 && file > 0),
                "macOS reports phys_footprint and the file backed bytes through mach"
            );
        } else {
            assert_eq!(fallback, None, "this target reads /proc and nothing else");
        }
    }

    /// The two `sysconf` values the limits derivation needs.
    #[test]
    fn reports_page_size_and_cores() {
        assert!(page_bytes() >= 4096);
        assert!(logical_cores() >= 1.0);
    }
}
