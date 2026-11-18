//! When the host's interrupt reached the process for a network card's channel, for diagnosis
//! only (work item 1.11d): a build with the feature `trace-first-frames`, which a node's
//! normal image never turns on.
//!
//! netvsc's trace times each of its first frames out from the commit to the read of the
//! completion, with when it began to wait and when its wake channel woke it. Between the
//! host's completion and that wake lies this process: SINT 2's interrupt reaches it, it finds
//! the channel's event flag and writes the channel's wake channel. So for the first [`TIMED`]
//! times a network card's channel is signalled, the time it was found, on the node's clock
//! (netvsc says where its own clock starts on it). They are said [`PER_LINE`] at a time, as
//! each line fills, never one by one: a line costs the console's time, which on Azure is
//! what is measured.

use alloc::{boxed::Box, format, string::String, vec::Vec};

use moruna_vmbus::ChannelId;

/// How many signals of a network card's channel are timed.
pub const TIMED: usize = 256;

/// The times said on one line.
pub const PER_LINE: usize = 16;

/// The trace: the network cards' channels, and the times their signals reached the process.
pub struct Interrupts {
    /// Nanoseconds on the node's clock.
    clock: Box<dyn FnMut() -> u64>,
    /// The channels opened for network cards.
    followed: Vec<ChannelId>,
    /// The times not yet said.
    times: Vec<u64>,
    /// The times noted so far.
    noted: usize,
}

impl Interrupts {
    /// A trace whose times are `clock`'s, in nanoseconds: the node's clock.
    pub fn new(clock: Box<dyn FnMut() -> u64>) -> Interrupts {
        Interrupts {
            clock,
            followed: Vec::new(),
            times: Vec::new(),
            noted: 0,
        }
    }

    /// `channel` was opened for a network card: its signals are timed.
    pub(crate) fn follow(&mut self, channel: ChannelId) {
        self.followed.push(channel);
    }

    /// The host signalled `channel`: the time is noted if it is a network card's.
    pub(crate) fn signalled(&mut self, channel: ChannelId) {
        if self.noted >= TIMED || !self.followed.contains(&channel) {
            return;
        }
        self.noted += 1;
        self.times.push((self.clock)());
    }

    /// A line of [`PER_LINE`] times, once they are noted ([`TIMED`] is a whole number of
    /// lines).
    pub(crate) fn take(&mut self) -> Option<String> {
        if self.times.len() < PER_LINE {
            return None;
        }
        let first = self.noted - self.times.len() + 1;
        let times: Vec<String> = self
            .times
            .drain(..)
            .map(|ns| format!("{}.{:03}", ns / 1_000_000, ns / 1_000 % 1_000))
            .collect();
        Some(format!(
            "vmbus: trace: a network card's channel signalled, its signals {first} to {} of {TIMED}, at (ms on the node's clock): {}",
            self.noted,
            times.join(" ")
        ))
    }
}
