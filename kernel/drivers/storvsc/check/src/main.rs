//! Item 1.10b's Azure check, on the kernel's user target (`node.rs`). On a host the program
//! is empty, and the library is what is tested.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![forbid(unsafe_code)]

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod node;

#[cfg(target_os = "none")]
moruna_svc::main!(node::run);

#[cfg(not(target_os = "none"))]
fn main() {}
