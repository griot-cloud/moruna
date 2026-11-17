//! `rt`: the runtime Rust's `std` calls for `target_os = "moruna"` (item 5.0, system design 5A0),
//! as Motor OS's `std` calls `moto-rt`. The platform layer in `kernel/vendor/rust-std` is a thin
//! file per `std::sys` module; what it needs from the kernel, and the logic between, is here, where
//! it is tested on a host.
//!
//! - [`start`] and [`exit`]: the program's entry hands the start page here before `main`, and its
//!   exit code after; [`abort`] ends it after a panic.
//! - [`futex`]: the futex calls with a timeout, which std's `Mutex`, `Condvar`, `RwLock`, `Once` and
//!   thread parking are built on.
//! - [`time`]: `Instant` from the clock page. `SystemTime` has no source: a native task is given no
//!   realtime base, so `SystemTime::now` is unsupported.
//! - [`tls`]: thread-local keys, in a table each thread's FS base points at.
//! - [`thread`]: spawn, join, sleep and yield.
//! - `stdio` and `memory` (feature `rt`): stdout and stderr as lines of the task's log
//!   ([`crate::plane::Log`]), and std's allocator over the process's heap.
//!
//! Every call the kernel answers goes through `os`, the one module a host test replaces.

pub mod futex;
mod lock;
#[cfg(feature = "rt")]
pub mod memory;
mod os;
#[cfg(feature = "rt")]
pub mod stdio;
pub mod thread;
pub mod time;
pub mod tls;

/// The code a process ends with when std aborts it: after a panic, as a panic in a program
/// without std does (the crate's panic handler).
pub const ABORT_CODE: i32 = 101;

/// The program's start, before `main`: records the start page the kernel passed the first
/// thread (`rdi`), and gives that thread its table of thread-local keys.
pub fn start(start_page: u64) {
    crate::start::init(start_page);
    tls::install_main();
}

/// Ends the process with `code`, once stdout's and stderr's last line is in the log.
#[cfg(feature = "rt")]
pub fn exit(code: i32) -> ! {
    stdio::finish();
    crate::exit(i64::from(code))
}

/// Ends the process with [`ABORT_CODE`]: std's `abort`, which a panic ends in. The log gets what
/// stdout and stderr hold, unless a thread is writing to it now.
#[cfg(feature = "rt")]
pub fn abort() -> ! {
    stdio::finish_unless_busy();
    crate::exit(i64::from(ABORT_CODE))
}

#[cfg(test)]
mod tests {
    extern crate std;

    #[test]
    fn the_start_gives_the_first_thread_its_table() {
        std::thread::spawn(|| {
            super::start(0);
            assert!(!super::os::table().is_null());
            super::tls::uninstall();
        })
        .join()
        .unwrap();
    }
}
