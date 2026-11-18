//! What `platform` needs of the machine, as traits: outbound connections as byte streams, the
//! core's monotonic clock, the node's wall clock, and the channel to `api`. The logic is written
//! against these, so every rule is tested on a host; [`crate::svc`] gives the node's own (and the
//! program, `src/main.rs`, the wall clock).

use alloc::{string::String, vec::Vec};
use core::{fmt, net::Ipv4Addr};

/// Why a connection was not made, or broke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// The peer refused the connection.
    Refused,
    /// The connection was not established in time, as the network server judges it.
    TimedOut,
    /// No interface reaches the address: the node has no address yet.
    Unreachable,
    /// The peer reset the connection.
    Reset,
    /// The caller's deadline passed first.
    Deadline,
    /// Anything else, in words.
    Other(String),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Refused => f.write_str("connection refused"),
            StreamError::TimedOut => f.write_str("connection timed out"),
            StreamError::Unreachable => f.write_str("no route to the address"),
            StreamError::Reset => f.write_str("connection reset"),
            StreamError::Deadline => f.write_str("the deadline passed"),
            StreamError::Other(why) => f.write_str(why),
        }
    }
}

/// One TCP connection, as bytes. Dropping it closes it.
pub trait Stream {
    /// Sends some of `bytes`, waiting for room until `deadline` (the monotonic clock, in
    /// nanoseconds): how many were taken, at least one.
    fn write(&mut self, bytes: &[u8], deadline: u64) -> Result<usize, StreamError>;

    /// Receives into `buffer`, waiting for bytes until `deadline`: how many arrived, and 0 once
    /// the peer has finished sending.
    fn read(&mut self, buffer: &mut [u8], deadline: u64) -> Result<usize, StreamError>;
}

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

    /// The node's own address on the interface `wan`.
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
