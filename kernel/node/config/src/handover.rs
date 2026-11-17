//! The hand-over (system design, section 1D, step 4; work item 1.4, step 6): what `platform` gives
//! `api` once node config is valid, as one JSON message on the channel the supervisor links
//! between them, and `api`'s answer.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use moruna_api::Cloud;

use crate::{Refusal, config::NodeConfig, json};

/// The most bytes a hand-over is: what one channel message carries
/// (`moruna_abi::limits::CHANNEL_MAX_BYTES`).
pub const MESSAGE_MAX: usize = 64 * 1024;

/// `api`'s answer to a hand-over, sent once its listener is open: one message of these bytes.
pub const READY: &[u8] = b"ready";

/// The longest instance ID.
pub const INSTANCE_MAX: usize = 64;

/// Whether `id` can be a node's instance ID: 1 to 64 ASCII letters, digits, `-`, `_` or `.`.
/// Both clouds give a UUID. The ID goes onto the console line, into token claims and into the
/// node's certificate, so nothing else is taken.
pub fn instance_valid(id: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
    !id.is_empty() && id.len() <= INSTANCE_MAX && id.bytes().all(allowed)
}

/// A cloud's name, as the console line, the node document and the hand-over write it.
pub fn cloud_name(cloud: Cloud) -> &'static str {
    match cloud {
        Cloud::OpenStack => "openstack",
        Cloud::Azure => "azure",
        Cloud::None => "none",
    }
}

/// What `platform` hands `api`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handover {
    /// The validated node config.
    pub config: NodeConfig,
    /// The cloud's ID for this machine.
    pub instance: String,
    /// The cloud `platform` found.
    pub cloud: Cloud,
    /// The wall clock, in Unix seconds, as `platform` read it.
    pub wall_clock: u64,
    /// The core's monotonic clock, in nanoseconds, when `wall_clock` was read.
    pub read_at_ns: u64,
    /// The node's memory: the machine account's limit, which every service and job is carved
    /// from, as the supervisor gave it to `platform`. The node document's `memory_bytes`.
    pub memory_bytes: u64,
}

/// Why a hand-over is not sent or not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoverError {
    /// It is this many bytes, more than one channel message carries.
    TooLarge(usize),
    /// It is not the hand-over's JSON.
    Shape(Refusal),
    /// Its cloud is none this node knows.
    Cloud,
    /// Its instance ID is not one (see [`instance_valid`]).
    Instance,
    /// Its node config is refused.
    Config(Refusal),
}

impl fmt::Display for HandoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandoverError::TooLarge(bytes) => write!(
                f,
                "the hand-over is {bytes} bytes, and a channel message carries {MESSAGE_MAX}"
            ),
            HandoverError::Shape(refusal) => write!(f, "the hand-over is refused: {refusal}"),
            HandoverError::Cloud => f.write_str("the hand-over's `cloud` is not a cloud"),
            HandoverError::Instance => {
                f.write_str("the hand-over's `instance` is not an instance ID")
            }
            HandoverError::Config(refusal) => {
                write!(f, "the hand-over's node config is refused: {refusal}")
            }
        }
    }
}

impl Handover {
    /// The message `platform` sends: the hand-over as JSON.
    pub fn encode(&self) -> Result<Vec<u8>, HandoverError> {
        let message = serde_json::json!({
            "cloud": cloud_name(self.cloud),
            "instance": self.instance,
            "wall_clock": self.wall_clock,
            "read_at_ns": self.read_at_ns,
            "memory_bytes": self.memory_bytes,
            "config": self.config.document(),
        })
        .to_string()
        .into_bytes();
        if message.len() > MESSAGE_MAX {
            return Err(HandoverError::TooLarge(message.len()));
        }
        Ok(message)
    }

    /// The hand-over a message holds, for `api`. The node config in it is validated again, on a
    /// node of the hand-over's own `memory_bytes`.
    pub fn decode(message: &[u8]) -> Result<Handover, HandoverError> {
        if message.len() > MESSAGE_MAX {
            return Err(HandoverError::TooLarge(message.len()));
        }
        let json = json::parse(message).map_err(HandoverError::Shape)?;
        let read = || -> Result<_, Refusal> {
            let fields = json.object("the hand-over")?;
            fields.only(&[
                "cloud",
                "instance",
                "wall_clock",
                "read_at_ns",
                "memory_bytes",
                "config",
            ])?;
            Ok((
                fields.need("cloud", "cloud")?.string("cloud")?,
                fields.need("instance", "instance")?.string("instance")?,
                fields
                    .need("wall_clock", "wall_clock")?
                    .unsigned("wall_clock")?,
                fields
                    .need("read_at_ns", "read_at_ns")?
                    .unsigned("read_at_ns")?,
                fields
                    .need("memory_bytes", "memory_bytes")?
                    .unsigned("memory_bytes")?,
                fields.need("config", "config")?,
            ))
        };
        let (cloud, instance, wall_clock, read_at_ns, memory_bytes, config) =
            read().map_err(HandoverError::Shape)?;
        let cloud = [Cloud::OpenStack, Cloud::Azure, Cloud::None]
            .into_iter()
            .find(|known| cloud_name(*known) == cloud)
            .ok_or(HandoverError::Cloud)?;
        if !instance_valid(instance) {
            return Err(HandoverError::Instance);
        }
        Ok(Handover {
            config: NodeConfig::read(config, memory_bytes).map_err(HandoverError::Config)?,
            instance: instance.to_string(),
            cloud,
            wall_clock,
            read_at_ns,
            memory_bytes,
        })
    }

    /// The wall clock, in Unix seconds, when the core's monotonic clock reads `monotonic_ns`:
    /// the clock `platform` read, carried forward by the monotonic time since.
    pub fn wall_clock_at(&self, monotonic_ns: u64) -> u64 {
        let since = monotonic_ns.saturating_sub(self.read_at_ns) / 1_000_000_000;
        self.wall_clock.saturating_add(since)
    }
}
