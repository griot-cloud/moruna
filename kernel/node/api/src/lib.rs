//! The node's `api` process (system design 1C and 2B), step 1: it owns the node's one
//! listening port, TCP 443, and closes every connection it accepts. That proves the
//! node's address, its route and the cloud's firewall rule. Step 2 gives the port TLS
//! and the API's operations: the library `moruna-api-server`, which the program wires
//! to these sockets (work item 2.2b).
//!
//! The network server opens a listener on one of its interfaces' addresses, never on
//! every address at once. So the listener is opened on the address of the node's one
//! interface, `wan`, once DHCP has given it one: the process asks the network server
//! for its interfaces until `wan` has an IPv4 address.
//!
//! Every wait here is on the network server, which ends within 30 s of its start if no
//! lease comes (system design 1B); a request to a server that has ended fails, and the
//! process then stops with that failure, as `moruna: fatal api: <reason>`.
//!
//! This library is the process's logic, so that it is tested on a host against the
//! network server's own stack; `main.rs` gives it the process's start handles.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use core::fmt;

use moruna_svc::{Account, CallError, Channel, DEADLINE_INFINITE, Signals, deadline_in, wait_one};
use netstack_protocol::{Addr, Endpoint, Info, net};

#[cfg(test)]
mod tests;

/// The node's one listening port (system design 1C).
pub const PORT: u16 = 443;

/// The node's one interface (system design 1C).
pub const INTERFACE: &str = "wan";

/// The connections the listener holds established and not yet accepted: as many as the
/// sockets of the grant it listens under (`sockets=64`, work item 1.3).
pub const BACKLOG: u32 = 64;

/// How long the wait for the interface's address sleeps between two asks.
pub const ADDRESS_POLL_NS: u64 = 100_000_000;

/// Why the process cannot go on.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The supervisor gave the process no handle in this start slot.
    NoSlot(&'static str),
    /// The network server has no interface called [`INTERFACE`].
    NoInterface,
    /// A request to the network server failed: which, and how.
    Network(&'static str, CallError<net::Error>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoSlot(slot) => write!(f, "no {slot} slot"),
            Error::NoInterface => write!(f, "the network server has no interface {INTERFACE}"),
            Error::Network(request, error) => write!(f, "{request}: {error:?}"),
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
/// server. Its requests wait as long as the server takes: an accept has no deadline.
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

/// The IPv4 address of the node's interface, or `None` until DHCP has set it.
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

/// Waits for the node's address, asking the network server every
/// [`ADDRESS_POLL_NS`] until its interface has one.
pub fn await_address(client: &mut net::Client) -> Result<Addr, Error> {
    loop {
        if let Some(found) = address(&asked("info", client.info())?)? {
            return Ok(found);
        }
        // Sleeps until the next ask, or until the server has gone, which that ask
        // then reports.
        match wait_one(
            &client.core().channel().0,
            Signals::PEER_CLOSED,
            deadline_in(ADDRESS_POLL_NS),
        ) {
            Ok(_) | Err(moruna_svc::Error::TimedOut) => {}
            Err(e) => return Err(Error::Network("wait", CallError::Kernel(e))),
        }
    }
}

/// Opens the node's listener, on `address` at port 443: its socket, and where it
/// listens.
pub fn listen(client: &mut net::Client, address: Addr) -> Result<(u32, Endpoint), Error> {
    asked(
        "listen",
        client.listen(Endpoint::new(address, PORT), BACKLOG),
    )
}

/// Step 1's whole service: waits for the next connection and closes it at once. Whom
/// it was from.
pub fn turn(client: &mut net::Client, listener: u32) -> Result<Endpoint, Error> {
    let opened = asked("accept", client.accept(listener))?;
    asked("close", client.close(opened.socket))?;
    Ok(opened.remote)
}
