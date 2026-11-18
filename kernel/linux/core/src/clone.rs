//! `clone` and `clone3` of a thread (SDD 28; item 5.3b): what a thread library asks for,
//! read into the one request the server's `forward_clone` takes. A new process (`fork`,
//! `vfork`, a `clone` without `CLONE_VM`) is not served, nor what only `clone3` can ask
//! (an exit signal, chosen thread ids, a cgroup, its flags above 32 bits): each is
//! refused with `ENOSYS` and counted, as a call taken only in part is.

use crate::types::errno;

pub const CLONE_VM: u64 = 0x100;
pub const CLONE_SIGHAND: u64 = 0x800;
pub const CLONE_VFORK: u64 = 0x4000;
pub const CLONE_THREAD: u64 = 0x10000;
pub const CLONE_SETTLS: u64 = 0x80000;
pub const CLONE_PARENT_SETTID: u64 = 0x10_0000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
pub const CLONE_CHILD_SETTID: u64 = 0x100_0000;

/// `struct clone_args` (`clone3`): byte offsets of its words.
pub mod args {
    pub const FLAGS: usize = 0;
    pub const PIDFD: usize = 8;
    pub const CHILD_TID: usize = 16;
    pub const PARENT_TID: usize = 24;
    pub const EXIT_SIGNAL: usize = 32;
    pub const STACK: usize = 40;
    pub const STACK_SIZE: usize = 48;
    pub const TLS: usize = 56;
    pub const SET_TID: usize = 64;
    pub const SET_TID_SIZE: usize = 72;
    pub const CGROUP: usize = 80;
    /// `CLONE_ARGS_SIZE_VER0`: the least `clone3` takes.
    pub const SIZE_MIN: u64 = 64;
    /// `CLONE_ARGS_SIZE_VER2`: every word this personality knows.
    pub const SIZE_KNOWN: usize = 88;
    /// The most `clone3` reads: a page, as Linux's.
    pub const SIZE_MAX: u64 = 4096;
}

/// A thread to start, as `forward_clone` takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thread {
    /// Its first stack pointer.
    pub stack: u64,
    /// Its FS base; zero for the parent's.
    pub tls: u64,
    /// The word cleared and woken when it ends; zero for none.
    pub clear_tid: u64,
    /// The parent's word its id is written to before it runs; zero for none.
    pub parent_tid: u64,
    /// The word its id is written to once it is made (`CLONE_CHILD_SETTID`); zero for
    /// none.
    pub child_tid: u64,
}

/// A request not served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not a thread as `clone` makes one: `ENOSYS`, counted under the call's number.
    NotThread,
    /// Linux's error for the arguments.
    Errno(u64),
}

/// Whether `flags` ask for a thread: the process's memory, signal handlers and thread
/// group, and no `vfork`.
fn thread_flags(flags: u64) -> bool {
    let thread = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    flags & thread == thread && flags & CLONE_VFORK == 0
}

/// The thread `flags` ask for on `stack`, its words as the flags name them.
fn thread(flags: u64, stack: u64, parent_tid: u64, child_tid: u64, tls: u64) -> Thread {
    let named = |flag: u64, word: u64| if flags & flag != 0 { word } else { 0 };
    Thread {
        stack,
        tls: named(CLONE_SETTLS, tls),
        clear_tid: named(CLONE_CHILD_CLEARTID, child_tid),
        parent_tid: named(CLONE_PARENT_SETTID, parent_tid),
        child_tid: named(CLONE_CHILD_SETTID, child_tid),
    }
}

/// `clone(flags, stack, parent_tid, child_tid, tls)`: a thread, or `EINVAL` without a
/// stack.
pub fn clone(a: [u64; 6]) -> Result<Thread, Refusal> {
    let [flags, stack, parent_tid, child_tid, tls, _] = a;
    if !thread_flags(flags) {
        return Err(Refusal::NotThread);
    }
    if stack == 0 {
        return Err(Refusal::Errno(errno::EINVAL));
    }
    Ok(thread(flags, stack, parent_tid, child_tid, tls))
}

/// How many bytes of `struct clone_args` `clone3(args, size)` gives: `EINVAL` below
/// `SIZE_MIN`, `E2BIG` above a page, as Linux answers.
pub fn args_len(size: u64) -> Result<usize, u64> {
    if size < args::SIZE_MIN {
        return Err(errno::EINVAL);
    }
    if size > args::SIZE_MAX {
        return Err(errno::E2BIG);
    }
    Ok(size as usize)
}

fn word(bytes: &[u8], at: usize) -> u64 {
    bytes
        .get(at..at + 8)
        .and_then(|w| w.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

/// `clone3` of `bytes`, the `struct clone_args` given (at least `SIZE_MIN` of them, from
/// [`args_len`]), mapped onto `clone`'s thread: the same flags make a thread, and the
/// stack is `stack .. stack + stack_size`, its pointer the top. Bytes past the words
/// known must be zero (`E2BIG`); a stack must have a size and a size a stack (`EINVAL`).
pub fn clone3(bytes: &[u8]) -> Result<Thread, Refusal> {
    if bytes.iter().skip(args::SIZE_KNOWN).any(|b| *b != 0) {
        return Err(Refusal::Errno(errno::E2BIG));
    }
    let flags = word(bytes, args::FLAGS);
    let only_clone3 = flags >> 32 != 0
        || word(bytes, args::EXIT_SIGNAL) != 0
        || word(bytes, args::SET_TID) != 0
        || word(bytes, args::SET_TID_SIZE) != 0
        || word(bytes, args::CGROUP) != 0;
    if only_clone3 || !thread_flags(flags) {
        return Err(Refusal::NotThread);
    }
    let (stack, size) = (word(bytes, args::STACK), word(bytes, args::STACK_SIZE));
    if stack == 0 || size == 0 {
        return Err(Refusal::Errno(errno::EINVAL));
    }
    let top = stack
        .checked_add(size)
        .ok_or(Refusal::Errno(errno::EINVAL))?;
    Ok(thread(
        flags,
        top,
        word(bytes, args::PARENT_TID),
        word(bytes, args::CHILD_TID),
        word(bytes, args::TLS),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// What glibc's `pthread_create` asks of `clone` and of `clone3`.
    const GLIBC: u64 = CLONE_VM
        | 0x200 // CLONE_FS
        | 0x400 // CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_THREAD
        | 0x40000 // CLONE_SYSVSEM
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID;

    fn args(words: &[(usize, u64)], size: usize) -> alloc::vec::Vec<u8> {
        let mut bytes = vec![0u8; size];
        for (at, w) in words {
            bytes[*at..*at + 8].copy_from_slice(&w.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn lx_t19_clone_of_a_thread() {
        let t = clone([GLIBC, 0x7000_0000, 0x6000, 0x6008, 0x5000, 0]).unwrap();
        assert_eq!(
            t,
            Thread {
                stack: 0x7000_0000,
                tls: 0x5000,
                clear_tid: 0x6008,
                parent_tid: 0x6000,
                child_tid: 0,
            }
        );
        // Words the flags do not name are not used.
        let t = clone([
            CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_CHILD_SETTID,
            0x7000_0000,
            0x6000,
            0x6008,
            0x5000,
            0,
        ])
        .unwrap();
        assert_eq!((t.tls, t.clear_tid, t.parent_tid), (0, 0, 0));
        assert_eq!(t.child_tid, 0x6008);
        // A process, a vfork, or a thread without a stack.
        assert_eq!(clone([0x11, 0, 0, 0, 0, 0]), Err(Refusal::NotThread));
        assert_eq!(
            clone([GLIBC | CLONE_VFORK, 0x7000, 0, 0, 0, 0]),
            Err(Refusal::NotThread)
        );
        assert_eq!(
            clone([GLIBC & !CLONE_VM, 0x7000, 0, 0, 0, 0]),
            Err(Refusal::NotThread)
        );
        assert_eq!(
            clone([GLIBC, 0, 0, 0, 0, 0]),
            Err(Refusal::Errno(errno::EINVAL))
        );
    }

    #[test]
    fn lx_t19_clone3_of_a_thread() {
        // glibc 2.41's `clone3` for `pthread_create`: its stack's base and size.
        let glibc = [
            (args::FLAGS, GLIBC),
            (args::CHILD_TID, 0x6008),
            (args::PARENT_TID, 0x6008),
            (args::STACK, 0x7000_0000),
            (args::STACK_SIZE, 0x80_0000),
            (args::TLS, 0x5000),
        ];
        let want = Thread {
            stack: 0x7080_0000,
            tls: 0x5000,
            clear_tid: 0x6008,
            parent_tid: 0x6008,
            child_tid: 0,
        };
        assert_eq!(clone3(&args(&glibc, 88)), Ok(want));
        // The first version's 64 bytes, and a larger struct of zeros past the known.
        assert_eq!(clone3(&args(&glibc, 64)), Ok(want));
        assert_eq!(clone3(&args(&glibc, 128)), Ok(want));
        let mut longer = args(&glibc, 128);
        longer[100] = 1;
        assert_eq!(clone3(&longer), Err(Refusal::Errno(errno::E2BIG)));
        // CLONE_CHILD_SETTID's word is `child_tid`.
        let settid = [
            (
                args::FLAGS,
                CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_CHILD_SETTID,
            ),
            (args::CHILD_TID, 0x6010),
            (args::STACK, 0x7000_0000),
            (args::STACK_SIZE, 0x1000),
        ];
        let t = clone3(&args(&settid, 88)).unwrap();
        assert_eq!((t.child_tid, t.clear_tid, t.tls), (0x6010, 0, 0));
        assert_eq!(t.stack, 0x7000_1000);
    }

    #[test]
    fn lx_t19_clone3_refused() {
        let base = [
            (args::FLAGS, GLIBC),
            (args::STACK, 0x7000_0000),
            (args::STACK_SIZE, 0x1000),
        ];
        // What only clone3 asks: an exit signal, chosen ids, a cgroup, flags above 32
        // bits (CLONE_CLEAR_SIGHAND, CLONE_INTO_CGROUP); and a process or a vfork.
        for (at, w) in [
            (args::EXIT_SIGNAL, 17),
            (args::SET_TID, 0x6000),
            (args::SET_TID_SIZE, 1),
            (args::CGROUP, 3),
            (args::FLAGS, GLIBC | 1 << 32),
            (args::FLAGS, GLIBC | 1 << 33),
            (args::FLAGS, 0x11),
            (args::FLAGS, GLIBC | CLONE_VFORK),
        ] {
            let mut words = base.to_vec();
            words.push((at, w));
            assert_eq!(
                clone3(&args(&words, 88)),
                Err(Refusal::NotThread),
                "{at} {w:#x}"
            );
        }
        // A stack without a size, a size without a stack, a stack past the top.
        for (stack, size) in [(0x7000_0000, 0), (0, 0x1000), (u64::MAX, 2)] {
            let words = [
                (args::FLAGS, GLIBC),
                (args::STACK, stack),
                (args::STACK_SIZE, size),
            ];
            assert_eq!(
                clone3(&args(&words, 88)),
                Err(Refusal::Errno(errno::EINVAL))
            );
        }
        assert_eq!(args_len(63), Err(errno::EINVAL));
        assert_eq!(args_len(0), Err(errno::EINVAL));
        assert_eq!(args_len(4097), Err(errno::E2BIG));
        assert_eq!(args_len(64), Ok(64));
        assert_eq!(args_len(4096), Ok(4096));
        // Too few bytes to hold a word reads it as zero.
        assert_eq!(word(&[1, 2, 3], 0), 0);
    }
}
