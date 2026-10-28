//! Channels (SDD 20, f.4): a pair of endpoints carrying messages of bytes and handles.
//!
//! Each endpoint has a queue of the messages sent to it, bounded in depth; a message is
//! bounded in bytes and handles. A handle in a message belongs to no process: it was
//! removed from the sender's table and is inserted into the reader's. When an endpoint
//! closes, the messages queued to it are dropped, which closes the handles they carry.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use moruna_abi::{Signals, limits};
use ostd::sync::SpinLock;

use crate::{object::Handle, signal::Signaller};

pub struct Message {
    pub bytes: Vec<u8>,
    pub handles: Vec<Handle>,
}

pub struct Inner {
    /// `queues[i]` holds the messages endpoint `i` will read.
    pub queues: [VecDeque<Message>; 2],
    pub open: [bool; 2],
}

pub struct Channel {
    pub inner: SpinLock<Inner>,
    ends: [Signaller; 2],
}

impl Channel {
    /// Recomputes both endpoints' signals from the state; call with the lock held.
    pub fn refresh(&self, inner: &Inner) {
        for side in 0..2 {
            let peer = 1 - side;
            let mut set = Signals::NONE;
            if !inner.queues[side].is_empty() {
                set = set | Signals::READABLE;
            }
            if inner.open[peer] && inner.queues[peer].len() < limits::CHANNEL_DEPTH {
                set = set | Signals::WRITABLE;
            }
            if !inner.open[peer] {
                set = set | Signals::PEER_CLOSED;
            }
            self.ends[side].update(Signals::ALL.difference(set), set);
        }
    }
}

/// One end of a channel. The end closes when its last handle is dropped.
pub struct Endpoint {
    pub channel: Arc<Channel>,
    pub side: usize,
}

impl Endpoint {
    pub fn signaller(&self) -> &Signaller {
        &self.channel.ends[self.side]
    }

    pub fn peer(&self) -> usize {
        1 - self.side
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let dropped;
        {
            let mut inner = self.channel.inner.lock();
            inner.open[self.side] = false;
            dropped = core::mem::take(&mut inner.queues[self.side]);
            self.channel.refresh(&inner);
        }
        // The queued messages and the handles they carry are dropped here, outside the
        // lock, because dropping a handle may close another channel's endpoint.
        drop(dropped);
    }
}

/// Creates a channel and returns its two endpoints.
pub fn create() -> (Arc<Endpoint>, Arc<Endpoint>) {
    let channel = Arc::new(Channel {
        inner: SpinLock::new(Inner {
            queues: [VecDeque::new(), VecDeque::new()],
            open: [true, true],
        }),
        ends: [
            Signaller::new(Signals::WRITABLE),
            Signaller::new(Signals::WRITABLE),
        ],
    });
    (
        Arc::new(Endpoint {
            channel: channel.clone(),
            side: 0,
        }),
        Arc::new(Endpoint { channel, side: 1 }),
    )
}
