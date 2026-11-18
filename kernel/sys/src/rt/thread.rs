//! Threads: the one thread path (system design 5B), which std's `thread::spawn` and the C
//! library's `pthread_create` both take, and sleep and yield.
//!
//! A thread gets a stack of its own (a memory object, with a guard page below it) and its block
//! of ELF TLS with its control block ([`super::tls`]), which its FS base points at before its
//! runtime's code runs on it. Every thread ends the same way, whichever runtime started it
//! ([`end_current`]): the C library's work at a thread's end, then std's, each set by its
//! runtime ([`at_thread_end`]), so a thread runs both runtimes' thread-local destructors; then
//! the thread frees its block and ends, or [`exit`] does so from anywhere in it. A joined
//! thread's stack is unmapped by the join, once the kernel says the thread has ended; a thread
//! dropped without a join is kept on a list, and its stack (and its block, when it ended with its
//! block kept: [`Thread::detach_releasing`]) is freed by the next spawn after the kernel says it
//! has ended.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
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
    /// The thread's block when it ended with it kept, freed when dropped, after the thread has
    /// ended.
    kept: Kept,
}

/// A block that outlived its thread's code: the C library's joinable thread keeps its block,
/// which its `pthread_t` lies in, until its join or its detach. Null for none.
struct Kept(*mut tls::Control);

// SAFETY: the block is no thread's once its thread has ended, and only `Kept`'s drop frees it.
unsafe impl Send for Kept {}

impl Drop for Kept {
    fn drop(&mut self) {
        // SAFETY: a `Running` is dropped once its thread has ended (`reap`), and the block is
        // that thread's, which nothing reads after its end; null frees nothing.
        unsafe { tls::release(self.0) };
    }
}

/// The runtimes whose code runs on a thread (system design 5B), each with its own work at a
/// thread's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
    /// The C library: its cleanup handlers and its thread-local and key destructors.
    Libc = 0,
    /// std: its thread-locals' destructors and its handle of the thread.
    Std = 1,
}

/// Each runtime's work at a thread's end, in the order [`end_current`] runs them; null for none.
static ENDS: [AtomicPtr<()>; 2] = [
    AtomicPtr::new(ptr::null_mut()),
    AtomicPtr::new(ptr::null_mut()),
];

/// Sets `runtime`'s work at the end of every thread but the first: its start's to set, once,
/// before the program's threads run.
pub fn at_thread_end(runtime: Runtime, end: unsafe extern "C" fn()) {
    ENDS[runtime as usize].store(end as *mut (), Ordering::Release);
}

/// The end of the calling thread, whichever runtime started it: the C library's work first,
/// since POSIX runs a thread's cleanup handlers before its thread-local destructors, then std's.
/// A thread [`spawn`] started runs it once its code has returned; the C library's
/// `pthread_exit` runs it before it ends its thread.
///
/// # Safety
///
/// It is the calling thread's end: nothing reads the values of its thread-locals after.
pub unsafe fn end_current() {
    for end in &ENDS {
        let end = end.load(Ordering::Acquire);
        if !end.is_null() {
            // SAFETY: only `at_thread_end` stores here, a function of this type.
            let end: unsafe extern "C" fn() = unsafe { core::mem::transmute(end) };
            // SAFETY: the caller's contract: the thread's end.
            unsafe { end() };
        }
    }
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
                kept: Kept(ptr::null_mut()),
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

/// A started thread's life: its block installed, its runtime's code run, its end
/// ([`end_current`]), its block freed.
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
    // SAFETY: the thread's code has returned: its end.
    unsafe { end_current() };
    tls::release_current();
}

/// Ends the calling thread from anywhere in its code: its end run ([`end_current`]) and its
/// block freed. Its stack goes with its join, or after it as for any detached thread.
///
/// # Safety
///
/// As [`end_current`]'s: nothing on the thread's stack refers to its thread-locals' values.
#[cfg(not(test))]
pub unsafe fn exit() -> ! {
    // SAFETY: the caller's contract.
    unsafe { end_current() };
    tls::release_current();
    os::thread_exit()
}

impl Thread {
    /// Waits for the kernel to say the thread has ended, then unmaps its stack.
    pub fn join(mut self) {
        if let Some(running) = self.running.take() {
            os::join(&running.handle);
        }
    }

    /// Detaches a thread that ends, or has ended, with its block kept (the C library's joinable
    /// thread, detached after its end): its stack and `control` are freed once the kernel says
    /// it has ended, never while its last instructions may still run.
    ///
    /// # Safety
    ///
    /// `control` is this thread's block, kept past its code's end, which nothing else frees and
    /// nothing reads after the thread has ended.
    pub unsafe fn detach_releasing(mut self, control: *mut tls::Control) {
        if let Some(mut running) = self.running.take() {
            running.kept = Kept(control);
            DETACHED.lock().push(running);
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

    static KEPT: AtomicPtr<tls::Control> = AtomicPtr::new(ptr::null_mut());
    static HOLD: AtomicU32 = AtomicU32::new(0);

    extern "C" fn keep_block_until_let_go(_: usize) {
        // As the C library's joinable thread does: its block outlives its code, since the
        // runtime finds no block of its to free.
        KEPT.store(tls::current(), Ordering::SeqCst);
        os::install(ptr::null_mut());
        while HOLD.load(Ordering::Acquire) == 0 {
            futex::futex_wait(&HOLD, 0, None);
        }
    }

    #[test]
    fn a_block_kept_past_its_thread_s_end_is_freed_with_its_stack_once_it_has_ended() {
        let _only = DETACHING.lock().unwrap();
        let before = DETACHED.lock().len();
        let thread = spawn(0, keep_block_until_let_go, 0).unwrap();
        while KEPT.load(Ordering::SeqCst).is_null() {
            std::thread::yield_now();
        }
        // SAFETY: the thread's block, which it no longer has, and which only this frees.
        unsafe { thread.detach_releasing(KEPT.load(Ordering::SeqCst)) };
        reap();
        assert_eq!(DETACHED.lock().len(), before + 1, "still running: kept");
        HOLD.store(1, Ordering::Release);
        futex::futex_wake_all(&HOLD);
        while DETACHED.lock().len() > before {
            reap();
            std::thread::yield_now();
        }
    }

    std::thread_local! {
        static ENDING: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
    /// The runtimes' ends the threads of [`every_thread_ends_with_the_c_library_s_end_then_std_s`]
    /// ran, in order.
    static ENDS_RUN: Mutex<Vec<Runtime>> = Mutex::new(Vec::new());

    unsafe extern "C" fn libc_end() {
        if ENDING.with(core::cell::Cell::get) {
            ENDS_RUN.lock().unwrap().push(Runtime::Libc);
        }
    }

    unsafe extern "C" fn std_end() {
        if ENDING.with(core::cell::Cell::get) {
            ENDS_RUN.lock().unwrap().push(Runtime::Std);
        }
    }

    extern "C" fn mark_ending(_: usize) {
        ENDING.with(|e| e.set(true));
    }

    #[test]
    fn every_thread_ends_with_the_c_library_s_end_then_std_s() {
        // Every test's threads run these from here on: they record only the threads marked.
        at_thread_end(Runtime::Libc, libc_end);
        at_thread_end(Runtime::Std, std_end);
        spawn(0, mark_ending, 0).unwrap().join();
        assert_eq!(*ENDS_RUN.lock().unwrap(), [Runtime::Libc, Runtime::Std]);
        spawn(0, mark_ending, 0).unwrap().join();
        assert_eq!(ENDS_RUN.lock().unwrap().len(), 4, "each thread, once");
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
