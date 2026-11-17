//! The node's API server as a library (system design, section 2B): TLS 1.3, HTTP/1.1, the
//! operations of `moruna-api` (2A) and the limits, over a byte-stream transport the caller
//! drives.
//!
//! The program `moruna-node-api` wires it to the network server's sockets and to `platform`'s
//! hand-over. Everything else is here, and tested on the host:
//!
//! - [`Identity`]: the operator's certificate and key, or the node's own, made at boot, and the
//!   fingerprint the console shows.
//! - [`Server`]: a state machine. The caller [accepts](Server::accept) each connection as a
//!   [`Transport`], [polls](Server::poll) when a stream changes or the
//!   [deadline](Server::deadline) passes, and supplies the time through a [`Clock`]. No threads,
//!   no async runtime.
//! - The limits: [`HEADER_LIMIT`] and [`BODY_LIMIT`] (over them, `invalid`),
//!   [`CONNECTION_LIMIT`] (over it, `busy`) and [`IDLE_LIMIT_MS`]; each answer over a limit
//!   closes its connection.
//!
//! The crate is `no_std` with `alloc`. On the kernel, ring's randomness comes from
//! `moruna_svc::random`, as the network server's does.
#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

mod api;
mod der;
mod http;
mod identity;
#[cfg(target_os = "none")]
mod random;
mod server;
mod tls;
mod transport;

pub use api::NodeFacts;
pub use http::{BODY_LIMIT, HEADER_LIMIT};
pub use identity::{Identity, IdentityError, VALID_AFTER_SECONDS, VALID_BEFORE_SECONDS};
pub use server::{CONNECTION_LIMIT, Config, IDLE_LIMIT_MS, Server};
pub use transport::{Clock, Transport, TransportError};
