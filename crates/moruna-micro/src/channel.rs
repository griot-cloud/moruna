//! The host protocol on a channel of the Moruna kernel (system design 3A): the engine's end of
//! the channel `node-admission` starts it with, granted under a slot (`moruna serve --listen
//! channel:<slot>`), whose other end is `api`'s.
//!
//! The protocol is a byte stream, and a channel carries messages: each message is the next
//! piece of the stream, at most [`MESSAGE_BYTES`], with no handle, and the stream ends when the
//! peer closes its end. A message is sent whole or not at all; while the peer's queue is full
//! the sender waits for room, at most [`WRITE_TIMEOUT`], as a write to a socket does on Linux.
//!
//! One thread reads and another writes: a read waits on the channel and on a doorbell of its
//! own, which [`Endpoint::close`] rings, so closing returns a reader that is waiting.

use std::io;
use std::sync::Mutex;
use std::time::Duration;

use moruna_svc::{Channel, DEADLINE_INFINITE, Error, Signals, WaitItem};

/// The largest message a channel carries (`moruna_abi::limits::CHANNEL_MAX_BYTES`).
pub const MESSAGE_BYTES: usize = 64 * 1024;
/// How long a send may wait for room before the peer is treated as gone, as on Linux (MH 4.3).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The engine's end of a channel granted to it.
pub struct Endpoint {
    channel: Channel,
    /// Rung by [`Endpoint::close`]; a reader waits on `bell.1` beside the channel.
    bell: (Channel, Channel),
    /// Sends one at a time, so that two writers' messages never interleave in one wait.
    sending: Mutex<()>,
}

fn kernel(what: &str, error: Error) -> io::Error {
    let kind = match error {
        Error::PeerClosed => io::ErrorKind::BrokenPipe,
        Error::TimedOut => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{what}: the channel: {}", error.name()))
}

impl Endpoint {
    /// The channel granted under `slot`.
    pub fn open(slot: &str) -> io::Result<Endpoint> {
        let handle = crate::start::take(slot).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "the engine was granted no `{slot}` channel (it holds {:?})",
                    crate::start::slots()
                ),
            )
        })?;
        let bell = Channel::create().map_err(|e| kernel("channel_create", e))?;
        Ok(Endpoint {
            channel: Channel(handle),
            bell,
            sending: Mutex::new(()),
        })
    }

    /// Sends `bytes`, at most [`MESSAGE_BYTES`], as one message.
    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let _one = self.sending.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = moruna_svc::deadline_in(WRITE_TIMEOUT.as_nanos() as u64);
        loop {
            match self.channel.write_with(bytes, &mut []) {
                Ok(()) => return Ok(()),
                Err(Error::ShouldWait) => {
                    let awaited = Signals::WRITABLE | Signals::PEER_CLOSED;
                    let seen = moruna_svc::wait_one(&self.channel.0, awaited, deadline)
                        .map_err(|e| kernel("waiting for room", e))?;
                    if seen.contains(Signals::PEER_CLOSED) && !seen.contains(Signals::WRITABLE) {
                        return Err(kernel("send", Error::PeerClosed));
                    }
                }
                Err(e) => return Err(kernel("send", e)),
            }
        }
    }

    /// Receives the next message's bytes into `buf`, at least [`MESSAGE_BYTES`] long, waiting
    /// for one; `Ok(0)` once the peer has closed and every message is read, or this end is
    /// closed. An empty message carries nothing and is passed over.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.channel.read(buf, &mut []) {
                Ok((0, _)) => continue,
                Ok((count, _)) => return Ok(count),
                Err(Error::PeerClosed) => return Ok(0),
                Err(Error::ShouldWait) => {}
                Err(e) => return Err(kernel("recv", e)),
            }
            let awaited = Signals::READABLE | Signals::PEER_CLOSED;
            let mut items = [
                WaitItem::new(&self.channel.0, awaited),
                WaitItem::new(&self.bell.1.0, awaited),
            ];
            moruna_svc::wait(&mut items, DEADLINE_INFINITE)
                .map_err(|e| kernel("waiting for a message", e))?;
            // What a wait observes is every signal the handle has, a writable bell among them:
            // the bell has rung only when it is readable (or its other end is gone).
            if items[1].observed().intersects(awaited) {
                return Ok(0);
            }
            let seen = items[0].observed();
            if seen.contains(Signals::PEER_CLOSED) && !seen.contains(Signals::READABLE) {
                return Ok(0);
            }
        }
    }

    /// Rings the doorbell: a reader waiting here returns the end of the stream. The channel
    /// itself closes when the endpoint is dropped.
    pub fn close(&self) {
        let _ = self.bell.0.write_with(&[1], &mut []);
    }
}
