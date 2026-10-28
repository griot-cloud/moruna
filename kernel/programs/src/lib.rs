//! What the KB1 test programs share.

#![no_std]
#![forbid(unsafe_code)]

use moruna_sys::{Console, exit, start};

/// The exit code of a program whose manifest entry lacks a slot it needs.
pub const MISSING_SLOT: i64 = 90;

/// The console slot, or the process ends with [`MISSING_SLOT`].
pub fn console() -> Console {
    match start::take("console") {
        Some(h) => Console(h),
        None => exit(MISSING_SLOT),
    }
}

/// The handle in `slot`, or the process ends with [`MISSING_SLOT`].
pub fn slot(name: &str) -> moruna_sys::Handle {
    match start::take(name) {
        Some(h) => h,
        None => exit(MISSING_SLOT),
    }
}

/// Milliseconds as nanoseconds.
pub const fn ms(n: u64) -> u64 {
    n * 1_000_000
}
