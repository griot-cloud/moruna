// SPDX-License-Identifier: MPL-2.0

//! The futex lock every lock here is built on.
//!
//! A user process cannot turn preemption or interrupts off, so a holder may be descheduled
//! while it holds a lock; a waiter spins briefly, then sleeps on the core's futex. The
//! word is 0 when free, 1 when held, 2 when held with sleepers (Drepper, "Futexes Are
//! Tricky").

use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::backend;

const FREE: u32 = 0;
const HELD: u32 = 1;
const CONTENDED: u32 = 2;
const SPINS: u32 = 100;

/// A futex word used as a lock.
#[derive(Debug)]
pub(crate) struct RawWord(AtomicU32);

impl RawWord {
    pub(crate) const fn new() -> Self {
        Self(AtomicU32::new(FREE))
    }

    pub(crate) fn try_lock(&self) -> bool {
        self.0
            .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    pub(crate) fn lock(&self) {
        if self.try_lock() {
            return;
        }
        for _ in 0..SPINS {
            core::hint::spin_loop();
            if self.0.load(Ordering::Relaxed) == FREE && self.try_lock() {
                return;
            }
        }
        while self.0.swap(CONTENDED, Ordering::Acquire) != FREE {
            backend::futex_wait(&self.0, CONTENDED, backend::FOREVER);
        }
    }

    /// # Safety
    ///
    /// The caller holds the lock.
    pub(crate) unsafe fn unlock(&self) {
        if self.0.swap(FREE, Ordering::Release) == CONTENDED {
            backend::futex_wake(&self.0, 1);
        }
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.0.load(Ordering::Relaxed) != FREE
    }
}

/// A value behind a [`RawWord`], with no atomic-mode guard: the RCU monitor's own lock.
pub(crate) struct RawLock<T> {
    word: RawWord,
    val: UnsafeCell<T>,
}

// SAFETY: one holder at a time reaches the value.
unsafe impl<T: Send> Sync for RawLock<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Send for RawLock<T> {}

impl<T> RawLock<T> {
    pub(crate) const fn new(val: T) -> Self {
        Self {
            word: RawWord::new(),
            val: UnsafeCell::new(val),
        }
    }

    pub(crate) fn lock(&self) -> RawGuard<'_, T> {
        self.word.lock();
        RawGuard { lock: self }
    }
}

pub(crate) struct RawGuard<'a, T> {
    lock: &'a RawLock<T>,
}

impl<T> Deref for RawGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the lock is held.
        unsafe { &*self.lock.val.get() }
    }
}

impl<T> DerefMut for RawGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the lock is held.
        unsafe { &mut *self.lock.val.get() }
    }
}

impl<T> Drop for RawGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: the guard holds the lock.
        unsafe { self.lock.word.unlock() };
    }
}

/// Where waiters for a lock with its own state word sleep: a release bumps the sequence
/// and wakes sleepers, if there are any.
#[derive(Debug)]
pub(crate) struct Parking {
    seq: AtomicU32,
    sleepers: AtomicU32,
}

impl Parking {
    pub(crate) const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            sleepers: AtomicU32::new(0),
        }
    }

    /// Calls `attempt` until it succeeds, spinning briefly, then sleeping between tries.
    pub(crate) fn wait<R>(&self, mut attempt: impl FnMut() -> Option<R>) -> R {
        for _ in 0..SPINS {
            if let Some(r) = attempt() {
                return r;
            }
            core::hint::spin_loop();
        }
        loop {
            self.sleepers.fetch_add(1, Ordering::SeqCst);
            let seq = self.seq.load(Ordering::SeqCst);
            if let Some(r) = attempt() {
                self.sleepers.fetch_sub(1, Ordering::SeqCst);
                return r;
            }
            backend::futex_wait(&self.seq, seq, backend::FOREVER);
            self.sleepers.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Called after every release.
    pub(crate) fn notify(&self) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        if self.sleepers.load(Ordering::SeqCst) != 0 {
            backend::futex_wake(&self.seq, u32::MAX);
        }
    }
}
