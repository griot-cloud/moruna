//! The receive side of a network driver (SDD 29, f.2): which connections have
//! subscribed, each one's bounded queue of frames, the receive each may have waiting,
//! the link and the connections waiting for it to change, and the card's counters.
//!
//! A driver hands every frame its device delivers to [`Hub::deliver`], which queues a
//! copy for each subscribed connection (dropping and counting it for a connection
//! whose queue is full), and then takes [`Hub::answers`]: each waiting receive,
//! answered with what its connection's queue holds. It is generic over the deferred
//! answer `P` (a dispatcher's `Pending`), so it is tested on a host as it runs.

use alloc::{collections::BTreeMap, collections::VecDeque, vec::Vec};

use crate::{Frame, Link, QUEUE_BYTES, QUEUE_FRAMES, RECEIVE_BYTES, Stats, net};

/// An answer now, or deferred until the hub answers it.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer<T> {
    Now(T),
    Later,
}

struct Inbox<P> {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    dropped: u64,
    waiting: Option<(P, u32)>,
}

pub struct Hub<P> {
    inboxes: BTreeMap<u64, Inbox<P>>,
    link: Link,
    link_waiters: Vec<(u64, P)>,
    pub tx_frames: u64,
    pub tx_bytes: u64,
    pub rx_frames: u64,
    pub rx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
}

impl<P> Hub<P> {
    /// A hub whose link is `up` at `speed_mbps`, generation 1.
    pub fn new(up: bool, speed_mbps: u32) -> Hub<P> {
        Hub {
            inboxes: BTreeMap::new(),
            link: Link {
                up,
                speed_mbps,
                generation: 1,
            },
            link_waiters: Vec::new(),
            tx_frames: 0,
            tx_bytes: 0,
            rx_frames: 0,
            rx_bytes: 0,
            rx_errors: 0,
            tx_errors: 0,
        }
    }

    pub fn link(&self) -> Link {
        self.link
    }

    /// Connections subscribed.
    pub fn subscribers(&self) -> usize {
        self.inboxes.len()
    }

    /// Queues frames for `connection` from now on; subscribing again changes nothing.
    pub fn subscribe(&mut self, connection: u64) {
        self.inboxes.entry(connection).or_insert(Inbox {
            frames: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            waiting: None,
        });
    }

    /// At most `max` of `connection`'s frames now, or later (taking `defer()`) when
    /// it has none.
    pub fn receive(
        &mut self,
        connection: u64,
        max: u32,
        defer: impl FnOnce() -> P,
    ) -> Result<Answer<Vec<Frame>>, net::Error> {
        let inbox = self
            .inboxes
            .get_mut(&connection)
            .ok_or(net::Error::NotSubscribed)?;
        if inbox.waiting.is_some() {
            return Err(net::Error::InUse);
        }
        let max = max.clamp(1, QUEUE_FRAMES as u32);
        if inbox.frames.is_empty() {
            inbox.waiting = Some((defer(), max));
            return Ok(Answer::Later);
        }
        Ok(Answer::Now(take(inbox, max)))
    }

    /// Queues a copy of `frame` for every subscribed connection.
    pub fn deliver(&mut self, frame: &[u8]) {
        self.rx_frames += 1;
        self.rx_bytes += frame.len() as u64;
        for inbox in self.inboxes.values_mut() {
            if inbox.frames.len() >= QUEUE_FRAMES || inbox.bytes + frame.len() > QUEUE_BYTES {
                inbox.dropped += 1;
                continue;
            }
            inbox.bytes += frame.len();
            inbox.frames.push_back(frame.to_vec());
        }
    }

    /// Answers every waiting receive whose connection has frames.
    pub fn answers(&mut self, out: &mut Vec<(P, Vec<Frame>)>) {
        for inbox in self.inboxes.values_mut() {
            if inbox.frames.is_empty() {
                continue;
            }
            if let Some((pending, max)) = inbox.waiting.take() {
                out.push((pending, take(inbox, max)));
            }
        }
    }

    /// The link now, or later (taking `defer()`) if its generation is `generation`.
    pub fn link_changed(
        &mut self,
        connection: u64,
        generation: u32,
        defer: impl FnOnce() -> P,
    ) -> Answer<Link> {
        if generation != self.link.generation {
            return Answer::Now(self.link);
        }
        self.link_waiters.push((connection, defer()));
        Answer::Later
    }

    /// Records the link the device reports; if it changed, its generation moves on and
    /// every connection waiting for a change is answered.
    pub fn set_link(&mut self, up: bool, speed_mbps: u32, out: &mut Vec<(P, Link)>) {
        if up == self.link.up && speed_mbps == self.link.speed_mbps {
            return;
        }
        self.link = Link {
            up,
            speed_mbps,
            generation: self.link.generation.wrapping_add(1).max(1),
        };
        for (_, pending) in self.link_waiters.drain(..) {
            out.push((pending, self.link));
        }
    }

    /// The counters, and `connection`'s drops.
    pub fn stats(&self, connection: u64) -> Stats {
        Stats {
            tx_frames: self.tx_frames,
            tx_bytes: self.tx_bytes,
            rx_frames: self.rx_frames,
            rx_bytes: self.rx_bytes,
            rx_dropped: self.inboxes.get(&connection).map_or(0, |i| i.dropped),
            rx_errors: self.rx_errors,
            tx_errors: self.tx_errors,
        }
    }

    /// `connection` has closed: its queue, its waiting receive and its link waits go.
    pub fn closed(&mut self, connection: u64) {
        self.inboxes.remove(&connection);
        self.link_waiters.retain(|(c, _)| *c != connection);
    }
}

/// Up to `max` frames from the front of `inbox`, within one reply.
fn take<P>(inbox: &mut Inbox<P>, max: u32) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut bytes = 0;
    while out.len() < max as usize {
        let Some(front) = inbox.frames.front() else {
            break;
        };
        let cost = 4 + front.len();
        if !out.is_empty() && bytes + cost > RECEIVE_BYTES {
            break;
        }
        bytes += cost;
        let frame = inbox.frames.pop_front().unwrap_or_default();
        inbox.bytes -= frame.len();
        out.push(Frame(frame));
    }
    out
}
