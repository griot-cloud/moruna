//! The receive side of a network driver (SDD 29, f.2): which connections have
//! subscribed, each one's bounded queue of frames, the receives each has waiting, the
//! link and the connections waiting for it to change, and the card's counters.
//!
//! A driver hands every frame its device delivers to [`Hub::deliver`], which queues a
//! copy for each subscribed connection (dropping and counting it for a connection
//! whose queue is full), and then takes [`Hub::answers`]: each connection's waiting
//! receives, answered in the order they came with what its queue holds. A client of 1.1
//! may keep receives waiting until together they take a full queue
//! ([`crate::receives`], work item 6.3af); a client of 1.0 keeps one. It is generic
//! over the deferred answer `P` (a dispatcher's `Pending`), so it is tested on a host
//! as it runs.

use alloc::{collections::BTreeMap, collections::VecDeque, vec::Vec};

use crate::{
    Frame, Link, QUEUE_BYTES, QUEUE_FRAMES, RECEIVE_BYTES, Stats, answer_frames, net, queue_frames,
};

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
    /// The receives waiting, the oldest first: each one's answer and its `max`.
    waiting: VecDeque<(P, u32)>,
    /// Whether the client says 1.1 or later, and may keep several receives waiting.
    several: bool,
}

pub struct Hub<P> {
    inboxes: BTreeMap<u64, Inbox<P>>,
    /// The card's largest frame, which bounds the receives a connection keeps waiting.
    max_frame: u32,
    link: Link,
    link_waiters: Vec<(u64, P)>,
    pub tx_frames: u64,
    pub tx_bytes: u64,
    pub rx_frames: u64,
    pub rx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    /// Frames dropped for a connection whose queue was full, every connection's.
    pub rx_dropped: u64,
    /// Receives answered with frames, and the frames they carried (work item 6.3ab).
    pub answered: u64,
    pub answered_frames: u64,
    /// Receives answered that left frames queued once every receive waiting was answered:
    /// their connection takes the frames slower than the card has them (work item 6.3af).
    pub backlog: u64,
}

impl<P> Hub<P> {
    /// A hub of a card whose largest frame is `max_frame`, its link `up` at `speed_mbps`,
    /// generation 1.
    pub fn new(up: bool, speed_mbps: u32, max_frame: u32) -> Hub<P> {
        Hub {
            inboxes: BTreeMap::new(),
            max_frame,
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
            rx_dropped: 0,
            answered: 0,
            answered_frames: 0,
            backlog: 0,
        }
    }

    pub fn link(&self) -> Link {
        self.link
    }

    /// Connections subscribed.
    pub fn subscribers(&self) -> usize {
        self.inboxes.len()
    }

    /// Queues frames for `connection`, a client of minor version `minor`, from now on;
    /// subscribing again changes nothing.
    pub fn subscribe(&mut self, connection: u64, minor: u16) {
        self.inboxes.entry(connection).or_insert(Inbox {
            frames: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            waiting: VecDeque::new(),
            several: minor >= 1,
        });
    }

    /// At most `max` of `connection`'s frames now, or later (taking `defer()`) when it
    /// has none or receives wait before this one. A client of 1.1 may keep receives
    /// waiting while together they take less than a full queue; a client of 1.0, while
    /// none waits. Past that, `InUse`.
    pub fn receive(
        &mut self,
        connection: u64,
        max: u32,
        defer: impl FnOnce() -> P,
    ) -> Result<Answer<Vec<Frame>>, net::Error> {
        let max_frame = self.max_frame;
        let inbox = self
            .inboxes
            .get_mut(&connection)
            .ok_or(net::Error::NotSubscribed)?;
        let max = max.clamp(1, QUEUE_FRAMES as u32);
        if inbox.waiting.is_empty() && !inbox.frames.is_empty() {
            let frames = take(inbox, max);
            self.answered += 1;
            self.answered_frames += frames.len() as u64;
            self.backlog += u64::from(!inbox.frames.is_empty());
            return Ok(Answer::Now(frames));
        }
        let room = if inbox.several {
            let taken: u32 = inbox
                .waiting
                .iter()
                .map(|(_, max)| answer_frames(*max, max_frame))
                .sum();
            taken < queue_frames(max_frame)
        } else {
            inbox.waiting.is_empty()
        };
        if !room {
            return Err(net::Error::InUse);
        }
        inbox.waiting.push_back((defer(), max));
        Ok(Answer::Later)
    }

    /// Queues a copy of `frame` for every subscribed connection.
    pub fn deliver(&mut self, frame: &[u8]) {
        self.rx_frames += 1;
        self.rx_bytes += frame.len() as u64;
        for inbox in self.inboxes.values_mut() {
            if inbox.frames.len() >= QUEUE_FRAMES || inbox.bytes + frame.len() > QUEUE_BYTES {
                inbox.dropped += 1;
                self.rx_dropped += 1;
                continue;
            }
            inbox.bytes += frame.len();
            inbox.frames.push_back(frame.to_vec());
        }
    }

    /// Answers each connection's waiting receives, the oldest first, while its queue
    /// has frames. An answer is backlog only if frames are still queued once every
    /// receive waiting has been answered.
    pub fn answers(&mut self, out: &mut Vec<(P, Vec<Frame>)>) {
        let (mut answered, mut frames, mut backlog) = (0, 0, 0);
        for inbox in self.inboxes.values_mut() {
            let mut any = false;
            while !inbox.frames.is_empty() {
                let Some((pending, max)) = inbox.waiting.pop_front() else {
                    break;
                };
                let taken = take(inbox, max);
                answered += 1;
                frames += taken.len() as u64;
                any = true;
                out.push((pending, taken));
            }
            backlog += u64::from(any && !inbox.frames.is_empty());
        }
        self.answered += answered;
        self.answered_frames += frames;
        self.backlog += backlog;
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

    /// `connection` has closed: its queue, its waiting receives and its link waits go.
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
