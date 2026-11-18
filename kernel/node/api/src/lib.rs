//! The node's `api` process (system design 1C, 2B and 2C; work items 1.3 and 2.2b): it
//! owns the node's one listening port, TCP 443, and serves the node's API there through
//! the library `moruna-api-server`: TLS 1.3, HTTP/1.1, the operations, tokens, the audit
//! record and MCP. This crate only wires that library to the network server's sockets and
//! to `platform`'s hand-over.
//!
//! - **The hand-over** (system design 1D, work item 1.4): the first message on the
//!   `platform` link, the validated node config with the instance, the cloud, the wall
//!   clock and the node's memory ([`handover`]). The server's identity, token keys and
//!   node document come from it ([`node`]).
//! - **The identity's line:** `moruna: tls sha256=<fingerprint>`, printed as soon as the
//!   identity is made, before the listener opens and so before `ready`.
//! - **The listener** follows `wan`'s lease (system design 1C). The network server opens a
//!   listener on one of its interfaces' addresses, never on every address, so the process
//!   asks for `wan`'s address every [`ADDRESS_POLL_NS`] while it has no listener and every
//!   [`LEASE_POLL_NS`] while it listens; when the address is no longer the listener's, the
//!   listener closes and one opens at the next ([`network::Network`]).
//! - **`ready`:** once the listener first opens, the process answers `platform` with
//!   [`moruna_node_config::READY`], once, and the node document's `boot_ms` is the core's
//!   monotonic clock then.
//! - **Serving:** each accepted socket is one server connection, its bytes moved by
//!   requests the process sends to the network server without waiting on them
//!   ([`network::Conn`]). One thread: a loop takes the network server's answers, polls the
//!   server, sends what is to send, and waits for the next answer or deadline ([`run`]).
//!
//! Every wait here is on the network server, which ends within 30 s of its start if no
//! lease comes (system design 1B), or on `platform`; a request to a server that has ended
//! fails, and the process then stops with that failure, as `moruna: fatal api: <reason>`.
//!
//! This library is the process's logic, tested on a host against the network server's own
//! stack; `main.rs` gives it the process's start handles.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::{format, rc::Rc, string::String, vec, vec::Vec};
use core::{cell::RefCell, convert::Infallible, fmt};

use moruna_api_server::{IdentityError, Server};
use moruna_node_config::{Handover, HandoverError, READY};
use moruna_svc::{Account, CallError, Channel, DEADLINE_INFINITE, Handle, Signals, now, wait_one};
use netstack_protocol::{Addr, Endpoint, Info, net};

pub mod network;
pub mod node;

#[cfg(test)]
mod tests;

/// The node's one listening port (system design 1C).
pub const PORT: u16 = 443;

/// The node's one interface (system design 1C).
pub const INTERFACE: &str = "wan";

/// The connections the listener holds established and not yet accepted: as many as the
/// sockets of the grant it listens under (`sockets=64`, work item 1.3).
pub const BACKLOG: u32 = 64;

/// How often the process asks for `wan`'s address while it has no listener.
pub const ADDRESS_POLL_NS: u64 = 100_000_000;

/// How often it asks again while it listens, to follow the lease.
pub const LEASE_POLL_NS: u64 = 1_000_000_000;

/// Why the process cannot go on.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The supervisor gave the process no handle in this start slot.
    NoSlot(&'static str),
    /// The network server has no interface called [`INTERFACE`].
    NoInterface,
    /// A request to the network server failed: which, and how.
    Network(&'static str, CallError<net::Error>),
    /// The kernel refused something the process needs for its connections: what, and why.
    Kernel(&'static str, moruna_svc::Error),
    /// The link to `platform` failed: what was being done, and the kernel's refusal.
    Link(&'static str, moruna_svc::Error),
    /// A message from `platform` carries handles, which no message of the hand-over does.
    LinkHandles,
    /// `platform`'s hand-over is refused.
    Handover(HandoverError),
    /// The server's TLS identity could not be made: the operator's `tls` (when `true`), or
    /// the node's own.
    Identity(bool, IdentityError),
    /// The server could not be made from its config.
    Server(String),
    /// A request to [`TEST_EXIT_PATH`](moruna_api_server::TEST_EXIT_PATH) came: the
    /// `cloud-test` image's way to end the process, which is no failure (work item 2.2c).
    #[cfg(feature = "test-exit")]
    TestExit,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoSlot(slot) => write!(f, "no {slot} slot"),
            Error::NoInterface => write!(f, "the network server has no interface {INTERFACE}"),
            Error::Network(request, error) => write!(f, "{request}: {error:?}"),
            Error::Kernel(what, error) => write!(f, "{what}: {}", error.name()),
            Error::Link(what, error) => {
                write!(f, "the link to platform: {what}: {}", error.name())
            }
            Error::LinkHandles => f.write_str("a message from platform carries handles"),
            Error::Handover(error) => write!(f, "{error}"),
            Error::Identity(true, error) => write!(f, "node config's tls: {error}"),
            Error::Identity(false, error) => write!(f, "the node's own TLS identity: {error}"),
            Error::Server(why) => write!(f, "the API server: {why}"),
            #[cfg(feature = "test-exit")]
            Error::TestExit => write!(f, "a request to {}", moruna_api_server::TEST_EXIT_PATH),
        }
    }
}

/// The process's last line when it cannot go on: the node's failure line, its stage this
/// service's name (system design 1B).
pub struct Fatal<'a>(pub &'a Error);

impl fmt::Display for Fatal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "moruna: fatal api: {}", self.0)
    }
}

/// Names the request a failure came from.
fn asked<T>(request: &'static str, result: Result<T, CallError<net::Error>>) -> Result<T, Error> {
    result.map_err(|error| Error::Network(request, error))
}

/// A connection to the network server through `listener`, the process's `network`
/// slot, with `account`, the process's own, as the owner of everything it lends the
/// server. Its requests wait as long as the server takes.
pub fn connect(listener: &Channel, account: Account) -> Result<net::Client, Error> {
    let channel = asked(
        "connect",
        moruna_svc::connect(listener).map_err(CallError::typed),
    )?;
    let mut client = asked(
        "hello",
        net::Client::connect(channel, DEADLINE_INFINITE, account),
    )?;
    client.core().set_timeout(DEADLINE_INFINITE);
    Ok(client)
}

/// The IPv4 address of the node's interface, or `None` while DHCP has not set it.
pub fn address(info: &Info) -> Result<Option<Addr>, Error> {
    let interface = info
        .interfaces
        .iter()
        .find(|i| i.name == INTERFACE)
        .ok_or(Error::NoInterface)?;
    Ok(interface
        .addrs
        .iter()
        .map(|c| c.addr)
        .find(|a| a.is_v4() && a.is_host()))
}

/// What the listener did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// It opened here.
    Listening(Endpoint),
    /// It closed here: `wan`'s address is no longer this one.
    Unlistened(Endpoint),
}

impl Event {
    /// What the process says of it on the console.
    pub fn line(&self) -> String {
        match self {
            Event::Listening(at) => format!("api: listening on {at}"),
            Event::Unlistened(at) => {
                format!("api: stopped listening on {at}: {INTERFACE}'s address changed")
            }
        }
    }
}

/// `platform`'s hand-over: the first message on the `platform` link, waited for as long
/// as it takes, and decoded (its node config validated again).
pub fn handover(platform: &Channel) -> Result<Handover, Error> {
    let message = loop {
        match platform.needed() {
            Ok(Some((bytes, handles))) => {
                let mut message = vec![0; bytes];
                let mut carried: Vec<Option<Handle>> = (0..handles).map(|_| None).collect();
                if bytes != 0 || handles != 0 {
                    platform
                        .read(&mut message, &mut carried)
                        .map_err(|e| Error::Link("reading the hand-over", e))?;
                }
                if handles != 0 {
                    // They close as they drop.
                    return Err(Error::LinkHandles);
                }
                break message;
            }
            // Nothing yet: waits for it, or for `platform` to go, which the next look
            // reports.
            Ok(None) => {
                wait_one(
                    &platform.0,
                    Signals::READABLE | Signals::PEER_CLOSED,
                    DEADLINE_INFINITE,
                )
                .map_err(|e| Error::Link("waiting for the hand-over", e))?;
            }
            Err(e) => return Err(Error::Link("waiting for the hand-over", e)),
        }
    };
    Handover::decode(&message).map_err(Error::Handover)
}

/// Answers `platform`'s hand-over: [`READY`], once the listener is open.
pub fn ready(platform: &Channel) -> Result<(), Error> {
    loop {
        match platform.write_with(READY, &mut []) {
            Ok(()) => return Ok(()),
            // Its queue is full: waits for it to read, or to go.
            Err(moruna_svc::Error::ShouldWait) => {
                wait_one(
                    &platform.0,
                    Signals::WRITABLE | Signals::PEER_CLOSED,
                    DEADLINE_INFINITE,
                )
                .map_err(|e| Error::Link("answering ready", e))?;
            }
            Err(e) => return Err(Error::Link("answering ready", e)),
        }
    }
}

/// The whole process, on its connection to the network server and its link to
/// `platform`, on a node of `cores` CPUs: the hand-over, the server made from it and its
/// identity's line, then the loop that serves, saying each line with `say` and answering
/// `platform` once its listener first opens. It returns only when it cannot go on.
pub fn run(
    client: net::Client,
    platform: &Channel,
    cores: u32,
    say: &mut dyn FnMut(&str),
) -> Result<Infallible, Error> {
    let handover = handover(platform)?;
    let config = node::config(&handover, cores, now())?;
    say(&format!(
        "moruna: tls sha256={}",
        config.identity.fingerprint()
    ));
    let mut server = Server::new(config, node::NodeClock::new(&handover))
        .map_err(|e| Error::Server(format!("{e:?}")))?;
    let network = Rc::new(RefCell::new(network::Network::new(client)));
    let mut answered = false;
    loop {
        let accepted = network.borrow_mut().take()?;
        for socket in accepted {
            server.accept(network::Conn::new(network.clone(), socket));
        }
        let at = now();
        if let Some(event) = network.borrow_mut().tend(at)? {
            say(&event.line());
            if !answered && matches!(event, Event::Listening(_)) {
                // System design 1B: from the core's first instruction to ready.
                server.facts_mut().boot_ms = at / 1_000_000;
                ready(platform)?;
                answered = true;
            }
        }
        server.poll();
        network.borrow_mut().flush()?;
        // `cloud-test` only (work item 2.2c): the answer to the request is sent; the process
        // ends.
        #[cfg(feature = "test-exit")]
        if server.exit_requested() {
            return Err(Error::TestExit);
        }
        let mut deadline = network.borrow().look_at();
        if let Some(ms) = server.deadline() {
            deadline = deadline.min(ms.saturating_mul(1_000_000));
        }
        network.borrow_mut().wait(deadline)?;
    }
}
