//! The VMBus client: contact, offers, GPADLs, opening and closing channels, rescinds and
//! unload, as a state machine the caller drives.
//!
//! The order of operations is that of OpenVMM's client, read at commit `e7e7df77` as a
//! reference. Each step below names where it is there:
//!
//! - `client:` is `openvmm: vm/devices/vmbus/vmbus_client/src/lib.rs`;
//! - `driver:` is `openvmm: vm/devices/vmbus/vmbus_client/src/driver.rs`;
//! - `server:` is `openvmm: vm/devices/vmbus/vmbus_server/src/channels.rs`, the host's
//!   side, where a step depends on what the host accepts.
//!
//! Where OpenVMM's client panics on a message it does not expect, this one refuses the
//! message with a [`HostError`] and stays as it was.

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;

use guid::Guid;
use vmbus_core::OutgoingMessage;
use vmbus_core::VersionInfo;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::ConnectionState;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::Message;
use vmbus_core::protocol::MessageType;
use vmbus_core::protocol::ParseError;
use vmbus_core::protocol::Version;

use crate::gpadl;
use crate::gpadl::MAX_PAGES;
use crate::hv::Hv;
use crate::hv::HvError;
use crate::hv::HvMessage;
use crate::hv::MAX_PAYLOAD;
use crate::hv::MESSAGE_TYPE;
use crate::hv::MemoryId;
use crate::hv::PostError;

/// The versions the client offers, newest first: every version `vmbus_core` defines from
/// the newest down to Windows 10's, 4.0. Nothing older is ever offered.
pub const VERSIONS: [Version; 7] = [
    Version::Copper,
    Version::Iron,
    Version::Win10Rs5,
    Version::Win10Rs4,
    Version::Win10Rs3_1,
    Version::Win10Rs3_0,
    Version::Win10,
];

/// Channel IDs run from 1 to 2,047: a channel's ID is also its event flag, SINT 2 has
/// 2,048 flags, and flag 0 is not a channel's (`server:1067-1069`, `MAX_CHANNELS`).
pub const MAX_CHANNEL_ID: u32 = 2047;

/// Where messages go before version 5.0, and so for the whole of a 4.0 or 4.1 connection
/// (`openvmm: vm/devices/vmbus/vmbus_server/src/lib.rs:292`, `MESSAGE_CONNECTION_ID`).
const LEGACY_CONNECTION: u32 = 1;

/// Where a request for contact goes from version 5.0 on (the same file, line 293,
/// `MULTICLIENT_MESSAGE_CONNECTION_ID`). The host's answer then names the connection ID
/// for every later message (`server:2539-2544`).
const CONTACT_CONNECTION: u32 = 4;

/// Every channel, and the connection's own messages, target CPU 0 (system design 1F).
const TARGET_VP: u32 = 0;

/// The client opens a channel once at a time, so its open requests need no telling
/// apart (`client:1144`).
const OPEN_ID: u32 = 0;

/// One channel the host offers: a device, or one more channel of a device.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The channel's ID for as long as the offer stands.
    pub channel: ChannelId,
    /// What kind of device it is.
    pub interface: Guid,
    /// Which device of that kind it is.
    pub instance: Guid,
    /// Zero for a device's first channel, and its number among the device's others.
    pub subchannel: u16,
}

/// The memory of a channel's two rings: whole pages, the outgoing ring first and the
/// incoming ring after it. Each ring is one page for its control words and then its data
/// (`driver:133-143`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RingPair {
    /// The memory, as the caller's [`Hv`] knows it.
    pub memory: MemoryId,
    /// How many of its pages are the outgoing ring: at least 2, leaving at least 2.
    pub outgoing_pages: u32,
}

/// Where the connection stands.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClientState {
    /// No contact with the host.
    Disconnected,
    /// A version is offered and the host has not answered.
    Connecting,
    /// The host accepted a version and is sending its offers.
    RequestingOffers,
    /// The first offers are all delivered: channels can be opened.
    Connected,
    /// Unload is asked and the host has not confirmed it.
    Disconnecting,
}

/// Why contact failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// The host refused every version from the newest down to 4.0.
    NoVersion,
    /// The host knows the version and still could not connect.
    Refused(ConnectionState),
}

/// What happened, for the caller to act on. [`Client::poll`] gives them one at a time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The host offers a channel: one of the first offers, or a device added later.
    Offer(Offer),
    /// Contact is made at this version and the first offers have all been given.
    Connected { version: VersionInfo },
    /// Contact failed. The client is disconnected and may try again.
    ConnectFailed(ConnectError),
    /// The host has the GPADL: it may be named to the device.
    GpadlCreated { channel: ChannelId, gpadl: GpadlId },
    /// The host refused the GPADL, with this status. Its memory was never shared.
    GpadlFailed {
        channel: ChannelId,
        gpadl: GpadlId,
        status: i32,
    },
    /// The GPADL is gone: its memory is the caller's alone again.
    GpadlTorndown { channel: ChannelId, gpadl: GpadlId },
    /// The channel is open: its rings carry packets.
    Opened { channel: ChannelId },
    /// The host refused the open, with this status. The ring memory is the caller's alone
    /// again.
    OpenFailed { channel: ChannelId, status: i32 },
    /// The channel is closed and its ring memory is the caller's alone again: after
    /// [`Client::close`], or after [`Event::Rescinded`] for a channel that was open or
    /// opening.
    Closed { channel: ChannelId },
    /// The host took the offer back: the device is gone. The client refuses the channel
    /// from now on, closes it if it was open or opening ([`Event::Closed`] follows), tears
    /// down its other GPADLs ([`Event::GpadlTorndown`] for each), and then forgets it.
    Rescinded { channel: ChannelId },
    /// The host confirmed the unload: the client is disconnected and has forgotten every
    /// channel and GPADL.
    Unloaded,
}

/// Why the client refused what its caller asked. Nothing was sent and nothing changed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// `connect` needs a disconnected client.
    NotDisconnected,
    /// The operation needs contact made and the first offers delivered.
    NotConnected,
    /// The host has offered no such channel, or the client has forgotten it.
    UnknownChannel(ChannelId),
    /// The host rescinded the channel's offer.
    Rescinded(ChannelId),
    /// `open` needs a channel that is neither open, opening nor closing.
    NotClosed(ChannelId),
    /// `close` and `signal_host` need an open channel.
    NotOpen(ChannelId),
    /// The caller's [`Hv`] knows no such memory.
    UnknownMemory(MemoryId),
    /// A GPADL names 1 to [`MAX_PAGES`] pages; the memory has this many.
    PageCount(usize),
    /// Each ring needs a page of control words and at least one of data.
    RingPages { pages: usize, outgoing: u32 },
    /// The channel has no such GPADL.
    UnknownGpadl(GpadlId),
    /// The GPADL is still being created, is already being torn down, or is the channel's
    /// ring, which `close` tears down.
    GpadlBusy(GpadlId),
}

/// What went wrong on the host's side. [`HostError::Post`] is Hyper-V refusing a post;
/// every other kind is the client refusing a message from the host, which is dropped,
/// leaving the client as it was before it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    /// Hyper-V refused to post a message, with this status. The message stays first in
    /// line ([`Client::has_unsent`]); the `vmbus` process treats this as fatal.
    Post(HvError),
    /// Not a VMBus message: SINT 2 carried this SynIC message type.
    NotVmbus(u32),
    /// The SynIC header claims this many bytes of payload; a slot holds 240.
    Length(u8),
    /// Too short to hold its type, or shorter than its type's fields.
    TooSmall(Option<MessageType>),
    /// Not a VMBus message type, or not one the negotiated version has. Before contact is
    /// made, only an answer to it is.
    Unsupported(MessageType),
    /// A message only a guest sends, or the answer to a request this client never makes.
    NotForGuest(MessageType),
    /// A message of the protocol at a point where the protocol does not have it.
    Unexpected(MessageType),
    /// The host accepted a version from 5.0 on and named no connection ID for it.
    ConnectionId(u32),
    /// An offer for a channel ID outside 1 to [`MAX_CHANNEL_ID`].
    ChannelId(ChannelId),
    /// An offer for a channel ID that is already offered.
    DuplicateOffer(ChannelId),
    /// A message about a channel the host has not offered.
    UnknownChannel(ChannelId),
    /// A second rescind of the same offer.
    AlreadyRescinded(ChannelId),
    /// An answer about a GPADL the client did not ask that of.
    Gpadl(GpadlId),
    /// An open result for another open than the one asked.
    OpenId(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotDisconnected => f.write_str("the client is not disconnected"),
            Error::NotConnected => f.write_str("the client is not connected"),
            Error::UnknownChannel(id) => write!(f, "channel {} is not offered", id.0),
            Error::Rescinded(id) => write!(f, "channel {} was rescinded", id.0),
            Error::NotClosed(id) => write!(f, "channel {} is not closed", id.0),
            Error::NotOpen(id) => write!(f, "channel {} is not open", id.0),
            Error::UnknownMemory(id) => write!(f, "memory {:#x} is not known", id.0),
            Error::PageCount(pages) => {
                write!(f, "a GPADL names 1 to {MAX_PAGES} pages, not {pages}")
            }
            Error::RingPages { pages, outgoing } => write!(
                f,
                "a ring pair of {pages} pages cannot give {outgoing} to the outgoing ring"
            ),
            Error::UnknownGpadl(id) => write!(f, "GPADL {:#x} is not known", id.0),
            Error::GpadlBusy(id) => write!(f, "GPADL {:#x} cannot be torn down now", id.0),
        }
    }
}

impl core::error::Error for Error {}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::Post(status) => write!(f, "Hyper-V refused a post: {status}"),
            HostError::NotVmbus(typ) => write!(f, "SynIC message type {typ:#x} is not VMBus's"),
            HostError::Length(len) => write!(f, "a message of {len} bytes in a 240-byte slot"),
            HostError::TooSmall(Some(typ)) => write!(f, "message {} is too short", typ.0),
            HostError::TooSmall(None) => f.write_str("a message too short to have a type"),
            HostError::Unsupported(typ) => write!(f, "message {} is not supported", typ.0),
            HostError::NotForGuest(typ) => write!(f, "message {} is not for a guest", typ.0),
            HostError::Unexpected(typ) => write!(f, "message {} was not expected", typ.0),
            HostError::ConnectionId(id) => write!(f, "connection ID {id} for messages"),
            HostError::ChannelId(id) => write!(f, "an offer of channel ID {}", id.0),
            HostError::DuplicateOffer(id) => write!(f, "channel {} offered twice", id.0),
            HostError::UnknownChannel(id) => {
                write!(f, "a message about channel {}, unoffered", id.0)
            }
            HostError::AlreadyRescinded(id) => write!(f, "channel {} rescinded twice", id.0),
            HostError::Gpadl(id) => write!(f, "an answer about GPADL {:#x}, unasked", id.0),
            HostError::OpenId(id) => write!(f, "an open result for open {id}, unasked"),
        }
    }
}

impl core::error::Error for HostError {}

/// The connection's state, with what each state has to remember (`client:427-456`).
#[derive(Copy, Clone)]
enum State {
    Disconnected,
    Connecting { version: Version },
    RequestingOffers { version: VersionInfo },
    Connected { version: VersionInfo },
    Disconnecting { version: VersionInfo },
}

/// Where one GPADL stands (`client:1625-1635`).
#[derive(Copy, Clone, PartialEq, Eq)]
enum GpadlState {
    /// Its messages are sent, or waiting to be, and the host has not answered.
    Creating,
    /// The host has it.
    Created,
    /// Its teardown is sent, or waiting to be, and the host has not answered.
    TearingDown,
}

/// Where a channel's rings stand (`client:510-531`, with the ring's GPADL, which
/// OpenVMM leaves to the channel's user, `driver:87-173`).
#[derive(Copy, Clone)]
enum Ring {
    /// Offered and not open.
    Closed,
    /// `open` is asked and the ring's GPADL is being created. The incoming ring starts
    /// `incoming_at` pages into it.
    Creating { gpadl: GpadlId, incoming_at: u32 },
    /// The open request is sent, or waiting to be, and the host has not answered.
    Opening { gpadl: GpadlId },
    /// Open.
    Open { gpadl: GpadlId },
    /// The ring's GPADL is being torn down. `then` is what the caller is told once it is
    /// gone.
    Ending { gpadl: GpadlId, then: Event },
}

struct Channel {
    offer: protocol::OfferChannel,
    rescinded: bool,
    ring: Ring,
    /// Every GPADL of the channel, the ring's among them.
    gpadls: BTreeMap<GpadlId, GpadlState>,
}

/// The guest's end of VMBus.
///
/// The caller owns the loop: it asks for something ([`connect`](Client::connect),
/// [`open`](Client::open), ...), then calls [`poll`](Client::poll) until it returns
/// `Ok(None)`, and again whenever SINT 2 interrupts. The client starts no thread and
/// waits for nothing.
pub struct Client {
    state: State,
    /// Where messages go once contact is made.
    connection: u32,
    channels: BTreeMap<ChannelId, Channel>,
    /// The channel of every GPADL: `GpadlTorndown` names the GPADL alone (`client:1746`).
    gpadl_channel: BTreeMap<GpadlId, ChannelId>,
    next_gpadl: u32,
    /// Messages not yet posted, each with the connection ID it goes to (`client:1638-1644`).
    unsent: VecDeque<(u32, OutgoingMessage)>,
    events: VecDeque<Event>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// A disconnected client.
    pub fn new() -> Client {
        Client {
            state: State::Disconnected,
            connection: LEGACY_CONNECTION,
            channels: BTreeMap::new(),
            gpadl_channel: BTreeMap::new(),
            next_gpadl: 1,
            unsent: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    /// Where the connection stands.
    pub fn state(&self) -> ClientState {
        match self.state {
            State::Disconnected => ClientState::Disconnected,
            State::Connecting { .. } => ClientState::Connecting,
            State::RequestingOffers { .. } => ClientState::RequestingOffers,
            State::Connected { .. } => ClientState::Connected,
            State::Disconnecting { .. } => ClientState::Disconnecting,
        }
    }

    /// The version the host accepted, once it has (`client:459-466`).
    pub fn version(&self) -> Option<VersionInfo> {
        match self.state {
            State::RequestingOffers { version }
            | State::Connected { version }
            | State::Disconnecting { version } => Some(version),
            State::Disconnected | State::Connecting { .. } => None,
        }
    }

    /// Every offer that stands, by channel ID. A rescinded offer is not among them.
    pub fn offers(&self) -> impl Iterator<Item = Offer> + '_ {
        self.channels
            .values()
            .filter(|channel| !channel.rescinded)
            .map(|channel| offer_of(&channel.offer))
    }

    /// The whole offer message of a channel, for the fields [`Offer`] leaves out.
    pub fn offer(&self, channel: ChannelId) -> Option<&protocol::OfferChannel> {
        self.channels.get(&channel).map(|channel| &channel.offer)
    }

    /// Whether the channel is open.
    pub fn is_open(&self, channel: ChannelId) -> bool {
        self.channels
            .get(&channel)
            .is_some_and(|channel| matches!(channel.ring, Ring::Open { .. }))
    }

    /// Whether messages wait for a host that was busy. The caller calls
    /// [`poll`](Client::poll) again after a pause while this is true.
    pub fn has_unsent(&self) -> bool {
        !self.unsent.is_empty()
    }

    /// Starts contact: offers the host the newest version (`client:675-679`). The answer
    /// comes through [`poll`](Client::poll): [`Event::Offer`] for each channel, then
    /// [`Event::Connected`], or [`Event::ConnectFailed`].
    pub fn connect(&mut self) -> Result<(), Error> {
        let State::Disconnected = self.state else {
            return Err(Error::NotDisconnected);
        };
        self.contact(VERSIONS[0]);
        Ok(())
    }

    /// Offers the host one version (`client:593-634`).
    ///
    /// The client asks for no feature: it has no client ID, no monitor pages, never
    /// pauses the connection, and takes each channel's event flag and connection ID from
    /// its offer. So the message is `InitiateContact` at every version, which
    /// `InitiateContact2` extends only for a client ID.
    fn contact(&mut self, version: Version) {
        // From 5.0 the field that was the interrupt page says which SINT and VTL the host
        // answers on, and the request goes to its own connection ID (`server:2157-2180`).
        // Before 5.0 the host has ignored the interrupt page since Windows 8
        // (`server:2193-2201`), so it is zero.
        let (connection, target) = if version >= Version::Win10Rs3_1 {
            let target = protocol::TargetInfo::new()
                .with_sint(vmbus_core::VMBUS_SINT)
                .with_vtl(0)
                .with_feature_flags(FeatureFlags::new().into_bits());
            (CONTACT_CONNECTION, target.into_bits())
        } else {
            (LEGACY_CONNECTION, 0)
        };
        let message = protocol::InitiateContact {
            version_requested: version as u32,
            target_message_vp: TARGET_VP,
            interrupt_page_or_target_info: target,
            parent_to_child_monitor_page_gpa: 0,
            child_to_parent_monitor_page_gpa: 0,
        };
        self.unsent
            .push_back((connection, OutgoingMessage::new(&message)));
        self.state = State::Connecting { version };
    }

    /// Shares `memory` with the host as a GPADL of `channel` (`client:1242-1295`). The
    /// answer comes through [`poll`](Client::poll): [`Event::GpadlCreated`] or
    /// [`Event::GpadlFailed`].
    pub fn create_gpadl(
        &mut self,
        hv: &mut impl Hv,
        channel: ChannelId,
        memory: MemoryId,
    ) -> Result<GpadlId, Error> {
        self.usable(channel)?;
        let pages = pages_of(hv, memory)?;
        Ok(self.start_gpadl(channel, &pages))
    }

    /// Tears down a GPADL that [`create_gpadl`](Client::create_gpadl) made
    /// (`client:1297-1341`). [`Event::GpadlTorndown`] says when its memory is the caller's
    /// alone again.
    pub fn teardown_gpadl(&mut self, channel: ChannelId, gpadl: GpadlId) -> Result<(), Error> {
        let entry = self.usable(channel)?;
        let state = entry.gpadls.get(&gpadl).ok_or(Error::UnknownGpadl(gpadl))?;
        let ring = match entry.ring {
            Ring::Closed => None,
            Ring::Creating { gpadl, .. }
            | Ring::Opening { gpadl }
            | Ring::Open { gpadl }
            | Ring::Ending { gpadl, .. } => Some(gpadl),
        };
        if *state != GpadlState::Created || ring == Some(gpadl) {
            return Err(Error::GpadlBusy(gpadl));
        }
        self.start_teardown(channel, gpadl);
        Ok(())
    }

    /// Opens `channel` on the rings in `ring`, targeting CPU 0: first the ring's GPADL,
    /// then the open request (`driver:95-129`). The answer comes through
    /// [`poll`](Client::poll): [`Event::Opened`] or [`Event::OpenFailed`].
    pub fn open(
        &mut self,
        hv: &mut impl Hv,
        channel: ChannelId,
        ring: RingPair,
    ) -> Result<(), Error> {
        let entry = self.usable(channel)?;
        let Ring::Closed = entry.ring else {
            return Err(Error::NotClosed(channel));
        };
        let pages = pages_of(hv, ring.memory)?;
        // The host splits the GPADL at the incoming ring's first page and refuses a split
        // that leaves either side empty (`openvmm:
        // vm/devices/vmbus/vmbus_channel/src/gpadl_ring.rs:54-73`); a ring with a control
        // page and no data carries nothing.
        let outgoing = ring.outgoing_pages as usize;
        if outgoing < 2 || pages.len() < outgoing.saturating_add(2) {
            return Err(Error::RingPages {
                pages: pages.len(),
                outgoing: ring.outgoing_pages,
            });
        }
        let gpadl = self.start_gpadl(channel, &pages);
        if let Some(entry) = self.channels.get_mut(&channel) {
            entry.ring = Ring::Creating {
                gpadl,
                incoming_at: ring.outgoing_pages,
            };
        }
        Ok(())
    }

    /// Closes an open channel: the close request, then the teardown of the ring's GPADL
    /// (`driver:159-169`). The host confirms no close (`client:1588-1605`), so
    /// [`Event::Closed`] comes when the GPADL is gone.
    pub fn close(&mut self, channel: ChannelId) -> Result<(), Error> {
        let entry = self.usable(channel)?;
        let Ring::Open { gpadl } = entry.ring else {
            return Err(Error::NotOpen(channel));
        };
        entry.ring = Ring::Ending {
            gpadl,
            then: Event::Closed { channel },
        };
        self.send(OutgoingMessage::new(&protocol::CloseChannel {
            channel_id: channel,
        }));
        self.start_teardown(channel, gpadl);
        Ok(())
    }

    /// Tells the host there is something for it on an open channel: the connection ID of
    /// the channel's offer, flag 0 (`client:1834-1851`).
    pub fn signal_host(&mut self, hv: &mut impl Hv, channel: ChannelId) -> Result<(), Error> {
        let entry = self.usable(channel)?;
        let Ring::Open { .. } = entry.ring else {
            return Err(Error::NotOpen(channel));
        };
        hv.signal_event(entry.offer.connection_id, 0);
        Ok(())
    }

    /// The open channels the host has signalled since they were last asked about. A
    /// channel's event flag is its ID (`server:1277-1281`); each is tested and cleared.
    pub fn take_signals(&mut self, hv: &mut impl Hv) -> Vec<ChannelId> {
        self.channels
            .iter()
            .filter(|(_, channel)| matches!(channel.ring, Ring::Open { .. }))
            .filter(|(id, _)| hv.test_and_clear_event(id.0 as u16))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Ends the connection (`client:636-644`). [`Event::Unloaded`] comes when the host
    /// confirms it; until then the client refuses every other request.
    pub fn unload(&mut self) -> Result<(), Error> {
        let (State::Connected { version } | State::RequestingOffers { version }) = self.state
        else {
            return Err(Error::NotConnected);
        };
        self.state = State::Disconnecting { version };
        self.send(OutgoingMessage::new(&protocol::Unload {}));
        Ok(())
    }

    /// Does what can be done now, and gives the next event if there is one.
    ///
    /// Each call posts the messages that are waiting, in order, until the host is busy
    /// (`client:1685-1704`), and takes the host's messages one at a time until one of them
    /// is an event for the caller. `Ok(None)` means there is nothing more for now: the
    /// caller waits for SINT 2, or pauses and polls again if
    /// [`has_unsent`](Client::has_unsent).
    ///
    /// An error is either Hyper-V refusing a post ([`HostError::Post`]), or a message from
    /// the host that the client refused. A refused message is dropped, nothing else is
    /// lost, and the next call goes on with the next one.
    pub fn poll(&mut self, hv: &mut impl Hv) -> Result<Option<Event>, HostError> {
        loop {
            self.flush(hv)?;
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            let Some(message) = hv.take_message() else {
                return Ok(None);
            };
            self.handle(&message)?;
        }
    }

    /// Posts the waiting messages, oldest first, and stops at the first the host has no
    /// room for or refuses: it stays first in line.
    fn flush(&mut self, hv: &mut impl Hv) -> Result<(), HostError> {
        while let Some((connection, message)) = self.unsent.front() {
            match hv.post_message(*connection, MESSAGE_TYPE, message.data()) {
                Ok(()) => {
                    self.unsent.pop_front();
                }
                Err(PostError::Busy) => return Ok(()),
                Err(PostError::Hv(status)) => return Err(HostError::Post(status)),
            }
        }
        Ok(())
    }

    /// Queues a message for the connection the host named.
    fn send(&mut self, message: OutgoingMessage) {
        self.unsent.push_back((self.connection, message));
    }

    /// The channel, if the connection and the channel can take a request.
    fn usable(&mut self, channel: ChannelId) -> Result<&mut Channel, Error> {
        let State::Connected { .. } = self.state else {
            return Err(Error::NotConnected);
        };
        let entry = self
            .channels
            .get_mut(&channel)
            .ok_or(Error::UnknownChannel(channel))?;
        if entry.rescinded {
            return Err(Error::Rescinded(channel));
        }
        Ok(entry)
    }

    /// Queues the messages that create a GPADL of `channel` over `pages`, under an ID no
    /// GPADL of the connection has: a body message names its GPADL by that alone.
    fn start_gpadl(&mut self, channel: ChannelId, pages: &[u64]) -> GpadlId {
        let gpadl = loop {
            let id = GpadlId(self.next_gpadl);
            self.next_gpadl = self.next_gpadl.checked_add(1).unwrap_or(1);
            if !self.gpadl_channel.contains_key(&id) {
                break id;
            }
        };
        self.gpadl_channel.insert(gpadl, channel);
        if let Some(entry) = self.channels.get_mut(&channel) {
            entry.gpadls.insert(gpadl, GpadlState::Creating);
        }
        for message in gpadl::create(channel, gpadl, pages) {
            self.unsent.push_back((self.connection, message));
        }
        gpadl
    }

    /// Queues the teardown of a GPADL the host has (`client:1319-1336`).
    fn start_teardown(&mut self, channel: ChannelId, gpadl: GpadlId) {
        if let Some(entry) = self.channels.get_mut(&channel) {
            entry.gpadls.insert(gpadl, GpadlState::TearingDown);
        }
        self.send(OutgoingMessage::new(&protocol::GpadlTeardown {
            channel_id: channel,
            gpadl_id: gpadl,
        }));
    }

    /// Forgets a rescinded channel once nothing of it is left with the host, and tells
    /// the host its ID is free (`client:1779-1795`).
    fn release(&mut self, channel: ChannelId) {
        let gone = self
            .channels
            .get(&channel)
            .is_some_and(|entry| entry.rescinded && entry.gpadls.is_empty());
        if gone {
            self.channels.remove(&channel);
            self.send(OutgoingMessage::new(&protocol::RelIdReleased {
                channel_id: channel,
            }));
        }
    }

    /// Takes one message from the host (`client:1021-1098`).
    fn handle(&mut self, message: &HvMessage) -> Result<(), HostError> {
        let typ = message.header.typ.0;
        if typ != MESSAGE_TYPE {
            return Err(HostError::NotVmbus(typ));
        }
        let len = usize::from(message.header.len);
        if len > MAX_PAYLOAD {
            return Err(HostError::Length(message.header.len));
        }
        let parsed =
            Message::parse(&message.payload_buffer[..len], self.version()).map_err(|error| {
                match error {
                    ParseError::MessageTooSmall(typ) => HostError::TooSmall(typ),
                    ParseError::InvalidMessageType(typ) => HostError::Unsupported(typ),
                }
            })?;

        // Once unload is asked, the host's answers to what came before it no longer
        // matter: everything is forgotten when it confirms.
        if let State::Disconnecting { .. } = self.state {
            if let Message::UnloadComplete(..) = parsed {
                self.unloaded();
            }
            return Ok(());
        }

        match parsed {
            // The three answers to contact differ only in length (`client:1026-1038`).
            Message::VersionResponse3(response, ..) => {
                self.version_response(response.version_response2.version_response)
            }
            Message::VersionResponse2(response, ..) => {
                self.version_response(response.version_response)
            }
            Message::VersionResponse(response, ..) => self.version_response(response),
            Message::OfferChannel(offer, ..) => self.offered(offer),
            Message::AllOffersDelivered(..) => self.offers_delivered(),
            Message::RescindChannelOffer(rescind, ..) => self.rescinded(rescind.channel_id),
            Message::GpadlCreated(created, ..) => self.gpadl_created(created),
            Message::GpadlTorndown(torndown, ..) => self.gpadl_torndown(torndown.gpadl_id),
            Message::OpenResult(result, ..) => self.open_result(result),
            Message::UnloadComplete(..) => Err(HostError::Unexpected(MessageType::UNLOAD_COMPLETE)),
            // Answers to requests this client never makes.
            Message::ModifyChannelResponse(..) => {
                Err(HostError::NotForGuest(MessageType::MODIFY_CHANNEL_RESPONSE))
            }
            Message::ModifyConnectionResponse(..) => Err(HostError::NotForGuest(
                MessageType::MODIFY_CONNECTION_RESPONSE,
            )),
            Message::TlConnectResult(..) => Err(HostError::NotForGuest(
                MessageType::TL_CONNECT_REQUEST_RESULT,
            )),
            Message::CloseReservedChannelResponse(..) => {
                Err(HostError::NotForGuest(MessageType::CLOSE_RESERVED_RESPONSE))
            }
            Message::PauseResponse(..) => Err(HostError::NotForGuest(MessageType::PAUSE_RESPONSE)),
            // Messages only a guest sends (`client:1074-1095`).
            Message::RequestOffers(..) => Err(HostError::NotForGuest(MessageType::REQUEST_OFFERS)),
            Message::OpenChannel2(..) | Message::OpenChannel(..) => {
                Err(HostError::NotForGuest(MessageType::OPEN_CHANNEL))
            }
            Message::CloseChannel(..) => Err(HostError::NotForGuest(MessageType::CLOSE_CHANNEL)),
            Message::GpadlHeader(..) => Err(HostError::NotForGuest(MessageType::GPADL_HEADER)),
            Message::GpadlBody(..) => Err(HostError::NotForGuest(MessageType::GPADL_BODY)),
            Message::GpadlTeardown(..) => Err(HostError::NotForGuest(MessageType::GPADL_TEARDOWN)),
            Message::RelIdReleased(..) => Err(HostError::NotForGuest(MessageType::REL_ID_RELEASED)),
            Message::InitiateContact2(..) | Message::InitiateContact(..) => {
                Err(HostError::NotForGuest(MessageType::INITIATE_CONTACT))
            }
            Message::Unload(..) => Err(HostError::NotForGuest(MessageType::UNLOAD)),
            Message::OpenReservedChannel(..) => {
                Err(HostError::NotForGuest(MessageType::OPEN_RESERVED_CHANNEL))
            }
            Message::CloseReservedChannel(..) => {
                Err(HostError::NotForGuest(MessageType::CLOSE_RESERVED_CHANNEL))
            }
            Message::TlConnectRequest2(..) | Message::TlConnectRequest(..) => {
                Err(HostError::NotForGuest(MessageType::TL_CONNECT_REQUEST))
            }
            Message::ModifyChannel(..) => Err(HostError::NotForGuest(MessageType::MODIFY_CHANNEL)),
            Message::ModifyConnection(..) => {
                Err(HostError::NotForGuest(MessageType::MODIFY_CONNECTION))
            }
            Message::Pause(..) => Err(HostError::NotForGuest(MessageType::PAUSE)),
            Message::Resume(..) => Err(HostError::NotForGuest(MessageType::RESUME)),
        }
    }

    /// The host's answer to an offered version (`client:688-742`).
    fn version_response(&mut self, response: protocol::VersionResponse) -> Result<(), HostError> {
        let State::Connecting { version } = self.state else {
            return Err(HostError::Unexpected(MessageType::VERSION_RESPONSE));
        };
        if response.version_supported == 0 {
            // Refused: the next version down, and below 4.0 none (`client:724-741`).
            let next = VERSIONS
                .iter()
                .skip_while(|offered| **offered != version)
                .nth(1);
            match next {
                Some(next) => self.contact(*next),
                None => {
                    self.state = State::Disconnected;
                    self.events
                        .push_back(Event::ConnectFailed(ConnectError::NoVersion));
                }
            }
            return Ok(());
        }
        if response.connection_state != ConnectionState::SUCCESSFUL {
            self.state = State::Disconnected;
            self.events
                .push_back(Event::ConnectFailed(ConnectError::Refused(
                    response.connection_state,
                )));
            return Ok(());
        }
        // From 5.0 the answer names the connection ID for every later message; before
        // it, the field repeats the version (`server:2539-2544`).
        if version >= Version::Win10Rs3_1 {
            if response.selected_version_or_connection_id == 0 {
                return Err(HostError::ConnectionId(0));
            }
            self.connection = response.selected_version_or_connection_id;
        } else {
            self.connection = LEGACY_CONNECTION;
        }
        // The client asked for no feature, so it has none, whatever the host lists.
        let version = VersionInfo {
            version,
            feature_flags: FeatureFlags::new(),
        };
        // Contact is made: ask for the offers at once (`client:717-722`).
        self.state = State::RequestingOffers { version };
        self.send(OutgoingMessage::new(&protocol::RequestOffers {}));
        Ok(())
    }

    /// An offer: one of the first, or a device added later (`client:785-811`).
    fn offered(&mut self, offer: protocol::OfferChannel) -> Result<(), HostError> {
        let (State::RequestingOffers { .. } | State::Connected { .. }) = self.state else {
            return Err(HostError::Unexpected(MessageType::OFFER_CHANNEL));
        };
        let id = offer.channel_id;
        if id.0 == 0 || id.0 > MAX_CHANNEL_ID {
            return Err(HostError::ChannelId(id));
        }
        if self.channels.contains_key(&id) {
            return Err(HostError::DuplicateOffer(id));
        }
        self.channels.insert(
            id,
            Channel {
                offer,
                rescinded: false,
                ring: Ring::Closed,
                gpadls: BTreeMap::new(),
            },
        );
        self.events.push_back(Event::Offer(offer_of(&offer)));
        Ok(())
    }

    /// The first offers are all given (`client:850-874`).
    fn offers_delivered(&mut self) -> Result<(), HostError> {
        let State::RequestingOffers { version } = self.state else {
            return Err(HostError::Unexpected(MessageType::ALL_OFFERS_DELIVERED));
        };
        self.state = State::Connected { version };
        self.events.push_back(Event::Connected { version });
        Ok(())
    }

    /// The host takes an offer back (`client:813-848`). The channel is closed for the
    /// caller at once; what the host still holds of it is torn down, the ring's GPADL
    /// with no close request since the host has closed it already (`driver:230-235`,
    /// `driver:159-169`), and then the channel is released.
    fn rescinded(&mut self, id: ChannelId) -> Result<(), HostError> {
        let Some(entry) = self.channels.get_mut(&id) else {
            return Err(HostError::UnknownChannel(id));
        };
        if entry.rescinded {
            return Err(HostError::AlreadyRescinded(id));
        }
        entry.rescinded = true;
        // A ring whose GPADL is still being created ends when the host answers for it.
        if let Ring::Opening { gpadl } | Ring::Open { gpadl } = entry.ring {
            entry.ring = Ring::Ending {
                gpadl,
                then: Event::Closed { channel: id },
            };
        }
        let created: Vec<GpadlId> = entry
            .gpadls
            .iter()
            .filter(|(_, state)| **state == GpadlState::Created)
            .map(|(gpadl, _)| *gpadl)
            .collect();
        self.events.push_back(Event::Rescinded { channel: id });
        for gpadl in created {
            self.start_teardown(id, gpadl);
        }
        self.release(id);
        Ok(())
    }

    /// The host's answer to a GPADL (`client:876-903`).
    fn gpadl_created(&mut self, created: protocol::GpadlCreated) -> Result<(), HostError> {
        let (id, gpadl) = (created.channel_id, created.gpadl_id);
        let Some(entry) = self.channels.get_mut(&id) else {
            return Err(HostError::UnknownChannel(id));
        };
        if entry.gpadls.get(&gpadl) != Some(&GpadlState::Creating) {
            return Err(HostError::Gpadl(gpadl));
        }
        let ring = match entry.ring {
            Ring::Creating {
                gpadl: ring,
                incoming_at,
            } if ring == gpadl => Some(incoming_at),
            _ => None,
        };
        let rescinded = entry.rescinded;

        if created.status != protocol::STATUS_SUCCESS {
            entry.gpadls.remove(&gpadl);
            self.gpadl_channel.remove(&gpadl);
            let event = match ring {
                // After a rescind the caller has been told the channel is gone; what is
                // left to say is that its ring memory is free.
                Some(_) if rescinded => Event::Closed { channel: id },
                Some(_) => Event::OpenFailed {
                    channel: id,
                    status: created.status,
                },
                None => Event::GpadlFailed {
                    channel: id,
                    gpadl,
                    status: created.status,
                },
            };
            if ring.is_some() {
                entry.ring = Ring::Closed;
            }
            self.events.push_back(event);
            self.release(id);
            return Ok(());
        }

        entry.gpadls.insert(gpadl, GpadlState::Created);
        match ring {
            // The host has the rings: ask it to open the channel on them, targeting
            // CPU 0 (`client:1142-1151`, `driver:113-129`). With no feature negotiated the
            // request is `OpenChannel` (`client:1188-1197`).
            Some(incoming_at) if !rescinded => {
                entry.ring = Ring::Opening { gpadl };
                self.send(OutgoingMessage::new(&protocol::OpenChannel {
                    channel_id: id,
                    open_id: OPEN_ID,
                    ring_buffer_gpadl_id: gpadl,
                    target_vp: TARGET_VP,
                    downstream_ring_buffer_page_offset: incoming_at,
                    user_data: protocol::UserDefinedData::default(),
                }));
            }
            // Rescinded while the ring's GPADL was on its way: take it back.
            Some(_) => {
                entry.ring = Ring::Ending {
                    gpadl,
                    then: Event::Closed { channel: id },
                };
                self.start_teardown(id, gpadl);
            }
            None => {
                self.events
                    .push_back(Event::GpadlCreated { channel: id, gpadl });
                if rescinded {
                    self.start_teardown(id, gpadl);
                }
            }
        }
        Ok(())
    }

    /// The host's answer to an open request (`client:905-948`).
    fn open_result(&mut self, result: protocol::OpenResult) -> Result<(), HostError> {
        let id = result.channel_id;
        let Some(entry) = self.channels.get_mut(&id) else {
            return Err(HostError::UnknownChannel(id));
        };
        let Ring::Opening { gpadl } = entry.ring else {
            // OpenVMM logs an open result for a channel that is not opening and goes on
            // (`client:915-930`). Refusing it is the same: it is named and dropped.
            return Err(HostError::Unexpected(MessageType::OPEN_CHANNEL_RESULT));
        };
        if result.open_id != OPEN_ID {
            return Err(HostError::OpenId(result.open_id));
        }
        if result.status == protocol::STATUS_SUCCESS as u32 {
            entry.ring = Ring::Open { gpadl };
            self.events.push_back(Event::Opened { channel: id });
        } else {
            // Not open: take the ring's GPADL back, then tell the caller (`driver:211-214`).
            entry.ring = Ring::Ending {
                gpadl,
                then: Event::OpenFailed {
                    channel: id,
                    status: result.status as i32,
                },
            };
            self.start_teardown(id, gpadl);
        }
        Ok(())
    }

    /// The host has let go of a GPADL (`client:950-976`).
    fn gpadl_torndown(&mut self, gpadl: GpadlId) -> Result<(), HostError> {
        let tearing_down = self.gpadl_channel.get(&gpadl).and_then(|id| {
            let entry = self.channels.get_mut(id)?;
            (entry.gpadls.get(&gpadl) == Some(&GpadlState::TearingDown)).then_some((*id, entry))
        });
        let Some((id, entry)) = tearing_down else {
            return Err(HostError::Gpadl(gpadl));
        };
        entry.gpadls.remove(&gpadl);
        let event = match entry.ring {
            Ring::Ending { gpadl: ring, then } if ring == gpadl => {
                entry.ring = Ring::Closed;
                then
            }
            _ => Event::GpadlTorndown { channel: id, gpadl },
        };
        self.gpadl_channel.remove(&gpadl);
        self.events.push_back(event);
        self.release(id);
        Ok(())
    }

    /// The host confirmed the unload (`client:978-988`): it has forgotten every channel
    /// and GPADL, and so does the client.
    fn unloaded(&mut self) {
        self.state = State::Disconnected;
        self.connection = LEGACY_CONNECTION;
        self.channels.clear();
        self.gpadl_channel.clear();
        self.unsent.clear();
        self.events.push_back(Event::Unloaded);
    }
}

/// The short form of an offer message.
fn offer_of(offer: &protocol::OfferChannel) -> Offer {
    Offer {
        channel: offer.channel_id,
        interface: offer.interface_id,
        instance: offer.instance_id,
        subchannel: offer.subchannel_index,
    }
}

/// The page numbers of `memory`, if a GPADL can name them.
fn pages_of(hv: &mut impl Hv, memory: MemoryId) -> Result<Vec<u64>, Error> {
    let pages = hv
        .page_numbers(memory)
        .ok_or(Error::UnknownMemory(memory))?;
    if pages.is_empty() || pages.len() > MAX_PAGES {
        return Err(Error::PageCount(pages.len()));
    }
    Ok(pages)
}
