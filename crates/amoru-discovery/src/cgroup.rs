//! Cgroup v2 and the v1 fallback (e.2).
//!
//! The six v2 files are parsed directly, with no `cgroups-rs` or `procfs` dependency (section l),
//! and every entry point takes the cgroup root and the `/proc` root as parameters so the tests
//! point them at a fixture directory. That is also why this module runs, and is tested, on a host
//! with no cgroups at all.

use std::path::{Path, PathBuf};

/// The v1 "unlimited" sentinel in `memory.limit_in_bytes`.
const V1_UNLIMITED: u64 = 9_223_372_036_854_771_712;

/// Which cgroup hierarchy this process is in, and where its files are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cgroup {
    /// cgroup v2: one directory holding `memory.*` and `cpu.*`.
    V2(PathBuf),
    /// cgroup v1: separate `memory` and `cpu` controller directories.
    V1 { memory: PathBuf, cpu: PathBuf },
    /// No cgroup was found, so the OS values stand.
    None,
}

/// The memory and CPU numbers one cgroup declares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CgroupLimits {
    /// `memory.max`, the kill line; `None` when it is `max` or unreadable.
    pub(crate) memory_max: Option<u64>,
    /// `memory.high`, the ceiling the platform asks for; `None` when unset.
    pub(crate) memory_high: Option<u64>,
}

/// One reading of the live memory counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MemoryStat {
    /// Anonymous bytes plus unevictable bytes (DS-I4).
    pub(crate) anon: u64,
    /// Page cache bytes charged to the cgroup.
    pub(crate) file: u64,
}

/// Locate the cgroup this process is in (e.2).
///
/// `proc_root/self/cgroup`'s `0::<path>` line names the v2 path under `cgroup_root`. When that
/// directory has no `memory.max` the root itself is tried, which is what a container sees when
/// its cgroup namespace is its own root. v1 is used only when v2 is absent.
pub(crate) fn locate(cgroup_root: &Path, proc_root: &Path) -> Cgroup {
    if let Some(relative) = v2_relative_path(proc_root) {
        let joined = cgroup_root.join(relative.trim_start_matches('/'));
        if joined.join("memory.max").is_file() {
            return Cgroup::V2(joined);
        }
    }
    if cgroup_root.join("memory.max").is_file() {
        return Cgroup::V2(cgroup_root.to_path_buf());
    }
    let memory = cgroup_root.join("memory");
    let cpu = cgroup_root.join("cpu");
    if memory.join("memory.limit_in_bytes").is_file() {
        return Cgroup::V1 { memory, cpu };
    }
    Cgroup::None
}

/// The `0::<path>` line of `/proc/self/cgroup`, which only cgroup v2 writes.
fn v2_relative_path(proc_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join("self/cgroup")).ok()?;
    parse_proc_self_cgroup(&text)
}

/// Pull the v2 path out of a `/proc/self/cgroup` body.
pub(crate) fn parse_proc_self_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::").map(|path| path.trim().to_string()))
}

/// Read the limits of a located cgroup.
pub(crate) fn limits(cgroup: &Cgroup) -> CgroupLimits {
    match cgroup {
        Cgroup::V2(dir) => CgroupLimits {
            memory_max: read_opt_bytes(&dir.join("memory.max")),
            memory_high: read_opt_bytes(&dir.join("memory.high")),
        },
        Cgroup::V1 { memory, .. } => {
            let max = read_u64(&memory.join("memory.limit_in_bytes"))
                .filter(|value| *value != V1_UNLIMITED && *value != u64::MAX);
            CgroupLimits {
                memory_max: max,
                memory_high: None,
            }
        }
        Cgroup::None => CgroupLimits::default(),
    }
}

/// The CPU quota in cores, or `None` when the cgroup sets none.
pub(crate) fn cpu_quota(cgroup: &Cgroup) -> Option<f64> {
    match cgroup {
        Cgroup::V2(dir) => {
            let text = std::fs::read_to_string(dir.join("cpu.max")).ok()?;
            parse_cpu_max(&text)
        }
        Cgroup::V1 { cpu, .. } => {
            let quota = read_i64(&cpu.join("cpu.cfs_quota_us"))?;
            let period = read_i64(&cpu.join("cpu.cfs_period_us"))?;
            if quota <= 0 || period <= 0 {
                return None;
            }
            Some(quota as f64 / period as f64)
        }
        Cgroup::None => None,
    }
}

/// `cpu.max` is `"<quota> <period>"` or `"max <period>"`.
pub(crate) fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut parts = text.split_whitespace();
    let quota = parts.next()?;
    let period: f64 = parts.next().unwrap_or("100000").parse().ok()?;
    if quota == "max" || period <= 0.0 {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    if quota <= 0.0 {
        return None;
    }
    Some(quota / period)
}

/// Parse a v2 `memory.stat` body: `anon + unevictable` and `file` (DS-I4).
pub(crate) fn parse_memory_stat_v2(text: &str) -> MemoryStat {
    let mut stat = MemoryStat::default();
    for line in text.lines() {
        let mut parts = line.split_ascii_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "anon" | "unevictable" => stat.anon = stat.anon.saturating_add(value),
            "file" => stat.file = value,
            _ => {}
        }
    }
    stat
}

/// Parse a v1 `memory.stat` body: `rss` is the anonymous set, `cache` the page cache.
pub(crate) fn parse_memory_stat_v1(text: &str) -> MemoryStat {
    let mut stat = MemoryStat::default();
    for line in text.lines() {
        let mut parts = line.split_ascii_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "rss" => stat.anon = value,
            "cache" => stat.file = value,
            _ => {}
        }
    }
    stat
}

/// Parse a v2 `cpu.stat` body for `throttled_usec`.
pub(crate) fn parse_throttled_usec_v2(text: &str) -> u64 {
    field(text, "throttled_usec").unwrap_or(0)
}

/// Parse a v1 `cpu.stat` body: `throttled_time` is nanoseconds.
pub(crate) fn parse_throttled_usec_v1(text: &str) -> u64 {
    field(text, "throttled_time").unwrap_or(0) / 1_000
}

/// The first `"<key> <value>"` line's value.
fn field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_ascii_whitespace();
        if parts.next()? == key {
            parts.next()?.parse::<u64>().ok()
        } else {
            None
        }
    })
}

/// `memory.current`, for the notes only (e.2).
pub(crate) fn memory_current(cgroup: &Cgroup) -> Option<u64> {
    match cgroup {
        Cgroup::V2(dir) => read_u64(&dir.join("memory.current")),
        Cgroup::V1 { memory, .. } => read_u64(&memory.join("memory.usage_in_bytes")),
        Cgroup::None => None,
    }
}

/// A file holding either a number of bytes or the word `max`.
fn read_opt_bytes(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text == "max" {
        return None;
    }
    text.parse::<u64>().ok()
}

/// A file holding one unsigned number.
pub(crate) fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// A file holding one signed number (the v1 CPU files use -1 for "unset").
fn read_i64(path: &Path) -> Option<i64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Fixture cgroup directories: the whole cgroup path is unit tested against these, because the
    //! development host is macOS and has no `/sys/fs/cgroup` at all.

    use std::path::{Path, PathBuf};

    /// A temporary directory that deletes itself.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        /// A fresh directory under the system temporary directory.
        pub(crate) fn new(tag: &str) -> TempDir {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "amoru-discovery-{tag}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("fixture directory");
            TempDir(path)
        }

        /// The directory itself.
        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write one file under `dir`, creating parents.
    pub(crate) fn write(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture parent");
        }
        std::fs::write(path, body).expect("fixture file");
    }

    /// A `/proc` fixture whose `self/cgroup` names the v2 root.
    pub(crate) fn proc_root(dir: &Path, cgroup_line: &str) -> PathBuf {
        let root = dir.join("proc");
        write(&root, "self/cgroup", cgroup_line);
        root
    }

    /// A cgroup v2 fixture at `<dir>/cgroup` with the six files of e.2.
    pub(crate) fn cgroup_v2(
        dir: &Path,
        memory_max: &str,
        memory_high: &str,
        cpu_max: &str,
    ) -> PathBuf {
        let root = dir.join("cgroup");
        write(&root, "memory.max", memory_max);
        write(&root, "memory.high", memory_high);
        write(
            &root,
            "memory.stat",
            "anon 1048576\nfile 2097152\nunevictable 0\n",
        );
        write(&root, "memory.current", "3145728");
        write(&root, "cpu.max", cpu_max);
        write(&root, "cpu.stat", "usage_usec 10\nthrottled_usec 42\n");
        root
    }

    /// A cgroup v1 fixture at `<dir>/cgroup1`.
    pub(crate) fn cgroup_v1(dir: &Path, limit: &str, quota: &str, period: &str) -> PathBuf {
        let root = dir.join("cgroup1");
        write(&root, "memory/memory.limit_in_bytes", limit);
        write(&root, "memory/memory.stat", "rss 1048576\ncache 2097152\n");
        write(&root, "memory/memory.usage_in_bytes", "3145728");
        write(&root, "cpu/cpu.cfs_quota_us", quota);
        write(&root, "cpu/cpu.cfs_period_us", period);
        write(&root, "cpu/cpu.stat", "throttled_time 42000\n");
        root
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    /// e.2: the v2 file map is located from `/proc/self/cgroup` and read.
    #[test]
    fn locates_and_reads_cgroup_v2() {
        let tmp = TempDir::new("v2");
        let root = cgroup_v2(tmp.path(), "2147483648", "max", "400000 100000");
        let proc_dir = proc_root(tmp.path(), "0::/\n");
        let found = locate(&root, &proc_dir);
        assert_eq!(found, Cgroup::V2(root.clone()));
        let limits = limits(&found);
        assert_eq!(limits.memory_max, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(limits.memory_high, None);
        assert_eq!(cpu_quota(&found), Some(4.0));
        assert_eq!(memory_current(&found), Some(3_145_728));
    }

    /// A nested cgroup path in `/proc/self/cgroup` resolves under the root.
    #[test]
    fn locates_a_nested_cgroup_path() {
        let tmp = TempDir::new("nested");
        let root = tmp.path().join("cgroup");
        let nested = root.join("kubepods/pod7");
        std::fs::create_dir_all(&nested).expect("nested");
        write(&nested, "memory.max", "1073741824");
        let proc_dir = proc_root(tmp.path(), "0::/kubepods/pod7\n");
        assert_eq!(locate(&root, &proc_dir), Cgroup::V2(nested));
    }

    /// e.2: the v1 fallback is used only when v2 is absent, and the sentinel means "no limit".
    #[test]
    fn falls_back_to_cgroup_v1() {
        let tmp = TempDir::new("v1");
        let root = cgroup_v1(tmp.path(), "2147483648", "150000", "100000");
        let proc_dir = proc_root(tmp.path(), "3:memory:/\n");
        let found = locate(&root, &proc_dir);
        assert_eq!(
            found,
            Cgroup::V1 {
                memory: root.join("memory"),
                cpu: root.join("cpu"),
            }
        );
        assert_eq!(limits(&found).memory_max, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(limits(&found).memory_high, None);
        assert_eq!(cpu_quota(&found), Some(1.5));
        assert_eq!(memory_current(&found), Some(3_145_728));

        let unlimited = cgroup_v1(tmp.path(), "9223372036854771712", "-1", "100000");
        let found = locate(&unlimited, &proc_dir);
        assert_eq!(limits(&found).memory_max, None);
        assert_eq!(cpu_quota(&found), None);
    }

    /// No cgroup at all, which is this development host and every macOS host.
    #[test]
    fn reports_no_cgroup() {
        let tmp = TempDir::new("none");
        let found = locate(&tmp.path().join("absent"), &tmp.path().join("absent"));
        assert_eq!(found, Cgroup::None);
        assert_eq!(limits(&found), CgroupLimits::default());
        assert_eq!(cpu_quota(&found), None);
        assert_eq!(memory_current(&found), None);
    }

    /// The parsers of e.2, over bodies rather than files.
    #[test]
    fn parses_the_six_file_bodies() {
        assert_eq!(parse_proc_self_cgroup("0::/foo\n").as_deref(), Some("/foo"));
        assert_eq!(parse_proc_self_cgroup("2:cpu:/x\n"), None);

        assert_eq!(parse_cpu_max("400000 100000"), Some(4.0));
        assert_eq!(parse_cpu_max("50000 100000"), Some(0.5));
        assert_eq!(parse_cpu_max("max 100000"), None);
        assert_eq!(parse_cpu_max("400000"), Some(4.0));
        assert_eq!(parse_cpu_max("0 100000"), None);
        assert_eq!(parse_cpu_max("400000 0"), None);
        assert_eq!(parse_cpu_max(""), None);

        let v2 =
            parse_memory_stat_v2("anon 100\nfile 200\nunevictable 5\nslab 7\nbroken\nfile x\n");
        assert_eq!(
            v2,
            MemoryStat {
                anon: 105,
                file: 200
            }
        );
        let v1 = parse_memory_stat_v1("cache 200\nrss 100\ntotal_rss 999\n");
        assert_eq!(
            v1,
            MemoryStat {
                anon: 100,
                file: 200
            }
        );

        assert_eq!(
            parse_throttled_usec_v2("nr_periods 3\nthrottled_usec 42\n"),
            42
        );
        assert_eq!(parse_throttled_usec_v2("nr_periods 3\n"), 0);
        assert_eq!(parse_throttled_usec_v1("throttled_time 42000\n"), 42);
    }
}
