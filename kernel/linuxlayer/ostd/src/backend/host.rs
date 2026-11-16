// SPDX-License-Identifier: MPL-2.0

//! The host's side, for host tests: futexes on condition variables, the host's threads
//! and clock, and a fallible copy that fails on ranges a test marks.

extern crate std;

use alloc::{boxed::Box, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    ops::Range,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use std::{
    sync::{Condvar, Mutex, OnceLock},
    thread_local,
    time::Instant,
};

use super::{Dir, FOREVER};

const BUCKETS: usize = 64;

struct Bucket {
    lock: Mutex<()>,
    cond: Condvar,
}

fn bucket(word: &AtomicU32) -> &'static Bucket {
    static TABLE: OnceLock<Vec<Bucket>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        (0..BUCKETS)
            .map(|_| Bucket {
                lock: Mutex::new(()),
                cond: Condvar::new(),
            })
            .collect()
    });
    &table[(word as *const AtomicU32 as usize >> 2) % BUCKETS]
}

/// Sleeps while `word` holds `expected`, until a wake or `deadline`; false when it timed out.
pub(crate) fn futex_wait(word: &AtomicU32, expected: u32, deadline: u64) -> bool {
    let b = bucket(word);
    let mut guard = b.lock.lock().unwrap();
    if word.load(Ordering::Acquire) != expected {
        return true;
    }
    if deadline == FOREVER {
        drop(b.cond.wait(guard).unwrap());
        return true;
    }
    let start = now();
    if start >= deadline {
        return false;
    }
    let (g, timeout) = b
        .cond
        .wait_timeout(guard, Duration::from_nanos(deadline - start))
        .unwrap();
    guard = g;
    drop(guard);
    !timeout.timed_out() || now() < deadline
}

/// Wakes the threads sleeping on `word`. A bucket is shared, so every sleeper on it wakes
/// and checks its own word again.
pub(crate) fn futex_wake(word: &AtomicU32, _count: u32) -> u32 {
    let b = bucket(word);
    let _guard = b.lock.lock().unwrap();
    b.cond.notify_all();
    1
}

/// Nanoseconds on a monotonic clock.
pub(crate) fn now() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1
}

pub(crate) fn yield_now() {
    std::thread::yield_now();
}

/// Runs `f` on a new thread.
pub(crate) fn spawn(f: Box<dyn FnOnce() + Send>) -> Result<(), ()> {
    std::thread::Builder::new()
        .spawn(f)
        .map(|_| ())
        .map_err(|_| ())
}

thread_local! {
    static CURRENT: Cell<usize> = const { Cell::new(0) };
    static FAULTS: RefCell<Vec<Range<usize>>> = const { RefCell::new(Vec::new()) };
}

/// The calling thread's task slot.
pub(crate) fn current_slot() -> usize {
    CURRENT.with(Cell::get)
}

pub(crate) fn set_current_slot(value: usize) {
    CURRENT.with(|c| c.set(value));
}

pub(crate) fn debug_write(bytes: &[u8]) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(bytes);
}

pub(crate) fn random(buf: &mut [u8]) {
    use core::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    for (i, b) in buf.iter_mut().enumerate() {
        h.write_usize(i);
        *b = h.finish() as u8;
    }
}

pub(crate) fn num_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

pub(crate) fn exit(code: u32) -> ! {
    std::process::exit(code as i32)
}

/// Makes fallible copies on this thread fail on `range`, as a fault on a program's
/// unmapped page would. Host tests only.
pub fn fault_range(range: Range<usize>) {
    FAULTS.with(|f| f.borrow_mut().push(range));
}

/// How many bytes from `addr` can be touched before a marked range.
fn reachable(addr: usize, len: usize) -> usize {
    FAULTS.with(|f| {
        f.borrow()
            .iter()
            .filter(|r| r.start < addr + len && r.end > addr)
            .map(|r| r.start.saturating_sub(addr))
            .min()
            .unwrap_or(len)
    })
}

/// Copies `len` bytes; returns how many were not copied.
///
/// # Safety
///
/// Both ranges must be valid outside the marked ranges.
pub(crate) unsafe fn copy_fallible(dst: *mut u8, src: *const u8, len: usize, _dir: Dir) -> usize {
    let n = reachable(dst as usize, len).min(reachable(src as usize, len));
    // SAFETY: the caller keeps both ranges valid up to the first marked byte.
    unsafe { core::ptr::copy(src, dst, n) };
    len - n
}

/// Fills `len` bytes with `value`; returns how many were not filled.
///
/// # Safety
///
/// The range must be valid outside the marked ranges.
pub(crate) unsafe fn set_fallible(dst: *mut u8, value: u8, len: usize) -> usize {
    let n = reachable(dst as usize, len);
    // SAFETY: as for `copy_fallible`.
    unsafe { core::ptr::write_bytes(dst, value, n) };
    len - n
}

/// Loads a `u32` from the program's memory; `None` on a fault.
///
/// # Safety
///
/// `ptr` must be valid and aligned outside the marked ranges.
pub(crate) unsafe fn load_u32_fallible(ptr: *const u32) -> Option<u32> {
    if reachable(ptr as usize, 4) < 4 {
        return None;
    }
    // SAFETY: as above; the word is aligned, so the access is atomic.
    Some(unsafe { AtomicU32::from_ptr(ptr as *mut u32) }.load(Ordering::Acquire))
}

/// Compares and exchanges a `u32` of the program's memory; returns the old value, `None`
/// on a fault.
///
/// # Safety
///
/// As for `load_u32_fallible`.
pub(crate) unsafe fn cmpxchg_u32_fallible(ptr: *mut u32, old: u32, new: u32) -> Option<u32> {
    if reachable(ptr as usize, 4) < 4 {
        return None;
    }
    // SAFETY: as above.
    let word = unsafe { AtomicU32::from_ptr(ptr) };
    Some(
        match word.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(v) | Err(v) => v,
        },
    )
}
