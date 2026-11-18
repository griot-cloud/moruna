//! The client side (SDD 23, f.3): the handshake, calls matched to their replies by
//! request id, and events. A protocol's typed client (`protocol!`) wraps a
//! [`ClientCore`] and decodes what it returns.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};

use moruna_abi::{DEADLINE_INFINITE, Error, Signals};

use crate::{
    frame::{self, Frame, Kind, Transport},
    os::{Channel, now, wait_one},
    protocol::{ProtocolError, ProtocolId, Refused, Status},
    wire::{Encoder, Wire, WireError},
};

/// Why a call, a handshake or a wait for an event failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError<E> {
    /// The server closed the channel.
    PeerClosed,
    /// The deadline passed first.
    TimedOut,
    /// The request would not fit the channel's bound: refused before it was sent.
    TooLarge,
    /// The server's queue is full: it is not reading.
    Full,
    /// The server refused the handshake.
    Refused(Refused),
    /// The server's minor version predates this request; it was not sent.
    Unsupported,
    /// The server answered with one of the protocol's errors.
    Remote(E),
    /// The server answered with one of the SDK's statuses.
    Status(Status),
    /// The server sent something this protocol does not allow.
    Malformed,
    /// Any other refusal by the kernel.
    Kernel(Error),
}

impl<E> From<Transport> for CallError<E> {
    fn from(t: Transport) -> CallError<E> {
        match t {
            Transport::TooLarge => CallError::TooLarge,
            Transport::Full => CallError::Full,
            Transport::PeerClosed => CallError::PeerClosed,
            Transport::Malformed => CallError::Malformed,
            Transport::Unsupported => CallError::Unsupported,
            Transport::Kernel(e) => CallError::Kernel(e),
        }
    }
}

impl<E> From<WireError> for CallError<E> {
    fn from(e: WireError) -> CallError<E> {
        match e {
            WireError::TooLarge => CallError::TooLarge,
            _ => CallError::Malformed,
        }
    }
}

impl CallError<u32> {
    /// The same error with the protocol's code decoded; a code the protocol does not
    /// declare is `Malformed`.
    pub fn typed<E: ProtocolError>(self) -> CallError<E> {
        match self {
            CallError::Remote(code) => match E::from_code(code) {
                Some(e) => CallError::Remote(e),
                None => CallError::Malformed,
            },
            CallError::PeerClosed => CallError::PeerClosed,
            CallError::TimedOut => CallError::TimedOut,
            CallError::TooLarge => CallError::TooLarge,
            CallError::Full => CallError::Full,
            CallError::Refused(r) => CallError::Refused(r),
            CallError::Unsupported => CallError::Unsupported,
            CallError::Status(s) => CallError::Status(s),
            CallError::Malformed => CallError::Malformed,
            CallError::Kernel(e) => CallError::Kernel(e),
        }
    }
}

/// A deadline `timeout` nanoseconds from now; infinite stays infinite.
pub fn deadline_in(timeout: u64) -> u64 {
    if timeout == DEADLINE_INFINITE {
        DEADLINE_INFINITE
    } else {
        now().saturating_add(timeout).min(DEADLINE_INFINITE - 1)
    }
}

/// Waits until `channel` is readable or closed, or the deadline passes.
fn readable(channel: &Channel, deadline: u64) -> Result<(), CallError<u32>> {
    match wait_one(
        &channel.0,
        Signals::READABLE | Signals::PEER_CLOSED,
        deadline,
    ) {
        Ok(_) => Ok(()),
        Err(Error::TimedOut) => Err(CallError::TimedOut),
        Err(e) => Err(CallError::Kernel(e)),
    }
}

/// Reads the next frame, waiting for one until the deadline.
fn next_frame(channel: &Channel, deadline: u64) -> Result<Frame, CallError<u32>> {
    loop {
        if let Some(frame) = frame::recv(channel)? {
            return Ok(frame);
        }
        readable(channel, deadline)?;
    }
}

/// Opens a connection to the server listening on `listener` (SDD 23, f.4): a new
/// channel whose other end is sent as a `Connect` frame. The handshake is the
/// caller's (a typed client's `connect`).
pub fn connect(listener: &Channel) -> Result<Channel, CallError<u32>> {
    let (mine, theirs) = Channel::create().map_err(CallError::Kernel)?;
    let mut bytes = [0u8; frame::HEADER];
    bytes[0] = Kind::Connect as u8;
    let mut handles = [Some(theirs.0)];
    listener
        .write_with(&bytes, &mut handles)
        .map_err(|e| CallError::from(Transport::from(e)))?;
    Ok(mine)
}

/// How long a client waits for a server it was linked to at boot to come up. A server answers
/// its first hello only once it serves, after its own start (its devices, its disks, its
/// layers), which on a loaded machine under QEMU's emulator has taken more than the 5 s a check
/// gave its whole exchange. A check waits this long for the server ([`ready`]) and times only
/// its exchange after that.
pub const READY_TIMEOUT: u64 = 300 * 1_000_000_000;

/// The server listening on `listener`, up: a connection whose `Hello` for `id` it welcomed,
/// waited for until `deadline` (from [`READY_TIMEOUT`]). Its start is over by the time this
/// returns, so a check that times the server's behaviour starts its clock after it.
pub fn ready(
    listener: &Channel,
    id: ProtocolId,
    deadline: u64,
) -> Result<ClientCore, CallError<u32>> {
    ClientCore::connect(connect(listener)?, id, deadline)
}

/// One connection's client: the channel, the server's version, the request ids in
/// flight, and the events and early replies read while waiting for another reply.
pub struct ClientCore {
    channel: Channel,
    id: ProtocolId,
    server_minor: u16,
    next_txid: u32,
    in_flight: BTreeSet<u32>,
    replies: BTreeMap<u32, Frame>,
    events: VecDeque<Frame>,
    timeout: u64,
}

impl ClientCore {
    /// Says `Hello` for `id` on `channel` and waits for the answer until `deadline`.
    pub fn connect(
        channel: Channel,
        id: ProtocolId,
        deadline: u64,
    ) -> Result<ClientCore, CallError<u32>> {
        let mut hello = Encoder::new();
        id.hello().encode(&mut hello)?;
        frame::send(&channel, Kind::Hello, 0, 0, hello)?;
        let mut answer = next_frame(&channel, deadline)?;
        let server_minor = match answer.kind {
            Kind::Welcome => {
                let mut d = answer.decoder();
                let major = u16::decode(&mut d)?;
                let minor = u16::decode(&mut d)?;
                d.finish()?;
                if major != id.major {
                    return Err(CallError::Malformed);
                }
                minor
            }
            Kind::Refuse => {
                let mut d = answer.decoder();
                let refused = Refused::decode(&mut d)?;
                d.finish()?;
                return Err(CallError::Refused(refused));
            }
            _ => return Err(CallError::Malformed),
        };
        Ok(ClientCore {
            channel,
            id,
            server_minor,
            next_txid: 1,
            in_flight: BTreeSet::new(),
            replies: BTreeMap::new(),
            events: VecDeque::new(),
            timeout: DEADLINE_INFINITE,
        })
    }

    /// The protocol this client speaks.
    pub fn protocol(&self) -> ProtocolId {
        self.id
    }

    /// The server's minor version, from its `Welcome`.
    pub fn server_minor(&self) -> u16 {
        self.server_minor
    }

    /// How long a call waits for its reply, in nanoseconds; infinite by default.
    pub fn set_timeout(&mut self, timeout: u64) {
        self.timeout = timeout;
    }

    /// The channel, for waiting on it alongside others.
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Sends request `ordinal` with `body` and returns its id, without waiting.
    pub fn send(&mut self, ordinal: u16, body: Encoder) -> Result<u32, CallError<u32>> {
        let txid = self.next_txid;
        self.next_txid = self.next_txid.checked_add(1).unwrap_or(1);
        frame::send(&self.channel, Kind::Request, ordinal, txid, body)?;
        self.in_flight.insert(txid);
        Ok(txid)
    }

    /// Reads one frame and files it: an event in the queue, a reply under its id.
    fn pump(&mut self, deadline: u64) -> Result<(), CallError<u32>> {
        let frame = next_frame(&self.channel, deadline)?;
        match frame.kind {
            Kind::Event if frame.txid == 0 => self.events.push_back(frame),
            Kind::Reply | Kind::Failure if self.in_flight.remove(&frame.txid) => {
                self.replies.insert(frame.txid, frame);
            }
            _ => return Err(CallError::Malformed),
        }
        Ok(())
    }

    /// A filed reply as its caller is given it: a `Failure` as the error it names.
    fn answer(mut frame: Frame) -> Result<Frame, CallError<u32>> {
        if frame.kind == Kind::Reply {
            return Ok(frame);
        }
        let mut d = frame.decoder();
        let code = u32::decode(&mut d)?;
        d.finish()?;
        Err(match Status::from_code(code) {
            Some(status) => CallError::Status(status),
            None if code != 0 && code < Status::FIRST => CallError::Remote(code),
            None => CallError::Malformed,
        })
    }

    /// Waits for the reply to request `txid`. A `Failure` is returned as the error it
    /// names.
    pub fn reply(&mut self, txid: u32, deadline: u64) -> Result<Frame, CallError<u32>> {
        loop {
            if let Some(frame) = self.replies.remove(&txid) {
                return Self::answer(frame);
            }
            if !self.in_flight.contains(&txid) {
                return Err(CallError::Malformed);
            }
            self.pump(deadline)?;
        }
    }

    /// Reads one frame, waiting for it until `deadline`, and files it: a reply under its id,
    /// for whoever takes it ([`ClientCore::take_reply`]), an event in the queue. For a client
    /// several threads share, whose one reader waits on the channel itself, holding no lock
    /// while it waits, and then files what has arrived: `TimedOut` once nothing more is there.
    pub fn file(&mut self, deadline: u64) -> Result<(), CallError<u32>> {
        self.pump(deadline)
    }

    /// Reads every frame there is, without waiting, and files each as [`ClientCore::file`]
    /// does: how many it filed. `TimedOut` never comes from it: an empty channel ends it.
    ///
    /// **Read the channel to its end, file everything, then take each reply** (work item
    /// 1.3c). A client that waits for its channel to be readable and then asks for its
    /// replies one at a time without waiting ([`ClientCore::reply`] with a deadline already
    /// passed) loses wake-ups: asking for one reply reads, and files, every other answer it
    /// meets first, so an answer that comes after its own was asked for, and is read while
    /// another is looked for, sits filed with the channel empty, and nothing wakes the
    /// client for it again. Read with this once a turn, then take each reply
    /// ([`ClientCore::take_reply`]); an answer that comes after keeps the channel readable,
    /// and the next turn takes it.
    pub fn file_all(&mut self) -> Result<usize, CallError<u32>> {
        let mut filed = 0;
        loop {
            match self.pump(0) {
                Ok(()) => filed += 1,
                Err(CallError::TimedOut) => return Ok(filed),
                Err(error) => return Err(error),
            }
        }
    }

    /// The reply to request `txid` if it has been filed, without waiting, as
    /// [`ClientCore::reply`] gives it; `None` while it is still to come. A `txid` this client
    /// never sent, or whose reply was already taken, is `Malformed`.
    pub fn take_reply(&mut self, txid: u32) -> Option<Result<Frame, CallError<u32>>> {
        match self.replies.remove(&txid) {
            Some(frame) => Some(Self::answer(frame)),
            None if self.in_flight.contains(&txid) => None,
            None => Some(Err(CallError::Malformed)),
        }
    }

    /// Sends a request and waits for its reply, within the client's timeout.
    pub fn call(&mut self, ordinal: u16, body: Encoder) -> Result<Frame, CallError<u32>> {
        let deadline = deadline_in(self.timeout);
        let txid = self.send(ordinal, body)?;
        self.reply(txid, deadline)
    }

    /// The next event, waiting for one until `deadline`.
    pub fn next_event(&mut self, deadline: u64) -> Result<Frame, CallError<u32>> {
        loop {
            if let Some(frame) = self.events.pop_front() {
                return Ok(frame);
            }
            self.pump(deadline)?;
        }
    }

    /// Gives the channel back, ending the connection's client.
    pub fn into_channel(self) -> Channel {
        self.channel
    }
}
