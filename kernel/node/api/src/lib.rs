//! The node's `api` process (system design 1C and 2B), step 1: it owns the node's one
//! listening port, TCP 443, and closes every connection it accepts. That proves the
//! node's address, its route and the cloud's firewall rule. Step 2 gives the port TLS
//! and the API's operations: the library `moruna-api-server`, which the program wires
//! to these sockets (work item 2.2b).
//!
//! The network server opens a listener on one of its interfaces' addresses, never on
//! every address at once. So the listener is opened on the address of the node's one
//! interface, `wan`, once DHCP has given it one, and it follows the lease (system
//! design 1C): [`Service`] asks the network server for `wan`'s address every
//! [`ADDRESS_POLL_NS`] while it has no listener, and every [`LEASE_POLL_NS`] while it
//! listens. When the address is no longer the listener's, because the lease moved or
//! was lost, it closes the listener, and opens one on the address `wan` has next.
//!
//! Before it listens, the process takes `platform`'s hand-over (system design 1D,
//! work item 1.4): the first message on its `platform` link, the validated node config
//! with the instance, the cloud and the wall clock. It keeps it, for step 2 serves with
//! it. Once its listener first opens it answers `platform` with
//! [`moruna_node_config::READY`], once, and `platform` then says the node is ready.
//!
//! Every wait here is on the network server, which ends within 30 s of its start if no
//! lease comes (system design 1B), or on `platform`, which ends within 90 s if it finds
//! no instance data; a request to a server that has ended fails, and the process then
//! stops with that failure, as `moruna: fatal api: <reason>`.
//!
//! This library is the process's logic, so that it is tested on a host against the
//! network server's own stack; `main.rs` gives it the process's start handles.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};
use core::{convert::Infallible, fmt};

use moruna_node_config::{Handover, HandoverError, READY};
use moruna_svc::{
    Account, CallError, Channel, DEADLINE_INFINITE, Encoder, Handle, Signals, Wire, deadline_in,
    wait_one,
};
use netstack_protocol::{Addr, Endpoint, Info, Opened, net};

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

/// `moruna.net`'s `accept` (its request 4), which the process sends without waiting
/// for its answer, so that it can ask after the lease meanwhile.
const ACCEPT: u16 = 4;

/// Why the process cannot go on.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The supervisor gave the process no handle in this start slot.
    NoSlot(&'static str),
    /// The network server has no interface called [`INTERFACE`].
    NoInterface,
    /// A request to the network server failed: which, and how.
    Network(&'static str, CallError<net::Error>),
    /// The link to `platform` failed: what was being done, and the kernel's refusal.
    Link(&'static str, moruna_svc::Error),
    /// A message from `platform` carries handles, which no message of the hand-over does.
    LinkHandles,
    /// `platform`'s hand-over is refused.
    Handover(HandoverError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoSlot(slot) => write!(f, "no {slot} slot"),
            Error::NoInterface => write!(f, "the network server has no interface {INTERFACE}"),
            Error::Network(request, error) => write!(f, "{request}: {error:?}"),
            Error::Link(what, error) => {
                write!(f, "the link to platform: {what}: {}", error.name())
            }
            Error::LinkHandles => f.write_str("a message from platform carries handles"),
            Error::Handover(error) => write!(f, "{error}"),
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

/// What one step of the [`Service`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// It opened the node's listener here.
    Listening(Endpoint),
    /// It closed the listener here: `wan`'s address is no longer this one.
    Unlistened(Endpoint),
    /// It accepted a connection from here and closed it at once.
    Closed(Endpoint),
    /// Nothing happened within the step's wait.
    Idle,
}

impl Event {
    /// What the process says of it on the console, if anything.
    pub fn line(&self) -> Option<String> {
        match self {
            Event::Listening(at) => Some(format!("api: listening on {at}")),
            Event::Unlistened(at) => Some(format!(
                "api: stopped listening on {at}: {INTERFACE}'s address changed"
            )),
            Event::Closed(_) | Event::Idle => None,
        }
    }
}

/// Step 1's service: the node's one listener, on `wan`'s leased address at port 443,
/// closing each connection it accepts.
pub struct Service {
    client: net::Client,
    /// The listener's socket, and where it listens.
    listener: Option<(u32, Endpoint)>,
    /// The `accept` waiting on the listener: its request's id.
    accept: Option<u32>,
}

impl Service {
    /// The service, on a connection to the network server ([`connect`]).
    pub fn new(client: net::Client) -> Service {
        Service {
            client,
            listener: None,
            accept: None,
        }
    }

    /// Where it listens, if it does.
    pub fn listening(&self) -> Option<Endpoint> {
        self.listener.map(|(_, at)| at)
    }

    /// Waits for the next thing to do, at most a poll's time, and does it.
    pub fn step(&mut self) -> Result<Event, Error> {
        match self.listener {
            None => self.open(),
            Some((socket, at)) => self.serve(socket, at),
        }
    }

    /// With no listener: one on `wan`'s address, once it has one.
    fn open(&mut self) -> Result<Event, Error> {
        let Some(found) = address(&asked("info", self.client.info())?)? else {
            // Sleeps until the next ask, or until the server has gone, which that ask
            // then reports.
            match wait_one(
                &self.client.core().channel().0,
                Signals::PEER_CLOSED,
                deadline_in(ADDRESS_POLL_NS),
            ) {
                Ok(_) | Err(moruna_svc::Error::TimedOut) => return Ok(Event::Idle),
                Err(e) => return Err(Error::Network("wait", CallError::Kernel(e))),
            }
        };
        let (socket, at) = asked(
            "listen",
            self.client.listen(Endpoint::new(found, PORT), BACKLOG),
        )?;
        self.listener = Some((socket, at));
        Ok(Event::Listening(at))
    }

    /// With a listener: the next connection, closed at once; or, after a poll's time
    /// without one, the lease looked at again.
    fn serve(&mut self, socket: u32, at: Endpoint) -> Result<Event, Error> {
        let txid = match self.accept {
            Some(txid) => txid,
            None => {
                let mut args = Encoder::new();
                socket
                    .encode(&mut args)
                    .map_err(|_| Error::Network("accept", CallError::Malformed))?;
                let txid = asked(
                    "accept",
                    self.client
                        .core()
                        .send(ACCEPT, args)
                        .map_err(CallError::typed),
                )?;
                self.accept = Some(txid);
                txid
            }
        };
        match self
            .client
            .core()
            .reply(txid, deadline_in(LEASE_POLL_NS))
            .map_err(CallError::typed)
        {
            Ok(frame) => {
                self.accept = None;
                let opened = accepted(frame)?;
                asked("close", self.client.close(opened.socket))?;
                Ok(Event::Closed(opened.remote))
            }
            Err(CallError::TimedOut) => {
                if address(&asked("info", self.client.info())?)? == Some(at.addr) {
                    return Ok(Event::Idle);
                }
                self.unlisten(socket, txid)?;
                Ok(Event::Unlistened(at))
            }
            Err(error) => {
                self.accept = None;
                Err(Error::Network("accept", error))
            }
        }
    }

    /// Closes the listener, and takes the answer of the `accept` that waited on it.
    fn unlisten(&mut self, socket: u32, txid: u32) -> Result<(), Error> {
        asked("close", self.client.close(socket))?;
        self.listener = None;
        self.accept = None;
        match self
            .client
            .core()
            .reply(txid, DEADLINE_INFINITE)
            .map_err(CallError::typed)
        {
            // The listener's close answers it.
            Err(CallError::Remote(net::Error::Closed)) => Ok(()),
            // Or it had taken a connection just before: closed too.
            Ok(frame) => {
                let opened = accepted(frame)?;
                asked("close", self.client.close(opened.socket))
            }
            Err(error) => Err(Error::Network("accept", error)),
        }
    }
}

/// The connection an `accept`'s answer opened.
fn accepted(mut frame: moruna_svc::frame::Frame) -> Result<Opened, Error> {
    let mut decoder = frame.decoder();
    let opened = Opened::decode(&mut decoder)
        .and_then(|opened| decoder.finish().map(|()| opened))
        .map_err(|_| Error::Network("accept", CallError::Malformed))?;
    Ok(opened)
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
/// `platform`: the hand-over, then the [`Service`], saying each of its lines with
/// `say`, and answering `platform` once its listener first opens. It returns only when
/// it cannot go on.
pub fn run(
    client: net::Client,
    platform: &Channel,
    say: &mut dyn FnMut(&str),
) -> Result<Infallible, Error> {
    // Kept for as long as the process runs: step 2 serves with it (work item 2.2b).
    let _handover = handover(platform)?;
    let mut service = Service::new(client);
    let mut answered = false;
    loop {
        let event = service.step()?;
        if let Some(line) = event.line() {
            say(&line);
        }
        if !answered && matches!(event, Event::Listening(_)) {
            ready(platform)?;
            answered = true;
        }
    }
}
