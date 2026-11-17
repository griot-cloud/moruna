// SPDX-License-Identifier: MPL-2.0

//! The drop-in `linux` server's program (`target.rs`). On the host there is nothing to
//! run; its decisions are the library's, tested there.

#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod target;

#[cfg(not(target_os = "none"))]
fn main() {}
