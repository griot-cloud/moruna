//! A card's connection as the network server keeps it (SDD 31): receives and one wait for
//! the link always outstanding on it, and the sends in flight. The server waits on the
//! connection beside its clients, and when it is readable takes what the card has
//! answered, without waiting.
//!
//! **Receives enough to take a full queue are kept outstanding** (work item 6.3af), so one
//! is always waiting at the card while the server takes another's frames: on a card of
//! `moruna.driver.net` 1.1, `net_protocol::receives(batch, max_frame)`, the card's queue
//! of its largest frames over the frames one answer carries (24 at MTU 1500); on a card of
//! 1.0, one. With one, the frames that came while its answer was on the way waited in the
//! card's queue behind it, and once it was full were dropped.
//!
//! **Read the channel to its end, file everything, then take each reply** (work item
//! 1.3c). Asking a client core for one reply reads, and files, every other answer it
//! meets first. Asked one at a time, an answer that comes after its own was asked for
//! and is read while another is looked for stays filed, untaken, with the channel empty:
//! nothing wakes the server for it again. A receive's answer left so held the card's
//! frames back from the stack until a send of its own made the channel readable, and for
//! good once the stack had nothing to send: the node stopped acknowledging and answered
//! nothing. Filing first leaves no answer behind: one that comes after the channel was
//! read to its end keeps it readable, and the next turn takes it. The rule's one
//! implementation is `ClientCore::file_all`.

use alloc::{collections::VecDeque, format, string::String, vec::Vec};

use moruna_svc::{CallError, ClientCore, Encoder, Wire, frame::Frame as Message};
use net_protocol::{Frame, Link, SEND_FRAMES_MAX, receives};

use crate::meter::CardCounts;

/// What one send to a card carries besides its frames: the frame header, the count,
/// each frame's length.
const SEND_OVERHEAD: usize = 64;

/// The connection a card link reads: a client core, or a test's stand-in for one.
pub trait Answers {
    /// Sends request `ordinal` with `body`, without waiting; its id.
    fn send(&mut self, ordinal: u16, body: Encoder) -> Result<u32, CallError<u32>>;
    /// Reads every answer there is and files it (`ClientCore::file_all`): how many.
    fn file_all(&mut self) -> Result<usize, CallError<u32>>;
    /// The answer to `txid` if it has been filed; `None` while it is still to come.
    fn take(&mut self, txid: u32) -> Option<Result<Message, CallError<u32>>>;
    /// The card's minor version of the protocol, from its welcome.
    fn minor(&self) -> u16;
}

impl Answers for ClientCore {
    fn send(&mut self, ordinal: u16, body: Encoder) -> Result<u32, CallError<u32>> {
        ClientCore::send(self, ordinal, body)
    }
    fn file_all(&mut self) -> Result<usize, CallError<u32>> {
        ClientCore::file_all(self)
    }
    fn take(&mut self, txid: u32) -> Option<Result<Message, CallError<u32>>> {
        self.take_reply(txid)
    }
    fn minor(&self) -> u16 {
        self.server_minor()
    }
}

/// What the card had answered: the frames it received, and its link if that changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Taken {
    pub frames: Vec<Vec<u8>>,
    pub link: Option<bool>,
}

/// One card's connection and the requests outstanding on it.
pub struct CardLink<C = ClientCore> {
    /// The card's name in the device manager's directory.
    pub service: String,
    pub core: C,
    /// The receives outstanding, the oldest first: the card answers them in this order.
    receives: VecDeque<u32>,
    /// The receives kept outstanding: enough to take a full queue of the card's largest
    /// frames on a card of 1.1 (`net_protocol::receives`), one on a card of 1.0.
    outstanding: usize,
    link: Option<u32>,
    generation: u32,
    /// The sends outstanding: each request's id, and the frames it carries.
    sends: Vec<(u32, u32)>,
    /// The most frames one receive asks for.
    batch: u32,
    /// Why the card can no longer be used.
    pub failed: Option<String>,
    /// What has moved on the link (work item 6.3ab, `crate::meter`).
    pub counts: CardCounts,
}

impl<C: Answers> CardLink<C> {
    /// A card's link over `core`, its link last seen at `generation`, its largest frame
    /// `max_frame` (from its `info`), each receive asking for at most `batch` frames;
    /// nothing asked yet.
    pub fn new(
        service: String,
        core: C,
        generation: u32,
        batch: u32,
        max_frame: u32,
    ) -> CardLink<C> {
        let outstanding = if core.minor() >= 1 {
            receives(batch, max_frame) as usize
        } else {
            1
        };
        CardLink {
            service,
            core,
            receives: VecDeque::new(),
            outstanding,
            link: None,
            generation,
            sends: Vec::new(),
            batch,
            failed: None,
            counts: CardCounts::default(),
        }
    }

    /// Sends outstanding.
    pub fn sends(&self) -> usize {
        self.sends.len()
    }

    /// Receives outstanding.
    pub fn receives(&self) -> usize {
        self.receives.len()
    }

    /// Takes every answer the card has sent, without waiting: the channel read to its end
    /// first, then each reply taken (see the module's rule), the receives' in the order
    /// they were asked; then asks again for what was answered.
    pub fn pump(&mut self) -> Result<Taken, String> {
        self.core
            .file_all()
            .map_err(|e| format!("the card's answers: {e:?}"))?;
        let mut taken = Taken::default();
        let mut k = 0;
        while k < self.receives.len() {
            let Some(answer) = self.core.take(self.receives[k]) else {
                k += 1;
                continue;
            };
            let mut f = answer.map_err(|e| format!("receive: {e:?}"))?;
            let mut d = f.decoder();
            let frames =
                Vec::<Frame>::decode(&mut d).map_err(|e| format!("a receive's frames: {e:?}"))?;
            d.finish()
                .map_err(|e| format!("a receive's frames: {e:?}"))?;
            self.counts.answers += 1;
            self.counts.frames_in += frames.len() as u64;
            self.counts.bytes_in += frames.iter().map(|f| f.0.len() as u64).sum::<u64>();
            taken.frames.extend(frames.into_iter().map(|f| f.0));
            self.receives.remove(k);
        }
        if let Some(t) = self.link
            && let Some(answer) = self.core.take(t)
        {
            let mut f = answer.map_err(|e| format!("link_changed: {e:?}"))?;
            let mut d = f.decoder();
            let link = Link::decode(&mut d).map_err(|e| format!("a link: {e:?}"))?;
            d.finish().map_err(|e| format!("a link trailer: {e:?}"))?;
            self.generation = link.generation;
            taken.link = Some(link.up);
            self.link = None;
        }
        let mut k = 0;
        while k < self.sends.len() {
            let (id, frames) = self.sends[k];
            let Some(answer) = self.core.take(id) else {
                k += 1;
                continue;
            };
            // moruna.driver.net's `send` answers the frames' count once every one is on
            // the device's ring, and refuses a batch whole.
            let mut f = answer.map_err(|e| format!("send: {e:?}"))?;
            let mut d = f.decoder();
            let count = u32::decode(&mut d).map_err(|e| format!("send answer: {e:?}"))?;
            d.finish()
                .map_err(|e| format!("send answer trailer: {e:?}"))?;
            if count != frames {
                return Err(format!("a send of {frames} frames was answered {count}"));
            }
            self.sends.swap_remove(k);
        }
        self.ask()?;
        Ok(taken)
    }

    /// Keeps its receives and one wait for the link outstanding.
    pub fn ask(&mut self) -> Result<(), String> {
        while self.receives.len() < self.outstanding {
            let mut e = Encoder::new();
            self.batch.encode(&mut e).map_err(|e| format!("{e:?}"))?;
            let id = self
                .core
                .send(6, e)
                .map_err(|e| format!("receive: {e:?}"))?;
            self.receives.push_back(id);
        }
        if self.link.is_none() {
            let mut e = Encoder::new();
            self.generation
                .encode(&mut e)
                .map_err(|e| format!("{e:?}"))?;
            self.link = Some(
                self.core
                    .send(3, e)
                    .map_err(|e| format!("link_changed: {e:?}"))?,
            );
        }
        Ok(())
    }

    /// Sends the frames in `tx` (the stack's for this card), in batches that fit a
    /// message, while fewer than `in_flight` sends are outstanding.
    pub fn flush(&mut self, tx: &mut Vec<Vec<u8>>, in_flight: usize) -> Result<(), String> {
        loop {
            if self.sends.len() >= in_flight {
                return Ok(());
            }
            let (mut n, mut bytes) = (0, SEND_OVERHEAD);
            while n < tx.len()
                && n < SEND_FRAMES_MAX
                && bytes + 4 + tx[n].len() <= moruna_svc::limits::CHANNEL_MAX_BYTES
            {
                bytes += 4 + tx[n].len();
                n += 1;
            }
            if n == 0 {
                return Ok(());
            }
            let batch: Vec<Frame> = tx.drain(..n).map(Frame).collect();
            let frames = batch.len() as u32;
            let mut e = Encoder::new();
            batch.encode(&mut e).map_err(|e| format!("{e:?}"))?;
            let id = self.core.send(4, e).map_err(|e| format!("send: {e:?}"))?;
            self.counts.sends += 1;
            self.counts.frames_out += u64::from(frames);
            self.counts.bytes_out += (bytes - SEND_OVERHEAD - 4 * n) as u64;
            self.sends.push((id, frames));
        }
    }
}

#[cfg(test)]
mod tests;
