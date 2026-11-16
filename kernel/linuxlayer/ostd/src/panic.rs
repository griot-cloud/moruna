// SPDX-License-Identifier: MPL-2.0

//! Panics: the personality is built with `panic = "abort"`.

/// Aborts: ends the personality's process.
pub fn abort() -> ! {
    crate::backend::exit(101)
}
