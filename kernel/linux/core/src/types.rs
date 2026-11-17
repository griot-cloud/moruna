//! Linux's x86-64 structures and constants, as the personality reads and writes them in
//! a process's memory (SDD 28, e.1): byte layouts only, little endian.

use alloc::vec::Vec;

/// The errno values the personality answers with.
pub mod errno {
    pub const EPERM: u64 = 1;
    pub const ENOENT: u64 = 2;
    pub const ESRCH: u64 = 3;
    pub const EINTR: u64 = 4;
    pub const EIO: u64 = 5;
    pub const ENXIO: u64 = 6;
    pub const E2BIG: u64 = 7;
    pub const ENOEXEC: u64 = 8;
    pub const EBADF: u64 = 9;
    pub const ECHILD: u64 = 10;
    pub const EAGAIN: u64 = 11;
    pub const ENOMEM: u64 = 12;
    pub const EACCES: u64 = 13;
    pub const EFAULT: u64 = 14;
    pub const EEXIST: u64 = 17;
    pub const ENODEV: u64 = 19;
    pub const ENOTDIR: u64 = 20;
    pub const EISDIR: u64 = 21;
    pub const EINVAL: u64 = 22;
    pub const EMFILE: u64 = 24;
    pub const ENOTTY: u64 = 25;
    pub const EFBIG: u64 = 27;
    pub const ENOSPC: u64 = 28;
    pub const ESPIPE: u64 = 29;
    pub const EROFS: u64 = 30;
    pub const ERANGE: u64 = 34;
    pub const ENAMETOOLONG: u64 = 36;
    pub const ENOSYS: u64 = 38;
    pub const ENOTEMPTY: u64 = 39;
    pub const ELOOP: u64 = 40;
    pub const EOPNOTSUPP: u64 = 95;

    /// The value a call returns for `errno`: its negation.
    pub const fn fail(errno: u64) -> u64 {
        errno.wrapping_neg()
    }
}

/// File types and permission bits of `st_mode`.
pub mod mode {
    pub const S_IFMT: u32 = 0o170_000;
    pub const S_IFDIR: u32 = 0o040_000;
    pub const S_IFCHR: u32 = 0o020_000;
    pub const S_IFREG: u32 = 0o100_000;
    pub const S_IFLNK: u32 = 0o120_000;
}

/// `d_type` of a directory entry.
pub mod dtype {
    pub const DT_CHR: u8 = 2;
    pub const DT_DIR: u8 = 4;
    pub const DT_REG: u8 = 8;
    pub const DT_LNK: u8 = 10;
}

/// `openat`'s flags.
pub mod open {
    pub const O_ACCMODE: u64 = 0o3;
    pub const O_RDONLY: u64 = 0;
    pub const O_WRONLY: u64 = 0o1;
    pub const O_RDWR: u64 = 0o2;
    pub const O_CREAT: u64 = 0o100;
    pub const O_EXCL: u64 = 0o200;
    pub const O_NOCTTY: u64 = 0o400;
    pub const O_TRUNC: u64 = 0o1000;
    pub const O_APPEND: u64 = 0o2000;
    pub const O_NONBLOCK: u64 = 0o4000;
    pub const O_DSYNC: u64 = 0o10000;
    pub const O_ASYNC: u64 = 0o20000;
    pub const O_DIRECT: u64 = 0o40000;
    pub const O_LARGEFILE: u64 = 0o100000;
    pub const O_DIRECTORY: u64 = 0o200000;
    pub const O_NOFOLLOW: u64 = 0o400000;
    pub const O_NOATIME: u64 = 0o1000000;
    pub const O_CLOEXEC: u64 = 0o2000000;
    pub const O_SYNC: u64 = 0o4010000;
    pub const O_PATH: u64 = 0o10000000;
    pub const O_TMPFILE: u64 = 0o20200000;
    /// Every flag the personality takes; `O_TMPFILE` and anything else are refused.
    pub const KNOWN: u64 = O_ACCMODE
        | O_CREAT
        | O_EXCL
        | O_NOCTTY
        | O_TRUNC
        | O_APPEND
        | O_NONBLOCK
        | O_DSYNC
        | O_ASYNC
        | O_DIRECT
        | O_LARGEFILE
        | O_DIRECTORY
        | O_NOFOLLOW
        | O_NOATIME
        | O_CLOEXEC
        | O_SYNC
        | O_PATH;
}

/// `*at`'s directory and flags.
pub mod at {
    pub const AT_FDCWD: u64 = -100i64 as u64;
    pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    pub const AT_REMOVEDIR: u64 = 0x200;
    pub const AT_EMPTY_PATH: u64 = 0x1000;
}

/// What a `stat` structure says of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub ino: u64,
    /// Type and permission bits.
    pub mode: u32,
    pub nlink: u64,
    pub rdev: u64,
    pub size: u64,
}

/// The size of x86-64 `struct stat`.
pub const STAT_SIZE: usize = 144;

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}

impl Stat {
    /// `struct stat` for x86-64: device 0, owner root, times zero.
    pub fn encode(&self) -> [u8; STAT_SIZE] {
        let mut b = [0u8; STAT_SIZE];
        put(&mut b, 8, &self.ino.to_le_bytes());
        put(&mut b, 16, &self.nlink.to_le_bytes());
        put(&mut b, 24, &self.mode.to_le_bytes());
        put(&mut b, 40, &self.rdev.to_le_bytes());
        put(&mut b, 48, &self.size.to_le_bytes());
        put(&mut b, 56, &4096u64.to_le_bytes());
        put(&mut b, 64, &self.size.div_ceil(512).to_le_bytes());
        b
    }
}

/// One `linux_dirent64` record, padded to eight bytes: `d_ino`, `d_off` (the position
/// after this entry), `d_reclen`, `d_type`, the name and its NUL.
pub fn dirent64(ino: u64, next: u64, kind: u8, name: &[u8]) -> Vec<u8> {
    let len = (19 + name.len() + 1).div_ceil(8) * 8;
    let mut b = Vec::with_capacity(len);
    b.extend_from_slice(&ino.to_le_bytes());
    b.extend_from_slice(&next.to_le_bytes());
    b.extend_from_slice(&(len as u16).to_le_bytes());
    b.push(kind);
    b.extend_from_slice(name);
    b.resize(len, 0);
    b
}

/// `struct utsname`: six fields of 65 bytes.
pub fn utsname(fields: [&[u8]; 6]) -> [u8; 390] {
    let mut b = [0u8; 390];
    for (i, f) in fields.iter().enumerate() {
        let n = f.len().min(64);
        b[i * 65..i * 65 + n].copy_from_slice(&f[..n]);
    }
    b
}

/// One `struct iovec`: base and length, from 16 bytes.
pub fn iovec(b: &[u8]) -> Option<(u64, u64)> {
    let base = u64::from_le_bytes(b.get(0..8)?.try_into().ok()?);
    let len = u64::from_le_bytes(b.get(8..16)?.try_into().ok()?);
    Some((base, len))
}

/// `struct timespec` of nanoseconds.
pub fn timespec(ns: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    put(&mut b, 0, &(ns / 1_000_000_000).to_le_bytes());
    put(&mut b, 8, &(ns % 1_000_000_000).to_le_bytes());
    b
}

/// `struct rlimit`: the soft and the hard limit.
pub fn rlimit(soft: u64, hard: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    put(&mut b, 0, &soft.to_le_bytes());
    put(&mut b, 8, &hard.to_le_bytes());
    b
}

/// An unlimited resource.
pub const RLIM_INFINITY: u64 = u64::MAX;

/// The limits `getrlimit` gives: the stack 8 MiB, 1024 open files, the rest unlimited.
pub fn limit_of(resource: u64) -> Option<(u64, u64)> {
    match resource {
        // RLIMIT_STACK.
        3 => Some((8 << 20, RLIM_INFINITY)),
        // RLIMIT_NOFILE.
        7 => Some((1024, 1024)),
        0..=15 => Some((RLIM_INFINITY, RLIM_INFINITY)),
        _ => None,
    }
}

/// `struct kernel_sigaction` with an eight-byte mask: handler, flags, restorer, mask.
pub const SIGACTION_SIZE: usize = 32;
/// `SIG_DFL` and `SIG_IGN`.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;
/// Signals whose default action is to do nothing.
pub fn ignored_by_default(sig: u64) -> bool {
    // SIGCHLD, SIGCONT, SIGURG, SIGWINCH.
    matches!(sig, 17 | 18 | 23 | 28)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_t2_stat_layout() {
        let s = Stat {
            ino: 7,
            mode: mode::S_IFREG | 0o644,
            nlink: 1,
            rdev: 0,
            size: 5000,
        };
        let b = s.encode();
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 0o100644);
        assert_eq!(u64::from_le_bytes(b[48..56].try_into().unwrap()), 5000);
        assert_eq!(u64::from_le_bytes(b[64..72].try_into().unwrap()), 10);
        assert_eq!(u64::from_le_bytes(b[56..64].try_into().unwrap()), 4096);
    }

    #[test]
    fn lx_t2_dirent_padded() {
        let d = dirent64(5, 2, dtype::DT_DIR, b"usr");
        assert_eq!(d.len(), 24);
        assert_eq!(u16::from_le_bytes([d[16], d[17]]), 24);
        assert_eq!(d[18], dtype::DT_DIR);
        assert_eq!(&d[19..22], b"usr");
        assert_eq!(d[22], 0);
        assert_eq!(dirent64(1, 1, 8, b"abcde").len(), 32);
        assert_eq!(dirent64(1, 1, 8, b"abcd").len(), 24);
    }

    #[test]
    fn lx_t2_small_structures() {
        let u = utsname([b"Linux", b"moruna", b"6.12.0", b"#1", b"x86_64", b"(none)"]);
        assert_eq!(&u[0..5], b"Linux");
        assert_eq!(&u[4 * 65..4 * 65 + 6], b"x86_64");
        let long = [b'a'; 100];
        assert_eq!(utsname([&long, b"", b"", b"", b"", b""])[64], 0);
        let mut io = [0u8; 16];
        io[0] = 1;
        io[8] = 9;
        assert_eq!(iovec(&io), Some((1, 9)));
        assert_eq!(iovec(&io[..10]), None);
        let t = timespec(3_000_000_007);
        assert_eq!(u64::from_le_bytes(t[0..8].try_into().unwrap()), 3);
        assert_eq!(u64::from_le_bytes(t[8..16].try_into().unwrap()), 7);
        assert_eq!(limit_of(3), Some((8 << 20, RLIM_INFINITY)));
        assert_eq!(limit_of(7), Some((1024, 1024)));
        assert_eq!(limit_of(9), Some((RLIM_INFINITY, RLIM_INFINITY)));
        assert_eq!(limit_of(99), None);
        assert_eq!(
            u64::from_le_bytes(rlimit(1, 2)[8..16].try_into().unwrap()),
            2
        );
        assert!(ignored_by_default(17));
        assert!(!ignored_by_default(6));
        assert_eq!(errno::fail(errno::ENOSYS), (-38i64) as u64);
        assert_eq!(open::KNOWN & open::O_TMPFILE, open::O_DIRECTORY);
    }
}
