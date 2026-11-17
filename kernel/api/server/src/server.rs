//! The server: a state machine the caller drives. The caller accepts each connection and hands
//! it over, polls whenever a connection's stream has changed or a deadline has passed, and
//! supplies the time; the server reads, answers and writes through each connection's transport,
//! without threads or an async runtime.

use crate::{
    api::{Api, NodeFacts},
    http::Session,
    identity::Identity,
    tls::{self, Tls, WallClock},
    transport::{Clock, Transport, TransportError},
};
use alloc::{sync::Arc, vec, vec::Vec};
use rustls::ServerConfig;

/// The most connections served at once. A connection accepted beyond them has its first request
/// answered `busy`, then closes.
pub const CONNECTION_LIMIT: usize = 64;
/// How long a connection may move no bytes before it is closed, in milliseconds.
pub const IDLE_LIMIT_MS: u64 = 60_000;
/// The bytes read from a transport in one call.
const READ_CHUNK: usize = 16 * 1024;
/// The most rounds of reading, answering and writing one connection gets in one poll.
const ROUNDS: usize = 16;

/// What the server needs from node config and from the node.
#[derive(Clone, Debug)]
pub struct Config {
    /// The TLS identity: the operator's, or the node's own.
    pub identity: Identity,
    /// What the node document says.
    pub facts: NodeFacts,
}

/// The API server over connections of type `T`, with time from `C`.
pub struct Server<T: Transport, C: Clock> {
    tls: Arc<ServerConfig>,
    wall: Arc<WallClock>,
    clock: C,
    api: Api,
    connections: Vec<Connection<T>>,
    /// Where each read from a transport lands before TLS takes it.
    scratch: Vec<u8>,
}

/// One connection.
struct Connection<T> {
    transport: T,
    tls: Tls,
    session: Session,
    /// When bytes last moved, on the monotonic clock.
    active: u64,
    /// The client has closed its side of the stream.
    ended: bool,
}

/// Whether a connection stays open after a poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    Open,
    Close,
}

impl<T: Transport> Connection<T> {
    /// Reads, answers and writes until nothing moves, then says whether the connection stays.
    fn drive(&mut self, api: &mut Api, now_ms: u64, now: u64, scratch: &mut [u8]) -> Keep {
        for _ in 0..ROUNDS {
            let mut moved = false;
            while !self.ended && self.tls.room() > 0 && self.session.wants_input() {
                let room = self.tls.room().min(scratch.len());
                match self.transport.read(&mut scratch[..room]) {
                    Ok(0) => self.ended = true,
                    Ok(count) => {
                        self.tls.receive(&scratch[..count]);
                        self.active = now_ms;
                        moved = true;
                    }
                    Err(TransportError::WouldBlock) => break,
                    Err(TransportError::Broken) => return Keep::Close,
                }
            }
            let decrypted = match self.tls.process(&mut self.session) {
                Ok(progress) => progress,
                Err(_) => return Keep::Close,
            };
            moved |= decrypted;
            if (self.ended || self.tls.peer_closed()) && !decrypted {
                // Nothing more will arrive, and TLS has given all it holds: what has arrived is
                // answered, then the session closes.
                self.session.finish();
            }
            moved |= self.session.serve(api, now);
            while !self.tls.pending().is_empty() {
                match self.transport.write(self.tls.pending()) {
                    Ok(0) | Err(TransportError::WouldBlock) => break,
                    Ok(count) => {
                        self.tls.sent(count);
                        self.active = now_ms;
                        moved = true;
                    }
                    Err(TransportError::Broken) => return Keep::Close,
                }
            }
            if !moved {
                break;
            }
        }
        let drained = self.session.closing()
            && self.session.pending().is_empty()
            && self.tls.pending().is_empty();
        // Closed by this side's close_notify, or with nothing left to say before the handshake
        // could carry it.
        if self.tls.closed() || (drained && !self.tls.ready()) {
            return Keep::Close;
        }
        if now_ms.saturating_sub(self.active) >= IDLE_LIMIT_MS {
            return Keep::Close;
        }
        Keep::Open
    }
}

impl<T: Transport, C: Clock> Server<T, C> {
    /// A server for `config`, with time from `clock`.
    pub fn new(config: Config, clock: C) -> Result<Self, rustls::Error> {
        let wall = Arc::new(WallClock::default());
        wall.set(clock.unix_seconds());
        Ok(Self {
            tls: tls::config(&config.identity, wall.clone())?,
            wall,
            clock,
            api: Api::new(config.facts),
            connections: Vec::new(),
            scratch: vec![0; READ_CHUNK],
        })
    }

    /// Takes a connection the caller has accepted. While [`CONNECTION_LIMIT`] connections are
    /// served, the new one is answered `busy` at its first request, then closed.
    pub fn accept(&mut self, mut transport: T) {
        let refusing = self.served() >= CONNECTION_LIMIT;
        match Tls::new(self.tls.clone()) {
            Ok(tls) => self.connections.push(Connection {
                transport,
                tls,
                session: if refusing {
                    Session::refusing()
                } else {
                    Session::default()
                },
                active: self.clock.monotonic_ms(),
                ended: false,
            }),
            Err(_) => transport.close(),
        }
    }

    /// Moves every connection as far as it can go now: reads what has arrived, answers it,
    /// writes what can be written, and closes each connection that is done, broken or idle.
    pub fn poll(&mut self) {
        let now_ms = self.clock.monotonic_ms();
        let now = self.clock.unix_seconds();
        self.wall.set(now);
        let mut index = 0;
        while index < self.connections.len() {
            let keep = self.connections[index].drive(&mut self.api, now_ms, now, &mut self.scratch);
            if keep == Keep::Open {
                index += 1;
            } else {
                let mut connection = self.connections.remove(index);
                connection.transport.close();
            }
        }
    }

    /// The open connections, those being refused included.
    pub fn connections(&self) -> usize {
        self.connections.len()
    }

    /// The connections being served, those being refused left out.
    pub fn served(&self) -> usize {
        self.connections
            .iter()
            .filter(|c| !c.session.is_refusing())
            .count()
    }

    /// When the next connection will have been idle too long, on the monotonic clock: the
    /// caller polls again by then.
    pub fn deadline(&self) -> Option<u64> {
        self.connections
            .iter()
            .map(|c| c.active.saturating_add(IDLE_LIMIT_MS))
            .min()
    }

    /// What the node document says, for the caller to change (its health, say).
    pub fn facts_mut(&mut self) -> &mut NodeFacts {
        &mut self.api.facts
    }
}
