//! Node config version 1 and the hand-over (system design, section 1D; work item 1.4).
//!
//! **Node config** is the instance's user-data: the keys that verify API tokens, optionally the
//! operator's TLS identity, and optionally what the node needs to take function images.
//! [`NodeConfig::validate`] reads it and refuses anything that is not exactly version 1's
//! schema; each [`Refusal`] prints as the reason of the console's `moruna: fatal config:` line.
//!
//! **The hand-over** is what `platform` gives `api` once the config is valid: the config, the
//! instance ID, the cloud, the wall clock in Unix seconds, and the core's monotonic time when
//! that clock was read. On the channel the supervisor links between the two processes:
//!
//! 1. `platform` writes one message, the hand-over as JSON ([`Handover::encode`]). It carries
//!    no handles, and it fits one channel message ([`MESSAGE_MAX`]).
//! 2. `api` reads it ([`Handover::decode`]) and keeps it.
//! 3. Once its listener is open, `api` writes one message, [`READY`].
//!
//! `api` carries the wall clock forward with [`Handover::wall_clock_at`].
//!
//! The crate is `no_std` with `alloc`: both processes run on the microkernel.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

pub mod base64;
mod config;
mod handover;
mod json;
pub mod pem;

pub use config::{
    Api, CACHE_BYTES_DEFAULT, CACHE_BYTES_MIN, Functions, KEYS_MAX, MIB, NodeConfig, PublicKey,
    Refusal, Tls, VERSION,
};
pub use handover::{
    Handover, HandoverError, INSTANCE_MAX, MESSAGE_MAX, READY, cloud_name, instance_valid,
};
pub use moruna_api::Cloud;

#[cfg(test)]
mod tests;
