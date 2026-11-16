// SPDX-License-Identifier: MPL-2.0

//! B2's booted check (`target.rs`). On the host there is nothing to run; its verdict is
//! the library's, tested there.

#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod target;

#[cfg(not(target_os = "none"))]
fn main() {}
