//! Futexes (SDD 20, f.13): threads sleeping on a 32-bit word of their process's memory.
//!
//! A wait reads the word and queues the thread under one lock, so a wake that follows
//! a change of the word can never fall between the read and the sleep. A queue is
//! keyed by the process and the word's address: no word of one process names a queue
//! of another (KK-I1). A waiter carries a bitset; a wake wakes only waiters whose set
//! meets its own, which is what the bitset forms of Linux's futex need.

use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use moruna_abi::{DEADLINE_INFINITE, Error};
use ostd::sync::{SpinLock, Waiter, Waker};

use crate::{clock, process::Thread, usercopy};

/// Every bit: a waiter or a wake without a bitset.
pub const ALL: u32 = u32::MAX;

struct Sleeper {
    waker: Arc<Waker>,
    bitset: u32,
    woken: AtomicBool,
    /// The word whose queue holds it; a requeue moves it, under the queues' lock.
    addr: AtomicU64,
}

type Key = (u64, u64);

static QUEUES: SpinLock<BTreeMap<Key, VecDeque<Arc<Sleeper>>>> = SpinLock::new(BTreeMap::new());

/// Sleeps while the word at `addr` of the caller's memory holds `expected`, until a wake
/// meeting `bitset`, the deadline, or the thread being killed. `ShouldWait` when the
/// word holds another value, `TimedOut` at the deadline, `BadState` when killed.
pub fn wait(
    thread: &Thread,
    addr: u64,
    expected: u32,
    deadline: u64,
    bitset: u32,
) -> Result<(), Error> {
    if !addr.is_multiple_of(4) || bitset == 0 {
        return Err(Error::InvalidArgs);
    }
    let process = &thread.process;
    let key = (process.koid, addr);
    let (waiter, waker) = Waiter::new_pair();
    let sleeper = Arc::new(Sleeper {
        waker: waker.clone(),
        bitset,
        woken: AtomicBool::new(false),
        addr: AtomicU64::new(addr),
    });
    {
        // The word is read with the queue's lock held; it is populated first, so the
        // read under the lock never needs the space's lock.
        process.space.populate(addr, 4, false)?;
        let mut queues = QUEUES.lock();
        let value = usercopy::read_u32(&process.space, addr)?;
        if value != expected {
            return Err(Error::ShouldWait);
        }
        let queue = queues.entry(key).or_default();
        queue.try_reserve(1).map_err(|_| Error::NoMemory)?;
        queue.push_back(sleeper.clone());
    }
    thread.set_waker(Some(waker.clone()));
    let armed = (deadline != 0 && deadline != DEADLINE_INFINITE)
        .then(|| clock::arm_wake(deadline, waker.clone()));
    let result = loop {
        if sleeper.woken.load(Ordering::Acquire) {
            break Ok(());
        }
        if thread.killed() {
            break Err(Error::BadState);
        }
        if deadline != DEADLINE_INFINITE && clock::now() >= deadline {
            break Err(Error::TimedOut);
        }
        waiter.wait();
    };
    if let Some(armed) = armed {
        clock::disarm(armed);
    }
    thread.set_waker(None);
    if result.is_err() {
        // Leave the queue, unless a wake took this sleeper first: then the wake
        // counted it, and the wait succeeded.
        let mut queues = QUEUES.lock();
        let now_at = (key.0, sleeper.addr.load(Ordering::Relaxed));
        let removed = remove(&mut queues, now_at, &sleeper);
        if !removed && sleeper.woken.load(Ordering::Acquire) {
            return Ok(());
        }
    }
    result
}

fn remove(
    queues: &mut BTreeMap<Key, VecDeque<Arc<Sleeper>>>,
    key: Key,
    sleeper: &Arc<Sleeper>,
) -> bool {
    let Some(queue) = queues.get_mut(&key) else {
        return false;
    };
    let before = queue.len();
    queue.retain(|s| !Arc::ptr_eq(s, sleeper));
    let removed = queue.len() != before;
    if queue.is_empty() {
        queues.remove(&key);
    }
    removed
}

/// Wakes at most `count` threads of process `koid` sleeping on `addr` whose bitset
/// meets `bitset`; returns how many.
pub fn wake(koid: u64, addr: u64, count: u32, bitset: u32) -> Result<u32, Error> {
    if !addr.is_multiple_of(4) || bitset == 0 {
        return Err(Error::InvalidArgs);
    }
    let mut queues = QUEUES.lock();
    Ok(wake_locked(&mut queues, (koid, addr), count, bitset))
}

fn wake_locked(
    queues: &mut BTreeMap<Key, VecDeque<Arc<Sleeper>>>,
    key: Key,
    count: u32,
    bitset: u32,
) -> u32 {
    let Some(queue) = queues.get_mut(&key) else {
        return 0;
    };
    let mut woken = 0u32;
    let mut i = 0;
    while i < queue.len() && woken < count {
        if queue[i].bitset & bitset != 0 {
            if let Some(s) = queue.remove(i) {
                s.woken.store(true, Ordering::Release);
                s.waker.wake_up();
                woken += 1;
            }
        } else {
            i += 1;
        }
    }
    if queue.is_empty() {
        queues.remove(&key);
    }
    woken
}

/// Wakes at most `wake` threads sleeping on `from` and moves at most `requeue` of the
/// rest to `to`; with `expected`, only while the word at `from` holds it
/// (`ShouldWait` otherwise). Returns the woken and moved, together.
pub fn requeue(
    thread: &Thread,
    from: u64,
    to: u64,
    wake: u32,
    requeue: u32,
    expected: Option<u32>,
) -> Result<u32, Error> {
    if !from.is_multiple_of(4) || !to.is_multiple_of(4) {
        return Err(Error::InvalidArgs);
    }
    let process = &thread.process;
    if expected.is_some() {
        process.space.populate(from, 4, false)?;
    }
    let mut queues = QUEUES.lock();
    if let Some(expected) = expected
        && usercopy::read_u32(&process.space, from)? != expected
    {
        return Err(Error::ShouldWait);
    }
    let woken = wake_locked(&mut queues, (process.koid, from), wake, ALL);
    let mut moved = 0u32;
    if from != to && requeue > 0 {
        let mut taken = VecDeque::new();
        if let Some(queue) = queues.get_mut(&(process.koid, from)) {
            while moved < requeue {
                match queue.pop_front() {
                    Some(s) => {
                        taken.push_back(s);
                        moved += 1;
                    }
                    None => break,
                }
            }
            if queue.is_empty() {
                queues.remove(&(process.koid, from));
            }
        }
        if !taken.is_empty() {
            let target = queues.entry((process.koid, to)).or_default();
            if target.try_reserve(taken.len()).is_err() {
                // No room to move them: wake them instead, which a waiter tolerates.
                for s in taken {
                    s.woken.store(true, Ordering::Release);
                    s.waker.wake_up();
                }
            } else {
                for s in &taken {
                    s.addr.store(to, Ordering::Relaxed);
                }
                target.extend(taken);
            }
        }
    }
    Ok(woken + moved)
}
