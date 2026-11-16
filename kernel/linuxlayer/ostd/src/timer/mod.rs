// SPDX-License-Identifier: MPL-2.0

//! The timer tick, in user space.
//!
//! No interrupt ticks a user process. Jiffies are read off the core's monotonic clock,
//! and the callbacks OSTD runs on every tick run on a ticker thread, started by the first
//! registration, which sleeps on the core's futex between ticks.

mod jiffies;

use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub use jiffies::Jiffies;

use crate::{backend, sync::raw::RawLock};

/// Nanoseconds on the core's monotonic clock.
pub fn monotonic_nanos() -> u64 {
    backend::now()
}

/// The tick frequency.
pub const TIMER_FREQ: u64 = 1000;

type Callback = Box<dyn Fn() + Sync + Send>;

static CALLBACKS: RawLock<Vec<Callback>> = RawLock::new(Vec::new());
static STARTED: AtomicBool = AtomicBool::new(false);

/// Runs `func` on every tick. User space has one set of callbacks, run by one thread.
pub fn register_callback_on_cpu<F>(func: F)
where
    F: Fn() + Sync + Send + 'static,
{
    CALLBACKS.lock().push(Box::new(func));
    start_ticker();
}

/// Starts the ticker thread, once: tick callbacks and bottom halves run on it.
pub(crate) fn start_ticker() {
    if !STARTED.swap(true, Ordering::AcqRel) && backend::spawn(Box::new(ticker)).is_err() {
        STARTED.store(false, Ordering::Release);
    }
}

/// Runs every callback once, then the bottom half: what a tick does.
pub fn tick() {
    {
        let callbacks = CALLBACKS.lock();
        for callback in callbacks.iter() {
            callback();
        }
    }
    crate::irq::process_bottom_half(crate::irq::TIMER_IRQ);
}

fn ticker() {
    let never = AtomicU32::new(0);
    let period = 1_000_000_000 / TIMER_FREQ;
    let mut next = backend::now() + period;
    loop {
        while backend::now() < next {
            backend::futex_wait(&never, 0, next);
        }
        tick();
        next += period;
    }
}
