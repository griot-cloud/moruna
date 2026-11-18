//! moruna: Moruna's platform layer (kernel/vendor/rust-std/README.md), modelled on Motor OS's: each
//! `std::sys` module is a thin file over Moruna's runtime, `moruna_sys::rt` (kernel/sys/src/rt).
//! This module holds the program's entry and what every module shares.

#![forbid(unsafe_op_in_unsafe_fn)]

pub use moruna_sys::rt::futex;

use crate::io;

/// The program's entry: the kernel starts its first thread here, with the start page's
/// address and a stack as if called. std's runtime reads its arguments from the start page,
/// not from `main`'s.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "C" fn _start(start_page: u64) -> ! {
    moruna_sys::rt::start(start_page);
    unsafe extern "C" {
        fn main(argc: isize, argv: *const *const u8) -> i32;
    }
    // SAFETY: `main` is the one rustc generates for the program, which takes these.
    let code = unsafe { main(0, crate::ptr::null()) };
    moruna_sys::rt::exit(code)
}

// SAFETY: must be called only once during runtime initialization.
// NOTE: `rt::start` did the work before `main`.
pub unsafe fn init(_argc: isize, _argv: *const *const u8, _sigpipe: u8) {}

// SAFETY: must be called only once during runtime cleanup.
// NOTE: this is not guaranteed to run, for example when the program aborts.
pub unsafe fn cleanup() {
    moruna_sys::rt::stdio::finish();
}

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}

pub fn unsupported_err() -> io::Error {
    io::Error::UNSUPPORTED_PLATFORM
}

/// Ends the process after its log has what stdout and stderr hold: where a panic ends.
pub fn abort_internal() -> ! {
    moruna_sys::rt::abort()
}

/// An error the kernel answered, as an `io::Error` (see `sys::io::error`).
pub(crate) fn error(error: moruna_sys::Error) -> io::Error {
    io::Error::from_raw_os_error(error.code() as i32)
}
