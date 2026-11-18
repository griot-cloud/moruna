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
//!   [`CONNECTION_LIMIT`] (over it, `busy`), [`OPEN_LIMIT`] (over it, closed with no answer),
//!   [`IDLE_LIMIT_MS`] and [`HANDSHAKE_LIMIT_MS`]; each answer over a limit closes its
//!   connection.
//! - Memory: a request's head is read and its token checked before any of its body, so a body
//!   is read only for a call that takes one and is allowed; bodies share [`BODY_BUDGET`], and a
//!   body that does not fit is answered `busy`. An answer's body is written as TLS takes it,
//!   32 KiB at a time, from where it is kept: a report, a page of events, or an MCP tool's
//!   result holding either twice, is never copied whole.
//! - Tokens (2C): every operation but `health` needs a JWT signed with Ed25519 under one of the
//!   node's [`TokenKey`]s, for this node, expiring within [`MOST_AHEAD_SECONDS`], whose scope
//!   covers the operation. Each call of an operation but `health` leaves an audit record; the
//!   last [`AUDIT_LIMIT`] are kept in memory, and `node/audit` answers the newest
//!   [`AUDIT_ANSWER_LIMIT`], newest first.
//! - MCP (2D): `POST /mcp` on the same port with the same tokens, MCP's Streamable HTTP,
//!   stateless, at [`PROTOCOL_VERSION`]: one tool per operation, `moruna_<id>`. The stream of
//!   a job's events is `moruna_jobs_events`, which answers a page of them after a record, up to
//!   1,000 and 1 MiB, and the record to ask after next: an agent polls.
//! - Jobs (3A, 3B): the server is the engine's host. Through an [`EngineHost`] the node gives it
//!   ([`Server::with_engine`]), it has the engine check a document, starts a fresh engine for
//!   each job with the host protocol's `spec`, sends `cancel`, and reads `hello`, `heartbeat`,
//!   the framed event records, `report` and `exit`. One job runs at a time; the last
//!   [`JOBS_KEPT`] are kept with their state, heartbeat, exit and report; the last
//!   [`EVENTS_KEPT`] event and log records, within [`EVENT_BUFFER_BYTES`], are kept for
//!   `GET /v1/jobs/{id}/events`, which sends them as Server-Sent Events and replays them after
//!   `?from=`, the last record the reader has, as Server-Sent Events' `Last-Event-ID` is. What
//!   a job says is secret appears in no answer, event, log or report.
//! - Functions (3E): `POST /v1/functions` registers a function image by name when its Ed25519
//!   signature over the image's SHA-256 verifies under one of node config's `functions.keys`
//!   ([`FunctionKey`]); the entries, at most [`FUNCTIONS_LIMIT`], are kept in memory and listed by
//!   `GET /v1/functions`. A check's verdict refuses each function the job uses ([`Checked`]) that
//!   is not registered. Nothing is fetched here: the node fetches a job's images at its start.
//!
//! The crate is `no_std` with `alloc`. On the kernel, ring's randomness comes from
//! `moruna_svc::random`, as the network server's does.
#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
// The tests' helpers are shared with the end-to-end tests, which name this crate from outside.
#[cfg(test)]
extern crate self as moruna_api_server;
#[cfg(test)]
extern crate std;

mod api;
mod audit;
mod body;
mod der;
mod engine;
#[cfg(test)]
#[path = "../tests/common/engine.rs"]
mod fake;
mod functions;
mod http;
mod identity;
mod jobs;
mod json;
mod mcp;
#[cfg(target_os = "none")]
mod random;
mod server;
#[cfg(test)]
#[path = "../tests/common/signer.rs"]
mod signer;
mod tls;
mod token;
mod transport;

pub use api::NodeFacts;
#[cfg(feature = "test-exit")]
pub use api::TEST_EXIT_PATH;
pub use audit::{AUDIT_ANSWER_LIMIT, AUDIT_LIMIT};
pub use engine::{Checked, ENGINE_LINE_LIMIT, EngineHost, HostError, NoEngine};
pub use functions::{ADDRESS_LIMIT, FUNCTIONS_LIMIT, FunctionKey};
pub use http::{BODY_BUDGET, BODY_LIMIT, HEADER_LIMIT};
pub use identity::{Identity, IdentityError, VALID_AFTER_SECONDS, VALID_BEFORE_SECONDS};
pub use jobs::{EVENT_BUFFER_BYTES, EVENTS_KEPT, JOBS_KEPT, REPORT_BYTES};
pub use mcp::PROTOCOL_VERSION;
pub use server::{CONNECTION_LIMIT, Config, HANDSHAKE_LIMIT_MS, IDLE_LIMIT_MS, OPEN_LIMIT, Server};
pub use token::{MOST_AHEAD_SECONDS, TokenKey};
pub use transport::{Clock, Transport, TransportError};
