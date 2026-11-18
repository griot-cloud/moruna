//! The server: a state machine the caller drives. The caller accepts each connection and hands
//! it over, polls whenever a connection's stream has changed or a deadline has passed, and
//! supplies the time; the server reads, answers and writes through each connection's transport,
//! without threads or an async runtime.

use crate::{
    api::{Api, NodeFacts},
    engine::{EngineHost, NoEngine},
    functions::FunctionKey,
    http::{BODY_BUDGET, Budget, Session},
    identity::Identity,
    tls::{self, Tls, WallClock},
    token::TokenKey,
    transport::{Clock, Transport, TransportError},
};
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use rustls::ServerConfig;

/// The most connections served at once. A connection accepted beyond them has its first request
/// answered `busy`, then closes.
pub const CONNECTION_LIMIT: usize = 64;
/// The most connections open at once, those being refused and those still in their handshake
/// included. A connection accepted beyond them is closed at once, with no answer.
pub const OPEN_LIMIT: usize = 80;
/// How long a connection may wait on its client, moving no bytes, before it is closed, in
/// milliseconds: for a request, for the rest of one, or for the client to take an answer. A
/// connection whose request is being answered, with nothing for the client to take, is not idle
/// (system design 2's Limits).
pub const IDLE_LIMIT_MS: u64 = 60_000;
/// How long a connection's TLS handshake may take before the connection is closed, in
/// milliseconds.
pub const HANDSHAKE_LIMIT_MS: u64 = 10_000;
/// The most bytes read from a transport in one call.
const READ_CHUNK: usize = 16 * 1024;
/// What a read may take beyond the plaintext the session wants: TLS's own bytes.
const READ_SLACK: usize = 1024;
/// The most rounds of reading, answering and writing one connection gets in one poll: enough
/// for a request at its limits, so that what has arrived is not left waiting for the next poll.
const ROUNDS: usize = 1024;

/// What the server needs from node config and from the node.
#[derive(Clone, Debug)]
pub struct Config {
    /// The TLS identity: the operator's, or the node's own.
    pub identity: Identity,
    /// What the node document says. Its `instance` is also the `aud` every token must carry.
    pub facts: NodeFacts,
    /// The keys a token's signature is checked against: node config's `token_keys`.
    pub token_keys: Vec<TokenKey>,
    /// The keys a function image's signature is checked against: node config's
    /// `functions.keys`, none when it has none (system design 3E).
    pub function_keys: Vec<FunctionKey>,
}

/// The API server over connections of type `T`, with time from `C`.
pub struct Server<T: Transport, C: Clock> {
    tls: Arc<ServerConfig>,
    wall: Arc<WallClock>,
    clock: C,
    api: Api,
    connections: Vec<Connection<T>>,
    /// The bytes of request bodies the connections may still hold, all together.
    budget: Budget,
    /// Where each read from a transport lands before TLS takes it.
    scratch: Vec<u8>,
}

/// One connection.
struct Connection<T> {
    transport: T,
    tls: Tls,
    session: Session,
    /// When the connection was accepted, on the monotonic clock.
    accepted: u64,
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
    /// Each read is sized to what the session wants, so that what the client sends beyond it
    /// (a body the server will not read, say) stays in the transport.
    fn drive(
        &mut self,
        api: &mut Api,
        budget: &mut Budget,
        (now_ms, now): (u64, u64),
        scratch: &mut [u8],
    ) -> Keep {
        for _ in 0..ROUNDS {
            let mut moved = false;
            let wanted = self.session.wanted();
            if !self.ended && wanted > 0 {
                let room = self
                    .tls
                    .room()
                    .min(scratch.len())
                    .min(wanted.saturating_add(READ_SLACK));
                if room > 0 {
                    match self.transport.read(&mut scratch[..room]) {
                        Ok(0) => self.ended = true,
                        Ok(count) => {
                            self.tls.receive(&scratch[..count]);
                            self.active = now_ms;
                            moved = true;
                        }
                        Err(TransportError::WouldBlock) => {}
                        Err(TransportError::Broken) => return Keep::Close,
                    }
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
            moved |= self.session.serve(api, budget, now);
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
        if self.tls.handshaking() && now_ms.saturating_sub(self.accepted) >= HANDSHAKE_LIMIT_MS {
            return Keep::Close;
        }
        // Idle is the client's (system design 2's Limits): while its request is being answered
        // and nothing waits for it to take, the server is the one to move, so the clock starts
        // again from when it has. The request's own bounds end it (a fetch's, a launch's).
        if self.session.answering() && self.tls.pending().is_empty() {
            self.active = now_ms;
        }
        if now_ms.saturating_sub(self.active) >= IDLE_LIMIT_MS {
            return Keep::Close;
        }
        Keep::Open
    }

    /// When the connection must next be looked at: its idle limit, or its handshake's.
    fn deadline(&self) -> u64 {
        let idle = self.active.saturating_add(IDLE_LIMIT_MS);
        if self.tls.handshaking() {
            idle.min(self.accepted.saturating_add(HANDSHAKE_LIMIT_MS))
        } else {
            idle
        }
    }
}

impl<T: Transport, C: Clock> Server<T, C> {
    /// A server for `config`, with time from `clock`, on a node that runs no jobs: the job
    /// operations that need the engine answer `busy`.
    pub fn new(config: Config, clock: C) -> Result<Self, rustls::Error> {
        Self::with_engine(config, clock, Box::new(NoEngine))
    }

    /// A server for `config`, with time from `clock`, that runs jobs through `engine` (system
    /// design, section 3A). The caller polls when the engine has sent something, too.
    pub fn with_engine(
        config: Config,
        clock: C,
        engine: Box<dyn EngineHost>,
    ) -> Result<Self, rustls::Error> {
        let wall = Arc::new(WallClock::default());
        wall.set(clock.unix_seconds());
        Ok(Self {
            tls: tls::config(&config.identity, wall.clone())?,
            wall,
            clock,
            api: Api::new(
                config.facts,
                config.token_keys,
                config.function_keys,
                engine,
            ),
            connections: Vec::new(),
            budget: Budget::new(BODY_BUDGET),
            scratch: vec![0; READ_CHUNK],
        })
    }

    /// Takes a connection the caller has accepted. While [`OPEN_LIMIT`] connections are open,
    /// the new one is closed at once, with no answer; while [`CONNECTION_LIMIT`] are served, it
    /// is answered `busy` at its first request, then closed.
    pub fn accept(&mut self, mut transport: T) {
        if self.connections.len() >= OPEN_LIMIT {
            transport.close();
            return;
        }
        let refusing = self.served() >= CONNECTION_LIMIT;
        let now_ms = self.clock.monotonic_ms();
        match Tls::new(self.tls.clone()) {
            Ok(tls) => self.connections.push(Connection {
                transport,
                tls,
                session: if refusing {
                    Session::refusing()
                } else {
                    Session::default()
                },
                accepted: now_ms,
                active: now_ms,
                ended: false,
            }),
            Err(_) => transport.close(),
        }
    }

    /// Reads what the engine has sent, then moves every connection as far as it can go now:
    /// reads what has arrived, answers it, writes what can be written, and closes each
    /// connection that is done, broken, idle, or past its handshake's limit.
    pub fn poll(&mut self) {
        let now_ms = self.clock.monotonic_ms();
        let now = self.clock.unix_seconds();
        self.wall.set(now);
        self.api.pump(now);
        let mut index = 0;
        while index < self.connections.len() {
            let keep = self.connections[index].drive(
                &mut self.api,
                &mut self.budget,
                (now_ms, now),
                &mut self.scratch,
            );
            if keep == Keep::Open {
                index += 1;
            } else {
                let mut connection = self.connections.remove(index);
                connection.session.end(&mut self.api, &mut self.budget);
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

    /// When the next connection will have been idle too long, or in its handshake too long, on
    /// the monotonic clock: the caller polls again by then.
    pub fn deadline(&self) -> Option<u64> {
        self.connections.iter().map(Connection::deadline).min()
    }

    /// What the node document says, for the caller to change (its health, say).
    pub fn facts_mut(&mut self) -> &mut NodeFacts {
        &mut self.api.facts
    }

    /// Whether a request to [`TEST_EXIT_PATH`](crate::TEST_EXIT_PATH) has come: the caller then
    /// ends (feature `test-exit`, the `cloud-test` image only; work item 2.2c).
    #[cfg(feature = "test-exit")]
    pub fn exit_requested(&self) -> bool {
        self.api.exit
    }
}
