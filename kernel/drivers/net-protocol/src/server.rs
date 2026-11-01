//! The protocol's server side every network driver runs (SDD 29, f.2): the seven
//! requests answered over a [`Hub`], sends queued in order until their frames are on
//! the card's rings, and the answers they and the hub produce, gathered for the
//! driver's loop to send. A driver supplies only its card, as a [`Ring`].

use alloc::{collections::VecDeque, string::String, vec::Vec};

use moruna_svc::{Context, Decoder, Outcome, Pending, ProtocolError, Status, Wire};

use crate::{Answer, Frame, Hub, Info, check, net};

/// A card's transmit side.
pub trait Ring {
    /// Puts `frame` on a transmit ring; `Ok(false)` when the ring it belongs on is
    /// full, so the frame waits.
    fn put(&mut self, frame: &[u8]) -> Result<bool, String>;
    /// Tells the card of the frames put since the last flush.
    fn flush(&mut self) -> Result<(), String>;
}

/// A send whose frames are not all on a ring yet.
struct Sending {
    frames: VecDeque<Vec<u8>>,
    count: u32,
    pending: Pending,
}

pub struct Server {
    pub info: Info,
    pub hub: Hub<Pending>,
    sends: VecDeque<Sending>,
    /// Answers ready to send: the driver's loop sends them after each turn.
    pub ready: Vec<(Pending, Outcome)>,
    /// Why the card can no longer be used; the driver ends with it, and sends are
    /// refused `Io` meanwhile.
    pub failed: Option<String>,
}

impl Server {
    pub fn new(info: Info, link_up: bool, speed_mbps: u32) -> Server {
        Server {
            info,
            hub: Hub::new(link_up, speed_mbps),
            sends: VecDeque::new(),
            ready: Vec::new(),
            failed: None,
        }
    }

    /// Sends waiting for room.
    pub fn waiting(&self) -> usize {
        self.sends.len()
    }

    /// Puts waiting frames on the card, in order, until one finds its ring full;
    /// answers each send whose frames are all on a ring.
    pub fn transmit(&mut self, ring: &mut dyn Ring) -> Result<(), String> {
        let mut put = false;
        let result = loop {
            let Some(sending) = self.sends.front_mut() else {
                break Ok(());
            };
            let Some(frame) = sending.frames.front() else {
                if let Some(done) = self.sends.pop_front() {
                    self.ready.push((done.pending, Outcome::reply(done.count)));
                }
                continue;
            };
            match ring.put(frame) {
                Ok(true) => {
                    put = true;
                    self.hub.tx_frames += 1;
                    self.hub.tx_bytes += frame.len() as u64;
                    sending.frames.pop_front();
                }
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        if put {
            ring.flush()?;
        }
        result
    }

    /// Answers the receives the frames just delivered to the hub satisfy.
    pub fn delivered(&mut self) {
        let mut answers = Vec::new();
        self.hub.answers(&mut answers);
        for (pending, frames) in answers {
            self.ready.push((pending, Outcome::reply(frames)));
        }
    }

    /// Records the link the card reports, answering those waiting for a change.
    pub fn link(&mut self, up: bool, speed_mbps: u32) {
        let mut changed = Vec::new();
        self.hub.set_link(up, speed_mbps, &mut changed);
        for (pending, link) in changed {
            self.ready.push((pending, Outcome::reply(link)));
        }
    }

    /// Answers request `ordinal` of the connection `cx` is for.
    pub fn handle(
        &mut self,
        ring: &mut dyn Ring,
        cx: &mut Context<'_>,
        ordinal: u16,
        d: &mut Decoder<'_>,
    ) -> Outcome {
        let conn = cx.connection();
        let bad = Outcome::Status(Status::BadMessage);
        match ordinal {
            1 | 2 | 5 | 7 if d.finish().is_err() => bad,
            1 => Outcome::reply(self.info),
            2 => Outcome::reply(self.hub.link()),
            3 => {
                let Ok(generation) = u32::decode(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                match self.hub.link_changed(conn, generation, || cx.defer()) {
                    Answer::Now(link) => Outcome::reply(link),
                    Answer::Later => Outcome::Deferred,
                }
            }
            4 => {
                let Ok(frames) = Vec::<Frame>::decode(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                if let Err(e) = check(&frames, self.info.max_frame) {
                    return Outcome::Failure(e.code());
                }
                if self.failed.is_some() {
                    return Outcome::Failure(net::Error::Io.code());
                }
                self.sends.push_back(Sending {
                    count: frames.len() as u32,
                    frames: frames.into_iter().map(|f| f.0).collect(),
                    pending: cx.defer(),
                });
                if let Err(e) = self.transmit(ring) {
                    self.failed.get_or_insert(e);
                }
                Outcome::Deferred
            }
            5 => {
                self.hub.subscribe(conn);
                Outcome::reply(())
            }
            6 => {
                let Ok(max) = u32::decode(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                match self.hub.receive(conn, max, || cx.defer()) {
                    Ok(Answer::Now(frames)) => Outcome::reply(frames),
                    Ok(Answer::Later) => Outcome::Deferred,
                    Err(e) => Outcome::Failure(e.code()),
                }
            }
            7 => Outcome::reply(self.hub.stats(conn)),
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }

    /// Connection `conn` has closed.
    pub fn closed(&mut self, conn: u64) {
        self.hub.closed(conn);
    }
}
