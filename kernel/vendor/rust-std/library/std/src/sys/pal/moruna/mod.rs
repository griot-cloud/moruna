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
///
/// The program's init arrays run after the C library's initialization and before `main`, and
/// its fini arrays after `main` (item 5.2b): through the C library's `exit` when one is linked
/// (it defines `moruna_libc_exit`, which runs the `atexit` functions and flushes its streams
/// too), and here otherwise.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "C" fn _start(start_page: u64) -> ! {
    moruna_sys::rt::start(start_page);
    moruna_sys::rt::stdio::open();
    // std's part of every thread's end, on the one thread path: std's threads and the C
    // library's run it alike (item 5.2b).
    moruna_sys::rt::thread::at_thread_end(moruna_sys::rt::thread::Runtime::Std, thread_end);
    unsafe extern "C" {
        #[linkage = "extern_weak"]
        static moruna_libc_init: Option<unsafe extern "C" fn()>;
        #[linkage = "extern_weak"]
        static moruna_libc_exit: Option<unsafe extern "C" fn(code: i32) -> !>;
        fn main(argc: isize, argv: *const *const u8) -> i32;
    }
    // SAFETY: a C library that defines the hook initializes itself there, once, before `main`.
    if let Some(init) = unsafe { moruna_libc_init } {
        unsafe { init() };
    }
    // SAFETY: the linker's arrays of the program's initializers, each run once, in order.
    unsafe { init_arrays() };
    let argv = arguments();
    // SAFETY: `main` is the program's (rustc's for a Rust program, the C program's own), which
    // takes an argument count and a null-terminated vector of them.
    let code = unsafe { main(1, argv) };
    // The first thread's thread-locals, as Hermit's std does.
    // SAFETY: `main` has returned: nothing refers to them.
    unsafe { thread_end() };
    // SAFETY: the C library's `exit`, which ends the process through std's runtime.
    if let Some(exit) = unsafe { moruna_libc_exit } {
        unsafe { exit(code) };
    }
    // SAFETY: the linker's array of the program's finalizers, each run once, last first.
    unsafe { fini_array() };
    moruna_sys::rt::exit(code)
}

#[cfg(not(test))]
unsafe extern "C" {
    static __preinit_array_start: extern "C" fn();
    static __preinit_array_end: extern "C" fn();
    static __init_array_start: extern "C" fn();
    static __init_array_end: extern "C" fn();
    static __fini_array_start: extern "C" fn();
    static __fini_array_end: extern "C" fn();
}

/// std's part of a thread's end (`moruna_sys::rt::thread::end_current`), whichever runtime
/// started the thread: its thread-locals' destructors, as Hermit's std runs them itself, then its
/// handle of the thread. The first thread's runs after `main`.
///
/// # Safety
///
/// It is the calling thread's end: nothing refers to its thread-locals.
#[cfg(not(test))]
unsafe extern "C" fn thread_end() {
    // SAFETY: the caller's contract.
    unsafe { crate::sys::thread_local::destructors::run() };
    crate::rt::thread_cleanup();
}

/// Runs the functions between `start` and `end`, in order.
///
/// # Safety
///
/// The range is one of the linker's arrays of functions (`kernel/targets/x86_64-moruna.ld`).
#[cfg(not(test))]
unsafe fn run_array(start: *const extern "C" fn(), end: *const extern "C" fn()) {
    let mut f = start;
    while f < end {
        // SAFETY: the caller's contract: each entry is a function to call once.
        unsafe { (*f)() };
        f = f.wrapping_add(1);
    }
}

/// The program's `.preinit_array` then `.init_array`.
///
/// # Safety
///
/// Called once, by `_start`.
#[cfg(not(test))]
unsafe fn init_arrays() {
    // SAFETY: the linker's symbols bound each array.
    unsafe {
        run_array(
            &raw const __preinit_array_start,
            &raw const __preinit_array_end,
        );
        run_array(&raw const __init_array_start, &raw const __init_array_end);
    }
}

/// The program's `.fini_array`, last entry first.
///
/// # Safety
///
/// Called once, by `_start`, when no C library's `exit` runs it.
#[cfg(not(test))]
unsafe fn fini_array() {
    let start = &raw const __fini_array_start;
    let mut f = &raw const __fini_array_end;
    while f > start {
        f = f.wrapping_sub(1);
        // SAFETY: an entry of the array the linker's symbols bound.
        unsafe { (*f)() };
    }
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
