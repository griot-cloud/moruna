//! What `platform` needs of the machine, as traits: outbound connections as byte streams, the
//! core's monotonic clock, the node's wall clock, the channel to `api`, and the image
//! filesystem's word that it has mounted the engine's layer. The logic is written
//! against these, so every rule is tested on a host; [`crate::svc`] gives the node's own (and the
//! program, `src/main.rs`, the wall clock).

use alloc::{string::String, vec::Vec};
use core::{fmt, net::Ipv4Addr};

pub use moruna_node_http::{Stream, StreamError};

/// The node's network, as far as `platform` uses it.
pub trait Network {
    /// A connection it makes.
    type Stream<'a>: Stream
    where
        Self: 'a;

    /// Connects to `address` on `port`, waiting until `deadline`.
    fn connect(
        &mut self,
        address: Ipv4Addr,
        port: u16,
        deadline: u64,
    ) -> Result<Self::Stream<'_>, StreamError>;

    /// The node's own address on the interface `wan`: at once when it holds one, and
    /// otherwise as soon as it takes one, waiting for it until `deadline` (work item 1.11f).
    fn address(&mut self, deadline: u64) -> Result<Ipv4Addr, StreamError>;
}

/// The core's monotonic clock.
pub trait Clock {
    /// Nanoseconds on the clock now.
    fn now(&self) -> u64;

    /// Returns once the clock reads `at` or later.
    fn sleep_until(&mut self, at: u64);
}

/// The node's one wall clock, the core's (system design 3D), which `platform` alone may set:
/// `clock_set_realtime` on `root.clock`.
pub trait WallClock {
    /// The wall clock reads `utc_ns`, nanoseconds since 1970 (UTC), now; or why the core refused.
    fn set(&mut self, utc_ns: u64) -> Result<(), String>;
}

/// Why a message did not cross the link to `api`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// `api` closed its end.
    Closed,
    /// The deadline passed first.
    TimedOut,
    /// Anything else, in words.
    Other(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Closed => f.write_str("api closed its end of the link"),
            LinkError::TimedOut => f.write_str("the deadline passed"),
            LinkError::Other(why) => f.write_str(why),
        }
    }
}

/// A channel between `platform` and one start of `api`, as whole messages.
pub trait Link {
    /// Sends one message, waiting for room until `deadline`.
    fn send(&mut self, message: &[u8], deadline: u64) -> Result<(), LinkError>;

    /// The next message, waiting for one until `deadline`.
    fn receive(&mut self, deadline: u64) -> Result<Vec<u8>, LinkError>;
}

/// No deadline: a wait that ends only with what it waits for.
pub const FOREVER: u64 = u64::MAX;

/// The supervisor's channel to `platform` after the node's memory (PM, item 2.2c): at each
/// start of `api`, the first included, the supervisor sends the channel to that start.
pub trait Control {
    /// The channel to one start of `api`.
    type Api: Link;

    /// The channel to the next start of `api`, waiting for it until `deadline` ([`FOREVER`]
    /// for no deadline).
    fn next_api(&mut self, deadline: u64) -> Result<Self::Api, LinkError>;
}

/// The image filesystem, as `platform` hears from it (system design 1B; item 1.6e): the node is
/// ready only once `imagefs` has mounted the engine's layer from `system`.
pub trait EngineLayer {
    /// Returns once `imagefs` has mounted the engine's layer, waiting for it until `deadline`:
    /// [`LinkError::Closed`] if `imagefs` ended first, [`LinkError::TimedOut`] if the deadline
    /// passed first.
    fn mounted(&mut self, deadline: u64) -> Result<(), LinkError>;
}
