//! The machine's traits on the node itself: connections through the network server's sockets
//! (`moruna.net`), the core's monotonic clock, the supervisor's channel to `platform`, which
//! gives the node's memory and then the channel to each start of `api`, and the image
//! filesystem's listener linked to `platform`, whose answer says the engine's layer is mounted.
//!
//! They are written on `moruna-svc`, which on a host is a model of the same kernel calls, so they
//! are tested there too: the sockets against the real network server over its loopback.

use alloc::{format, string::String, vec, vec::Vec};
use core::net::{IpAddr, Ipv4Addr};

use imagefs_protocol::imagefs;
use moruna_node::{Bootstrap, restart::api_started};
use moruna_svc::{
    CallError, Channel, DEADLINE_INFINITE, Encoder, Error, Handle, Kind, Memory, ObjectType,
    Refused, Rights, Signals, Transport, Wire,
    frame::{self, Frame},
    map, now, wait_one,
};
use netstack_protocol::{Addr, Endpoint, Target, net};

use crate::machine::{Clock, Control, EngineLayer, Link, LinkError, Network, Stream, StreamError};

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

    /// The network server's `address` (`moruna.net` 3.3), which waits for the interface's
    /// address as `accept` waits for a connection, within this call's deadline.
    fn address(&mut self, deadline: u64) -> Result<Ipv4Addr, StreamError> {
        self.arm(deadline)?;
        let cidr = self
            .client
            .address(INTERFACE.into())
            .map_err(|error| match error {
                CallError::Remote(net::Error::NotFound) => StreamError::Other(format!(
                    "the network server has no interface {INTERFACE} that takes an IPv4 address"
                )),
                error => failure(error),
            })?;
        match cidr.addr.0 {
            IpAddr::V4(address) => Ok(address),
            IpAddr::V6(_) => Err(StreamError::Other(format!(
                "the network server gave {INTERFACE} an IPv6 address for its IPv4 one"
            ))),
        }
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
        // The loan is lent only after the copy, and to one call at a time: no other party
        // writes it meanwhile, so the copy is a bulk one (`Mapping::write`, work item 6.3ad).
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
        // Answered, the network server holds the loan no more (SDD 31): no other party writes
        // it during the copy, a bulk one (`Mapping::read`, work item 6.3ad).
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

/// The image filesystem's word that it has mounted the engine's layer (system design 1B; PM,
/// item 1.6e): `platform`'s slot `imagefs`, the client end of node.conf's
/// `link imagefs listen-platform platform imagefs`. imagefs serves its listeners only once it has
/// mounted every layer, so its `Welcome` to a `Hello` said through one says the engine's layer is
/// mounted, as [`moruna_svc::ready`] hears that a linked server has started. No new message: the
/// handshake is the protocol's own, and imagefs is unchanged.
///
/// The `Hello` is said when `platform` starts ([`ImageFs::hello`]), so that an answer which came
/// while it found its cloud and handed `api` its config is there when it looks, whatever its
/// deadline. Once it has heard, it lets the connection and the listener go, and holds no
/// filesystem.
pub struct ImageFs(Said);

/// Where the `Hello` to imagefs stands.
enum Said {
    /// Said on the connection, through the listener; no answer read yet.
    Waiting {
        _listener: Channel,
        connection: Channel,
    },
    /// imagefs has answered: the engine's layer is mounted.
    Mounted,
    /// It will not answer, and why.
    Failed(LinkError),
}

impl ImageFs {
    /// Says `Hello` to imagefs through `listener`, its `listen-platform` listener; the answer is
    /// read by [`EngineLayer::mounted`], which also says why it could not be said.
    pub fn hello(listener: Channel) -> ImageFs {
        let said = moruna_svc::connect(&listener)
            .map_err(|error| match error {
                CallError::PeerClosed => LinkError::Closed,
                error => LinkError::Other(format!("its listener: {error:?}")),
            })
            .and_then(|connection| {
                let mut hello = Encoder::new();
                imagefs::ID
                    .hello()
                    .encode(&mut hello)
                    .map_err(|error| LinkError::Other(format!("the Hello: {error:?}")))?;
                frame::send(&connection, Kind::Hello, 0, 0, hello).map_err(transport)?;
                Ok(Said::Waiting {
                    _listener: listener,
                    connection,
                })
            });
        ImageFs(said.unwrap_or_else(Said::Failed))
    }
}

/// A frame that did not cross the connection to imagefs.
fn transport(error: Transport) -> LinkError {
    match error {
        Transport::PeerClosed => LinkError::Closed,
        error => LinkError::Other(format!("the connection: {error:?}")),
    }
}

/// imagefs's answer to the `Hello`: a `Welcome` in its protocol's major version.
fn welcomed(mut answer: Frame) -> Result<(), LinkError> {
    let kind = answer.kind;
    let mut d = answer.decoder();
    match kind {
        Kind::Welcome => match u16::decode(&mut d) {
            Ok(major) if major == imagefs::ID.major => Ok(()),
            _ => Err(LinkError::Other(format!(
                "it welcomed a version of {} other than {}",
                imagefs::ID.name,
                imagefs::ID.major
            ))),
        },
        Kind::Refuse => Err(LinkError::Other(match Refused::decode(&mut d) {
            Ok(refused) => format!("it refused the Hello: {:?}", refused.reason),
            Err(_) => String::from("it refused the Hello"),
        })),
        kind => Err(LinkError::Other(format!(
            "it answered the Hello with a {kind:?} frame"
        ))),
    }
}

/// The first frame on `connection`, waited for until `deadline`.
fn answer(connection: &Channel, deadline: u64) -> Result<Frame, LinkError> {
    let mut closed = false;
    loop {
        match frame::recv(connection).map_err(transport)? {
            Some(answer) => return Ok(answer),
            None if closed => return Err(LinkError::Closed),
            None => {
                let seen = wait(
                    connection,
                    Signals::READABLE | Signals::PEER_CLOSED,
                    deadline,
                )?;
                closed = seen.contains(Signals::PEER_CLOSED);
            }
        }
    }
}

impl EngineLayer for ImageFs {
    fn mounted(&mut self, deadline: u64) -> Result<(), LinkError> {
        let heard = match &self.0 {
            Said::Mounted => return Ok(()),
            Said::Failed(error) => return Err(error.clone()),
            Said::Waiting { connection, .. } => answer(connection, deadline).and_then(welcomed),
        };
        match &heard {
            // Not yet: the Hello stands, and a later call may hear the answer.
            Err(LinkError::TimedOut) => {}
            // Heard: the connection and the listener go with what was waiting.
            Ok(()) => self.0 = Said::Mounted,
            Err(error) => self.0 = Said::Failed(error.clone()),
        }
        heard
    }
}
