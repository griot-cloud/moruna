//! Threads: the thread pointer (ELF TLS, x86_64 variant II: FS points at the thread's control
//! block, whose first word is its own address), futexes, and threads on the runtime's one thread
//! path, std's and the C library's (system design 5B).

use core::sync::atomic::AtomicU32;

use crate::os;
use crate::time::Timespec;
use crate::{Result, from_kernel};

/// Points the calling thread's FS base at `addr`: its control block.
pub fn set_thread_pointer(addr: usize) -> Result<()> {
    os::set_fs(addr).map_err(from_kernel)
}

/// The calling thread's FS base, zero before it has one.
pub fn thread_pointer() -> usize {
    os::fs_base()
}

/// Sleeps while `word` holds `expected`, until a wake or `deadline` (absolute, on the
/// monotonic clock, as Linux's `FUTEX_WAIT_BITSET` takes one): `EAGAIN` when it holds another
/// value, `ETIMEDOUT` at the deadline, `EINTR` when the kernel interrupted the wait.
pub fn futex_wait(word: &AtomicU32, expected: u32, deadline: Option<Timespec>) -> Result<()> {
    let until = match deadline {
        Some(t) => t.to_ns()?,
        None => u64::MAX,
    };
    os::futex_wait(word, expected, until).map_err(from_kernel)
}

/// Wakes at most `count` threads sleeping on `word`: how many.
pub fn futex_wake(word: &AtomicU32, count: u32) -> Result<u32> {
    os::futex_wake(word, count).map_err(from_kernel)
}

/// `sched_yield`: `thread_yield` (ABI 2.8), which gives the CPU to another ready thread.
pub fn yield_now() -> Result<()> {
    os::yield_now();
    Ok(())
}

/// Starts a thread on the one thread path (system design 5B): its stack (`stack` bytes at
/// least, with a guard page) and its block of TLS and control block are the runtime's, installed
/// before `main(arg)` runs. relibc's own part of the control block is zeroed; the thread makes it
/// relibc's `Tcb` first. The thread's runtime handle, which [`join`], [`detach`] or
/// [`detach_releasing`] takes once.
#[cfg(target_os = "moruna")]
pub fn spawn(stack: usize, main: extern "C" fn(usize), arg: usize) -> Result<usize> {
    os::spawn(stack, main, arg).map_err(from_kernel)
}

/// Waits for the kernel to say the thread `spawn` started has ended: only then may its kept
/// block be freed ([`release`]), never on a word the ending thread itself writes while it runs.
///
/// # Safety
///
/// `thread` is a handle [`spawn`] gave, taken once.
#[cfg(target_os = "moruna")]
pub unsafe fn join(thread: usize) {
    // SAFETY: the caller's contract.
    unsafe { os::join(thread) }
}

/// Detaches a thread that frees its own block at its end; its stack goes once it has ended.
///
/// # Safety
///
/// `thread` is a handle [`spawn`] gave, taken once.
#[cfg(target_os = "moruna")]
pub unsafe fn detach(thread: usize) {
    // SAFETY: the caller's contract.
    unsafe { os::detach(thread) }
}

/// Detaches a thread that ended with its block `control` kept: the runtime frees both its stack
/// and the block once the kernel says it has ended.
///
/// # Safety
///
/// `thread` is a handle [`spawn`] gave, taken once, and `control` that thread's block, which
/// nothing else frees.
#[cfg(target_os = "moruna")]
pub unsafe fn detach_releasing(thread: usize, control: usize) {
    // SAFETY: the caller's contract.
    unsafe { os::detach_releasing(thread, control) }
}

/// Sets the C library's part of every thread's end, std's threads' and its own: the runtime
/// runs it, then std's.
#[cfg(target_os = "moruna")]
pub fn at_thread_end(end: unsafe extern "C" fn()) {
    os::at_thread_end(end)
}

/// The calling thread's end, as every thread on the one thread path has it: the C library's
/// part (set by [`at_thread_end`]), then std's thread-locals' destructors.
///
/// # Safety
///
/// Nothing reads the values of the thread's thread-locals after.
#[cfg(target_os = "moruna")]
pub unsafe fn end_current() {
    // SAFETY: the caller's contract.
    unsafe { os::end_current() }
}

/// Ends the calling thread, its end already run ([`end_current`]), its block kept for whoever
/// frees it.
#[cfg(target_os = "moruna")]
pub fn exit() -> ! {
    os::thread_exit()
}

/// Frees a thread's block once nothing uses it: the joiner's, or a detached thread's own at its
/// end.
///
/// # Safety
///
/// `control` is a block the thread path made, for a thread that has ended or is the caller,
/// which reads none of its thread-locals after.
#[cfg(target_os = "moruna")]
pub unsafe fn release(control: usize) {
    // SAFETY: the caller's contract.
    unsafe { os::release(control) }
}

/// Where the bytes a thread's control block keeps for the C library end, from the block's
/// start: relibc's `Tcb` fits below it.
#[cfg(target_os = "moruna")]
pub const LIBC_END: usize = os::LIBC_END;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errno::{EAGAIN, EINVAL, ETIMEDOUT};
    use crate::{Errno, os};
    use core::sync::atomic::Ordering;
    use std::sync::Arc;

    #[test]
    fn the_thread_pointer_is_the_calling_threads() {
        assert_eq!(thread_pointer(), 0);
        set_thread_pointer(0x7000_0000).unwrap();
        assert_eq!(thread_pointer(), 0x7000_0000);
        let other = std::thread::spawn(thread_pointer).join().unwrap();
        assert_eq!(other, 0);
        assert_eq!(yield_now(), Ok(()));
    }

    #[test]
    fn a_futex_waits_for_its_word_or_its_deadline() {
        let word = AtomicU32::new(1);
        assert_eq!(futex_wait(&word, 0, None), Err(Errno(EAGAIN)));
        let soon = Timespec {
            sec: 0,
            nsec: os::monotonic_ns() as i64 % 1_000_000_000,
        };
        // A deadline already past.
        assert_eq!(futex_wait(&word, 1, Some(soon)), Err(Errno(ETIMEDOUT)));
        let bad = Timespec { sec: -1, nsec: 0 };
        assert_eq!(futex_wait(&word, 1, Some(bad)), Err(Errno(EINVAL)));
        let word = Arc::new(AtomicU32::new(0));
        let waker = {
            let word = word.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(5));
                word.store(1, Ordering::Release);
                futex_wake(&word, 1)
            })
        };
        assert_eq!(futex_wait(&word, 0, None), Ok(()));
        assert_eq!(waker.join().unwrap(), Ok(0));
    }
}
