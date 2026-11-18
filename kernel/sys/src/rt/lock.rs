//! The runtime's own lock, for what std's two streams share (the log) and the list of detached
//! threads. It is a futex lock with std's futex `Mutex`'s state machine: the word is 0 when
//! free, 1 when held, 2 when held and a thread may be asleep on it. Taking a free lock and
//! giving back one nobody waits for make no call; only a thread that finds it held sleeps, and
//! only a release that finds 2 wakes one.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use super::futex::{futex_wait, futex_wake};

const FREE: u32 = 0;
const HELD: u32 = 1;
/// Held, and a thread may be asleep on the word.
const CONTENDED: u32 = 2;

/// How many times a thread looks again at a held lock before it sleeps.
const SPINS: u32 = 100;

pub(crate) struct Lock<T> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: the value is reached only through a `Guard`, and the state machine lets one guard
// exist at a time, as a `Mutex` does.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    pub(crate) const fn new(value: T) -> Lock<T> {
        Lock {
            state: AtomicU32::new(FREE),
            value: UnsafeCell::new(value),
        }
    }

    /// Takes the lock, sleeping while another thread holds it.
    pub(crate) fn lock(&self) -> Guard<'_, T> {
        if self
            .state
            .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.contended();
        }
        Guard { lock: self }
    }

    /// Takes the lock if it is free.
    pub(crate) fn try_lock(&self) -> Option<Guard<'_, T>> {
        self.state
            .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Guard { lock: self })
    }

    #[cold]
    fn contended(&self) {
        let mut state = self.spin();
        if state == FREE {
            match self
                .state
                .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(now) => state = now,
            }
        }
        loop {
            // Marked contended before sleeping, so the holder's release wakes a thread. A
            // swap that finds it free has taken it, contended: a thread taken over from may
            // still be asleep, and the release after this one wakes it.
            if state != CONTENDED && self.state.swap(CONTENDED, Ordering::Acquire) == FREE {
                return;
            }
            futex_wait(&self.state, CONTENDED, None);
            state = self.spin();
        }
    }

    /// Looks at the word while it is held and nobody sleeps on it, briefly.
    fn spin(&self) -> u32 {
        let mut spins = SPINS;
        loop {
            let state = self.state.load(Ordering::Relaxed);
            if state != HELD || spins == 0 {
                return state;
            }
            core::hint::spin_loop();
            spins -= 1;
        }
    }

    fn unlock(&self) {
        if self.state.swap(FREE, Ordering::Release) == CONTENDED {
            futex_wake(&self.state);
        }
    }
}

pub(crate) struct Guard<'a, T> {
    lock: &'a Lock<T>,
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard is the one the state machine let exist.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, and the guard is borrowed mutably.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::vec::Vec;

    use super::*;

    fn state<T>(lock: &Lock<T>) -> u32 {
        lock.state.load(Ordering::SeqCst)
    }

    #[test]
    fn a_free_lock_is_taken_and_given_back_with_no_wait() {
        let lock = Lock::new(5);
        assert_eq!(state(&lock), FREE);
        {
            let mut guard = lock.lock();
            assert_eq!(state(&lock), HELD);
            *guard += 1;
            assert!(lock.try_lock().is_none(), "held: try_lock refuses");
        }
        assert_eq!(state(&lock), FREE);
        assert_eq!(*lock.try_lock().expect("free"), 6);
        assert_eq!(state(&lock), FREE);
    }

    #[test]
    fn a_thread_that_finds_it_held_marks_it_contended_and_is_woken_by_the_release() {
        let lock = Arc::new(Lock::new(0u32));
        let guard = lock.lock();
        let waiter = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                *lock.lock() += 1;
            })
        };
        // The waiter spins, then marks the word and sleeps.
        while state(&lock) != CONTENDED {
            thread::yield_now();
        }
        drop(guard);
        waiter.join().unwrap();
        assert_eq!(*lock.lock(), 1);
        assert_eq!(state(&lock), FREE);
    }

    #[test]
    fn a_lock_freed_during_the_spin_is_taken_without_sleeping() {
        let lock = Lock::new(());
        // Freed by the time the slow path looks: it takes it, uncontended.
        lock.contended();
        assert_eq!(state(&lock), HELD);
        lock.unlock();
        assert_eq!(state(&lock), FREE);
    }

    #[test]
    fn a_release_of_a_contended_lock_wakes_a_sleeper_even_if_none_is_left() {
        // A thread took it over from a sleeper: the word says contended, and the release
        // wakes, harmlessly, whoever is asleep.
        let lock = Lock::new(());
        let guard = lock.lock();
        lock.state.store(CONTENDED, Ordering::SeqCst);
        drop(guard);
        assert_eq!(state(&lock), FREE);
    }

    #[test]
    fn many_threads_each_get_the_lock_alone() {
        let lock = Arc::new(Lock::new(Vec::new()));
        let inside = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let lock = Arc::clone(&lock);
                let inside = Arc::clone(&inside);
                thread::spawn(move || {
                    for i in 0..500 {
                        let mut guard = lock.lock();
                        assert!(!inside.swap(true, Ordering::SeqCst), "two holders at once");
                        guard.push(t * 1000 + i);
                        inside.store(false, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(lock.lock().len(), 2000);
        assert_eq!(state(&lock), FREE);
    }
}
