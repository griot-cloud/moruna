//! Threads: the one thread path (system design 5B), which std's `thread::spawn` and the C
//! library's `pthread_create` both take, and sleep and yield.
//!
//! A thread gets a stack of its own (a memory object, with a guard page below it) and its block
//! of ELF TLS with its control block ([`super::tls`]), which its FS base points at before its
//! runtime's code runs on it. That code (std's thread start, or the C library's) runs its own
//! thread-local destructors before it returns; then the thread frees its block and ends, or
//! [`exit`] does so from anywhere in it. A joined thread's stack is unmapped by the join; a
//! thread dropped without a join is kept on a list, and its stack is unmapped by the next spawn
//! after it has ended.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU32;
use core::time::Duration;

use moruna_abi::{Error, PAGE_SIZE};

use super::lock::Lock;
use super::{futex, os, tls};
use crate::Result;

/// The stack of a thread std is asked for no size for: 2 MiB, as Linux's std gives, since
/// arrow-rs and query engines recurse deeply (PM, 2026-11-18). A guard page lies below it.
pub const DEFAULT_MIN_STACK_SIZE: usize = 2 << 20;

/// What a started thread runs, boxed by [`spawn`] and taken apart by the thread.
struct Start {
    main: extern "C" fn(usize),
    arg: usize,
    block: tls::Block,
}

/// A started thread's handle and stack.
struct Running {
    handle: os::ThreadHandle,
    /// Unmapped when dropped, after the thread has ended.
    _stack: os::Stack,
}

/// Threads dropped without a join.
static DETACHED: Lock<Vec<Running>> = Lock::new(Vec::new());

/// A thread std started.
pub struct Thread {
    running: Option<Running>,
}

/// The bytes mapped for a stack of `size`: whole pages, at least one, and the guard page below
/// them; `None` past what one memory object holds.
pub(crate) fn stack_bytes(size: usize) -> Option<usize> {
    let page = PAGE_SIZE as usize;
    let total = size
        .max(page)
        .checked_next_multiple_of(page)?
        .checked_add(page)?;
    (total <= os::STACK_MAX).then_some(total)
}

/// Starts a thread that runs `main(arg)` on a stack of at least `stack` bytes, with its own
/// thread-locals and control block.
pub fn spawn(stack: usize, main: extern "C" fn(usize), arg: usize) -> Result<Thread> {
    reap();
    let stack = os::Stack::map(stack_bytes(stack).ok_or(Error::NoMemory)?)?;
    let block = tls::Block::new(&os::template()).ok_or(Error::NoMemory)?;
    let start = Box::into_raw(Box::new(Start { main, arg, block }));
    match os::spawn(&stack, start.expose_provenance() as u64) {
        Ok(handle) => Ok(Thread {
            running: Some(Running {
                handle,
                _stack: stack,
            }),
        }),
        Err(error) => {
            // SAFETY: no thread was started, so the start is still this function's.
            drop(unsafe { Box::from_raw(start) });
            Err(error)
        }
    }
}

/// Where a thread [`spawn`] started begins: its stack is the one `spawn` mapped, and `start`
/// the start it boxed.
#[cfg(not(test))]
pub(super) extern "C" fn trampoline(start: u64) -> ! {
    // SAFETY: as this function's contract says.
    unsafe { run(start) };
    os::thread_exit()
}

/// A started thread's life: its block installed, its runtime's code run, its block freed.
///
/// # Safety
///
/// `start` is the address of the `Start` that [`spawn`] boxed for this thread, given once.
pub(super) unsafe fn run(start: u64) {
    // SAFETY: the caller's contract.
    let start: Box<Start> =
        unsafe { Box::from_raw(core::ptr::with_exposed_provenance_mut(start as usize)) };
    let Start { main, arg, block } = *start;
    block.install();
    main(arg);
    tls::release_current();
}

/// Ends the calling thread from anywhere in its code, as `pthread_exit` does: its block is
/// freed, so its runtime has run its thread-local destructors first. Its stack goes with its
/// join, or after it as for any detached thread.
#[cfg(not(test))]
pub fn exit() -> ! {
    tls::release_current();
    os::thread_exit()
}

impl Thread {
    /// Waits for the thread to end, then unmaps its stack.
    pub fn join(mut self) {
        if let Some(running) = self.running.take() {
            os::join(&running.handle);
        }
    }
}

impl Drop for Thread {
    /// Detaches the thread: its stack is unmapped once it has ended.
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            DETACHED.lock().push(running);
        }
    }
}

/// Unmaps the stacks of detached threads that have ended.
fn reap() {
    DETACHED
        .lock()
        .retain(|running| !os::ended(&running.handle));
}

/// Sleeps for at least `duration`.
pub fn sleep(duration: Duration) {
    sleep_until(futex::deadline(os::clock(), Some(duration)));
}

/// Sleeps until the monotonic clock reaches `deadline` (nanoseconds).
pub fn sleep_until(deadline: u64) {
    // A word nobody wakes: each wait ends at the deadline, or early if the kernel ends it.
    let word = AtomicU32::new(0);
    while os::clock() < deadline {
        futex::wait_until(&word, 0, deadline);
    }
}

/// Gives the CPU to another ready thread, if there is one (`thread_yield`, ABI 2.8): what a
/// library's spin-then-yield loop needs to give way.
pub fn yield_now() {
    os::yield_now();
}

/// The CPUs a program's threads can run on, from the start page.
pub fn available_parallelism() -> usize {
    os::cpus()
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn a_stack_is_whole_pages_with_a_guard_page_below() {
        let page = PAGE_SIZE as usize;
        assert_eq!(
            stack_bytes(0),
            Some(2 * page),
            "at least one page, and the guard"
        );
        assert_eq!(stack_bytes(1), Some(2 * page));
        assert_eq!(stack_bytes(page), Some(2 * page));
        assert_eq!(stack_bytes(page + 1), Some(3 * page));
        assert_eq!(
            stack_bytes(DEFAULT_MIN_STACK_SIZE),
            Some(DEFAULT_MIN_STACK_SIZE + page)
        );
        assert_eq!(stack_bytes(os::STACK_MAX), None, "the guard does not fit");
        assert_eq!(stack_bytes(usize::MAX), None);
    }

    #[test]
    fn a_stack_too_large_is_refused_before_anything_starts() {
        extern "C" fn never(_: usize) {
            unreachable!()
        }
        assert_eq!(spawn(usize::MAX, never, 0).err(), Some(Error::NoMemory));
    }

    static RAN: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn record(arg: usize) {
        // The thread has its block before its runtime's code runs, the template's image in
        // its TLS just below its control block.
        let control = tls::current();
        assert!(!control.is_null());
        // SAFETY: the thread's own block, whose TLS ends at its control block.
        let tls = unsafe {
            let c = &*control;
            core::slice::from_raw_parts(control.cast::<u8>().sub(c.tls_len), c.tls_len)
        };
        assert_eq!(&tls[..8], b"host tls");
        RAN.fetch_add(arg, Ordering::SeqCst);
    }

    #[test]
    fn a_thread_runs_its_main_with_its_argument_and_is_joined() {
        let threads: Vec<Thread> = (1..=3)
            .map(|n| spawn(DEFAULT_MIN_STACK_SIZE, record, n * 100).unwrap())
            .collect();
        for t in threads {
            t.join();
        }
        assert!(RAN.load(Ordering::SeqCst) >= 600);
    }

    static GATE: AtomicU32 = AtomicU32::new(0);
    /// Only this test detaches threads.
    static DETACHING: Mutex<()> = Mutex::new(());

    extern "C" fn wait_for_gate(_: usize) {
        while GATE.load(Ordering::Acquire) == 0 {
            futex::futex_wait(&GATE, 0, None);
        }
    }

    #[test]
    fn a_detached_thread_s_stack_is_freed_by_a_later_spawn_once_it_has_ended() {
        let _only = DETACHING.lock().unwrap();
        let before = DETACHED.lock().len();
        drop(spawn(0, wait_for_gate, 0).unwrap());
        assert_eq!(DETACHED.lock().len(), before + 1, "kept while it runs");
        reap();
        assert_eq!(DETACHED.lock().len(), before + 1, "still running: kept");
        GATE.store(1, Ordering::Release);
        futex::futex_wake_all(&GATE);
        while DETACHED.lock().len() > before {
            reap();
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_sleep_lasts_at_least_its_duration() {
        let before = os::clock();
        sleep(Duration::from_millis(15));
        assert!(os::clock() - before >= 15_000_000);
        // A deadline already past returns at once.
        sleep_until(0);
        sleep(Duration::ZERO);
    }

    #[test]
    fn yield_and_the_cpu_count() {
        yield_now();
        assert!(available_parallelism() >= 1);
    }
}
