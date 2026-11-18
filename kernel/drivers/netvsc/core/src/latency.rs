//! The first frames out, each timed from its commit to the read of its completion, for
//! diagnosis only (work item 1.11d): part of the trace a build with the feature
//! `trace-first-frames` keeps, which a node's normal image never turns on.
//!
//! On Azure, 1.11c's trace read send completions 10 to 26 ms after their commits. Between the
//! two, the node's netvsc writes the trace's lines on the console, and the core writes a line
//! to the serial port one byte at a time, with the CPU's interrupts off. So for the first
//! [`TIMED`] frames out, every frame and not only those the trace says, four times are noted:
//! the commit that hands the frame to the host; when netvsc last began to wait for the host
//! before the completion was read; when the host's signal last woke netvsc before that; and
//! the read. Beside `vmbus`'s times of the host's interrupt for the channel, they show where
//! the time went: to the console before the wait, to the host, or to the wake.
//!
//! Nothing is said until every timed frame's completion has been read, and then a line a
//! frame: a line said at once would add the console time it measures.

use alloc::{format, string::String, vec::Vec};

/// How many frames out are timed: the first, of every kind.
pub const TIMED: u32 = 32;

/// One timed frame: its times on the trace's clock, in nanoseconds.
struct Timed {
    /// Its number among the frames out, from 1.
    n: u32,
    transaction: u64,
    bytes: usize,
    /// When it was committed, and whether the host was signalled then.
    committed: Option<(u64, bool)>,
    /// When netvsc last began to wait, after the commit and before the read.
    waited: Option<u64>,
    /// When the host's signal last woke netvsc, after the commit and before the read.
    woken: Option<u64>,
    /// When its completion was read.
    read: Option<u64>,
}

/// The timings of the first [`TIMED`] frames out.
#[derive(Default)]
pub(crate) struct Latency {
    frames: Vec<Timed>,
    /// When netvsc last began to wait for the host.
    waited: Option<u64>,
    /// When the host's signal last woke netvsc.
    woken: Option<u64>,
    /// Whether the lines have been given.
    said: bool,
}

impl Latency {
    /// A frame of `bytes` written to the ring under `transaction`, not yet committed.
    pub(crate) fn sent(&mut self, transaction: u64, bytes: usize) {
        let n = self.frames.len() as u32 + 1;
        if n > TIMED {
            return;
        }
        self.frames.push(Timed {
            n,
            transaction,
            bytes,
            committed: None,
            waited: None,
            woken: None,
            read: None,
        });
    }

    /// The ring committed at `at`: every frame written since the last commit is the host's.
    pub(crate) fn committed(&mut self, at: u64, signalled: bool) {
        for frame in self.frames.iter_mut().filter(|f| f.committed.is_none()) {
            frame.committed = Some((at, signalled));
        }
    }

    /// netvsc begins to wait for the host, at `at`.
    pub(crate) fn waiting(&mut self, at: u64) {
        self.waited = Some(at);
    }

    /// The host's signal woke netvsc, at `at`.
    pub(crate) fn woken(&mut self, at: u64) {
        self.woken = Some(at);
    }

    /// The completion of the frame sent under `transaction` read at `at`. A section's
    /// transaction is used again once it is complete, so it is the frame of that transaction
    /// still in flight.
    pub(crate) fn completed(&mut self, at: u64, transaction: u64) {
        let (waited, woken) = (self.waited, self.woken);
        let Some(frame) = self
            .frames
            .iter_mut()
            .find(|f| f.transaction == transaction && f.committed.is_some() && f.read.is_none())
        else {
            return;
        };
        let since = frame.committed.map_or(0, |(at, _)| at);
        frame.waited = waited.filter(|&w| w >= since);
        frame.woken = woken.filter(|&w| w >= since);
        frame.read = Some(at);
    }

    /// The lines, once: when every timed frame's completion has been read.
    pub(crate) fn finished(&mut self) -> Option<Vec<String>> {
        if self.said
            || self.frames.len() < TIMED as usize
            || self.frames.iter().any(|f| f.read.is_none())
        {
            return None;
        }
        self.said = true;
        Some(self.frames.iter().map(line).collect())
    }
}

/// Nanoseconds as milliseconds to the microsecond.
fn ms(ns: u64) -> String {
    format!("{}.{:03}", ns / 1_000_000, ns / 1_000 % 1_000)
}

/// A timed frame, as the console says it.
fn line(frame: &Timed) -> String {
    let (committed, signalled) = frame.committed.unwrap_or_default();
    let read = frame.read.unwrap_or_default();
    let signal = if signalled { "signalled" } else { "no signal" };
    let waited = match frame.waited {
        Some(at) => format!("waited from {}", ms(at)),
        None => String::from("no wait"),
    };
    let woken = match frame.woken {
        Some(at) => format!("woken {}", ms(at)),
        None => String::from("not woken"),
    };
    format!(
        "frame out {}, {} bytes, timed: committed {} ({signal}); {waited}; {woken}; completion read {}, {} ms after the commit",
        frame.n,
        frame.bytes,
        ms(committed),
        ms(read),
        ms(read.saturating_sub(committed))
    )
}
