//! The network server's side of the process (work items 1.3 and 2.2b): the listener on
//! `wan`'s leased address at 443, which follows the lease, and the accepted sockets as the
//! API server's transport.
//!
//! Every byte moves by `moruna.net`'s requests on the process's one connection to the
//! network server, sent without waiting for their answers, so one thread serves every
//! connection:
//!
//! - an `accept` is kept outstanding on the listener; each answer is a new connection;
//! - each connection keeps one `receive_into` outstanding into memory of its own, and the
//!   bytes it brings wait for the server to read them;
//! - what the server writes is staged, and goes in one `send_from` at a time from memory of
//!   its own;
//! - a connection the server closes is closed once everything staged has gone.
//!
//! [`Network::take`] takes every answer that has come, [`Network::flush`] sends what is to
//! send, and [`Network::wait`] waits for the next answer. A connection's memory is charged
//! to the process's account, which the connection to the network server is bound to.

use alloc::{
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    vec,
    vec::Vec,
};
use core::cell::RefCell;

use moruna_api_server::{Transport, TransportError};
use moruna_svc::{CallError, Encoder, Memory, Rights, Signals, Wire, frame::Frame, map, wait_one};
use netstack_protocol::{Endpoint, Opened, net};

use crate::{ADDRESS_POLL_NS, BACKLOG, Error, Event, LEASE_POLL_NS, PORT, address, asked};

/// `moruna.net`'s requests the process sends without waiting for their answers.
const ACCEPT: u16 = 4;
const SEND_FROM: u16 = 5;
const RECEIVE_INTO: u16 = 6;
const CLOSE: u16 = 8;

/// The most bytes one receive brings: the size of each connection's receive memory.
pub const RECEIVE_BYTES: u32 = 16 * 1024;
/// The most bytes one send carries: the size of each connection's send memory.
pub const SEND_BYTES: u32 = 16 * 1024;
/// Bytes received that the server has not read, past which no more are asked for.
pub const RECEIVED_HOLD: usize = 64 * 1024;
/// Bytes the server has written that have not gone, past which a write waits.
pub const STAGED_HOLD: usize = 64 * 1024;

/// What an answer the process waits for answers.
#[derive(Clone, Copy, Debug)]
enum Request {
    /// An accept on this listener socket.
    Accept(u32),
    /// A receive on this socket.
    Receive(u32),
    /// A send on this socket.
    Send(u32),
    /// The close of this socket.
    Close(u32),
}

/// One accepted connection.
struct Stream {
    /// Bytes received that the server has not read.
    received: VecDeque<u8>,
    /// The client has finished sending.
    ended: bool,
    /// A receive is outstanding.
    receiving: bool,
    receive_memory: Memory,
    /// Bytes the server has written that have not gone.
    staged: VecDeque<u8>,
    /// A send is outstanding.
    sending: bool,
    send_memory: Memory,
    /// The network server refused a receive or a send on it: it is reset or gone.
    broken: bool,
    /// The server closed it: it closes once everything staged has gone.
    closing: bool,
    /// Its close has been sent.
    close_sent: bool,
}

/// The process's connection to the network server: its listener and its connections.
pub struct Network {
    client: net::Client,
    /// The listener's socket, and where it listens.
    listener: Option<(u32, Endpoint)>,
    /// An accept is outstanding on the listener.
    accepting: bool,
    /// When the lease is looked at next, on the core's monotonic clock.
    look_at: u64,
    /// The requests sent and not yet answered, by their ids.
    requests: BTreeMap<u32, Request>,
    streams: BTreeMap<u32, Stream>,
}

impl Network {
    /// The process's side, on its connection to the network server; no listener yet.
    pub fn new(client: net::Client) -> Network {
        Network {
            client,
            listener: None,
            accepting: false,
            look_at: 0,
            requests: BTreeMap::new(),
            streams: BTreeMap::new(),
        }
    }

    /// Where the listener listens, if one is open.
    pub fn listening(&self) -> Option<Endpoint> {
        self.listener.map(|(_, at)| at)
    }

    /// The connections open, the closing ones included.
    pub fn connections(&self) -> usize {
        self.streams.len()
    }

    /// When the lease is looked at next, on the core's monotonic clock.
    pub fn look_at(&self) -> u64 {
        self.look_at
    }

    /// Looks at `wan`'s lease, when it is time to at `now`: opens the listener on its
    /// address, or closes it when the address is no longer the listener's.
    pub fn tend(&mut self, now: u64) -> Result<Option<Event>, Error> {
        if now < self.look_at {
            return Ok(None);
        }
        let found = address(&asked("info", self.client.info())?)?;
        match (self.listener, found) {
            (None, None) => {
                self.look_at = now.saturating_add(ADDRESS_POLL_NS);
                Ok(None)
            }
            (None, Some(found)) => {
                let (socket, at) = asked(
                    "listen",
                    self.client.listen(Endpoint::new(found, PORT), BACKLOG),
                )?;
                self.listener = Some((socket, at));
                self.look_at = now.saturating_add(LEASE_POLL_NS);
                Ok(Some(Event::Listening(at)))
            }
            (Some((_, at)), Some(found)) if found == at.addr => {
                self.look_at = now.saturating_add(LEASE_POLL_NS);
                Ok(None)
            }
            (Some((socket, at)), _) => {
                // Its accept is answered `Closed`, which `take` takes. Connections it
                // accepted stay.
                asked("close", self.client.close(socket))?;
                self.listener = None;
                self.accepting = false;
                // The next address may be there already.
                self.look_at = now;
                Ok(Some(Event::Unlistened(at)))
            }
        }
    }

    /// Takes every answer that has come, without waiting: the sockets accepted since, each
    /// a new connection for the server.
    pub fn take(&mut self) -> Result<Vec<u32>, Error> {
        let mut accepted = Vec::new();
        let ids: Vec<u32> = self.requests.keys().copied().collect();
        for id in ids {
            let answer = match self.client.core().reply(id, 0) {
                Err(CallError::TimedOut) => continue,
                answer => answer.map_err(CallError::typed::<net::Error>),
            };
            let Some(request) = self.requests.remove(&id) else {
                continue;
            };
            match request {
                Request::Accept(listener) => {
                    let current = self.listener.is_some_and(|(socket, _)| socket == listener);
                    if current {
                        self.accepting = false;
                    }
                    match answer {
                        Ok(frame) => {
                            let opened: Opened = decoded(frame, "accept")?;
                            if self.open(opened.socket) {
                                accepted.push(opened.socket);
                            } else {
                                // No memory for it: it is closed, and the server never
                                // sees it.
                                let id = self.send(CLOSE, opened.socket, "close")?;
                                self.requests.insert(id, Request::Close(opened.socket));
                            }
                        }
                        // The listener has closed: its lease moved.
                        Err(CallError::Remote(net::Error::Closed)) if !current => {}
                        Err(error) => return Err(Error::Network("accept", error)),
                    }
                }
                Request::Receive(socket) => {
                    let answer = match answer {
                        Ok(frame) => Ok(decoded::<u32>(frame, "receive")?),
                        Err(CallError::Remote(refused)) => Err(refused),
                        Err(error) => return Err(Error::Network("receive", error)),
                    };
                    if let Some(stream) = self.streams.get_mut(&socket) {
                        stream.receiving = false;
                        match answer {
                            Ok(0) => stream.ended = true,
                            Ok(count) => {
                                let count = count.min(RECEIVE_BYTES) as usize;
                                let mut bytes = vec![0; count];
                                stream
                                    .receive_memory
                                    .map(0, u64::from(RECEIVE_BYTES), map::READ)
                                    .and_then(|m| m.read(0, &mut bytes))
                                    .map_err(|e| Error::Kernel("reading what came", e))?;
                                stream.received.extend(bytes);
                            }
                            Err(_) => stream.broken = true,
                        }
                    }
                }
                Request::Send(socket) => {
                    let answer = match answer {
                        Ok(frame) => Ok(decoded::<u32>(frame, "send")?),
                        Err(CallError::Remote(refused)) => Err(refused),
                        Err(error) => return Err(Error::Network("send", error)),
                    };
                    if let Some(stream) = self.streams.get_mut(&socket) {
                        stream.sending = false;
                        match answer {
                            Ok(taken) => {
                                let taken = (taken as usize).min(stream.staged.len());
                                stream.staged.drain(..taken);
                            }
                            Err(_) => stream.broken = true,
                        }
                    }
                }
                Request::Close(socket) => match answer {
                    Ok(_) | Err(CallError::Remote(_)) => {
                        self.streams.remove(&socket);
                    }
                    Err(error) => return Err(Error::Network("close", error)),
                },
            }
        }
        Ok(accepted)
    }

    /// A new connection, with memory of its own: whether there was memory for it.
    fn open(&mut self, socket: u32) -> bool {
        let account = &self.client.account().0;
        let (Ok(receive_memory), Ok(send_memory)) = (
            Memory::create_in(u64::from(RECEIVE_BYTES), account),
            Memory::create_in(u64::from(SEND_BYTES), account),
        ) else {
            return false;
        };
        self.streams.insert(
            socket,
            Stream {
                received: VecDeque::new(),
                ended: false,
                receiving: false,
                receive_memory,
                staged: VecDeque::new(),
                sending: false,
                send_memory,
                broken: false,
                closing: false,
                close_sent: false,
            },
        );
        true
    }

    /// Sends what is to send: an accept when none is outstanding, and on each connection
    /// a receive when there is room for what it brings, a send of what is staged, or its
    /// close once everything staged has gone.
    pub fn flush(&mut self) -> Result<(), Error> {
        if let Some((listener, _)) = self.listener
            && !self.accepting
        {
            let id = self.send(ACCEPT, listener, "accept")?;
            self.requests.insert(id, Request::Accept(listener));
            self.accepting = true;
        }
        let sockets: Vec<u32> = self.streams.keys().copied().collect();
        for socket in sockets {
            let mut requests = Vec::new();
            {
                let stream = self.streams.get_mut(&socket).expect("a socket's stream");
                if stream.close_sent {
                    continue;
                }
                if stream.closing
                    && (stream.broken || (stream.staged.is_empty() && !stream.sending))
                {
                    requests.push(Request::Close(socket));
                }
                if !stream.closing
                    && !stream.receiving
                    && !stream.ended
                    && !stream.broken
                    && stream.received.len() < RECEIVED_HOLD
                {
                    requests.push(Request::Receive(socket));
                }
                if !stream.sending && !stream.broken && !stream.staged.is_empty() {
                    requests.push(Request::Send(socket));
                }
            }
            for request in requests {
                self.request(request)?;
            }
        }
        Ok(())
    }

    /// Sends one of a connection's requests.
    fn request(&mut self, request: Request) -> Result<(), Error> {
        let id = match request {
            Request::Close(socket) => {
                let id = self.send(CLOSE, socket, "close")?;
                self.streams
                    .get_mut(&socket)
                    .expect("a socket's stream")
                    .close_sent = true;
                id
            }
            Request::Receive(socket) => {
                let stream = self.streams.get(&socket).expect("a socket's stream");
                let loan = loan(
                    &stream.receive_memory,
                    Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER,
                )?;
                let id = self.send(RECEIVE_INTO, (socket, loan, 0u64, RECEIVE_BYTES), "receive")?;
                self.streams
                    .get_mut(&socket)
                    .expect("a socket's stream")
                    .receiving = true;
                id
            }
            Request::Send(socket) => {
                let stream = self.streams.get_mut(&socket).expect("a socket's stream");
                let count = stream.staged.len().min(SEND_BYTES as usize);
                let bytes: Vec<u8> = stream.staged.iter().take(count).copied().collect();
                stream
                    .send_memory
                    .map(0, u64::from(SEND_BYTES), map::WRITE)
                    .and_then(|m| m.write(0, &bytes))
                    .map_err(|e| Error::Kernel("staging what goes", e))?;
                let loan = loan(
                    &stream.send_memory,
                    Rights::READ | Rights::MAP | Rights::TRANSFER,
                )?;
                stream.sending = true;
                self.send(SEND_FROM, (socket, loan, 0u64, count as u32), "send")?
            }
            Request::Accept(_) => unreachable!("an accept is sent by flush itself"),
        };
        self.requests.insert(id, request);
        Ok(())
    }

    /// Sends request `ordinal` with `args`, without waiting: its id.
    fn send(&mut self, ordinal: u16, args: impl Wire, what: &'static str) -> Result<u32, Error> {
        let mut body = Encoder::new();
        args.encode(&mut body)
            .map_err(|_| Error::Network(what, CallError::Malformed))?;
        asked(
            what,
            self.client
                .core()
                .send(ordinal, body)
                .map_err(CallError::typed),
        )
    }

    /// Waits until an answer comes, or the network server goes, or `deadline`, on the
    /// core's monotonic clock, passes.
    pub fn wait(&mut self, deadline: u64) -> Result<(), Error> {
        let channel = &self.client.core().channel().0;
        match wait_one(channel, Signals::READABLE | Signals::PEER_CLOSED, deadline) {
            Ok(_) | Err(moruna_svc::Error::TimedOut) => Ok(()),
            Err(e) => Err(Error::Kernel("waiting for the network server", e)),
        }
    }
}

/// A handle to `memory` with `rights`, lent to the network server for one request.
fn loan(memory: &Memory, rights: Rights) -> Result<Memory, Error> {
    memory
        .0
        .duplicate(rights)
        .map(Memory)
        .map_err(|e| Error::Kernel("lending a connection's memory", e))
}

/// An answer's value.
fn decoded<T: Wire>(mut frame: Frame, what: &'static str) -> Result<T, Error> {
    let mut decoder = frame.decoder();
    T::decode(&mut decoder)
        .and_then(|value| decoder.finish().map(|()| value))
        .map_err(|_| Error::Network(what, CallError::Malformed))
}

/// One accepted connection, as the API server's transport.
pub struct Conn {
    network: Rc<RefCell<Network>>,
    socket: u32,
}

impl Conn {
    /// The connection on `socket`, which [`Network::take`] accepted.
    pub fn new(network: Rc<RefCell<Network>>, socket: u32) -> Conn {
        Conn { network, socket }
    }
}

impl Transport for Conn {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let mut network = self.network.borrow_mut();
        let Some(stream) = network.streams.get_mut(&self.socket) else {
            return Err(TransportError::Broken);
        };
        if !stream.received.is_empty() {
            let count = buf.len().min(stream.received.len());
            for (to, from) in buf.iter_mut().zip(stream.received.drain(..count)) {
                *to = from;
            }
            return Ok(count);
        }
        if stream.broken {
            Err(TransportError::Broken)
        } else if stream.ended {
            Ok(0)
        } else {
            Err(TransportError::WouldBlock)
        }
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, TransportError> {
        let mut network = self.network.borrow_mut();
        let Some(stream) = network.streams.get_mut(&self.socket) else {
            return Err(TransportError::Broken);
        };
        if stream.broken || stream.closing {
            return Err(TransportError::Broken);
        }
        let room = STAGED_HOLD.saturating_sub(stream.staged.len());
        if room == 0 && !buf.is_empty() {
            return Err(TransportError::WouldBlock);
        }
        let count = buf.len().min(room);
        stream.staged.extend(&buf[..count]);
        Ok(count)
    }

    fn close(&mut self) {
        if let Some(stream) = self.network.borrow_mut().streams.get_mut(&self.socket) {
            stream.closing = true;
        }
    }
}
