// SPDX-License-Identifier: MPL-2.0

//! Panics: the personality is built with `panic = "abort"`, so nothing unwinds.

use alloc::boxed::Box;
use core::any::Any;

/// Aborts: ends the personality's process.
pub fn abort() -> ! {
    crate::backend::exit(101)
}

/// Runs `f`. With `panic = "abort"` a panic in it ends the process, so it never returns
/// `Err`.
pub fn catch_unwind<R, F: FnOnce() -> R>(f: F) -> Result<R, Box<dyn Any + Send>> {
    Ok(f())
}

/// Starts a panic carrying `payload`: with nothing to unwind, it aborts.
#[expect(
    clippy::boxed_local,
    reason = "OSTD's signature, which the layer calls"
)]
pub fn begin_panic<T: Send + 'static>(_payload: Box<T>) -> ! {
    abort()
}

/// Prints the stack: the core prints a faulting process's, so nothing here.
pub fn print_stack_trace() {}
