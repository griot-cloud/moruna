//! `relibc-moruna`: what relibc's system calls are on Moruna's core (system design 5B, item
//! 5.2b). relibc's `Sys` for `target_os = "moruna"` (`kernel/vendor/relibc/src/platform/moruna`)
//! is a thin layer over this crate: relibc's types in, these calls, relibc's types out.
//!
//! Everything here speaks Linux's ABI: its errno values, `mmap` flags, clock ids and file
//! modes, which relibc's headers give a C program on Moruna (the `target_os` sites take Linux's
//! constants). What Moruna has is mapped onto the kernel's calls through `moruna-sys`:
//!
//! - anonymous `mmap`, `munmap` and `mprotect`, over memory objects ([`memory`]);
//! - the FS base (ELF TLS, x86_64 variant II), futexes, threads and their end ([`thread`]);
//! - the monotonic clock from the clock page, and sleep ([`time`]);
//! - `RANDOM_READ` ([`random`]);
//! - descriptors 0 to 2, the task's standard streams: 0 is empty, 1 and 2 are the log
//!   ([`fd`]);
//! - exit, the process's name, `uname` ([`process`]).
//!
//! What a function has no use for (files by path, processes, signals, sockets) is not here:
//! relibc's `Sys` answers `ENOSYS` or `ENOENT` for it, as 5A0 says.
//!
//! Every call the kernel answers goes through `os`, the one module a host test replaces with
//! stand-ins over the host's own memory, clock and threads.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod errno;
pub mod fd;
pub mod memory;
mod os;
pub mod process;
pub mod random;
pub mod thread;
pub mod time;

/// An error as relibc carries it: an errno value, Linux's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i32);

/// What every call here returns.
pub type Result<T> = core::result::Result<T, Errno>;

/// The errno a kernel error answers as, by the kernel's code (`moruna_abi::Error`).
pub fn from_kernel(code: u32) -> Errno {
    use errno::*;
    Errno(match code {
        1 => EBADF,
        2 | 4 | 10 | 13 => EINVAL,
        3 => EACCES,
        5 | 12 | 15 => ENOMEM,
        6 => EAGAIN,
        7 => EPIPE,
        8 => ETIMEDOUT,
        9 => ERANGE,
        11 => ENOSYS,
        14 => EFAULT,
        16 => EINTR,
        _ => EIO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kernel_error_has_an_errno() {
        let all: std::vec::Vec<i32> = (1..=16).map(|c| from_kernel(c).0).collect();
        assert_eq!(
            all,
            [
                errno::EBADF,
                errno::EINVAL,
                errno::EACCES,
                errno::EINVAL,
                errno::ENOMEM,
                errno::EAGAIN,
                errno::EPIPE,
                errno::ETIMEDOUT,
                errno::ERANGE,
                errno::EINVAL,
                errno::ENOSYS,
                errno::ENOMEM,
                errno::EINVAL,
                errno::EFAULT,
                errno::ENOMEM,
                errno::EINTR,
            ]
        );
        assert_eq!(from_kernel(0), Errno(errno::EIO));
        assert_eq!(from_kernel(99), Errno(errno::EIO));
    }
}
