//! The clock and the deadline queue (SDD 20, f.6).
//!
//! The clock is the TSC scaled to nanoseconds since boot. Deadlines live in one queue
//! ordered by time; OSTD's 1 kHz timer interrupt pops every entry that is due, firing a
//! timer object or waking a waiting thread. Every mutation of the queue, and of a
//! timer's armed state, happens under the queue's lock, which disables interrupts.

use alloc::{collections::BTreeMap, sync::Arc, sync::Weak};
use core::sync::atomic::{AtomicU64, Ordering};

use moruna_abi::{DEADLINE_INFINITE, Signals};
use ostd::{
    arch::{read_tsc, tsc_freq},
    sync::{LocalIrqDisabled, SpinLock, Waker},
    timer,
};

use crate::signal::Signaller;

static TSC_HZ: AtomicU64 = AtomicU64::new(0);
static TSC_ORIGIN: AtomicU64 = AtomicU64::new(0);
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// A queue key: the deadline and a sequence number that makes it unique.
pub type Key = (u64, u64);

enum Entry {
    Timer(Weak<Timer>),
    Wake(Arc<Waker>),
}

static QUEUE: SpinLock<BTreeMap<Key, Entry>, LocalIrqDisabled> = SpinLock::new(BTreeMap::new());

/// Starts the clock and the deadline queue. Fails if OSTD could not measure the TSC.
pub fn init() -> Result<(), &'static str> {
    let hz = tsc_freq();
    if hz == 0 {
        return Err("the TSC frequency is unknown, so the clock cannot run");
    }
    TSC_HZ.store(hz, Ordering::Release);
    TSC_ORIGIN.store(read_tsc(), Ordering::Release);
    timer::register_callback_on_cpu(tick);
    Ok(())
}

/// Nanoseconds since the clock started.
pub fn now() -> u64 {
    let hz = TSC_HZ.load(Ordering::Acquire);
    if hz == 0 {
        return 0;
    }
    let ticks = read_tsc().wrapping_sub(TSC_ORIGIN.load(Ordering::Acquire));
    let ns = (ticks as u128 * 1_000_000_000) / hz as u128;
    u64::try_from(ns).unwrap_or(u64::MAX - 1)
}

fn next_key(deadline: u64) -> Key {
    (deadline, SEQUENCE.fetch_add(1, Ordering::Relaxed))
}

/// Queues a wake-up of `waker` at `deadline`; returns the key to cancel it with.
pub fn arm_wake(deadline: u64, waker: Arc<Waker>) -> Key {
    let key = next_key(deadline);
    QUEUE.lock().insert(key, Entry::Wake(waker));
    key
}

/// Cancels a queued wake-up; a no-op if it already fired.
pub fn disarm(key: Key) {
    QUEUE.lock().remove(&key);
}

fn tick() {
    let now = now();
    let mut queue = QUEUE.lock();
    while let Some(entry) = queue.first_entry() {
        if entry.key().0 > now {
            break;
        }
        match entry.remove() {
            Entry::Timer(timer) => {
                if let Some(timer) = timer.upgrade() {
                    timer.armed.store(0, Ordering::Relaxed);
                    timer.signaller.update(Signals::NONE, Signals::SIGNALED);
                }
            }
            Entry::Wake(waker) => {
                waker.wake_up();
            }
        }
    }
}

/// A timer object: raises `SIGNALED` when its deadline passes.
pub struct Timer {
    pub signaller: Signaller,
    /// The sequence number of the queued entry, zero when not armed.
    armed: AtomicU64,
    deadline: AtomicU64,
}

impl Timer {
    pub fn new() -> Timer {
        Timer {
            signaller: Signaller::new(Signals::NONE),
            armed: AtomicU64::new(0),
            deadline: AtomicU64::new(0),
        }
    }

    /// Clears `SIGNALED` and arms the timer for `deadline`; zero disarms it. A deadline
    /// already past fires on the next tick.
    pub fn set(self: &Arc<Self>, deadline: u64) {
        let mut queue = QUEUE.lock();
        let seq = self.armed.swap(0, Ordering::Relaxed);
        if seq != 0 {
            queue.remove(&(self.deadline.load(Ordering::Relaxed), seq));
        }
        self.signaller.update(Signals::SIGNALED, Signals::NONE);
        if deadline != 0 && deadline != DEADLINE_INFINITE {
            let key = next_key(deadline);
            self.deadline.store(deadline, Ordering::Relaxed);
            self.armed.store(key.1, Ordering::Relaxed);
            queue.insert(key, Entry::Timer(Arc::downgrade(self)));
        }
    }
}
