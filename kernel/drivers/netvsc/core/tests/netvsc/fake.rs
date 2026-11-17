//! The host's side, for the tests: a fake netvsp behind `moruna-vmbus`'s in-memory
//! channel (`testing::TestChannel`).
//!
//! The fake does what OpenVMM's netvsp does (`vm/devices/net/netvsp/src/lib.rs` at
//! `e7e7df77`), step for step, and is strict where netvsp is and a little beyond: every
//! NVSP message has the packet size of the version in use, every request comes once and
//! in order, every RNDIS message's length is its section's, and a frame carries nothing
//! but its fields and its bytes. What the guest does wrong is recorded in
//! [`Host::violations`], which a test of a well-behaved exchange asserts is empty.
//!
//! It can also misbehave on demand: [`Host::mangle`] alters its answers, and the `raw`
//! methods put any packet at all on the ring to the guest.
//!
//! A test holds the fake as `Rc<RefCell<Host>>` and the card as a [`Card`]. What a test
//! has the host send is queued, and goes on the ring the next time the card does
//! anything, or the host runs.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    sync::atomic::Ordering,
};

use guestmem_core::ranges::PagedRange;
use moruna_netvsc::{Buffer, Event, Netvsc, NetvscError, Shared};
use moruna_vmbus::{
    Channel, ChannelError, GpadlId, MemoryId, Wait,
    testing::{self, Device, Memory, TestChannel},
};
use netvsp_protocol::{
    protocol::{self, Status, Version},
    rndisprot::{self, Oid},
};
use vmbus_ring::{
    IncomingPacketType, OutgoingPacketType, Ring, RingMem, TransferPageRange, WriteError,
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// Bytes of a page.
pub const PAGE: usize = 4096;
/// netvsp's `RX_RESERVED_CONTROL_BUFFERS`: the receive sections kept for RNDIS
/// completions and indications.
pub const CONTROL_SECTIONS: u32 = 16;
/// netvsp's `RX_HEADER_LEN`: where a frame starts in its receive section.
pub const FRAME_AT: usize = 256;
/// netvsp's transaction for what it says of a virtual function.
pub const VF_TRANSACTION: u64 = 0x8000_0000_0000_0000;
/// The receive buffer's ID, as the guest names it and the host echoes it.
pub const RECEIVE_SET: u16 = 0;
/// The address the fake card has.
pub const MAC: [u8; 6] = [0x00, 0x15, 0x5d, 0x01, 0x02, 0x03];
/// The deadline every test gives the card: on the test channel, any deadline has
/// passed as soon as the host has nothing to say.
pub const DEADLINE: u64 = 30_000_000_000;

/// How long the card's two buffers are, in bytes.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub receive_buffer: usize,
    pub send_buffer: usize,
}

/// The buffers most tests start with: 18 receive sections (16 for control, 2 for
/// frames) and 2 send sections.
pub const SMALL: Settings = Settings {
    receive_buffer: 8 * PAGE,
    send_buffer: 4 * PAGE,
};
/// Buffers with room for traffic: 145 receive sections and 10 send sections.
pub const ROOMY: Settings = Settings {
    receive_buffer: 64 * PAGE,
    send_buffer: 16 * PAGE,
};

/// A deterministic generator (SplitMix64), so every run of a test sees the same bytes.
pub struct Random(pub u64);

impl Random {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `bound`.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// One time in `n`.
    pub fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }

    /// A frame of `len` bytes that a card may send: random bytes, with an individual
    /// source address.
    pub fn frame(&mut self, len: usize) -> Vec<u8> {
        let mut frame = self.bytes(len);
        frame[6] &= !1;
        frame
    }
}

/// One of the card's buffers: the test channel's memory, and the length it claims.
#[derive(Clone)]
pub struct Bytes {
    pub memory: Memory,
    pub len: usize,
}

impl Shared for Bytes {
    fn len(&self) -> usize {
        self.len
    }
    fn read(&self, at: usize, data: &mut [u8]) {
        self.memory.read(at, data);
    }
    fn write(&self, at: usize, data: &[u8]) {
        self.memory.write(at, data);
    }
}

/// The fake netvsp, as the test channel's device.
pub struct Netvsp(Rc<RefCell<Host>>);

impl Device for Netvsp {
    /// The host runs at once, as far as it can.
    fn signalled(&mut self, ring: &mut testing::Host) {
        self.0.borrow_mut().run(ring);
    }

    /// The guest is about to wait with no signal pending: the host does what it does
    /// only then, and runs.
    fn idle(&mut self, ring: &mut testing::Host) {
        self.0.borrow_mut().waited_for(ring);
    }

    fn shared(&mut self, gpadl: GpadlId, memory: &Memory) -> bool {
        let mut host = self.0.borrow_mut();
        if host.shares_left == 0 {
            return false;
        }
        host.shares_left -= 1;
        host.gpadls.insert(gpadl.0, memory.clone());
        true
    }
}

/// The driver under test.
pub type Driver = Netvsc<TestChannel<Netvsp>, Bytes>;

/// A channel with the fake netvsp behind it and the two buffers the card will share,
/// before the card starts.
pub struct Parts {
    pub channel: TestChannel<Netvsp>,
    pub receive: Buffer<Bytes>,
    pub send: Buffer<Bytes>,
    pub host: Rc<RefCell<Host>>,
}

/// The parts of a card whose ring to the host and ring to the guest have these many
/// bytes.
pub fn parts(to_host: usize, to_guest: usize, settings: Settings) -> Parts {
    let host = Rc::new(RefCell::new(Host::new()));
    let mut channel = TestChannel::new(to_host, to_guest, Netvsp(host.clone())).unwrap();
    let mut buffer = |id: u64, bytes: usize| {
        let memory = Memory::new(bytes.div_ceil(PAGE));
        channel.add_memory(MemoryId(id), memory.clone());
        Buffer {
            id: MemoryId(id),
            memory: Bytes { memory, len: bytes },
        }
    };
    let receive = buffer(1, settings.receive_buffer);
    let send = buffer(2, settings.send_buffer);
    Parts {
        channel,
        receive,
        send,
        host,
    }
}

impl Parts {
    /// Starts the card, after what the host was told to send first has gone.
    pub fn start(mut self) -> Result<Card, NetvscError> {
        self.host.borrow_mut().flush(self.channel.host());
        let driver = Netvsc::start(self.channel, self.receive, self.send, Some(DEADLINE))?;
        Ok(Card {
            driver,
            host: self.host,
        })
    }
}

/// The card under test, and the host it talks to. Before the card does anything, what
/// the test had the host send goes on the ring.
pub struct Card {
    driver: Driver,
    host: Rc<RefCell<Host>>,
}

impl Card {
    fn flush(&mut self) {
        self.host.borrow_mut().flush(self.driver.channel().host());
    }

    pub fn send(&mut self, frame: &[u8]) -> Result<bool, NetvscError> {
        self.flush();
        self.driver.send(frame)
    }

    pub fn poll(&mut self) -> Result<Option<Got>, NetvscError> {
        self.flush();
        Ok(match self.driver.poll()? {
            None => None,
            Some(Event::Frame(frame)) => Some(Got::Frame(frame.to_vec())),
            Some(Event::Link(up)) => Some(Got::Link(up)),
        })
    }

    pub fn mac(&self) -> [u8; 6] {
        self.driver.mac()
    }

    pub fn mtu(&self) -> u32 {
        self.driver.mtu()
    }

    pub fn link_up(&self) -> bool {
        self.driver.link_up()
    }

    /// Waits on the card's channel, as its caller does between polls.
    pub fn wait(&mut self) -> Result<Wait, ChannelError> {
        self.flush();
        self.driver.channel().wait(Some(DEADLINE))
    }

    /// Has the host read everything the card has sent, and answer.
    pub fn run_host(&mut self) {
        self.host.borrow_mut().run(self.driver.channel().host());
    }

    /// How many times the card has signalled the host.
    pub fn signals(&mut self) -> usize {
        self.driver.channel().host_signals()
    }
}

/// A card started against a host `prepare` has set up, over rings of these many bytes.
pub fn start_on(
    to_host: usize,
    to_guest: usize,
    settings: Settings,
    prepare: impl FnOnce(&mut Host),
) -> (Result<Card, NetvscError>, Rc<RefCell<Host>>) {
    let parts = parts(to_host, to_guest, settings);
    let host = parts.host.clone();
    prepare(&mut host.borrow_mut());
    (parts.start(), host)
}

/// A card started against a host `prepare` has set up, with 64 KiB rings.
pub fn start_with(
    settings: Settings,
    prepare: impl FnOnce(&mut Host),
) -> (Result<Card, NetvscError>, Rc<RefCell<Host>>) {
    start_on(16 * PAGE, 16 * PAGE, settings, prepare)
}

/// The card, which must have started.
pub fn up(card: Result<Card, NetvscError>) -> Card {
    match card {
        Ok(card) => card,
        Err(e) => panic!("the card did not start: {e}"),
    }
}

/// Why the card did not start, which it must not have.
pub fn down(card: Result<Card, NetvscError>) -> NetvscError {
    match card {
        Ok(_) => panic!("the card started"),
        Err(e) => e,
    }
}

/// A started card and its host.
pub fn started(settings: Settings) -> (Card, Rc<RefCell<Host>>) {
    let (card, host) = start_with(settings, |_| ());
    (up(card), host)
}

/// What a poll answered, as the test's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Got {
    Frame(Vec<u8>),
    Link(bool),
}

/// One poll.
pub fn poll(card: &mut Card) -> Result<Option<Got>, NetvscError> {
    card.poll()
}

/// Everything the host has sent, which must all be good.
pub fn drain(card: &mut Card) -> Vec<Got> {
    let mut all = Vec::new();
    loop {
        match card.poll() {
            Ok(Some(got)) => all.push(got),
            Ok(None) => return all,
            Err(e) => panic!("a poll failed: {e}"),
        }
    }
}

/// Asserts the error is a refusal of what the host sent, and says `part`.
#[track_caller]
pub fn assert_malformed(e: &NetvscError, part: &str) {
    match e {
        NetvscError::Malformed(what) if what.contains(part) => {}
        other => panic!("expected a refusal saying {part:?}, got: {other}"),
    }
}

/// Asserts the guest did nothing a strict host refuses.
#[track_caller]
pub fn assert_clean(host: &Rc<RefCell<Host>>) {
    let host = host.borrow();
    assert!(host.violations.is_empty(), "{:#?}", host.violations);
}

/// Writes a 32-bit field of an answer.
pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// A mangle that leaves every answer alone but the `nth` of `step`, which `change`
/// rewrites or drops.
pub fn on(
    step: Step,
    nth: usize,
    mut change: impl FnMut(&mut Host, &mut Vec<u8>) -> Fate + 'static,
) -> Option<Mangle> {
    let mut seen = 0;
    Some(Box::new(move |host, this, bytes| {
        if this != step {
            return Fate::Send;
        }
        seen += 1;
        if seen == nth + 1 {
            change(host, bytes)
        } else {
            Fate::Send
        }
    }))
}

/// Which of the host's answers a [`Host::mangle`] is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The completion of an NVSP message from the guest: that message's type. The
    /// bytes are the completion's payload.
    Answer(u32),
    /// An RNDIS message the host is about to send: its type. The bytes are the whole
    /// message, as it will lie in a receive section.
    Rndis(u32),
}

/// What a [`Host::mangle`] may do to an answer besides rewriting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Send,
    /// The answer is never sent.
    Drop,
}

/// Shown each answer before it is sent, with the host to do more with: what it has the
/// host send from in there goes before the answer, and what it leaves in
/// [`Host::then`] after.
pub type Mangle = Box<dyn FnMut(&mut Host, Step, &mut Vec<u8>) -> Fate>;

/// When the host completes the packet an RNDIS request came in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completes {
    /// Before it answers the request, as netvsp does.
    First,
    /// Right after it answers the request.
    AfterAnswer,
    /// Only once the guest waits for it.
    OnWait,
}

/// Something for the ring to the guest.
enum Out {
    Completion(u64, Vec<u8>),
    InBand(Option<u64>, Vec<u8>),
    Transfer(u64, u16, Vec<u8>, Vec<TransferPageRange>),
    Direct(u64, Vec<u8>),
    /// A packet written descriptor by descriptor: its type, where its payload starts
    /// (in eight-byte words), its flags, its transaction, and the bytes after the
    /// descriptor.
    Raw(u16, u16, u16, u64, Vec<u8>),
    /// A write pointer no ring has.
    Corrupt,
    /// A read pointer no ring has, on the ring to the host.
    CorruptToHost,
}

struct ReceiveBuffer {
    memory: Memory,
    section: usize,
    control: Vec<u32>,
    data: VecDeque<u32>,
}

struct SendBuffer {
    memory: Memory,
    section: usize,
}

/// The fake netvsp.
pub struct Host {
    gpadls: BTreeMap<u32, Memory>,

    // What a test sets.
    /// The NVSP versions the host accepts.
    pub versions: Vec<u32>,
    /// What the MTU query answers, instead of the NDIS configuration's less a header.
    pub mtu: Option<u32>,
    /// Bytes of a send section.
    pub send_section: u32,
    /// Bytes of a receive section, instead of netvsp's for the MTU.
    pub receive_section: Option<u32>,
    /// A host that does not read what the guest sends.
    pub paused: bool,
    /// A host that keeps its completions of sent frames to itself.
    pub hold: bool,
    /// The status sent frames are completed with.
    pub send_status: Status,
    /// The serial of a virtual function the host associates as soon as RNDIS is
    /// initialised, as netvsp does.
    pub vf: Option<u32>,
    /// How many more times memory can be shared.
    pub shares_left: usize,
    /// A host that judges nothing the guest sends (for a host that itself misbehaves).
    pub lenient: bool,
    /// Rewrites, or drops, the host's answers.
    pub mangle: Option<Mangle>,
    /// RNDIS messages to send right after the answer in hand.
    pub then: Vec<Vec<u8>>,
    /// When the packet of an RNDIS request is completed.
    pub completes: Completes,
    /// A paused host reads again as soon as the guest waits for it.
    pub wakes_on_wait: bool,
    /// The host takes its offer back: the channel closes once what is queued has gone.
    pub rescind: bool,
    /// Completions put off until the guest waits.
    deferred: Vec<u64>,

    // What the guest negotiated.
    version: Option<u32>,
    config: Option<protocol::Message2SendNdisConfig>,
    ndis: Option<protocol::Message1SendNdisVersion>,
    receive: Option<ReceiveBuffer>,
    send: Option<SendBuffer>,
    initialized: bool,
    /// The packet filter the guest set.
    pub filter: u32,

    // What happened.
    /// Every version the guest offered, in order.
    pub offers: Vec<u32>,
    /// Every NVSP message type the guest sent, in order.
    pub messages: Vec<u32>,
    /// Every frame the guest sent, in order.
    pub frames: Vec<Vec<u8>>,
    /// What the guest did that a strict host refuses.
    pub violations: Vec<String>,
    /// How many times the guest acknowledged what the host said of a virtual function.
    pub vf_acknowledged: usize,
    /// The statuses the guest completed receives with.
    pub received: Vec<Status>,
    /// The guest's transactions not yet completed.
    open: BTreeSet<u64>,
    /// Completions of sent frames, kept while `hold` is set.
    held: Vec<u64>,
    /// The receive sections lent to the guest, by the transaction that lent them.
    lent: BTreeMap<u64, Vec<u32>>,
    /// Frames to deliver once the filter is set and a section is free.
    backlog: VecDeque<Vec<u8>>,
    /// What goes on the ring to the guest next, in order, as it has room.
    queue: VecDeque<Out>,
    /// The host has itself made a ring unusable.
    broken: bool,
}

/// The NVSP packet size of a version, in the ring's whole words, as netvsp sizes its
/// messages.
fn packet_size(version: Option<u32>) -> usize {
    match version {
        Some(version) if version >= Version::V61 as u32 => protocol::PACKET_SIZE_V61,
        _ => protocol::PACKET_SIZE_V1.next_multiple_of(8),
    }
}

fn plain<T: FromBytes>(bytes: &[u8]) -> Option<T> {
    T::read_from_prefix(bytes).ok().map(|(value, _)| value)
}

/// An RNDIS message: its header, its fields, and what follows them.
pub fn rndis_message<T: IntoBytes + Immutable>(
    message_type: u32,
    fields: &T,
    extra: &[u8],
) -> Vec<u8> {
    let len = size_of::<rndisprot::MessageHeader>() + size_of::<T>() + extra.len();
    let header = rndisprot::MessageHeader {
        message_type,
        message_length: len as u32,
    };
    [header.as_bytes(), fields.as_bytes(), extra].concat()
}

/// A frame as netvsp lays it out in a receive section of `section` bytes
/// (`write_header` in `buffers.rs`): the RNDIS fields, a checksum record, and the frame
/// 256 bytes in. The message claims the whole section.
pub fn rndis_frame(frame: &[u8], section: usize) -> Vec<u8> {
    let packet = rndisprot::Packet {
        data_offset: (FRAME_AT - size_of::<rndisprot::MessageHeader>()) as u32,
        data_length: frame.len() as u32,
        oob_data_offset: 0,
        oob_data_length: 0,
        num_oob_data_elements: 0,
        per_packet_info_offset: size_of::<rndisprot::Packet>() as u32,
        per_packet_info_length: 16,
        vc_handle: 0,
        reserved: 0,
    };
    let checksum = rndisprot::PerPacketInfo {
        size: 16,
        typ: rndisprot::PPI_TCP_IP_CHECKSUM,
        per_packet_information_offset: 12,
    };
    let header = rndisprot::MessageHeader {
        message_type: rndisprot::MESSAGE_TYPE_PACKET_MSG,
        message_length: section as u32,
    };
    let mut bytes = [
        header.as_bytes(),
        packet.as_bytes(),
        checksum.as_bytes(),
        0u32.as_bytes(),
    ]
    .concat();
    bytes.resize(FRAME_AT, 0);
    bytes.extend_from_slice(frame);
    bytes
}

impl Host {
    fn new() -> Host {
        Host {
            gpadls: BTreeMap::new(),
            versions: vec![
                Version::V1 as u32,
                Version::V2 as u32,
                Version::V4 as u32,
                Version::V5 as u32,
                Version::V6 as u32,
                Version::V61 as u32,
            ],
            mtu: None,
            send_section: 6144,
            receive_section: None,
            paused: false,
            hold: false,
            send_status: Status::SUCCESS,
            vf: None,
            shares_left: usize::MAX,
            lenient: false,
            mangle: None,
            then: Vec::new(),
            completes: Completes::First,
            wakes_on_wait: false,
            rescind: false,
            deferred: Vec::new(),
            version: None,
            config: None,
            ndis: None,
            receive: None,
            send: None,
            initialized: false,
            filter: 0,
            offers: Vec::new(),
            messages: Vec::new(),
            frames: Vec::new(),
            violations: Vec::new(),
            vf_acknowledged: 0,
            received: Vec::new(),
            open: BTreeSet::new(),
            held: Vec::new(),
            lent: BTreeMap::new(),
            backlog: VecDeque::new(),
            queue: VecDeque::new(),
            broken: false,
        }
    }

    fn violation(&mut self, what: String) {
        if !self.lenient {
            self.violations.push(what);
        }
    }

    /// An NVSP message of the packet size in use.
    pub fn nvsp<T: IntoBytes + Immutable>(&self, message_type: u32, body: &T) -> Vec<u8> {
        let mut bytes = [message_type.as_bytes(), body.as_bytes()].concat();
        bytes.resize(packet_size(self.version), 0);
        bytes
    }

    /// The NVSP message that goes with a receive.
    pub fn rndis_packet(&self, channel_type: u32) -> Vec<u8> {
        self.nvsp(
            protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
            &protocol::Message1SendRndisPacket {
                channel_type,
                send_buffer_section_index: 0xffff_ffff,
                send_buffer_section_size: 0,
            },
        )
    }

    /// Bytes of a receive section.
    pub fn section(&self) -> usize {
        self.receive.as_ref().expect("a receive buffer").section
    }

    /// How many receive sections are free for frames.
    pub fn free_sections(&self) -> usize {
        self.receive.as_ref().expect("a receive buffer").data.len()
    }

    /// How many receive sections are free for control messages.
    pub fn free_control_sections(&self) -> usize {
        self.receive
            .as_ref()
            .expect("a receive buffer")
            .control
            .len()
    }

    /// The version the guest negotiated.
    pub fn version(&self) -> Option<u32> {
        self.version
    }

    /// The NDIS configuration and version the guest stated.
    pub fn ndis(
        &self,
    ) -> (
        protocol::Message2SendNdisConfig,
        protocol::Message1SendNdisVersion,
    ) {
        (
            self.config.expect("a configuration"),
            self.ndis.expect("a version"),
        )
    }

    /// Reads everything the guest has sent and answers it, then sends what was waiting.
    pub fn run(&mut self, ring: &mut testing::Host) {
        while !self.paused {
            let received = match ring.read() {
                Ok(Some(received)) => received,
                Ok(None) => break,
                Err(_) if self.broken => break,
                Err(e) => panic!("the guest corrupted its ring: {e}"),
            };
            match received.packet.typ {
                IncomingPacketType::Completion => {
                    self.completed(received.packet.transaction_id.unwrap(), &received.payload);
                }
                IncomingPacketType::InBand => {
                    self.message(received.packet.transaction_id, &received.payload);
                }
                _ => self.violation("a packet with ranges, which this host takes none of".into()),
            }
        }
        self.flush(ring);
    }

    /// The guest waits, and nothing has signalled it.
    fn waited_for(&mut self, ring: &mut testing::Host) {
        if self.wakes_on_wait {
            self.paused = false;
        }
        for transaction in std::mem::take(&mut self.deferred) {
            self.complete(transaction, Status::SUCCESS);
        }
        self.run(ring);
    }

    /// Puts on the ring what waits for it: queued packets while the ring has room, and
    /// frames while the filter lets them through and a section is free.
    pub fn flush(&mut self, ring: &mut testing::Host) {
        loop {
            while let Some(out) = self.queue.pop_front() {
                if !self.write(ring, &out) {
                    self.queue.push_front(out);
                    return;
                }
            }
            if self.filter == 0 || self.backlog.is_empty() || self.free_sections() == 0 {
                break;
            }
            let frame = self.backlog.pop_front().unwrap();
            self.deliver(&[&frame]);
        }
        // Nothing waits for room on the ring any more.
        if !self.broken {
            ring.outgoing().set_pending_send_size(0).unwrap();
        }
        if self.rescind {
            ring.rescind();
        }
    }

    /// Sends a packet as soon as the ring has room, after what is queued before it.
    fn emit(&mut self, out: Out) {
        self.queue.push_back(out);
    }

    /// Writes one packet; `false` when the ring has no room for it yet.
    fn write(&mut self, ring: &mut testing::Host, out: &Out) -> bool {
        let gpns = [5u64];
        let pages = [PagedRange::new(0, 64, &gpns).unwrap()];
        let (typ, transaction, payload) = match out {
            Out::Completion(transaction, payload) => {
                (OutgoingPacketType::Completion, *transaction, payload)
            }
            Out::InBand(Some(transaction), payload) => (
                OutgoingPacketType::InBandWithCompletion,
                *transaction,
                payload,
            ),
            Out::InBand(None, payload) => (OutgoingPacketType::InBandNoCompletion, 0, payload),
            Out::Transfer(transaction, set, payload, ranges) => (
                OutgoingPacketType::TransferPages(*set, ranges),
                *transaction,
                payload,
            ),
            Out::Direct(transaction, payload) => {
                (OutgoingPacketType::GpaDirect(&pages), *transaction, payload)
            }
            Out::Raw(packet_type, data_offset8, flags, transaction, rest) => {
                let memory = ring.outgoing().mem();
                let control = memory.control();
                let at = control[0].load(Ordering::SeqCst) as usize;
                let length8 = ((16 + rest.len()) / 8) as u16;
                let bytes = [
                    packet_type.as_bytes(),
                    data_offset8.as_bytes(),
                    length8.as_bytes(),
                    flags.as_bytes(),
                    transaction.as_bytes(),
                    rest,
                    // The footer: a reserved word, and where the packet starts.
                    0u32.as_bytes(),
                    (at as u32).as_bytes(),
                ]
                .concat();
                memory.write_at(at, &bytes);
                let next = (at + bytes.len()) % memory.len();
                control[0].store(next as u32, Ordering::SeqCst);
                ring.signal_driver();
                return true;
            }
            Out::Corrupt => {
                ring.outgoing().mem().control()[0].store(0xdead, Ordering::SeqCst);
                ring.signal_driver();
                self.broken = true;
                return true;
            }
            Out::CorruptToHost => {
                ring.incoming().mem().control()[1].store(0xdead, Ordering::SeqCst);
                self.broken = true;
                return true;
            }
        };
        match ring.write(typ, transaction, payload) {
            Ok(()) => true,
            Err(WriteError::Full(bytes)) => {
                // The guest signals once it has read that much.
                ring.outgoing().set_pending_send_size(bytes).unwrap();
                false
            }
            // A ring this host broke takes nothing more.
            Err(WriteError::Corrupt(_)) if self.broken => true,
            Err(WriteError::Corrupt(e)) => panic!("the ring to the guest: {e}"),
        }
    }

    fn mangled(&mut self, step: Step, bytes: &mut Vec<u8>) -> Fate {
        // The mangle is out of the host while it runs, so what it has the host send is
        // not itself mangled.
        let Some(mut mangle) = self.mangle.take() else {
            return Fate::Send;
        };
        let fate = mangle(self, step, bytes);
        self.mangle = Some(mangle);
        fate
    }

    /// Sends what a mangle left to follow the answer in hand.
    fn follow(&mut self) {
        for message in std::mem::take(&mut self.then) {
            self.control_as_is(message);
        }
    }

    /// Completes the guest's NVSP message `message_type` with `payload`.
    fn answer(&mut self, message_type: u32, transaction: u64, mut payload: Vec<u8>) {
        self.open.remove(&transaction);
        if self.mangled(Step::Answer(message_type), &mut payload) == Fate::Send {
            self.emit(Out::Completion(transaction, payload));
        }
        self.follow();
    }

    /// Completes the packet an RNDIS message of the guest's came in.
    fn complete(&mut self, transaction: u64, status: Status) {
        let complete = protocol::Message1SendRndisPacketComplete { status };
        let answer = self.nvsp(
            protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
            &complete,
        );
        self.answer(
            protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
            transaction,
            answer,
        );
    }

    /// One NVSP message from the guest.
    fn message(&mut self, transaction: Option<u64>, payload: &[u8]) {
        let size = packet_size(self.version);
        if payload.len() != size {
            self.violation(format!(
                "an NVSP message of {} bytes, where the packet size is {size}",
                payload.len()
            ));
        }
        let Some(header) = plain::<protocol::MessageHeader>(payload) else {
            return self.violation("a packet too short for an NVSP message".into());
        };
        let message_type = header.message_type;
        self.messages.push(message_type);
        let body = &payload[size_of::<protocol::MessageHeader>()..];
        // Every message this guest sends asks for a completion.
        let Some(transaction) = transaction else {
            return self.violation(format!("message {message_type} asks for no completion"));
        };
        if !self.open.insert(transaction) {
            self.violation(format!(
                "transaction {transaction:#x} used again before it was completed"
            ));
        }
        let ready = self.config.is_some()
            && self.ndis.is_some()
            && self.receive.is_some()
            && self.send.is_some();
        match message_type {
            protocol::MESSAGE_TYPE_INIT if self.version.is_none() => {
                let init: protocol::MessageInit = plain(body).unwrap();
                self.offers.push(init.protocol_version);
                let accepted = self.versions.contains(&init.protocol_version);
                let complete = protocol::MessageInitComplete {
                    deprecated: protocol::INVALID_PROTOCOL_VERSION,
                    maximum_mdl_chain_length: 34,
                    status: if accepted {
                        Status::SUCCESS
                    } else {
                        Status::NONE
                    },
                };
                // netvsp answers at the size in use before the version is known.
                let answer = self.nvsp(protocol::MESSAGE_TYPE_INIT_COMPLETE, &complete);
                if accepted {
                    self.version = Some(init.protocol_version);
                }
                self.answer(message_type, transaction, answer);
            }
            _ if self.version.is_none() => {
                self.violation(format!("message {message_type} before the version"));
            }
            protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG if self.config.is_none() && !ready => {
                let mut config: protocol::Message2SendNdisConfig = plain(body).unwrap();
                // netvsp, as the vmswitch: an MTU out of range is taken as the default.
                if !(1514..=9216).contains(&config.mtu) {
                    config.mtu = 1514;
                }
                self.config = Some(config);
                self.answer(message_type, transaction, Vec::new());
            }
            protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION if self.ndis.is_none() && !ready => {
                self.ndis = Some(plain(body).unwrap());
                self.answer(message_type, transaction, Vec::new());
            }
            protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER if self.receive.is_none() => {
                let message: protocol::Message1SendReceiveBuffer = plain(body).unwrap();
                self.receive_buffer(transaction, message);
            }
            protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER if self.send.is_none() => {
                let message: protocol::Message1SendSendBuffer = plain(body).unwrap();
                let memory = self.gpadls.get(&message.gpadl_handle.0).cloned();
                let status = match memory {
                    Some(memory) => {
                        self.send = Some(SendBuffer {
                            memory,
                            section: self.send_section as usize,
                        });
                        Status::SUCCESS
                    }
                    None => {
                        self.violation("a send buffer whose GPADL the host never saw".into());
                        Status::FAILURE
                    }
                };
                let complete = protocol::Message1SendSendBufferComplete {
                    status,
                    section_size: self.send_section,
                };
                let answer =
                    self.nvsp(protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER_COMPLETE, &complete);
                self.answer(message_type, transaction, answer);
            }
            protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET if ready => {
                let message: protocol::Message1SendRndisPacket = plain(body).unwrap();
                self.rndis(transaction, message);
            }
            _ => {
                self.violation(format!(
                    "message {message_type} out of order, twice, or of no use to this host"
                ));
            }
        }
    }

    /// The receive buffer: netvsp sizes its sections for the MTU the guest configured,
    /// keeps the first sixteen for control, and needs one more at least.
    fn receive_buffer(&mut self, transaction: u64, message: protocol::Message1SendReceiveBuffer) {
        let message_type = protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER;
        let Some(config) = self.config else {
            return self.violation("the receive buffer before the NDIS configuration".into());
        };
        if message.id != RECEIVE_SET {
            self.violation(format!("receive buffer ID {}", message.id));
        }
        let section = self
            .receive_section
            .unwrap_or(FRAME_AT as u32 + config.mtu + 36);
        let memory = self.gpadls.get(&message.gpadl_handle.0).cloned();
        let count = match &memory {
            Some(memory) => (memory.len() / section as usize) as u32,
            None => {
                self.violation("a receive buffer whose GPADL the host never saw".into());
                0
            }
        };
        let status = if count > CONTROL_SECTIONS {
            self.receive = Some(ReceiveBuffer {
                memory: memory.unwrap(),
                section: section as usize,
                control: (0..CONTROL_SECTIONS).rev().collect(),
                data: (CONTROL_SECTIONS..count).collect(),
            });
            Status::SUCCESS
        } else {
            Status::FAILURE
        };
        let complete = protocol::Message1SendReceiveBufferComplete {
            status,
            num_sections: 1,
            sections: [protocol::ReceiveBufferSection {
                offset: 0,
                sub_allocation_size: section,
                num_sub_allocations: count,
                end_offset: section * count,
            }],
        };
        let answer = self.nvsp(
            protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER_COMPLETE,
            &complete,
        );
        self.answer(message_type, transaction, answer);
    }

    /// An RNDIS message the guest put in a send section.
    fn rndis(&mut self, transaction: u64, message: protocol::Message1SendRndisPacket) {
        let complete = |host: &mut Host, status: Status| host.complete(transaction, status);
        let send = self.send.as_ref().unwrap();
        let (memory, section) = (send.memory.clone(), send.section);
        let index = message.send_buffer_section_index as usize;
        let len = message.send_buffer_section_size as usize;
        if message.send_buffer_section_index == 0xffff_ffff {
            self.violation("an RNDIS message outside the send buffer".into());
            return complete(self, Status::FAILURE);
        }
        if len > section || (index + 1) * section > memory.len() {
            self.violation(format!("{len} bytes in send section {index}"));
            return complete(self, Status::FAILURE);
        }
        let mut bytes = vec![0; len];
        memory.read(index * section, &mut bytes);
        let Some(header) = plain::<rndisprot::MessageHeader>(&bytes) else {
            self.violation(format!("an RNDIS message of {len} bytes"));
            return complete(self, Status::FAILURE);
        };
        if header.message_length as usize != len {
            self.violation(format!(
                "an RNDIS message of {} bytes in {len} bytes of a section",
                header.message_length
            ));
            return complete(self, Status::FAILURE);
        }
        let fields = &bytes[size_of::<rndisprot::MessageHeader>()..];
        let control = header.message_type != rndisprot::MESSAGE_TYPE_PACKET_MSG;
        let channel = if control {
            protocol::CONTROL_CHANNEL_TYPE
        } else {
            protocol::DATA_CHANNEL_TYPE
        };
        if message.channel_type != channel {
            self.violation(format!(
                "RNDIS message {:#x} on channel type {}",
                header.message_type, message.channel_type
            ));
        }
        if !control {
            return self.frame(transaction, fields);
        }
        // netvsp completes the packet at once and answers the request after it.
        if self.completes == Completes::First {
            complete(self, Status::SUCCESS);
        }
        let answer = match header.message_type {
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG if !self.initialized => {
                let Some(request) = plain::<rndisprot::InitializeRequest>(fields) else {
                    return self.violation("a short RNDIS initialise".into());
                };
                if (request.major_version, request.minor_version) != (1, 0) {
                    self.violation(format!(
                        "RNDIS {}.{}",
                        request.major_version, request.minor_version
                    ));
                }
                self.initialized = true;
                rndis_message(
                    rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT,
                    &rndisprot::InitializeComplete {
                        request_id: request.request_id,
                        status: rndisprot::STATUS_SUCCESS,
                        major_version: rndisprot::MAJOR_VERSION,
                        minor_version: rndisprot::MINOR_VERSION,
                        device_flags: rndisprot::DF_CONNECTIONLESS,
                        medium: rndisprot::MEDIUM_802_3,
                        max_packets_per_message: 8,
                        max_transfer_size: 0xEFFF_FFFF,
                        packet_alignment_factor: 3,
                        af_list_offset: 0,
                        af_list_size: 0,
                    },
                    &[],
                )
            }
            rndisprot::MESSAGE_TYPE_QUERY_MSG if self.initialized => {
                let Some(request) = plain::<rndisprot::QueryRequest>(fields) else {
                    return self.violation("a short RNDIS query".into());
                };
                if request.information_buffer_length != 0 || request.information_buffer_offset != 0
                {
                    self.violation("a query with input".into());
                }
                let value = match request.oid {
                    Oid::OID_802_3_PERMANENT_ADDRESS | Oid::OID_802_3_CURRENT_ADDRESS => {
                        Some(MAC.to_vec())
                    }
                    Oid::OID_GEN_MAXIMUM_FRAME_SIZE => {
                        let mtu = self.mtu.unwrap_or(self.config.unwrap().mtu - 14);
                        Some(mtu.to_le_bytes().to_vec())
                    }
                    _ => None,
                };
                let status = match value {
                    Some(_) => rndisprot::STATUS_SUCCESS,
                    None => rndisprot::STATUS_NOT_SUPPORTED,
                };
                let value = value.unwrap_or_default();
                rndis_message(
                    rndisprot::MESSAGE_TYPE_QUERY_CMPLT,
                    &rndisprot::QueryComplete {
                        request_id: request.request_id,
                        status,
                        information_buffer_length: value.len() as u32,
                        information_buffer_offset: size_of::<rndisprot::QueryComplete>() as u32,
                    },
                    &value,
                )
            }
            rndisprot::MESSAGE_TYPE_SET_MSG if self.initialized => {
                let Some(request) = plain::<rndisprot::SetRequest>(fields) else {
                    return self.violation("a short RNDIS set".into());
                };
                let value = &fields[size_of::<rndisprot::SetRequest>()..];
                if request.information_buffer_offset as usize != size_of::<rndisprot::SetRequest>()
                    || request.information_buffer_length as usize != value.len()
                {
                    self.violation("a set whose value is not right after its fields".into());
                }
                let status = match (request.oid, <[u8; 4]>::try_from(value)) {
                    (Oid::OID_GEN_CURRENT_PACKET_FILTER, Ok(filter)) => {
                        self.filter = u32::from_le_bytes(filter);
                        rndisprot::STATUS_SUCCESS
                    }
                    _ => rndisprot::STATUS_NOT_SUPPORTED,
                };
                rndis_message(
                    rndisprot::MESSAGE_TYPE_SET_CMPLT,
                    &rndisprot::SetComplete {
                        request_id: request.request_id,
                        status,
                    },
                    &[],
                )
            }
            other => {
                return self.violation(format!(
                    "RNDIS message {other:#x} out of order, or of no use to this host"
                ));
            }
        };
        let initialize = header.message_type == rndisprot::MESSAGE_TYPE_INITIALIZE_MSG;
        self.control(answer);
        match self.completes {
            Completes::First => {}
            Completes::AfterAnswer => complete(self, Status::SUCCESS),
            Completes::OnWait => self.deferred.push(transaction),
        }
        // netvsp says what it has to of a virtual function right after the answer.
        if let (true, Some(serial)) = (initialize, self.vf) {
            self.offer_vf(serial);
        }
    }

    /// A frame the guest sent: REMOTE_NDIS_PACKET_MSG's fields, then the frame and
    /// nothing else.
    fn frame(&mut self, transaction: u64, fields: &[u8]) {
        let Some(packet) = plain::<rndisprot::Packet>(fields) else {
            return self.violation("a short RNDIS packet message".into());
        };
        if !self.initialized {
            self.violation("a frame before RNDIS was initialised".into());
        }
        let start = packet.data_offset as usize;
        let end = start + packet.data_length as usize;
        let plain_frame = packet.oob_data_offset == 0
            && packet.oob_data_length == 0
            && packet.num_oob_data_elements == 0
            && packet.per_packet_info_offset == 0
            && packet.per_packet_info_length == 0
            && packet.vc_handle == 0
            && packet.reserved == 0;
        if !plain_frame || start != size_of::<rndisprot::Packet>() || end != fields.len() {
            self.violation(format!("a frame with more than its bytes: {packet:?}"));
        } else if packet.data_length < 14 {
            self.violation(format!("a frame of {} bytes", packet.data_length));
        } else {
            self.frames.push(fields[start..end].to_vec());
        }
        if self.hold {
            self.held.push(transaction);
        } else {
            self.complete(transaction, self.send_status);
        }
    }

    /// Completes every frame whose completion was held.
    pub fn release(&mut self) {
        self.hold = false;
        for transaction in std::mem::take(&mut self.held) {
            self.complete(transaction, self.send_status);
        }
    }

    /// Forgets the completions it held: the guest never hears of those frames.
    pub fn forget_held(&mut self) {
        self.held.clear();
    }

    /// A completion from the guest: it gives receive sections back, or acknowledges what
    /// the host said of a virtual function.
    fn completed(&mut self, transaction: u64, payload: &[u8]) {
        if transaction == VF_TRANSACTION {
            self.vf_acknowledged += 1;
            if !payload.is_empty() {
                self.violation("an acknowledgement with a payload".into());
            }
            return;
        }
        let Some(sections) = self.lent.remove(&transaction) else {
            return self.violation(format!(
                "a completion of transaction {transaction:#x}, which lent nothing"
            ));
        };
        let receive = self.receive.as_mut().unwrap();
        for section in sections {
            if section < CONTROL_SECTIONS {
                receive.control.push(section);
            } else {
                receive.data.push_back(section);
            }
        }
        let complete = plain::<protocol::MessageHeader>(payload)
            .filter(|header| {
                header.message_type == protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE
            })
            .and_then(|_| plain::<protocol::Message1SendRndisPacketComplete>(&payload[4..]));
        match complete {
            Some(complete) if payload.len() == packet_size(self.version) => {
                self.received.push(complete.status);
            }
            _ => self.violation(format!("a completion of {} bytes", payload.len())),
        }
    }

    /// Sends an RNDIS control message, in a control section. It is the test's to see
    /// that one is free.
    pub fn control(&mut self, mut message: Vec<u8>) {
        let message_type = plain::<rndisprot::MessageHeader>(&message)
            .map(|header| header.message_type)
            .unwrap_or(0);
        if self.mangled(Step::Rndis(message_type), &mut message) == Fate::Send {
            self.control_as_is(message);
        }
        self.follow();
    }

    /// Sends an RNDIS control message as it is.
    fn control_as_is(&mut self, message: Vec<u8>) {
        let receive = self.receive.as_mut().unwrap();
        let section = receive.control.pop().expect("a free control section");
        let at = section as usize * receive.section;
        receive.memory.write(at, &message);
        let range = TransferPageRange {
            byte_count: message.len() as u32,
            byte_offset: at as u32,
        };
        self.lent.insert(section.into(), vec![section]);
        let payload = self.rndis_packet(protocol::CONTROL_CHANNEL_TYPE);
        self.emit(Out::Transfer(
            section.into(),
            RECEIVE_SET,
            payload,
            vec![range],
        ));
    }

    /// Indicates the link's state, as netvsp's `indicate_status` does.
    pub fn link(&mut self, up: bool) {
        self.indicate(if up {
            rndisprot::STATUS_MEDIA_CONNECT
        } else {
            rndisprot::STATUS_MEDIA_DISCONNECT
        });
    }

    /// Indicates a status.
    pub fn indicate(&mut self, status: u32) {
        self.control(rndis_message(
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
            &rndisprot::IndicateStatus {
                status,
                status_buffer_length: 0,
                status_buffer_offset: 0,
            },
            &[],
        ));
    }

    /// Associates a virtual function, as netvsp's `guest_vf_is_available` does: a
    /// message of its own that asks for a completion.
    pub fn offer_vf(&mut self, serial: u32) {
        let message = self.nvsp(
            protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
            &protocol::Message4SendVfAssociation {
                vf_allocated: 1,
                serial_number: serial,
            },
        );
        self.emit(Out::InBand(Some(VF_TRANSACTION), message));
    }

    /// Delivers a frame to the guest once the filter lets it through and a section is
    /// free.
    pub fn inject(&mut self, frame: &[u8]) {
        self.backlog.push_back(frame.to_vec());
    }

    /// Delivers frames in one receive, a section each, as netvsp's
    /// `process_endpoint_rx` does: the transaction is the first section. It is the
    /// test's to see that the sections are free.
    pub fn deliver(&mut self, frames: &[&[u8]]) {
        let receive = self.receive.as_mut().unwrap();
        let mut sections = Vec::new();
        let mut ranges = Vec::new();
        for frame in frames {
            let section = receive.data.pop_front().expect("a free receive section");
            let at = section as usize * receive.section;
            receive
                .memory
                .write(at, &rndis_frame(frame, receive.section));
            sections.push(section);
            // netvsp names the whole section, whatever the length of the message in
            // it.
            ranges.push(TransferPageRange {
                byte_count: receive.section as u32,
                byte_offset: at as u32,
            });
        }
        let transaction = u64::from(sections[0]);
        self.lent.insert(transaction, sections);
        let payload = self.rndis_packet(protocol::DATA_CHANNEL_TYPE);
        self.emit(Out::Transfer(transaction, RECEIVE_SET, payload, ranges));
    }

    // What no well-behaved host does.

    /// Writes bytes anywhere in the receive buffer, at once.
    pub fn scribble(&mut self, at: usize, bytes: &[u8]) {
        self.receive.as_ref().unwrap().memory.write(at, bytes);
    }

    /// Puts a completion on the ring to the guest.
    pub fn raw_completion(&mut self, transaction: u64, payload: Vec<u8>) {
        self.emit(Out::Completion(transaction, payload));
    }

    /// Puts a message of its own on the ring to the guest.
    pub fn raw_message(&mut self, transaction: Option<u64>, payload: Vec<u8>) {
        self.emit(Out::InBand(transaction, payload));
    }

    /// Puts a receive on the ring to the guest.
    pub fn raw_receive(
        &mut self,
        transaction: u64,
        set: u16,
        payload: Vec<u8>,
        ranges: Vec<TransferPageRange>,
    ) {
        self.emit(Out::Transfer(transaction, set, payload, ranges));
    }

    /// Puts a packet naming guest pages on the ring to the guest.
    pub fn raw_direct(&mut self, transaction: u64, payload: Vec<u8>) {
        self.emit(Out::Direct(transaction, payload));
    }

    /// Puts a packet on the ring to the guest descriptor by descriptor: its type, where
    /// its payload starts (in eight-byte words), its flags and transaction, and the
    /// bytes after the descriptor (whole words).
    pub fn raw_descriptor(
        &mut self,
        packet_type: u16,
        data_offset8: u16,
        flags: u16,
        transaction: u64,
        rest: &[u8],
    ) {
        assert!(rest.len().is_multiple_of(8));
        self.emit(Out::Raw(
            packet_type,
            data_offset8,
            flags,
            transaction,
            rest.to_vec(),
        ));
    }

    /// Sets the ring to the guest's write pointer to something no ring's is.
    pub fn corrupt_ring(&mut self) {
        self.emit(Out::Corrupt);
    }

    /// Sets the ring to the host's read pointer, which is the host's to write, to
    /// something no ring's is.
    pub fn corrupt_ring_to_host(&mut self) {
        self.emit(Out::CorruptToHost);
    }
}
