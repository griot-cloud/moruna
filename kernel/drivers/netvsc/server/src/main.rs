//! The `netvsc` process (system design 1F; work item 1.11b), on the kernel's user target:
//! the library's driver over the node's calls (`node.rs`). On a host there is no core to
//! run on, so the program is empty there and the library is what is tested.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![forbid(unsafe_code)]

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod node;

#[cfg(target_os = "none")]
moruna_sys::main!(node::run);

#[cfg(not(target_os = "none"))]
fn main() {}
