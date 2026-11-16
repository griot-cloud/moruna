// SPDX-License-Identifier: MPL-2.0

//! The RCU monitor in user space.
//!
//! OSTD ends a grace period when every CPU has passed a context switch. A user process
//! sees no context switches, so here a reader is any atomic-mode guard (a disabled
//! preemption or IRQ guard, so any spin lock held), counted in one of two epochs. A
//! callback waits for the epoch it was queued in to empty, and runs on the reclaimer
//! thread, which holds no lock of the caller's.

use alloc::{boxed::Box, collections::VecDeque};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::SeqCst};

use crate::{backend, sync::raw::RawLock};

type Callback = Box<dyn FnOnce() + Send + 'static>;

/// The current epoch; readers count under its parity.
static EPOCH: AtomicUsize = AtomicUsize::new(0);
static READERS: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
/// Bumped when a reader count reaches zero or a callback is queued; the reclaimer
/// sleeps on it.
static KICK: AtomicU32 = AtomicU32::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

struct Queue {
    /// Callbacks queued in each epoch, oldest first.
    callbacks: VecDeque<(usize, Callback)>,
}

static QUEUE: RawLock<Queue> = RawLock::new(Queue {
    callbacks: VecDeque::new(),
});
/// Held by a pass of `reclaim`, so a pass on a yielding thread waits for the callbacks
/// another pass is running, as OSTD's grace periods end before a yield returns.
static RUNNING: RawLock<()> = RawLock::new(());

/// Enters a read-side section; returns the parity to leave it with.
pub(crate) fn reader_enter() -> usize {
    loop {
        let epoch = EPOCH.load(SeqCst);
        READERS[epoch & 1].fetch_add(1, SeqCst);
        if EPOCH.load(SeqCst) == epoch {
            return epoch & 1;
        }
        // The epoch flipped between the load and the count: count under the new one.
        reader_exit(epoch & 1);
    }
}

/// Leaves a read-side section entered with parity `parity`.
pub(crate) fn reader_exit(parity: usize) {
    if READERS[parity].fetch_sub(1, SeqCst) == 1 && STARTED.load(SeqCst) {
        kick();
    }
}

fn kick() {
    KICK.fetch_add(1, SeqCst);
    backend::futex_wake(&KICK, 1);
}

/// Runs `f` once every reader section that began before this call has ended.
pub(super) fn after_grace_period(f: Callback) {
    {
        let mut queue = QUEUE.lock();
        queue.callbacks.push_back((EPOCH.load(SeqCst), f));
    }
    if !STARTED.swap(true, SeqCst) && backend::spawn(Box::new(reclaimer)).is_err() {
        STARTED.store(false, SeqCst);
    }
    kick();
}

/// Runs the callbacks whose grace period has ended; returns whether any remain.
///
/// The epoch moves from `cur` to `cur + 1` only once the readers of `cur - 1` (the same
/// parity as `cur + 1`) are gone, so when that parity is empty every reader of an epoch
/// before `cur` has ended, and the callbacks queued before `cur` may run.
pub(crate) fn reclaim() -> bool {
    let _running = RUNNING.lock();
    loop {
        let cur = EPOCH.load(SeqCst);
        if READERS[(cur + 1) & 1].load(SeqCst) != 0 {
            return true;
        }
        let due: VecDeque<_> = {
            let mut queue = QUEUE.lock();
            let n = queue.callbacks.iter().take_while(|(e, _)| *e < cur).count();
            queue.callbacks.drain(..n).collect()
        };
        for (_, f) in due {
            f();
        }
        if QUEUE.lock().callbacks.is_empty() {
            return false;
        }
        // What is left was queued in `cur`: move new readers on, and wait for `cur`'s.
        let _ = EPOCH.compare_exchange(cur, cur + 1, SeqCst, SeqCst);
    }
}

fn reclaimer() {
    loop {
        let seen = KICK.load(SeqCst);
        if !reclaim() || KICK.load(SeqCst) == seen {
            backend::futex_wait(&KICK, seen, backend::FOREVER);
        }
    }
}
