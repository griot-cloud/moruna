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
//!
//! A driver waits on its device as well as its clients: [`Dispatcher::watch`] adds any
//! waitable object it owns (an interrupt, a timer) to the same `wait`, and is called
//! when it signals. A request the device answers later is deferred: the handler takes a
//! [`Pending`] from [`Context::defer`] and returns [`Outcome::Deferred`], and whoever
//! holds the `Pending` (the watch callback, another request, or the program between
//! turns) completes it with the answer.

use alloc::{
    boxed::Box,
    collections::{BTreeSet, VecDeque},
    vec::Vec,
};

use moruna_abi::{DEADLINE_INFINITE, Error, ObjectType, Signals, limits};

use crate::{
    frame::{self, Frame, Kind, Transport},
    os::{Channel, Handle, WaitItem, wait},
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
    /// The answer comes later, through the [`Pending`] the handler took with
    /// [`Context::defer`].
    Deferred,
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

/// A request whose answer was deferred: the connection and the request's id. The
/// dispatcher takes one answer for it and refuses a second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    connection: ConnectionId,
    txid: u32,
}

impl Pending {
    pub fn connection(&self) -> ConnectionId {
        self.connection
    }
}

/// Answers to deferred requests, sent by the dispatcher at the end of the turn.
#[derive(Default)]
pub struct Completions(VecDeque<(Pending, Outcome)>);

impl Completions {
    /// Answers `pending` with `outcome` (not `Deferred`).
    pub fn complete(&mut self, pending: Pending, outcome: Outcome) {
        self.0.push_back((pending, outcome));
    }
}

/// What a handler may do besides answering: know its connection, send events on it,
/// defer its answer, answer other deferred requests, and close the connection after
/// the reply.
pub struct Context<'a> {
    channel: &'a Channel,
    connection: ConnectionId,
    client_minor: u16,
    close: bool,
    txid: u32,
    deferred: bool,
    completions: &'a mut Completions,
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

    /// Defers the answer to this request: the handler returns `Outcome::Deferred`, and
    /// the answer is whatever the returned `Pending` is later completed with.
    pub fn defer(&mut self) -> Pending {
        self.deferred = true;
        Pending {
            connection: self.connection,
            txid: self.txid,
        }
    }

    /// Answers another deferred request, at the end of this turn.
    pub fn complete(&mut self, pending: Pending, outcome: Outcome) {
        self.completions.complete(pending, outcome);
    }
}

/// Something a dispatcher waits on besides its channels: a driver's interrupt, a timer.
/// It owns its handle, so it can acknowledge what it is told.
pub trait Watch {
    /// The handle waited on.
    fn handle(&self) -> &Handle;
    /// The signals waited for.
    fn signals(&self) -> Signals;
    /// One of the signals is raised (`seen` is what the wait observed). Returns
    /// whether to keep watching; the object is dropped when it does not. A signal that
    /// stays raised (an interrupt not acknowledged) calls this again every turn.
    fn signaled(&mut self, seen: Signals, done: &mut Completions) -> bool;
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
        /// The ids of the requests whose answers are deferred.
        deferred: BTreeSet<u32>,
    },
    Listener {
        channel: Channel,
        factory: Factory,
    },
    Watch(Box<dyn Watch>),
}

impl Slot {
    fn waited(&self) -> (&Handle, Signals) {
        match self {
            Slot::Connection { channel, .. } | Slot::Listener { channel, .. } => {
                (&channel.0, Signals::READABLE | Signals::PEER_CLOSED)
            }
            Slot::Watch(w) => (w.handle(), w.signals()),
        }
    }

    fn ready(&self, seen: Signals) -> bool {
        match self {
            Slot::Connection { .. } | Slot::Listener { .. } => {
                seen.intersects(Signals::READABLE | Signals::PEER_CLOSED)
            }
            Slot::Watch(w) => seen.intersects(w.signals()),
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

/// The dispatch loop: at most `limits::WAIT_MAX_ITEMS` handles (connections,
/// listeners and watched handles together), waited on at once.
#[derive(Default)]
pub struct Dispatcher {
    slots: Vec<Slot>,
    next_id: ConnectionId,
    completions: Completions,
}

impl Dispatcher {
    pub fn new() -> Dispatcher {
        Dispatcher::default()
    }

    /// The handles held: connections, listeners and watched handles.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The channels held, connections and listeners, leaving out watched handles: a
    /// server that watches objects for as long as it serves (a supervisor its
    /// children) ends when this is zero.
    pub fn channels(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| !matches!(s, Slot::Watch(_)))
            .count()
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
            deferred: BTreeSet::new(),
        });
        Ok(id)
    }

    /// Waits on `watch`'s handle in the same `wait` as the channels, calling it
    /// whenever one of its signals is raised.
    pub fn watch(&mut self, watch: Box<dyn Watch>) -> Result<(), Error> {
        self.room()?;
        self.slots.push(Slot::Watch(watch));
        Ok(())
    }

    /// Answers a deferred request now. `PeerClosed` when its connection has gone (the
    /// answer and any handles in it are dropped); `Malformed` when the request was not
    /// deferred or was already answered.
    pub fn complete(&mut self, pending: Pending, outcome: Outcome) -> Result<(), Transport> {
        let mut result = Err(Transport::PeerClosed);
        let mut closed = None;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Slot::Connection {
                channel,
                id,
                deferred,
                handler,
                ..
            } = slot
            else {
                continue;
            };
            if *id != pending.connection {
                continue;
            }
            if matches!(outcome, Outcome::Deferred) || !deferred.remove(&pending.txid) {
                return Err(Transport::Malformed);
            }
            result = answer(channel, pending.txid, outcome);
            if result.is_err() {
                handler.closed();
                closed = Some(index);
            }
            break;
        }
        if let Some(index) = closed {
            self.slots.remove(index);
        }
        result
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
                .map(|s| {
                    let (handle, signals) = s.waited();
                    WaitItem::new(handle, signals)
                })
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
            let kept = if slot.ready(seen) {
                self.serve_slot(slot, seen, &mut arrived)
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
        // Answers deferred this turn and completed this turn. One whose connection has
        // gone is dropped with it, which `complete` reports to a caller between turns.
        while let Some((pending, outcome)) = self.completions.0.pop_front() {
            let _ = self.complete(pending, outcome);
        }
        Ok(Turn::Served)
    }

    /// Serves the frames ready on one slot; `None` once the slot is to be closed.
    fn serve_slot(
        &mut self,
        slot: Slot,
        seen: Signals,
        arrived: &mut Vec<(Channel, Box<dyn Handler>)>,
    ) -> Option<Slot> {
        match slot {
            Slot::Watch(mut w) => w
                .signaled(seen, &mut self.completions)
                .then_some(Slot::Watch(w)),
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
                mut deferred,
            } => {
                for _ in 0..FRAMES_PER_TURN {
                    let open = match (frame::recv(&channel), welcomed) {
                        (Ok(None), _) => break,
                        (Ok(Some(f)), None) => {
                            welcomed = handshake(&channel, handler.protocol(), f);
                            welcomed.is_some()
                        }
                        (Ok(Some(f)), Some(minor)) => {
                            let cx = Context {
                                channel: &channel,
                                connection: id,
                                client_minor: minor,
                                close: false,
                                txid: f.txid,
                                deferred: false,
                                completions: &mut self.completions,
                            };
                            request(cx, handler.as_mut(), f, &mut deferred)
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
                    deferred,
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
    mut cx: Context<'_>,
    handler: &mut dyn Handler,
    mut f: Frame,
    deferred: &mut BTreeSet<u32>,
) -> bool {
    // A request id already deferred is one the client reused while it is in flight.
    if f.kind != Kind::Request || f.txid == 0 || deferred.contains(&f.txid) {
        return false;
    }
    let (ordinal, txid) = (f.ordinal, f.txid);
    let outcome = {
        let mut d = f.decoder();
        handler.handle(&mut cx, ordinal, &mut d)
    };
    // Whatever the handler did not take of the request's handles closes here.
    drop(f);
    let sent = match (outcome, cx.deferred) {
        (Outcome::Deferred, true) => {
            deferred.insert(txid);
            Ok(())
        }
        // A handler that defers and answers, or answers `Deferred` without deferring,
        // has broken its own contract: the connection closes rather than leave the
        // client with two answers or none.
        (Outcome::Deferred, false) | (_, true) => Err(Transport::Malformed),
        (outcome, false) => answer(cx.channel, txid, outcome),
    };
    sent.is_ok() && !cx.close
}

/// Sends the answer to request `txid`.
fn answer(channel: &Channel, txid: u32, outcome: Outcome) -> Result<(), Transport> {
    match outcome {
        Outcome::Reply(body) => match frame::send(channel, Kind::Reply, 0, txid, body) {
            Err(Transport::TooLarge) => failure(channel, txid, Status::ReplyTooLarge as u32),
            other => other,
        },
        Outcome::Failure(code) => failure(channel, txid, code),
        Outcome::Status(status) => failure(channel, txid, status as u32),
        Outcome::Deferred => Err(Transport::Malformed),
    }
}

fn failure(channel: &Channel, txid: u32, code: u32) -> Result<(), Transport> {
    let mut body = Encoder::new();
    body.put_raw(&code.to_le_bytes());
    frame::send(channel, Kind::Failure, 0, txid, body)
}
