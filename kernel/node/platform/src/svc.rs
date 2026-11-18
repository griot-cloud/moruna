//! The machine's traits on the node itself: connections through the network server's sockets
//! (`moruna.net`), the core's monotonic clock, and the supervisor's channel to `platform`, which
//! gives the node's memory and then the channel to each start of `api`.
//!
//! They are written on `moruna-svc`, which on a host is a model of the same kernel calls, so they
//! are tested there too: the sockets against the real network server over its loopback.

use alloc::{format, string::String, vec, vec::Vec};
use core::net::{IpAddr, Ipv4Addr};

use moruna_node::{Bootstrap, restart::api_started};
use moruna_svc::{
    CallError, Channel, DEADLINE_INFINITE, Error, Handle, Memory, ObjectType, Rights, Signals, map,
    now, wait_one,
};
use netstack_protocol::{Addr, Endpoint, Target, net};

use crate::machine::{Clock, Control, Link, LinkError, Network, Stream, StreamError};

/// The interface the node's address is read from.
pub const INTERFACE: &str = "wan";

/// The bytes one send or receive moves: the size of the memory lent to the network server.
const LOAN_BYTES: usize = 64 * 1024;

/// The node's network: a client of the network server, holding whatever grant its listener has.
pub struct Wan {
    client: net::Client,
    /// Memory of the client's own account, lent to the network server for each send and
    /// receive, as `moruna.net` requires.
    loan: Memory,
}

fn kernel(error: Error) -> StreamError {
    StreamError::Other(format!("the kernel refused: {}", error.name()))
}

fn failure(error: CallError<net::Error>) -> StreamError {
    match error {
        CallError::Remote(net::Error::Refused) => StreamError::Refused,
        CallError::Remote(net::Error::TimedOut) => StreamError::TimedOut,
        CallError::Remote(net::Error::NoRoute) => StreamError::Unreachable,
        CallError::Remote(net::Error::Reset) => StreamError::Reset,
        CallError::TimedOut => StreamError::Deadline,
        other => StreamError::Other(format!("the network server answered {other:?}")),
    }
}

impl Wan {
    /// The network through `client`.
    pub fn new(client: net::Client) -> Result<Wan, StreamError> {
        let loan = Memory::create_in(LOAN_BYTES as u64, &client.account().0).map_err(kernel)?;
        Ok(Wan { client, loan })
    }

    /// Gives the client's next call until `deadline`.
    fn arm(&mut self, deadline: u64) -> Result<(), StreamError> {
        let left = match deadline {
            DEADLINE_INFINITE => DEADLINE_INFINITE,
            deadline => deadline.saturating_sub(now()),
        };
        if left == 0 {
            return Err(StreamError::Deadline);
        }
        self.client.core().set_timeout(left);
        Ok(())
    }

    /// A handle to the lent memory with `rights`, for the network server to keep for one call.
    fn lend(&self, rights: Rights) -> Result<Memory, StreamError> {
        self.loan.0.duplicate(rights).map(Memory).map_err(kernel)
    }
}

impl Network for Wan {
    type Stream<'a> = Socket<'a>;

    fn connect(
        &mut self,
        address: Ipv4Addr,
        port: u16,
        deadline: u64,
    ) -> Result<Socket<'_>, StreamError> {
        self.arm(deadline)?;
        let to = Endpoint::new(Addr(IpAddr::V4(address)), port);
        let opened = self
            .client
            .open(Target::Endpoint(to), None)
            .map_err(failure)?;
        Ok(Socket {
            wan: self,
            socket: opened.socket,
        })
    }

    fn address(&mut self, deadline: u64) -> Result<Ipv4Addr, StreamError> {
        self.arm(deadline)?;
        let info = self.client.info().map_err(failure)?;
        info.interfaces
            .iter()
            .filter(|interface| interface.name == INTERFACE)
            .flat_map(|interface| interface.addrs.iter())
            .find_map(|cidr| match cidr.addr.0 {
                IpAddr::V4(address) => Some(address),
                IpAddr::V6(_) => None,
            })
            .ok_or_else(|| {
                StreamError::Other(format!("the interface {INTERFACE} has no IPv4 address"))
            })
    }
}

/// One connection through the network server. Dropping it closes the socket.
pub struct Socket<'a> {
    wan: &'a mut Wan,
    socket: u32,
}

impl Stream for Socket<'_> {
    fn write(&mut self, bytes: &[u8], deadline: u64) -> Result<usize, StreamError> {
        let count = bytes.len().min(LOAN_BYTES);
        self.wan.arm(deadline)?;
        self.wan
            .loan
            .map(0, LOAN_BYTES as u64, map::WRITE)
            .and_then(|mapping| mapping.write(0, &bytes[..count]))
            .map_err(kernel)?;
        let source = self
            .wan
            .lend(Rights::READ | Rights::MAP | Rights::TRANSFER)?;
        let taken = self
            .wan
            .client
            .send_from(self.socket, source, 0, count as u32)
            .map_err(failure)?;
        Ok(taken as usize)
    }

    fn read(&mut self, buffer: &mut [u8], deadline: u64) -> Result<usize, StreamError> {
        let most = buffer.len().min(LOAN_BYTES);
        self.wan.arm(deadline)?;
        let destination = self
            .wan
            .lend(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)?;
        let count = self
            .wan
            .client
            .receive_into(self.socket, destination, 0, most as u32)
            .map_err(failure)? as usize;
        if count > most {
            return Err(StreamError::Other(String::from(
                "the network server gave more bytes than were asked for",
            )));
        }
        self.wan
            .loan
            .map(0, LOAN_BYTES as u64, map::READ)
            .and_then(|mapping| mapping.read(0, &mut buffer[..count]))
            .map_err(kernel)?;
        Ok(count)
    }
}

impl Drop for Socket<'_> {
    fn drop(&mut self) {
        // The connection is over either way; a socket the server already closed is no error.
        let _ = self.wan.client.close(self.socket);
    }
}

/// The core's monotonic clock. A sleep is a wait, with a deadline, on a channel nothing writes to.
pub struct Monotonic {
    quiet: (Channel, Channel),
}

impl Monotonic {
    /// The clock, with the channel its sleeps wait on.
    pub fn new() -> Result<Monotonic, Error> {
        Ok(Monotonic {
            quiet: Channel::create()?,
        })
    }
}

impl Clock for Monotonic {
    fn now(&self) -> u64 {
        now()
    }

    fn sleep_until(&mut self, at: u64) {
        while now() < at {
            // Both ends are held and neither is written, so this returns when `at` has come.
            let _ = wait_one(&self.quiet.0.0, Signals::READABLE, at);
        }
    }
}

/// A channel as whole messages that carry no handle: the link to one start of `api` (the
/// hand-over, and its answer).
pub struct ChannelLink(pub Channel);

/// `platform`'s bootstrap channel, its `supervisor` slot, whose end the supervisor keeps (PM,
/// item 2.2c). Its first message is the node's memory: the machine account's limit, which every
/// service and job is carved from (item 1.4, PM 2026-11-18). Then each start of `api`, the
/// first included, sends [`moruna_node::restart::API_STARTED`] with that start's channel.
pub struct ChannelControl(pub Channel);

impl ChannelControl {
    /// The node's memory, the first message, read by `deadline`; anything but `platform`'s
    /// bootstrap is refused.
    pub fn memory(&mut self, deadline: u64) -> Result<u64, String> {
        let (message, handles) = receive(&self.0, deadline).map_err(|error| match error {
            LinkError::Closed => String::from("the supervisor closed its end and sent none"),
            error => format!("the supervisor's message: {error}"),
        })?;
        Bootstrap::platform_memory(&message, handles.len())
            .ok_or_else(|| String::from("the supervisor's message is not platform's bootstrap"))
    }
}

impl Control for ChannelControl {
    type Api = ChannelLink;

    fn next_api(&mut self, deadline: u64) -> Result<ChannelLink, LinkError> {
        let (message, mut handles) = receive(&self.0, deadline)?;
        if !api_started(&message, handles.len()) {
            return Err(LinkError::Other(String::from(
                "the supervisor sent something other than api-started",
            )));
        }
        let handle = handles
            .pop()
            .flatten()
            .ok_or_else(|| LinkError::Other(String::from("api-started carries no handle")))?;
        match handle.info() {
            Ok((ObjectType::Channel, _)) => Ok(ChannelLink(Channel(handle))),
            Ok(_) => Err(LinkError::Other(String::from(
                "api-started carries something other than a channel",
            ))),
            Err(error) => Err(refused(error)),
        }
    }
}

/// Waits on `channel` for any of `signals` until `deadline`, and says which were seen.
fn wait(channel: &Channel, signals: Signals, deadline: u64) -> Result<Signals, LinkError> {
    match wait_one(&channel.0, signals, deadline) {
        Ok(seen) => Ok(seen),
        Err(Error::TimedOut) => Err(LinkError::TimedOut),
        Err(error) => Err(refused(error)),
    }
}

/// The next whole message on `channel` and the handles it carries, waited for until `deadline`.
fn receive(channel: &Channel, deadline: u64) -> Result<(Vec<u8>, Vec<Option<Handle>>), LinkError> {
    let mut closed = false;
    loop {
        let (bytes, handles) = match channel.needed().map_err(refused)? {
            Some(sizes) => sizes,
            None if closed => return Err(LinkError::Closed),
            None => {
                let seen = wait(channel, Signals::READABLE | Signals::PEER_CLOSED, deadline)?;
                closed = seen.contains(Signals::PEER_CLOSED);
                continue;
            }
        };
        let mut message = vec![0; bytes];
        let mut carried: Vec<Option<Handle>> = Vec::new();
        carried.resize_with(handles, || None);
        if bytes != 0 || handles != 0 {
            channel.read(&mut message, &mut carried).map_err(refused)?;
        }
        return Ok((message, carried));
    }
}

fn refused(error: Error) -> LinkError {
    match error {
        Error::PeerClosed => LinkError::Closed,
        error => LinkError::Other(format!("the kernel refused: {}", error.name())),
    }
}

impl Link for ChannelLink {
    fn send(&mut self, message: &[u8], deadline: u64) -> Result<(), LinkError> {
        loop {
            match self.0.write_with(message, &mut []) {
                Ok(()) => return Ok(()),
                // The peer's queue is full: wait for it to read.
                Err(Error::ShouldWait) => {
                    wait(&self.0, Signals::WRITABLE | Signals::PEER_CLOSED, deadline)?;
                }
                Err(error) => return Err(refused(error)),
            }
        }
    }

    fn receive(&mut self, deadline: u64) -> Result<Vec<u8>, LinkError> {
        let (message, handles) = receive(&self.0, deadline)?;
        if !handles.is_empty() {
            // Neither message of the hand-over carries a handle. These close as they drop.
            return Err(LinkError::Other(String::from(
                "a message on the link carries handles",
            )));
        }
        Ok(message)
    }
}
