//! moruna: Moruna's platform layer (kernel/vendor/rust-std/README.md), modelled on Motor OS's: each
//! `std::sys` module is a thin file over Moruna's runtime, `moruna_sys::rt` (kernel/sys/src/rt).
//! This module holds the program's entry and what every module shares.

#![forbid(unsafe_op_in_unsafe_fn)]

pub use moruna_sys::rt::futex;

use crate::io;

/// The program's entry, the one for std and the C library alike (system design 5B): the kernel
/// starts the first thread here, with the start page's address and a stack as if called. In
/// order: the first thread's thread-locals and control block; std's log, which stdout and stderr
/// (and the C library's descriptors 1 and 2) write to; the C library's own initialization when
/// one is linked (it defines `moruna_libc_init`); then `main`, with the process's name as its one
/// argument. std's runtime reads its arguments from the start page itself.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "C" fn _start(start_page: u64) -> ! {
    moruna_sys::rt::start(start_page);
    moruna_sys::rt::stdio::open();
    unsafe extern "C" {
        #[linkage = "extern_weak"]
        static moruna_libc_init: Option<unsafe extern "C" fn()>;
        fn main(argc: isize, argv: *const *const u8) -> i32;
    }
    // SAFETY: a C library that defines the hook initializes itself there, once, before `main`.
    if let Some(init) = unsafe { moruna_libc_init } {
        unsafe { init() };
    }
    let argv = arguments();
    // SAFETY: `main` is the program's (rustc's for a Rust program, the C program's own), which
    // takes an argument count and a null-terminated vector of them.
    let code = unsafe { main(1, argv) };
    // The first thread's thread-locals, as Hermit's std does.
    unsafe {
        crate::sys::thread_local::destructors::run();
    }
    crate::rt::thread_cleanup();
    moruna_sys::rt::exit(code)
}

/// `main`'s arguments: the process's name, NUL-terminated, then null. Made once; never freed.
#[cfg(not(test))]
fn arguments() -> *const *const u8 {
    let mut name = moruna_sys::start::process_name().as_bytes().to_vec();
    name.push(0);
    let name: *const u8 = Box::leak(name.into_boxed_slice()).as_ptr();
    Box::leak(Box::new([name, crate::ptr::null()])).as_ptr()
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
