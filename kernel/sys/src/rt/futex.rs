//! The futex calls std's locks are built on (std's `sys::futex`): its `Mutex`, `Condvar`,
//! `RwLock`, `Once` and thread parking are std's own futex implementations, over these.

use core::sync::atomic::AtomicU32;
use core::time::Duration;

use moruna_abi::{DEADLINE_INFINITE, Error};

use super::os;

/// A word to wait on: the core's futex calls take 32 bits.
pub type Futex = AtomicU32;
/// The value a [`Futex`] holds.
pub type Primitive = u32;
/// std's smaller futex: the core's word is 32 bits, so the same.
pub type SmallFutex = AtomicU32;
/// The value a [`SmallFutex`] holds.
pub type SmallPrimitive = u32;

/// The core's deadline for a wait of `timeout` from `now`, on its monotonic clock: none for no
/// timeout, and none past the clock's end (`u64::MAX` is no deadline).
pub(crate) fn deadline(now: u64, timeout: Option<Duration>) -> u64 {
    match timeout {
        None => DEADLINE_INFINITE,
        Some(timeout) => now.saturating_add(u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX)),
    }
}

/// What a wait's answer means to std: `false` only when the deadline passed. A word that held
/// another value, a wake, or a wait the kernel ended early is `true`, which std's locks read as
/// "look at the word again".
pub(crate) fn woken(answer: crate::Result<()>) -> bool {
    answer != Err(Error::TimedOut)
}

/// Sleeps while `futex` holds `expected`, until a wake or `timeout`; `false` on timeout.
pub fn futex_wait(futex: &AtomicU32, expected: u32, timeout: Option<Duration>) -> bool {
    let deadline = match timeout {
        None => DEADLINE_INFINITE,
        Some(_) => deadline(os::clock(), timeout),
    };
    woken(os::wait(futex, expected, deadline))
}

/// Sleeps while `futex` holds `expected`, until a wake or the clock reaches `deadline`.
pub(crate) fn wait_until(futex: &AtomicU32, expected: u32, deadline: u64) -> bool {
    woken(os::wait(futex, expected, deadline))
}

/// Wakes one thread sleeping on `futex`; whether there was one.
pub fn futex_wake(futex: &AtomicU32) -> bool {
    os::wake(futex, 1) > 0
}

/// Wakes every thread sleeping on `futex`.
pub fn futex_wake_all(futex: &AtomicU32) {
    os::wake(futex, u32::MAX);
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn a_deadline_is_the_clock_plus_the_timeout_and_never_wraps() {
        assert_eq!(deadline(5, None), DEADLINE_INFINITE);
        assert_eq!(deadline(5, Some(Duration::ZERO)), 5);
        assert_eq!(deadline(1_000, Some(Duration::from_micros(3))), 4_000);
        assert_eq!(deadline(10, Some(Duration::from_secs(1))), 1_000_000_010);
        // Past the clock's end, and past u64 nanoseconds: no deadline, not an early one.
        assert_eq!(
            deadline(u64::MAX - 1, Some(Duration::from_nanos(5))),
            DEADLINE_INFINITE
        );
        assert_eq!(deadline(1, Some(Duration::MAX)), DEADLINE_INFINITE);
    }

    #[test]
    fn only_a_passed_deadline_is_a_timeout() {
        assert!(woken(Ok(())));
        assert!(woken(Err(Error::ShouldWait)), "the word held another value");
        assert!(
            woken(Err(Error::Interrupted)),
            "an early end is a spurious wake"
        );
        assert!(woken(Err(Error::BadState)));
        assert!(!woken(Err(Error::TimedOut)));
    }

    #[test]
    fn a_wait_on_a_changed_word_returns_at_once() {
        let word = AtomicU32::new(1);
        assert!(futex_wait(&word, 0, None));
        assert!(futex_wait(&word, 0, Some(Duration::from_secs(60))));
    }

    #[test]
    fn a_wait_times_out_at_its_deadline() {
        // Another test's wake may end a wait early, which the contract allows: each loop
        // waits again until the answer is the timeout.
        let word = AtomicU32::new(0);
        let deadline = os::clock() + 20_000_000;
        while wait_until(&word, 0, deadline) {}
        assert!(os::clock() >= deadline);
        let before = os::clock();
        while futex_wait(&word, 0, Some(Duration::from_millis(5))) {}
        assert!(os::clock() - before >= 5_000_000);
        assert!(
            !wait_until(&word, 0, os::clock()),
            "a deadline now has passed"
        );
    }

    #[test]
    fn a_wake_ends_a_wait() {
        let word = Arc::new(AtomicU32::new(0));
        let waiter = {
            let word = Arc::clone(&word);
            thread::spawn(move || {
                while word.load(Ordering::Acquire) == 0 {
                    futex_wait(&word, 0, None);
                }
            })
        };
        word.store(1, Ordering::Release);
        futex_wake_all(&word);
        waiter.join().unwrap();
        // A wake with nobody asleep on the word is harmless.
        futex_wake(&word);
    }
}
