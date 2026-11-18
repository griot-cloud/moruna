//! The device manager (SDD 25, f.12), on the kernel's user target: `node.rs`. On a host
//! there is no kernel to run on, so the program is empty there and the library, the
//! directory, is what is tested.

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
