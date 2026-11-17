//! A fake host: the host's side of VMBus, built from `vmbus_core`'s messages, and the
//! SynIC it reaches the guest through.
//!
//! It is strict. A message of the wrong size, to the wrong connection ID, or at a point
//! where the protocol does not have it is a panic that says which rule the client broke,
//! so a test that passes has kept every rule on its way. The rules are the ones OpenVMM's
//! server keeps (`openvmm: vm/devices/vmbus/vmbus_server/src/channels.rs`), plus the ones
//! system design 1F adds: every channel targets CPU 0, and no version below 4.0 is asked.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;

use hvdef::HvMessageType;
use hvdef::Vtl;
use moruna_vmbus::Busy;
use moruna_vmbus::ChannelId;
use moruna_vmbus::Client;
use moruna_vmbus::ConnectionState;
use moruna_vmbus::Event;
use moruna_vmbus::GpadlId;
use moruna_vmbus::Guid;
use moruna_vmbus::Hv;
use moruna_vmbus::HvMessage;
use moruna_vmbus::MemoryId;
use moruna_vmbus::MessageType;
use moruna_vmbus::Version;
use moruna_vmbus::VersionInfo;
use vmbus_core::OutgoingMessage;
use vmbus_core::protocol;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::Message;
use vmbus_ring::gparange::MultiPagedRangeBuf;

/// Every version the protocol has had, oldest first: what a host of today accepts.
pub const ALL_VERSIONS: [Version; 11] = [
    Version::V1,
    Version::Win7,
    Version::Win8,
    Version::Win8_1,
    Version::Win10,
    Version::Win10Rs3_0,
    Version::Win10Rs3_1,
    Version::Win10Rs4,
    Version::Win10Rs5,
    Version::Iron,
    Version::Copper,
];

/// The connection ID this host names for messages once contact is made at 5.0 or later.
/// Neither 1 nor 4, so a client that ignores the answer is caught.
pub const NAMED_CONNECTION: u32 = 0x1234;

pub const NIC: Guid = guid::guid!("f8615163-df3e-46c5-913f-f2d2f965ed0e");
pub const SCSI: Guid = guid::guid!("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f");

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Connection {
    Disconnected,
    Connected { version: Version, offers_sent: bool },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ChannelState {
    Closed,
    Open { ring: u32 },
}

#[derive(Debug)]
struct Channel {
    state: ChannelState,
    rescinded: bool,
}

#[derive(Debug)]
struct Gpadl {
    channel: u32,
    /// How many 8-byte values the header promised.
    values: usize,
    buf: Vec<u64>,
    accepted: bool,
}

pub struct FakeHost {
    /// The versions this host accepts.
    pub versions: Vec<Version>,
    /// What it says of a version it accepts.
    pub connection_state: ConnectionState,
    /// The features it lists when it accepts 6.0.
    pub features: FeatureFlags,
    /// The devices it offers when asked: interface and instance.
    pub devices: Vec<(Guid, Guid)>,
    /// How many posts it refuses as busy before it takes one.
    pub busy: usize,
    /// Turn busy later: once it has taken this many posts, refuse this many.
    pub busy_at: Option<(usize, usize)>,
    /// The status of the next GPADL, if it is to fail.
    pub fail_gpadl: Option<i32>,
    /// The status of the next open, if it is to fail.
    pub fail_open: Option<u32>,
    /// Hold back the answers to GPADLs, to opens, to teardowns, until `release`.
    pub hold_gpadl: bool,
    pub hold_open: bool,
    pub hold_teardown: bool,
    /// The guest's memory, as its `Hv` knows it: page numbers by ID.
    pub memory: BTreeMap<u64, Vec<u64>>,

    /// Every message the guest posted that the host took: connection ID and type.
    pub posted: Vec<(u32, MessageType)>,
    /// Every version the guest ever asked for.
    pub asked: Vec<u32>,
    /// How many posts were refused as busy.
    pub refused: usize,
    /// Every event the guest signalled: connection ID and flag.
    pub signals: Vec<(u32, u16)>,
    /// Every channel ID the guest released.
    pub released: Vec<u32>,

    connection: Connection,
    channels: BTreeMap<u32, Channel>,
    gpadls: BTreeMap<u32, Gpadl>,
    next_channel: u32,
    held: VecDeque<OutgoingMessage>,
    /// The last version asked in this attempt at contact.
    last_asked: Option<u32>,
    /// SINT 2's message slot, and what waits behind it.
    inbox: VecDeque<HvMessage>,
    /// SINT 2's event flags that are set.
    flags: BTreeSet<u16>,
}

impl FakeHost {
    /// A host of today: every version, no device.
    pub fn new() -> FakeHost {
        FakeHost {
            versions: ALL_VERSIONS.to_vec(),
            connection_state: ConnectionState::SUCCESSFUL,
            features: FeatureFlags::new(),
            devices: Vec::new(),
            busy: 0,
            busy_at: None,
            fail_gpadl: None,
            fail_open: None,
            hold_gpadl: false,
            hold_open: false,
            hold_teardown: false,
            memory: BTreeMap::new(),
            posted: Vec::new(),
            asked: Vec::new(),
            refused: 0,
            signals: Vec::new(),
            released: Vec::new(),
            connection: Connection::Disconnected,
            channels: BTreeMap::new(),
            gpadls: BTreeMap::new(),
            next_channel: 1,
            held: VecDeque::new(),
            last_asked: None,
            inbox: VecDeque::new(),
            flags: BTreeSet::new(),
        }
    }

    /// A host that accepts nothing newer than `newest`.
    pub fn up_to(newest: Version) -> FakeHost {
        let mut host = FakeHost::new();
        host.versions.retain(|version| *version <= newest);
        host
    }

    /// Gives the guest `pages` pages of memory under `id`, at page numbers from `first`.
    pub fn add_memory(&mut self, id: u64, first: u64, pages: usize) -> MemoryId {
        self.memory
            .insert(id, (first..first + pages as u64).collect());
        MemoryId(id)
    }

    /// The types of the messages the guest posted, in order.
    pub fn types(&self) -> Vec<MessageType> {
        self.posted.iter().map(|(_, typ)| *typ).collect()
    }

    /// The page numbers of a GPADL the host has accepted.
    pub fn gpadl_pages(&self, gpadl: GpadlId) -> Option<Vec<u64>> {
        let gpadl = self.gpadls.get(&gpadl.0)?;
        gpadl.accepted.then(|| gpadl.buf[1..].to_vec())
    }

    /// How many GPADLs the host holds, whole or in pieces.
    pub fn gpadl_count(&self) -> usize {
        self.gpadls.len()
    }

    /// Whether the host has the channel open.
    pub fn is_open(&self, channel: ChannelId) -> bool {
        self.channels
            .get(&channel.0)
            .is_some_and(|channel| matches!(channel.state, ChannelState::Open { .. }))
    }

    /// The GPADL of an open channel's rings.
    pub fn ring_gpadl(&self, channel: ChannelId) -> Option<GpadlId> {
        match self.channels.get(&channel.0)?.state {
            ChannelState::Open { ring } => Some(GpadlId(ring)),
            ChannelState::Closed => None,
        }
    }

    /// Whether the host still knows the channel.
    pub fn knows(&self, channel: ChannelId) -> bool {
        self.channels.contains_key(&channel.0)
    }

    /// Offers one more device to a connected guest.
    pub fn offer(&mut self, interface: Guid, instance: Guid) -> ChannelId {
        assert!(
            matches!(
                self.connection,
                Connection::Connected {
                    offers_sent: true,
                    ..
                }
            ),
            "the host offers a device only after the first offers"
        );
        self.send_offer(interface, instance)
    }

    /// Takes a channel's offer back.
    pub fn rescind(&mut self, channel: ChannelId) {
        let entry = self.channels.get_mut(&channel.0).expect("a known channel");
        entry.rescinded = true;
        entry.state = ChannelState::Closed;
        self.send(OutgoingMessage::new(&protocol::RescindChannelOffer {
            channel_id: channel,
        }));
    }

    /// Signals the guest on a channel: sets the channel's event flag.
    pub fn signal_guest(&mut self, channel: ChannelId) {
        self.flags.insert(channel.0 as u16);
    }

    /// Tests a channel's event flag and clears it, as the guest does.
    pub fn test_and_clear(&mut self, channel: ChannelId) -> bool {
        self.flags.remove(&(channel.0 as u16))
    }

    /// Lets go of the answers held back by `hold`, oldest first.
    pub fn release(&mut self) {
        while let Some(held) = self.held.pop_front() {
            self.send(held);
        }
    }

    /// Queues a VMBus message for the guest.
    pub fn send(&mut self, message: OutgoingMessage) {
        self.send_bytes(message.data());
    }

    /// Queues any bytes for the guest as a VMBus message.
    pub fn send_bytes(&mut self, payload: &[u8]) {
        self.inbox
            .push_back(HvMessage::new(HvMessageType(1), 0, payload));
    }

    /// Queues a SynIC message exactly as given, whatever its header says.
    pub fn send_slot(&mut self, message: HvMessage) {
        self.inbox.push_back(message);
    }

    fn version(&self) -> Option<VersionInfo> {
        match self.connection {
            Connection::Disconnected => None,
            Connection::Connected { version, .. } => Some(VersionInfo {
                version,
                feature_flags: FeatureFlags::new(),
            }),
        }
    }

    fn send_offer(&mut self, interface: Guid, instance: Guid) -> ChannelId {
        let id = self.next_channel;
        self.next_channel += 1;
        self.channels.insert(
            id,
            Channel {
                state: ChannelState::Closed,
                rescinded: false,
            },
        );
        self.send(OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: interface,
            instance_id: instance,
            rsvd: [0; 4],
            flags: protocol::OfferFlags::new(),
            mmio_megabytes: 0,
            user_defined: protocol::UserDefinedData::default(),
            subchannel_index: 0,
            mmio_megabytes_optional: 0,
            channel_id: ChannelId(id),
            monitor_id: 0xff,
            monitor_allocated: 0,
            is_dedicated: 1,
            connection_id: connection_of(ChannelId(id)),
        }));
        ChannelId(id)
    }

    fn initiate_contact(&mut self, connection: u32, request: protocol::InitiateContact) {
        assert_eq!(
            self.connection,
            Connection::Disconnected,
            "contact is asked of a host that is not connected"
        );
        let asked = request.version_requested;
        if let Some(last) = self.last_asked {
            assert!(asked < last, "each version asked is older than the last");
        }
        self.last_asked = Some(asked);
        self.asked.push(asked);
        assert!(
            asked >= Version::Win10 as u32,
            "no version below 4.0 is asked"
        );
        assert_eq!(request.target_message_vp, 0, "messages target CPU 0");
        assert_eq!(request.parent_to_child_monitor_page_gpa, 0);
        assert_eq!(request.child_to_parent_monitor_page_gpa, 0);
        if asked >= Version::Win10Rs3_1 as u32 {
            assert_eq!(connection, 4, "from 5.0, contact is asked on connection 4");
            let target = protocol::TargetInfo::from_bits(request.interrupt_page_or_target_info);
            assert_eq!(target.sint(), 2, "the guest listens on SINT 2");
            assert_eq!(target.vtl(), 0);
            assert_eq!(request.interrupt_page_or_target_info >> 16 & 0xffff, 0);
            assert_eq!(target.feature_flags(), 0, "the client asks for no feature");
        } else {
            assert_eq!(
                connection, 1,
                "before 5.0, contact is asked on connection 1"
            );
            assert_eq!(request.interrupt_page_or_target_info, 0);
        }

        let Some(version) = self
            .versions
            .iter()
            .copied()
            .find(|version| *version as u32 == asked)
        else {
            if asked == Version::Win10 as u32 {
                // Nothing older will be asked: the next request starts a new attempt.
                self.last_asked = None;
            }
            self.send(OutgoingMessage::new(&protocol::VersionResponse {
                version_supported: 0,
                connection_state: ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 0,
            }));
            return;
        };
        let response = protocol::VersionResponse {
            version_supported: 1,
            connection_state: self.connection_state,
            padding: 0,
            selected_version_or_connection_id: if version >= Version::Win10Rs3_1 {
                NAMED_CONNECTION
            } else {
                version as u32
            },
        };
        if version >= Version::Copper {
            self.send(OutgoingMessage::new(&protocol::VersionResponse2 {
                version_response: response,
                supported_features: self.features.into_bits(),
            }));
        } else {
            self.send(OutgoingMessage::new(&response));
        }
        // The attempt is over either way: the next one starts from the newest again.
        self.last_asked = None;
        if self.connection_state == ConnectionState::SUCCESSFUL {
            self.connection = Connection::Connected {
                version,
                offers_sent: false,
            };
        }
    }

    fn request_offers(&mut self) {
        let Connection::Connected {
            version,
            offers_sent: false,
        } = self.connection
        else {
            panic!("offers are asked once, of a connected host");
        };
        self.connection = Connection::Connected {
            version,
            offers_sent: true,
        };
        for (interface, instance) in self.devices.clone() {
            self.send_offer(interface, instance);
        }
        self.send(OutgoingMessage::new(&protocol::AllOffersDelivered {}));
    }

    fn gpadl_header(&mut self, header: protocol::GpadlHeader, values: &[u8]) {
        let channel = header.channel_id.0;
        assert!(
            self.channels.contains_key(&channel),
            "a GPADL names a channel the host offers"
        );
        assert_eq!(header.count, 1, "the client's GPADLs are one range");
        assert_eq!(header.len % 8, 0, "a GPADL is whole 8-byte values");
        let promised = usize::from(header.len) / 8;
        assert!(promised >= 2, "a GPADL has a range and a page");
        let carried = promised.min(protocol::GpadlHeader::MAX_DATA_VALUES);
        assert_eq!(
            values.len(),
            carried * 8,
            "the header carries as many values as fit, and no more than the GPADL has"
        );
        assert_ne!(header.gpadl_id.0, 0);
        let previous = self.gpadls.insert(
            header.gpadl_id.0,
            Gpadl {
                channel,
                values: promised,
                buf: values_of(values),
                accepted: false,
            },
        );
        assert!(previous.is_none(), "a GPADL ID is used once at a time");
        self.gpadl_progress(header.gpadl_id);
    }

    fn gpadl_body(&mut self, body: protocol::GpadlBody, values: &[u8]) {
        assert_eq!(body.rsvd, 0);
        let gpadl = self
            .gpadls
            .get_mut(&body.gpadl_id.0)
            .expect("a body continues a GPADL whose header came first");
        assert!(
            !gpadl.accepted,
            "a body continues a GPADL that is not whole"
        );
        let left = gpadl.values - gpadl.buf.len();
        let carried = left.min(protocol::GpadlBody::MAX_DATA_VALUES);
        assert_eq!(
            values.len(),
            carried * 8,
            "a body carries as many values as fit, and no more than are left"
        );
        gpadl.buf.extend(values_of(values));
        self.gpadl_progress(body.gpadl_id);
    }

    /// Answers a GPADL once all its values have come.
    fn gpadl_progress(&mut self, id: GpadlId) {
        let gpadl = &self.gpadls[&id.0];
        if gpadl.buf.len() < gpadl.values {
            return;
        }
        let ranges = MultiPagedRangeBuf::from_range_buffer(1, gpadl.buf.clone())
            .expect("the GPADL's values describe one valid range");
        let range = ranges.first().expect("one range");
        assert_eq!(range.offset(), 0, "the range starts at its first page");
        assert_eq!(range.len(), range.gpns().len() * 4096, "whole pages");
        assert_eq!(
            range.gpns().len(),
            gpadl.values - 1,
            "a page number for each page"
        );
        let channel = ChannelId(gpadl.channel);

        let status = self.fail_gpadl.take().unwrap_or(protocol::STATUS_SUCCESS);
        if status == protocol::STATUS_SUCCESS {
            self.gpadls.get_mut(&id.0).expect("checked").accepted = true;
        } else {
            self.gpadls.remove(&id.0);
        }
        let answer = OutgoingMessage::new(&protocol::GpadlCreated {
            channel_id: channel,
            gpadl_id: id,
            status,
        });
        if self.hold_gpadl {
            self.held.push_back(answer);
        } else {
            self.send(answer);
        }
    }

    fn open_channel(&mut self, request: protocol::OpenChannel) {
        let id = request.channel_id.0;
        let channel = self
            .channels
            .get(&id)
            .expect("an open names a channel the host offers");
        if channel.rescinded {
            // The real host ignores an open for a channel it has rescinded.
            return;
        }
        assert_eq!(
            channel.state,
            ChannelState::Closed,
            "an open is asked of a closed channel"
        );
        assert_eq!(request.target_vp, 0, "every channel targets CPU 0");
        assert_eq!(request.open_id, 0);
        assert!(request.user_data.iter().all(|byte| *byte == 0));
        let ring = request.ring_buffer_gpadl_id.0;
        let gpadl = self
            .gpadls
            .get(&ring)
            .expect("the ring's GPADL is created before the open");
        assert!(gpadl.accepted, "the ring's GPADL is whole before the open");
        assert_eq!(gpadl.channel, id, "the ring's GPADL is the channel's own");
        let pages = gpadl.values - 1;
        let split = request.downstream_ring_buffer_page_offset as usize;
        assert!(
            split >= 2 && pages >= split + 2,
            "each ring has a control page and data: {pages} pages split at {split}"
        );

        let status = self
            .fail_open
            .take()
            .unwrap_or(protocol::STATUS_SUCCESS as u32);
        if status == protocol::STATUS_SUCCESS as u32 {
            self.channels.get_mut(&id).expect("checked").state = ChannelState::Open { ring };
        }
        let answer = OutgoingMessage::new(&protocol::OpenResult {
            channel_id: request.channel_id,
            open_id: request.open_id,
            status,
        });
        if self.hold_open {
            self.held.push_back(answer);
        } else {
            self.send(answer);
        }
    }

    fn close_channel(&mut self, request: protocol::CloseChannel) {
        let channel = self
            .channels
            .get_mut(&request.channel_id.0)
            .expect("a close names a channel the host offers");
        if channel.rescinded {
            return;
        }
        assert!(
            matches!(channel.state, ChannelState::Open { .. }),
            "a close is asked of an open channel"
        );
        channel.state = ChannelState::Closed;
    }

    fn gpadl_teardown(&mut self, request: protocol::GpadlTeardown) {
        let gpadl = self
            .gpadls
            .get(&request.gpadl_id.0)
            .expect("a teardown names a GPADL the host has");
        assert!(gpadl.accepted, "a teardown names a GPADL that is whole");
        assert_eq!(
            gpadl.channel, request.channel_id.0,
            "a teardown names the GPADL's own channel"
        );
        let channel = &self.channels[&gpadl.channel];
        assert_ne!(
            channel.state,
            ChannelState::Open {
                ring: request.gpadl_id.0
            },
            "the ring's GPADL is torn down after the channel is closed"
        );
        self.gpadls.remove(&request.gpadl_id.0);
        let answer = OutgoingMessage::new(&protocol::GpadlTorndown {
            gpadl_id: request.gpadl_id,
        });
        if self.hold_teardown {
            self.held.push_back(answer);
        } else {
            self.send(answer);
        }
    }

    fn rel_id_released(&mut self, request: protocol::RelIdReleased) {
        let id = request.channel_id.0;
        let channel = self
            .channels
            .get(&id)
            .expect("a release names a channel the host offers");
        assert!(channel.rescinded, "only a rescinded channel is released");
        assert!(
            self.gpadls.values().all(|gpadl| gpadl.channel != id),
            "a channel is released with none of its GPADLs left"
        );
        self.channels.remove(&id);
        self.released.push(id);
    }

    fn unload(&mut self) {
        assert!(
            matches!(self.connection, Connection::Connected { .. }),
            "unload is asked of a connected host"
        );
        self.connection = Connection::Disconnected;
        self.channels.clear();
        self.gpadls.clear();
        self.held.clear();
        self.last_asked = None;
        self.send(OutgoingMessage::new(&protocol::UnloadComplete {}));
    }
}

impl Hv for FakeHost {
    fn post_message(
        &mut self,
        connection_id: u32,
        message_type: u32,
        payload: &[u8],
    ) -> Result<(), Busy> {
        if let Some((at, times)) = self.busy_at
            && self.posted.len() == at
        {
            self.busy = times;
            self.busy_at = None;
        }
        if self.busy > 0 {
            self.busy -= 1;
            self.refused += 1;
            return Err(Busy);
        }
        assert_eq!(message_type, 1, "VMBus messages are SynIC type 1");
        assert!(payload.len() <= 240, "a message fits a SynIC slot");
        let message = Message::parse(payload, self.version())
            .unwrap_or_else(|error| panic!("the host cannot read the guest's message: {error}"));
        let typ = u32::from_le_bytes(payload[..4].try_into().expect("parsed"));
        self.posted.push((connection_id, MessageType(typ)));
        if !matches!(message, Message::InitiateContact(..)) {
            let named = match self.connection {
                Connection::Connected { version, .. } if version >= Version::Win10Rs3_1 => {
                    NAMED_CONNECTION
                }
                _ => 1,
            };
            assert_eq!(
                connection_id, named,
                "after contact, messages go to the connection the host named"
            );
        }
        // Every message but the two that carry a GPADL's values is exactly its fields.
        let exact = |rest: &[u8]| assert!(rest.is_empty(), "{typ}: no bytes after the fields");
        match message {
            Message::InitiateContact(request, rest) => {
                exact(rest);
                self.initiate_contact(connection_id, request);
            }
            Message::RequestOffers(_, rest) => {
                exact(rest);
                self.request_offers();
            }
            Message::GpadlHeader(header, values) => self.gpadl_header(header, values),
            Message::GpadlBody(body, values) => self.gpadl_body(body, values),
            Message::OpenChannel(request, rest) => {
                exact(rest);
                self.open_channel(request);
            }
            Message::CloseChannel(request, rest) => {
                exact(rest);
                self.close_channel(request);
            }
            Message::GpadlTeardown(request, rest) => {
                exact(rest);
                self.gpadl_teardown(request);
            }
            Message::RelIdReleased(request, rest) => {
                exact(rest);
                self.rel_id_released(request);
            }
            Message::Unload(_, rest) => {
                exact(rest);
                self.unload();
            }
            other => panic!("the client sends no such message: {other:?}"),
        }
        Ok(())
    }

    fn signal_event(&mut self, connection_id: u32, flag: u16) {
        self.signals.push((connection_id, flag));
    }

    fn take_message(&mut self) -> Option<HvMessage> {
        self.inbox.pop_front()
    }

    fn test_and_clear_event(&mut self, flag: u16) -> bool {
        self.flags.remove(&flag)
    }

    fn page_numbers(&mut self, memory: MemoryId) -> Option<Vec<u64>> {
        self.memory.get(&memory.0).cloned()
    }
}

/// The connection ID a channel's offer carries, as the real host forms it: the channel,
/// SINT 2, VTL 0 (`channels.rs:890`).
pub fn connection_of(channel: ChannelId) -> u32 {
    protocol::ConnectionId::new(channel.0, Vtl::Vtl0, vmbus_core::VMBUS_SINT).0
}

/// An offer of a network card as channel `channel`, whatever the host's own books say:
/// for a test to send an offer the fake host would not.
pub fn offer(channel: u32) -> OutgoingMessage {
    OutgoingMessage::new(&protocol::OfferChannel {
        interface_id: NIC,
        instance_id: instance(channel),
        rsvd: [0; 4],
        flags: protocol::OfferFlags::new(),
        mmio_megabytes: 0,
        user_defined: protocol::UserDefinedData::default(),
        subchannel_index: 0,
        mmio_megabytes_optional: 0,
        channel_id: ChannelId(channel),
        monitor_id: 0xff,
        monitor_allocated: 0,
        is_dedicated: 1,
        connection_id: 0x2000_u32.wrapping_add(channel),
    })
}

fn values_of(bytes: &[u8]) -> Vec<u64> {
    let (values, rest) = bytes.as_chunks::<8>();
    assert!(rest.is_empty(), "8-byte values");
    values
        .iter()
        .map(|value| u64::from_le_bytes(*value))
        .collect()
}

/// Polls until there is nothing more for now, and gives the events in order. A message
/// the client refuses is a failed test here: the fake host sends none.
pub fn drain(client: &mut Client, host: &mut FakeHost) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(event) = client.poll(host).expect("the fake host sends no garbage") {
        events.push(event);
    }
    events
}

/// A client connected to `host`, with the events of the connection.
pub fn connect(host: &mut FakeHost) -> (Client, Vec<Event>) {
    let mut client = Client::new();
    client.connect().expect("a new client is disconnected");
    let events = drain(&mut client, host);
    (client, events)
}

/// A client connected to a host of today that offers a network card and a disk, as
/// channels 1 and 2.
pub fn connected() -> (Client, FakeHost) {
    let mut host = FakeHost::new();
    host.devices = vec![(NIC, instance(1)), (SCSI, instance(2))];
    let (client, events) = connect(&mut host);
    assert!(matches!(events.last(), Some(Event::Connected { .. })));
    (client, host)
}

/// A device instance ID that differs by `n`.
pub fn instance(n: u32) -> Guid {
    Guid {
        data1: n,
        data2: 0x4d75,
        data3: 0x4e61,
        data4: *b"moruna!!",
    }
}
