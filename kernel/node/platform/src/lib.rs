//! `platform` (system design, section 1D; work item 1.4): the process that finds the node's cloud
//! from instance data, sets the wall clock from that answer's `Date` header, validates node
//! config, hands `api` its config, and prints the `cloud` and `ready` lines.
//!
//! This library is the logic, written against the traits of [`machine`] so every rule is tested
//! on a host: [`http`] is the minimal HTTP/1.1 client, [`instance`] finds the cloud with its
//! retry rules and its 90 s limit, [`date`] reads the `Date` header, [`wireserver`] is Azure's
//! ready report (section 1F, work item 1.12), and [`boot`] is the whole sequence with its fatal
//! lines. [`svc`] gives the traits on the node itself. The program (`src/main.rs`) only takes its
//! start handles and wires the two together.
//!
//! Node config's validation and the hand-over's message are `moruna-node-config`'s, which `api`
//! uses too.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
// The tests run on a host, with threads standing in for the node's other processes.
#[cfg(test)]
extern crate std;

pub mod boot;
pub mod date;
pub mod http;
pub mod instance;
pub mod machine;
pub mod svc;
pub mod wireserver;
mod xml;

#[cfg(test)]
mod tests;
