//! The server side (SDD 23, f.2): handlers, and the dispatch loop that serves them.
//!
//! A [`Dispatcher`] waits on every channel it holds at once and serves what arrives:
//! on a connection, the handshake and then requests, each answered by the connection's
//! [`Handler`] (one protocol per connection, any number of protocols per dispatcher);
//! on a listener, `Connect` frames, each of which brings a new connection's endpoint.
//! It returns when it holds no channel, so a server ends when its last client and
//! listener have gone.
//!
//! A connection that breaks the protocol (a frame of a kind a client may not send, a
//! request with no id, a second `Hello`) is closed. So is one whose client does not
//! read: a reply or event that finds the client's queue full closes the connection
//! rather than block the server, since one client must not stall the others.

use alloc::{boxed::Box, vec::Vec};

use moruna_abi::{DEADLINE_INFINITE, Error, ObjectType, Signals, limits};

use crate::{
    frame::{self, Frame, Kind, Transport},
    os::{Channel, WaitItem, wait},
    protocol::{Hello, ProtocolId, Refusal, Refused, Status},
    wire::{Decoder, Encoder, Wire},
};

/// What a handler answers a request with.
pub enum Outcome {
    /// The reply's body.
    Reply(Encoder),
    /// One of the protocol's error codes (1 to 65535).
    Failure(u32),
    /// One of the SDK's statuses.
    Status(Status),
}

impl Outcome {
    /// A reply carrying `value`; a value too large for a message is `ReplyTooLarge`.
    pub fn reply<T: Wire>(value: T) -> Outcome {
        let mut e = Encoder::new();
        match value.encode(&mut e) {
            Ok(()) => Outcome::Reply(e),
            Err(_) => Outcome::Status(Status::ReplyTooLarge),
        }
    }
}

/// Serves one protocol on one connection. `protocol!` generates one for every
/// protocol, over the protocol's server trait.
pub trait Handler {
    /// The protocol this handler serves; the handshake is checked against it.
    fn protocol(&self) -> ProtocolId;
    /// Answers request `ordinal`, whose arguments `body` holds.
    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, body: &mut Decoder<'_>) -> Outcome;
    /// The connection has closed; nothing more will arrive.
    fn closed(&mut self) {}
}

/// A connection's number within its dispatcher.
pub type ConnectionId = u64;

/// What a handler may do besides answering: know its connection, send events on it,
/// and close it after the reply.
pub struct Context<'a> {
    channel: &'a Channel,
    connection: ConnectionId,
    client_minor: u16,
    close: bool,
}

impl Context<'_> {
    pub fn connection(&self) -> ConnectionId {
        self.connection
    }

    /// The minor version the client said `Hello` with.
    pub fn client_minor(&self) -> u16 {
        self.client_minor
    }

    /// Sends event `ordinal`, which arrived in minor version `since`, with `body` on
    /// this connection, before the reply. An event newer than the client's minor is
    /// not sent (`Unsupported`). If the client's queue is full the connection closes
    /// after this request.
    pub fn emit(&mut self, ordinal: u16, since: u16, body: Encoder) -> Result<(), Transport> {
        if since > self.client_minor {
            return Err(Transport::Unsupported);
        }
        let sent = frame::send(self.channel, Kind::Event, ordinal, 0, body);
        if sent.is_err() {
            self.close = true;
        }
        sent
    }

    /// Closes the connection once the current request is answered.
    pub fn close(&mut self) {
        self.close = true;
    }
}

/// Makes a handler for each new connection a listener brings.
pub type Factory = Box<dyn FnMut() -> Box<dyn Handler>>;

enum Slot {
    Connection {
        channel: Channel,
        handler: Box<dyn Handler>,
        id: ConnectionId,
        /// The client's minor version, once its `Hello` is welcomed.
        welcomed: Option<u16>,
    },
    Listener {
        channel: Channel,
        factory: Factory,
    },
}

impl Slot {
    fn channel(&self) -> &Channel {
        match self {
            Slot::Connection { channel, .. } | Slot::Listener { channel, .. } => channel,
        }
    }
}

/// The most frames read from one channel per turn, so a busy client cannot keep the
/// others waiting.
const FRAMES_PER_TURN: usize = 16;

/// What a turn of the loop found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// It served what was ready.
    Served,
    /// The deadline passed with nothing ready.
    TimedOut,
    /// The dispatcher holds no channel.
    Empty,
}

/// The dispatch loop: at most `limits::WAIT_MAX_ITEMS` channels, connections and
/// listeners together, waited on at once.
#[derive(Default)]
pub struct Dispatcher {
    slots: Vec<Slot>,
    next_id: ConnectionId,
}

impl Dispatcher {
    pub fn new() -> Dispatcher {
        Dispatcher::default()
    }

    /// The channels held: connections and listeners.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn room(&self) -> Result<(), Error> {
        if self.slots.len() >= limits::WAIT_MAX_ITEMS {
            return Err(Error::Limit);
        }
        Ok(())
    }

    /// Serves `handler` on `channel`, whose client has yet to say `Hello`. `Limit` when
    /// the dispatcher already holds as many channels as one wait takes.
    pub fn serve(
        &mut self,
        channel: Channel,
        handler: Box<dyn Handler>,
    ) -> Result<ConnectionId, Error> {
        self.room()?;
        let id = self.next_id;
        self.next_id += 1;
        self.slots.push(Slot::Connection {
            channel,
            handler,
            id,
            welcomed: None,
        });
        Ok(id)
    }

    /// Accepts connections arriving on `channel`, serving each with a handler from
    /// `factory`.
    pub fn listen(&mut self, channel: Channel, factory: Factory) -> Result<(), Error> {
        self.room()?;
        self.slots.push(Slot::Listener { channel, factory });
        Ok(())
    }

    /// Serves until no channel is left.
    pub fn run(&mut self) -> Result<(), Error> {
        while self.turn(DEADLINE_INFINITE)? != Turn::Empty {}
        Ok(())
    }

    /// Waits once, until something is ready or `deadline`, and serves what is ready.
    pub fn turn(&mut self, deadline: u64) -> Result<Turn, Error> {
        if self.slots.is_empty() {
            return Ok(Turn::Empty);
        }
        let observed: Vec<Signals> = {
            let mut items: Vec<WaitItem> = self
                .slots
                .iter()
                .map(|s| WaitItem::new(&s.channel().0, Signals::READABLE | Signals::PEER_CLOSED))
                .collect();
            match wait(&mut items, deadline) {
                Ok(_) => {}
                Err(Error::TimedOut) => return Ok(Turn::TimedOut),
                Err(e) => return Err(e),
            }
            items.iter().map(WaitItem::observed).collect()
        };
        let mut keep = Vec::with_capacity(self.slots.len());
        let mut arrived = Vec::new();
        for (slot, seen) in core::mem::take(&mut self.slots).into_iter().zip(observed) {
            let kept = if seen.intersects(Signals::READABLE | Signals::PEER_CLOSED) {
                self.serve_slot(slot, &mut arrived)
            } else {
                Some(slot)
            };
            keep.extend(kept);
        }
        self.slots = keep;
        for (channel, handler) in arrived {
            // A connection that arrives with no room left is closed: its client sees
            // the peer close instead of a welcome.
            let _ = self.serve(channel, handler);
        }
        Ok(Turn::Served)
    }

    /// Serves the frames ready on one slot; `None` once the slot is to be closed.
    fn serve_slot(
        &mut self,
        slot: Slot,
        arrived: &mut Vec<(Channel, Box<dyn Handler>)>,
    ) -> Option<Slot> {
        match slot {
            Slot::Listener {
                channel,
                mut factory,
            } => {
                for _ in 0..FRAMES_PER_TURN {
                    match frame::recv(&channel) {
                        Ok(None) => break,
                        Ok(Some(f)) => arrived.push((accept(f)?, factory())),
                        Err(_) => return None,
                    }
                }
                Some(Slot::Listener { channel, factory })
            }
            Slot::Connection {
                channel,
                mut handler,
                id,
                mut welcomed,
            } => {
                for _ in 0..FRAMES_PER_TURN {
                    let open = match (frame::recv(&channel), welcomed) {
                        (Ok(None), _) => break,
                        (Ok(Some(f)), None) => {
                            welcomed = handshake(&channel, handler.protocol(), f);
                            welcomed.is_some()
                        }
                        (Ok(Some(f)), Some(minor)) => {
                            request(&channel, id, minor, handler.as_mut(), f)
                        }
                        (Err(_), _) => false,
                    };
                    if !open {
                        handler.closed();
                        return None;
                    }
                }
                Some(Slot::Connection {
                    channel,
                    handler,
                    id,
                    welcomed,
                })
            }
        }
    }
}

/// A listener's frame: `Connect`, an empty body and one channel endpoint.
fn accept(mut f: Frame) -> Option<Channel> {
    if f.kind != Kind::Connect
        || f.txid != 0
        || f.ordinal != 0
        || f.body_len() != 0
        || f.handle_count() != 1
    {
        return None;
    }
    let mut d = f.decoder();
    // A `Connect` frame has no body to name its handle: it is the only one.
    let endpoint = d.take_slot(0)?;
    match endpoint.info() {
        Ok((ObjectType::Channel, _)) => Some(Channel(endpoint)),
        _ => None,
    }
}

/// Answers the first frame of a connection; returns the client's minor version if the
/// connection stays open.
fn handshake(channel: &Channel, id: ProtocolId, mut f: Frame) -> Option<u16> {
    let verdict = if f.kind == Kind::Hello && f.txid == 0 && f.ordinal == 0 && f.handle_count() == 0
    {
        let mut d = f.decoder();
        match Hello::decode(&mut d) {
            Ok(hello) if d.finish().is_ok() => id.accepts(&hello).map(|()| hello.minor),
            _ => Err(Refusal::NotHello),
        }
    } else {
        Err(Refusal::NotHello)
    };
    let mut body = Encoder::new();
    match verdict {
        Ok(minor) => {
            let encoded = id
                .major
                .encode(&mut body)
                .and_then(|()| id.minor.encode(&mut body));
            (encoded.is_ok() && frame::send(channel, Kind::Welcome, 0, 0, body).is_ok())
                .then_some(minor)
        }
        Err(reason) => {
            let refused = Refused {
                reason,
                server: id.hello(),
            };
            if refused.encode(&mut body).is_ok() {
                // The connection closes whether or not the refusal could be queued.
                let _ = frame::send(channel, Kind::Refuse, 0, 0, body);
            }
            None
        }
    }
}

/// Answers one request; returns whether the connection stays open.
fn request(
    channel: &Channel,
    connection: ConnectionId,
    client_minor: u16,
    handler: &mut dyn Handler,
    mut f: Frame,
) -> bool {
    if f.kind != Kind::Request || f.txid == 0 {
        return false;
    }
    let (ordinal, txid) = (f.ordinal, f.txid);
    let mut cx = Context {
        channel,
        connection,
        client_minor,
        close: false,
    };
    let outcome = {
        let mut d = f.decoder();
        handler.handle(&mut cx, ordinal, &mut d)
    };
    // Whatever the handler did not take of the request's handles closes here.
    drop(f);
    let sent = match outcome {
        Outcome::Reply(body) => match frame::send(channel, Kind::Reply, 0, txid, body) {
            Err(Transport::TooLarge) => failure(channel, txid, Status::ReplyTooLarge as u32),
            other => other,
        },
        Outcome::Failure(code) => failure(channel, txid, code),
        Outcome::Status(status) => failure(channel, txid, status as u32),
    };
    sent.is_ok() && !cx.close
}

fn failure(channel: &Channel, txid: u32, code: u32) -> Result<(), Transport> {
    let mut body = Encoder::new();
    body.put_raw(&code.to_le_bytes());
    frame::send(channel, Kind::Failure, 0, txid, body)
}
