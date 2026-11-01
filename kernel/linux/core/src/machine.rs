//! What a program is told its machine has (SDD 30 f.3): the account its starter granted
//! it and the CPUs online, as Linux's `sysinfo`, `/proc/meminfo` and
//! `/sys/devices/system/cpu/online` say them. Inside a run's VM the machine is the budget
//! (MH 2.1), so a program that sizes itself from these, as the engine's discovery does,
//! sizes itself from the budget.
//!
//! The figures are the account's, which the kernel holds the program to (SDD 20 f.18):
//! the total is its limit, and what is free is what is left of it, its limit less what
//! is charged to it and what it holds for the accounts carved from it. A program started
//! without an account is told nothing of memory: `sysinfo` is refused and
//! `/proc/meminfo` is not found, rather than a figure nobody chose.

use alloc::{format, vec::Vec};

/// `struct sysinfo` on x86-64.
pub const SYSINFO_SIZE: usize = 112;

/// `/proc/meminfo`, the path a budget is read at.
pub const MEMINFO: &[u8] = b"/proc/meminfo";
/// The CPUs online.
pub const CPU_ONLINE: &[u8] = b"/sys/devices/system/cpu/online";

/// A program's memory: its account's limit, and what is left of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Memory {
    pub total: u64,
    pub free: u64,
}

/// `sysinfo`'s answer for `memory`: total and free RAM, no swap, one process,
/// `mem_unit` 1; `uptime` the seconds since boot.
pub fn sysinfo(memory: Memory, uptime_ns: u64) -> [u8; SYSINFO_SIZE] {
    let mut s = [0u8; SYSINFO_SIZE];
    let put = |s: &mut [u8; SYSINFO_SIZE], at: usize, v: u64| {
        s[at..at + 8].copy_from_slice(&v.to_le_bytes());
    };
    put(&mut s, 0, uptime_ns / 1_000_000_000);
    put(&mut s, 32, memory.total);
    put(&mut s, 40, memory.free);
    s[80..82].copy_from_slice(&1u16.to_le_bytes());
    s[104..108].copy_from_slice(&1u32.to_le_bytes());
    s
}

/// `/proc/meminfo` for `memory`.
pub fn meminfo(memory: Memory) -> Vec<u8> {
    let (total, free) = (memory.total / 1024, memory.free / 1024);
    format!(
        "MemTotal:       {total:>8} kB\nMemFree:        {free:>8} kB\nMemAvailable:   {free:>8} kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n"
    )
    .into_bytes()
}

/// `/sys/devices/system/cpu/online` for `cpus` CPUs: `0`, or `0-N`.
pub fn cpu_online(cpus: usize) -> Vec<u8> {
    match cpus {
        0 | 1 => b"0\n".to_vec(),
        n => format!("0-{}\n", n - 1).into_bytes(),
    }
}

/// The text a synthetic file holds, for a program with `memory` (none: it was granted
/// no account) on `cpus` CPUs; `None` for a path that is not one of them.
pub fn file(path: &[u8], memory: Option<Memory>, cpus: usize) -> Option<Vec<u8>> {
    match (path, memory) {
        (MEMINFO, Some(memory)) => Some(meminfo(memory)),
        (CPU_ONLINE, _) => Some(cpu_online(cpus)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(s: &[u8], at: usize) -> u64 {
        let mut w = [0u8; 8];
        w.copy_from_slice(&s[at..at + 8]);
        u64::from_le_bytes(w)
    }

    #[test]
    fn kd2_t2_sysinfo_and_meminfo_say_the_account() {
        let three = Memory {
            total: 3 << 30,
            free: 1 << 30,
        };
        let s = sysinfo(three, 7_500_000_000);
        assert_eq!(word(&s, 0), 7, "uptime in seconds");
        assert_eq!(word(&s, 32), 3 << 30, "totalram: the limit");
        assert_eq!(word(&s, 40), 1 << 30, "freeram: what is left of it");
        assert_eq!(word(&s, 64), 0, "no swap");
        assert_eq!(u16::from_le_bytes([s[80], s[81]]), 1, "procs");
        assert_eq!(
            u32::from_le_bytes([s[104], s[105], s[106], s[107]]),
            1,
            "mem_unit"
        );
        let half = Memory {
            total: 512 << 20,
            free: 256 << 20,
        };
        let text = meminfo(half);
        let text = core::str::from_utf8(&text).unwrap();
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("MemTotal:") && first.ends_with(" 524288 kB"),
            "{first}"
        );
        assert!(text.contains("MemFree:          262144 kB"));
        assert!(text.contains("MemAvailable:     262144 kB"));
        assert_eq!(cpu_online(1), b"0\n");
        assert_eq!(cpu_online(4), b"0-3\n");
        assert_eq!(file(MEMINFO, None, 2), None, "no account, no figure");
        assert!(file(MEMINFO, Some(half), 2).is_some());
        assert_eq!(file(CPU_ONLINE, None, 2), Some(b"0-1\n".to_vec()));
        assert_eq!(file(b"/proc/stat", Some(half), 2), None);
    }
}
