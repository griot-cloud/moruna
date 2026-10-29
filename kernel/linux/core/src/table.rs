//! The system call table (SDD 28, d.2): every Linux x86-64 call the personality
//! answers, by number, with who answers it. The kernel answers the fast calls itself;
//! the server answers the rest. A number not in the table is refused with `ENOSYS`
//! and counted against the process; so is a call the server takes only in part (a
//! `clone` that is not a thread, a `futex` operation the kernel does not have), under
//! its own number.

use moruna_abi::{forward::fast, forward::table, limits, write_u32};

/// Who answers a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The kernel, as the fast call named (SDD 20, f.14).
    Fast(u8),
    /// The personality server.
    Server,
}

/// The Linux x86-64 numbers of the calls in the table, and of the refused calls whose
/// names the report gives.
pub mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const STAT: u64 = 4;
    pub const FSTAT: u64 = 5;
    pub const LSTAT: u64 = 6;
    pub const POLL: u64 = 7;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const RT_SIGRETURN: u64 = 15;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const PWRITE64: u64 = 18;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const ACCESS: u64 = 21;
    pub const PIPE: u64 = 22;
    pub const SCHED_YIELD: u64 = 24;
    pub const MREMAP: u64 = 25;
    pub const MADVISE: u64 = 28;
    pub const DUP: u64 = 32;
    pub const DUP2: u64 = 33;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const SOCKET: u64 = 41;
    pub const CONNECT: u64 = 42;
    pub const CLONE: u64 = 56;
    pub const FORK: u64 = 57;
    pub const VFORK: u64 = 58;
    pub const EXECVE: u64 = 59;
    pub const EXIT: u64 = 60;
    pub const WAIT4: u64 = 61;
    pub const KILL: u64 = 62;
    pub const UNAME: u64 = 63;
    pub const FCNTL: u64 = 72;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    pub const FTRUNCATE: u64 = 77;
    pub const GETDENTS: u64 = 78;
    pub const GETCWD: u64 = 79;
    pub const CHDIR: u64 = 80;
    pub const RENAME: u64 = 82;
    pub const MKDIR: u64 = 83;
    pub const RMDIR: u64 = 84;
    pub const UNLINK: u64 = 87;
    pub const READLINK: u64 = 89;
    pub const UMASK: u64 = 95;
    pub const GETTIMEOFDAY: u64 = 96;
    pub const GETRLIMIT: u64 = 97;
    pub const SYSINFO: u64 = 99;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const GETPPID: u64 = 110;
    pub const GETPGRP: u64 = 111;
    pub const GETGROUPS: u64 = 115;
    pub const GETRESUID: u64 = 118;
    pub const GETRESGID: u64 = 120;
    pub const SIGALTSTACK: u64 = 131;
    pub const PRCTL: u64 = 157;
    pub const ARCH_PRCTL: u64 = 158;
    pub const SETRLIMIT: u64 = 160;
    pub const GETTID: u64 = 186;
    pub const TKILL: u64 = 200;
    pub const TIME: u64 = 201;
    pub const FUTEX: u64 = 202;
    pub const SCHED_SETAFFINITY: u64 = 203;
    pub const SCHED_GETAFFINITY: u64 = 204;
    pub const GETDENTS64: u64 = 217;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const FADVISE64: u64 = 221;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const CLOCK_GETRES: u64 = 229;
    pub const CLOCK_NANOSLEEP: u64 = 230;
    pub const EXIT_GROUP: u64 = 231;
    pub const TGKILL: u64 = 234;
    pub const WAITID: u64 = 247;
    pub const OPENAT: u64 = 257;
    pub const MKDIRAT: u64 = 258;
    pub const NEWFSTATAT: u64 = 262;
    pub const UNLINKAT: u64 = 263;
    pub const READLINKAT: u64 = 267;
    pub const FACCESSAT: u64 = 269;
    pub const SET_ROBUST_LIST: u64 = 273;
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    pub const PRLIMIT64: u64 = 302;
    pub const GETCPU: u64 = 309;
    pub const GETRANDOM: u64 = 318;
    pub const STATX: u64 = 332;
    pub const RSEQ: u64 = 334;
    pub const CLONE3: u64 = 435;
    pub const CLOSE_RANGE: u64 = 436;
    pub const FACCESSAT2: u64 = 439;
}

/// The table: number, name, who answers. In number order.
pub const TABLE: &[(u64, &str, Status)] = &[
    (nr::READ, "read", Status::Server),
    (nr::WRITE, "write", Status::Server),
    (nr::OPEN, "open", Status::Server),
    (nr::CLOSE, "close", Status::Server),
    (nr::STAT, "stat", Status::Server),
    (nr::FSTAT, "fstat", Status::Server),
    (nr::LSTAT, "lstat", Status::Server),
    (nr::LSEEK, "lseek", Status::Server),
    (nr::MMAP, "mmap", Status::Fast(fast::MMAP)),
    (nr::MPROTECT, "mprotect", Status::Fast(fast::MPROTECT)),
    (nr::MUNMAP, "munmap", Status::Fast(fast::MUNMAP)),
    (nr::BRK, "brk", Status::Fast(fast::BRK)),
    (nr::RT_SIGACTION, "rt_sigaction", Status::Server),
    (nr::RT_SIGPROCMASK, "rt_sigprocmask", Status::Server),
    (nr::IOCTL, "ioctl", Status::Server),
    (nr::PREAD64, "pread64", Status::Server),
    (nr::PWRITE64, "pwrite64", Status::Server),
    (nr::READV, "readv", Status::Server),
    (nr::WRITEV, "writev", Status::Server),
    (nr::ACCESS, "access", Status::Server),
    (
        nr::SCHED_YIELD,
        "sched_yield",
        Status::Fast(fast::SCHED_YIELD),
    ),
    (nr::MADVISE, "madvise", Status::Fast(fast::MADVISE)),
    (nr::DUP, "dup", Status::Server),
    (nr::DUP2, "dup2", Status::Server),
    (nr::NANOSLEEP, "nanosleep", Status::Fast(fast::NANOSLEEP)),
    (nr::GETPID, "getpid", Status::Server),
    (nr::CLONE, "clone", Status::Server),
    (nr::EXECVE, "execve", Status::Server),
    (nr::EXIT, "exit", Status::Fast(fast::EXIT)),
    (nr::WAIT4, "wait4", Status::Server),
    (nr::KILL, "kill", Status::Server),
    (nr::UNAME, "uname", Status::Server),
    (nr::FCNTL, "fcntl", Status::Server),
    (nr::FSYNC, "fsync", Status::Server),
    (nr::FDATASYNC, "fdatasync", Status::Server),
    (nr::FTRUNCATE, "ftruncate", Status::Server),
    (nr::GETCWD, "getcwd", Status::Server),
    (nr::CHDIR, "chdir", Status::Server),
    (nr::MKDIR, "mkdir", Status::Server),
    (nr::RMDIR, "rmdir", Status::Server),
    (nr::UNLINK, "unlink", Status::Server),
    (nr::READLINK, "readlink", Status::Server),
    (nr::UMASK, "umask", Status::Server),
    (
        nr::GETTIMEOFDAY,
        "gettimeofday",
        Status::Fast(fast::GETTIMEOFDAY),
    ),
    (nr::GETRLIMIT, "getrlimit", Status::Server),
    (nr::GETUID, "getuid", Status::Server),
    (nr::GETGID, "getgid", Status::Server),
    (nr::GETEUID, "geteuid", Status::Server),
    (nr::GETEGID, "getegid", Status::Server),
    (nr::GETPPID, "getppid", Status::Server),
    (nr::GETPGRP, "getpgrp", Status::Server),
    (nr::GETGROUPS, "getgroups", Status::Server),
    (nr::GETRESUID, "getresuid", Status::Server),
    (nr::GETRESGID, "getresgid", Status::Server),
    (nr::SIGALTSTACK, "sigaltstack", Status::Server),
    (nr::PRCTL, "prctl", Status::Server),
    (nr::ARCH_PRCTL, "arch_prctl", Status::Fast(fast::ARCH_PRCTL)),
    (nr::SETRLIMIT, "setrlimit", Status::Server),
    (nr::GETTID, "gettid", Status::Fast(fast::GETTID)),
    (nr::TKILL, "tkill", Status::Server),
    (nr::TIME, "time", Status::Fast(fast::TIME)),
    (nr::FUTEX, "futex", Status::Fast(fast::FUTEX)),
    (nr::SCHED_SETAFFINITY, "sched_setaffinity", Status::Server),
    (nr::SCHED_GETAFFINITY, "sched_getaffinity", Status::Server),
    (nr::GETDENTS64, "getdents64", Status::Server),
    (
        nr::SET_TID_ADDRESS,
        "set_tid_address",
        Status::Fast(fast::SET_TID_ADDRESS),
    ),
    (nr::FADVISE64, "fadvise64", Status::Server),
    (
        nr::CLOCK_GETTIME,
        "clock_gettime",
        Status::Fast(fast::CLOCK_GETTIME),
    ),
    (
        nr::CLOCK_GETRES,
        "clock_getres",
        Status::Fast(fast::CLOCK_GETRES),
    ),
    (
        nr::CLOCK_NANOSLEEP,
        "clock_nanosleep",
        Status::Fast(fast::CLOCK_NANOSLEEP),
    ),
    (nr::EXIT_GROUP, "exit_group", Status::Server),
    (nr::TGKILL, "tgkill", Status::Server),
    (nr::WAITID, "waitid", Status::Server),
    (nr::OPENAT, "openat", Status::Server),
    (nr::MKDIRAT, "mkdirat", Status::Server),
    (nr::NEWFSTATAT, "newfstatat", Status::Server),
    (nr::UNLINKAT, "unlinkat", Status::Server),
    (nr::READLINKAT, "readlinkat", Status::Server),
    (nr::FACCESSAT, "faccessat", Status::Server),
    (nr::DUP3, "dup3", Status::Server),
    (nr::PRLIMIT64, "prlimit64", Status::Server),
    (nr::GETRANDOM, "getrandom", Status::Fast(fast::GETRANDOM)),
    (nr::CLOSE_RANGE, "close_range", Status::Server),
    (nr::FACCESSAT2, "faccessat2", Status::Server),
];

/// Names of refused calls the report names, besides the table's.
const REFUSED_NAMES: &[(u64, &str)] = &[
    (nr::POLL, "poll"),
    (nr::RT_SIGRETURN, "rt_sigreturn"),
    (nr::PIPE, "pipe"),
    (nr::MREMAP, "mremap"),
    (nr::SOCKET, "socket"),
    (nr::CONNECT, "connect"),
    (nr::FORK, "fork"),
    (nr::VFORK, "vfork"),
    (nr::GETDENTS, "getdents"),
    (nr::RENAME, "rename"),
    (nr::SYSINFO, "sysinfo"),
    (nr::SET_ROBUST_LIST, "set_robust_list"),
    (nr::PIPE2, "pipe2"),
    (nr::GETCPU, "getcpu"),
    (nr::STATX, "statx"),
    (nr::RSEQ, "rseq"),
    (nr::CLONE3, "clone3"),
];

/// Who answers call `number`; `None` when it is refused.
pub fn status(number: u64) -> Option<Status> {
    TABLE
        .binary_search_by_key(&number, |(n, _, _)| *n)
        .ok()
        .map(|i| TABLE[i].2)
}

/// The call's name, when the table or the refused names know it.
pub fn name(number: u64) -> Option<&'static str> {
    TABLE
        .iter()
        .map(|(n, name, _)| (*n, *name))
        .chain(REFUSED_NAMES.iter().copied())
        .find(|(n, _)| *n == number)
        .map(|(_, name)| name)
}

/// The table `process_forward` takes (SDD 20, d.2): which numbers reach which fast
/// call.
pub fn forward_table() -> [u8; table::SIZE] {
    let mut bytes = [0u8; table::SIZE];
    write_u32(&mut bytes, table::MAGIC, table::MAGIC_VALUE);
    for (number, _, status) in TABLE {
        if let Status::Fast(call) = status
            && (*number as usize) < limits::FORWARD_FAST
        {
            bytes[table::CALLS + *number as usize] = *call;
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_abi::read_u32;

    #[test]
    fn lx_t1_table_in_order_and_named() {
        assert!(TABLE.windows(2).all(|w| w[0].0 < w[1].0));
        for (n, name, _) in TABLE {
            assert_eq!(super::name(*n), Some(*name));
            assert!(status(*n).is_some());
        }
        assert_eq!(status(nr::SOCKET), None);
        assert_eq!(super::name(nr::SOCKET), Some("socket"));
        assert_eq!(super::name(9999), None);
        assert!(TABLE.len() >= 60, "{} calls", TABLE.len());
        for (n, _) in REFUSED_NAMES {
            assert_eq!(status(*n), None, "{n} is both refused and in the table");
        }
    }

    #[test]
    fn lx_t1_forward_table_maps_fast_calls() {
        let t = forward_table();
        assert_eq!(read_u32(&t, table::MAGIC), Some(table::MAGIC_VALUE));
        assert_eq!(read_u32(&t, table::RESERVED), Some(0));
        assert_eq!(t[table::CALLS + nr::FUTEX as usize], fast::FUTEX);
        assert_eq!(t[table::CALLS + nr::MMAP as usize], fast::MMAP);
        assert_eq!(t[table::CALLS + nr::READ as usize], fast::NONE);
        let fast_count = TABLE
            .iter()
            .filter(|(_, _, s)| matches!(s, Status::Fast(_)))
            .count();
        let mapped = t[table::CALLS..]
            .iter()
            .filter(|c| **c != fast::NONE)
            .count();
        assert_eq!(fast_count, mapped);
        // Every fast call the kernel has is reached by some number.
        for call in 1..fast::COUNT {
            assert!(
                t[table::CALLS..].contains(&call),
                "fast call {call} unreached"
            );
        }
    }
}
