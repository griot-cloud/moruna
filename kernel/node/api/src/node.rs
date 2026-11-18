//! What the API server is made of on a node (work item 2.2b): its config from `platform`'s
//! hand-over, and its clock.
//!
//! - **The identity:** node config's `tls`, the operator's certificate and key, when it has
//!   one; otherwise the node's own, made now ([`Identity::generate`], system design 2B).
//! - **The token keys and the instance ID** (2C): node config's `token_keys`, and the
//!   hand-over's instance, which every token's `aud` must be.
//! - **The function keys** (3E): node config's `functions.keys`, which a function image's
//!   signature must verify under; none when it has none.
//! - **The node document** (2A): this crate's version, the node's cores, its memory, the
//!   instance, the cloud, the CPU executor with a slot a core, and health `ok`. `boot_ms`
//!   is set when the process answers `ready` ([`crate::run`]).
//! - **The wall clock:** the hand-over's, carried forward by the core's monotonic time since
//!   `platform` read it.

use alloc::{string::String, vec, vec::Vec};

use moruna_api::{Executor, ExecutorKind, NodeHealth};
use moruna_api_server::{Clock, Config, FunctionKey, Identity, NodeFacts, TokenKey};
use moruna_node_config::Handover;

use crate::Error;

/// The version the node document gives: this crate's.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The API server's config, from the hand-over, for a node of `cores` CPUs, at `now` on
/// the core's monotonic clock.
pub fn config(handover: &Handover, cores: u32, now: u64) -> Result<Config, Error> {
    let identity = match &handover.config.api.tls {
        Some(tls) => Identity::from_pem(&tls.certificate, &tls.key)
            .map_err(|error| Error::Identity(true, error))?,
        None => Identity::generate(&handover.instance, handover.wall_clock_at(now))
            .map_err(|error| Error::Identity(false, error))?,
    };
    Ok(Config {
        identity,
        facts: facts(handover, cores),
        token_keys: handover
            .config
            .api
            .token_keys
            .iter()
            .map(|key| TokenKey::new(key.0))
            .collect::<Vec<_>>(),
        function_keys: handover
            .config
            .functions
            .keys
            .iter()
            .map(|key| FunctionKey::new(key.0))
            .collect::<Vec<_>>(),
    })
}

/// The node document's facts: all but `boot_ms`, which is 0 until `ready`.
pub fn facts(handover: &Handover, cores: u32) -> NodeFacts {
    NodeFacts {
        version: String::from(VERSION),
        instance: handover.instance.clone(),
        cloud: handover.cloud,
        cores,
        memory_bytes: handover.memory_bytes,
        executors: vec![Executor {
            kind: ExecutorKind::Cpu,
            slots: cores,
        }],
        boot_ms: 0,
        health: NodeHealth::Ok,
    }
}

/// The server's clock: the core's monotonic clock, and the hand-over's wall clock carried
/// forward by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeClock {
    /// The wall clock, in Unix seconds, when `platform` read it.
    wall_clock: u64,
    /// The core's monotonic clock, in nanoseconds, then.
    read_at_ns: u64,
}

impl NodeClock {
    /// The clock the hand-over gives.
    pub fn new(handover: &Handover) -> NodeClock {
        NodeClock {
            wall_clock: handover.wall_clock,
            read_at_ns: handover.read_at_ns,
        }
    }

    /// The wall clock, in Unix seconds, when the core's monotonic clock reads `now`.
    pub fn unix_seconds_at(&self, now: u64) -> u64 {
        let since = now.saturating_sub(self.read_at_ns) / 1_000_000_000;
        self.wall_clock.saturating_add(since)
    }
}

impl Clock for NodeClock {
    fn monotonic_ms(&self) -> u64 {
        moruna_svc::now() / 1_000_000
    }

    fn unix_seconds(&self) -> u64 {
        self.unix_seconds_at(moruna_svc::now())
    }
}
