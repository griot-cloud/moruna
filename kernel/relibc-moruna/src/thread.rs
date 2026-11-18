//! Threads: the thread pointer (ELF TLS, x86_64 variant II: FS points at the thread's control
//! block, whose first word is its own address), futexes, a new thread on a stack its creator
//! prepared, and a thread's end.

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

/// `sched_yield`: the core's scheduler has no yield on this ABI (item 5.0 adds
/// `thread_yield`), so it returns at once, as POSIX allows.
pub fn yield_now() -> Result<()> {
    Ok(())
}

/// Starts a thread on `stack`, which its creator prepared as relibc does for Linux's `clone`:
/// the entry, then six arguments, popped into `rax`, `rdi`, `rsi`, `rdx`, `rcx`, `r8` and `r9`
/// before `call rax`. A thread whose entry returns ends.
///
/// # Safety
///
/// `stack` holds those seven words, below a 16-byte aligned end, and is the new thread's alone.
#[cfg(target_os = "moruna")]
pub unsafe fn spawn(stack: *mut usize) -> Result<()> {
    // The kernel starts the thread "as if called" below `stack_top`, where nothing is kept;
    // the entry then moves to the prepared stack.
    let top = (stack as usize) & !15;
    os::spawn(core_os::thread_start, top, stack as usize).map_err(from_kernel)
}

/// Unmaps the calling thread's stack (`len` bytes from `stack`; none when `len` is zero) and
/// ends the thread.
///
/// # Safety
///
/// The range is the calling thread's own stack, or empty, and nothing else uses it.
#[cfg(target_os = "moruna")]
pub unsafe fn exit(stack: usize, len: usize) -> ! {
    // SAFETY: the caller's contract.
    unsafe { os::exit_thread(stack, len) }
}

#[cfg(target_os = "moruna")]
mod core_os {
    /// A new thread's first code: the stack its creator prepared, its seven words popped, then
    /// the entry called, and the thread's end if it returns.
    #[unsafe(naked)]
    pub extern "C" fn thread_start(_stack: u64) -> ! {
        core::arch::naked_asm!(
            "mov rsp, rdi",
            "pop rax",
            "pop rdi",
            "pop rsi",
            "pop rdx",
            "pop rcx",
            "pop r8",
            "pop r9",
            "call rax",
            "mov rax, {exit}",
            "syscall",
            "ud2",
            exit = const moruna_abi::nr::THREAD_EXIT,
        )
    }
}

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
