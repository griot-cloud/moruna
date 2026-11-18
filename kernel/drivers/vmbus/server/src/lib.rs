//! The `vmbus` process (`architecture/moruna-os-system.md`, section 1F, "VMBus, split by
//! the microkernel rule"; work item 1.9c): `moruna-vmbus`'s client wired to the core's
//! Hyper-V mechanism, serving `moruna.driver.vmbus` to VMBus drivers.
//!
//! The core gives the process `root.hyperv` on Hyper-V only. With it the process posts
//! and takes VMBus's messages, reads the event flags, and waits on SINT 2's interrupt;
//! with its DMA grant it pins the memory it shares with the host, whose page numbers a
//! GPADL carries. Everything it needs of the node is behind [`Host`], so this library is
//! tested on a host against a fake of Hyper-V's side; `main.rs` is the process, which
//! implements [`Host`] over the core's calls.
//!
//! It makes contact as it starts, and says each offer on the console as it comes. A
//! driver, connected through the listener of its class of device, then asks for the
//! offers of its interface, opens a channel (the process makes the rings, pins them,
//! shares them as the channel's GPADL, and gives the driver the rings, the channel's
//! signal handle and a wake channel), shares more memory, and closes. Packets never pass
//! through the process: the driver signals the host itself, and the process only writes
//! an empty message to the wake channel of each channel the host signals, and closes it
//! when the channel is gone.
//!
//! A refusal of a post by Hyper-V, or contact the host refuses, is [`Fatal`]: the process
//! ends, and the supervisor stops the node with the reason on the console.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(feature = "trace-first-frames")]
pub mod trace;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    vec::Vec,
};
use core::{cell::RefCell, fmt, mem::offset_of, num::NonZeroU16};

use hvdef::{
    HV_MESSAGE_PAYLOAD_SIZE, HV_MESSAGE_SIZE, HvMessageFlags, HvMessageHeader, HvMessageType,
};
use moruna_svc::{
    Channel as Wake, ConnectionId, Context, Decoder, Factory, Handle, Handler, Outcome, Pending,
    ProtocolError, ProtocolId, Status, Wire,
};
use moruna_vmbus::{
    ChannelId, Client, ConnectError, Event, GpadlId, HostError, Hv, HvError, HvMessage, MemoryId,
    PostError, RingPair,
};
use vmbus_protocol::{
    Class, Id, Offer, Opened, rings_fit,
    vmbus::{self, Error},
};

/// The most messages from the host refused with a line each on the console; the rest are
/// only counted.
const REFUSED_LINES: u64 = 16;

/// What the process needs of the node besides Hyper-V's mechanism ([`Hv`]).
pub trait Host: Hv {
    /// Memory for a channel's rings, `pages` pages, pinned for the host to read and write:
    /// the driver's handle to it, and the ID [`Hv::page_numbers`] knows it by.
    fn rings(&mut self, pages: u32) -> Result<(Handle, MemoryId), Error>;

    /// Pins a driver's memory for the host to read and write: the ID
    /// [`Hv::page_numbers`] knows it by.
    fn pin(&mut self, memory: Handle) -> Result<MemoryId, Error>;

    /// The host has let go of `memory`: it is unpinned and forgotten.
    fn release(&mut self, memory: MemoryId);

    /// A handle that signals `connection_id` and nothing else, for a channel's driver.
    fn signal(&mut self, connection_id: u32) -> Result<Handle, Error>;

    /// Writes a line on the node's console.
    fn say(&mut self, line: fmt::Arguments<'_>);
}

/// Why the process cannot go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fatal {
    /// Hyper-V refused a post with a status other than "busy".
    Post(HvError),
    /// The host refused contact, or has no version the client offers.
    Contact(ConnectError),
}

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fatal::Post(status) => write!(f, "Hyper-V refused a post: {status}"),
            Fatal::Contact(error) => write!(f, "the host refused contact: {error:?}"),
        }
    }
}

/// Where contact with the host stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Contact {
    Connecting,
    Connected,
    Failed,
}

/// A channel a driver has opened, until the host has let go of its rings.
struct Channel {
    owner: ConnectionId,
    phase: Phase,
    rings: MemoryId,
    /// The process's end of the wake channel, until the channel is gone: dropping it
    /// closes it, which tells the driver.
    wake: Option<Wake>,
    /// What the driver is given once the host has opened the channel.
    given: Option<Opened>,
    /// The host has rescinded the offer.
    gone: bool,
}

/// Tells a driver the host signalled its channel: an empty message on its wake channel.
/// A full channel already holds one, and a closed one has no driver to tell.
fn wake(end: &Wake) {
    let _ = end.write_with(&[], &mut []);
}

/// Where a channel stands; each request's answer waits in it, `None` once its connection
/// has gone.
enum Phase {
    Opening(Option<Pending>),
    Open,
    Closing(Option<Pending>),
}

/// Memory a driver shared on a channel, until the host has let go of it.
struct Share {
    channel: ChannelId,
    owner: ConnectionId,
    memory: MemoryId,
    pending: Option<Pending>,
}

/// The process's state: the client, the channels drivers hold, and the answers that
/// are ready.
pub struct Vmbus<H: Host> {
    host: H,
    client: Client,
    contact: Contact,
    /// `offers` asked before contact was made, and the class of the connection asking.
    waiting: Vec<(Pending, Class, Id)>,
    channels: BTreeMap<ChannelId, Channel>,
    shares: BTreeMap<GpadlId, Share>,
    done: VecDeque<(Pending, Outcome)>,
    fatal: Option<Fatal>,
    refused: u64,
    /// When the host's signals of the network cards' channels reached the process, in a
    /// diagnosis build that asks (work item 1.11d).
    #[cfg(feature = "trace-first-frames")]
    trace: Option<trace::Interrupts>,
}

fn failure(error: Error) -> Outcome {
    Outcome::Failure(error.code())
}

/// The protocol's error for a request the client refused.
fn refusal(error: moruna_vmbus::Error) -> Error {
    use moruna_vmbus::Error as E;
    match error {
        E::NotDisconnected | E::NotConnected => Error::NotConnected,
        E::UnknownChannel(_) | E::Rescinded(_) => Error::NoSuchChannel,
        E::NotClosed(_) | E::GpadlBusy(_) => Error::Busy,
        E::NotOpen(_) | E::UnknownGpadl(_) => Error::NotOpen,
        E::UnknownMemory(_) | E::PageCount(_) => Error::BadMemory,
        E::RingPages { .. } => Error::BadRings,
    }
}

impl<H: Host> Vmbus<H> {
    /// The process over `host`, with no contact yet. Its contact and every channel target
    /// the VP `target_vp`: CPU 0's VP index, which `hyperv_interrupt` gives.
    pub fn new(host: H, target_vp: u32) -> Vmbus<H> {
        Vmbus {
            host,
            client: Client::new(target_vp),
            contact: Contact::Connecting,
            waiting: Vec::new(),
            channels: BTreeMap::new(),
            shares: BTreeMap::new(),
            done: VecDeque::new(),
            fatal: None,
            refused: 0,
            #[cfg(feature = "trace-first-frames")]
            trace: None,
        }
    }

    /// Times the host's signals of the network cards' channels opened from now on, with
    /// `trace`, and says them on the console (work item 1.11d).
    #[cfg(feature = "trace-first-frames")]
    pub fn trace_interrupts(&mut self, trace: trace::Interrupts) {
        self.trace = Some(trace);
    }

    /// The node the process runs on.
    pub fn host(&self) -> &H {
        &self.host
    }

    /// The node the process runs on.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Makes contact with the host. The answer comes with SINT 2's interrupt.
    pub fn start(&mut self) {
        if self.client.connect().is_ok() {
            self.pump();
        }
    }

    /// SINT 2's interrupt: the drivers of the channels the host signalled are woken, and
    /// the host's messages are taken.
    pub fn interrupted(&mut self) {
        for channel in self.client.take_signals(&mut self.host) {
            #[cfg(feature = "trace-first-frames")]
            if let Some(trace) = self.trace.as_mut() {
                trace.signalled(channel);
            }
            if let Some(end) = self.channels.get(&channel).and_then(|e| e.wake.as_ref()) {
                wake(end);
            }
        }
        #[cfg(feature = "trace-first-frames")]
        if let Some(line) = self.trace.as_mut().and_then(trace::Interrupts::take) {
            self.host.say(format_args!("{line}"));
        }
        self.pump();
    }

    /// Whether messages wait for a host that was busy: [`retry`](Self::retry) after a
    /// pause.
    pub fn has_unsent(&self) -> bool {
        self.client.has_unsent()
    }

    /// Posts the messages a busy host had no room for.
    pub fn retry(&mut self) {
        self.pump();
    }

    /// Why the process cannot go on, if it cannot.
    pub fn fatal(&self) -> Option<Fatal> {
        self.fatal
    }

    /// The next answer that is ready for a request that waited.
    pub fn finished(&mut self) -> Option<(Pending, Outcome)> {
        self.done.pop_front()
    }

    /// How many messages from the host were refused.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// How many requests wait for the host: offers asked before contact, and opens,
    /// shares and closes the host has yet to answer.
    pub fn pending(&self) -> usize {
        let channels = self.channels.values().filter(|entry| {
            matches!(
                entry.phase,
                Phase::Opening(Some(_)) | Phase::Closing(Some(_))
            )
        });
        let shares = self.shares.values().filter(|share| share.pending.is_some());
        self.waiting.len() + channels.count() + shares.count()
    }

    /// How many channels drivers hold, from their opening until the host has let go of
    /// their rings.
    pub fn channels(&self) -> usize {
        self.channels.len()
    }

    /// Does what the client can do now, and acts on each event.
    fn pump(&mut self) {
        loop {
            match self.client.poll(&mut self.host) {
                Ok(Some(event)) => self.event(event),
                Ok(None) => return,
                Err(HostError::Post(status)) => {
                    self.fail(Fatal::Post(status));
                    return;
                }
                // The message is dropped and nothing else is lost (`Client::poll`).
                Err(refused) => {
                    self.refused += 1;
                    if self.refused <= REFUSED_LINES {
                        self.host.say(format_args!(
                            "vmbus: refused a message from the host: {refused}"
                        ));
                    }
                }
            }
        }
    }

    fn fail(&mut self, fatal: Fatal) {
        self.contact = Contact::Failed;
        if self.fatal.is_none() {
            self.fatal = Some(fatal);
            self.host.say(format_args!("vmbus: {fatal}"));
        }
        for (pending, _, _) in core::mem::take(&mut self.waiting) {
            self.done.push_back((pending, failure(Error::NotConnected)));
        }
    }

    /// The offers of `interface` that stand, none unless it is `class`'s.
    fn list(&self, class: Class, interface: Id) -> Vec<Offer> {
        self.client
            .offers()
            .filter(|offer| offer.interface == interface.0 && interface == class.interface())
            .map(|offer| Offer {
                channel: offer.channel.0,
                instance: Id(offer.instance),
                subchannel: offer.subchannel,
            })
            .collect()
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Offer(offer) => self.host.say(format_args!(
                "vmbus: offer {} instance {} channel {} subchannel {}",
                offer.interface, offer.instance, offer.channel.0, offer.subchannel
            )),
            Event::Connected { version } => {
                self.contact = Contact::Connected;
                self.host.say(format_args!(
                    "vmbus: connected, version {:?}, CPU 0 is VP {}",
                    version.version,
                    self.client.target_vp()
                ));
                for (pending, class, interface) in core::mem::take(&mut self.waiting) {
                    let offers = self.list(class, interface);
                    self.done.push_back((pending, Outcome::reply(offers)));
                }
            }
            Event::ConnectFailed(error) => self.fail(Fatal::Contact(error)),
            Event::GpadlCreated { gpadl, .. } => self.created(gpadl),
            Event::GpadlFailed { gpadl, status, .. } => {
                if let Some(share) = self.shares.remove(&gpadl) {
                    self.host.release(share.memory);
                    self.host.say(format_args!(
                        "vmbus: the host refused memory on channel {}: status {status:#x}",
                        share.channel.0
                    ));
                    if let Some(pending) = share.pending {
                        self.done.push_back((pending, failure(Error::Refused)));
                    }
                }
            }
            Event::GpadlTorndown { gpadl, .. } => {
                if let Some(share) = self.shares.remove(&gpadl) {
                    self.host.release(share.memory);
                    // Torn down before it was ever created: the channel is gone.
                    if let Some(pending) = share.pending {
                        self.done
                            .push_back((pending, failure(Error::NoSuchChannel)));
                    }
                }
            }
            Event::Opened { channel } => self.opened(channel),
            Event::OpenFailed { channel, status } => {
                // The wake channel goes with the entry.
                if let Some(entry) = self.channels.remove(&channel) {
                    self.host.release(entry.rings);
                    self.host.say(format_args!(
                        "vmbus: the host refused to open channel {}: status {status:#x}",
                        channel.0
                    ));
                    if let Phase::Opening(Some(pending)) = entry.phase {
                        self.done.push_back((pending, failure(Error::Refused)));
                    }
                }
            }
            Event::Closed { channel } => {
                // The wake channel goes with the entry, which tells the driver.
                if let Some(entry) = self.channels.remove(&channel) {
                    self.host.release(entry.rings);
                    match entry.phase {
                        Phase::Closing(Some(pending)) => {
                            self.done.push_back((pending, Outcome::reply(())));
                        }
                        // Rescinded while it was opening.
                        Phase::Opening(Some(pending)) => {
                            self.done
                                .push_back((pending, failure(Error::NoSuchChannel)));
                        }
                        _ => {}
                    }
                }
            }
            Event::Rescinded { channel } => {
                self.host
                    .say(format_args!("vmbus: channel {} rescinded", channel.0));
                if let Some(entry) = self.channels.get_mut(&channel) {
                    entry.gone = true;
                    // Closing the wake channel tells the driver the device is gone.
                    entry.wake = None;
                }
            }
            Event::Unloaded => {}
        }
    }

    /// The host has a GPADL a driver shared: the driver has its ID, unless the channel
    /// has gone out of the driver's hands meanwhile, when it is torn down at once.
    fn created(&mut self, gpadl: GpadlId) {
        let Some(share) = self.shares.get_mut(&gpadl) else {
            return;
        };
        let open = self
            .channels
            .get(&share.channel)
            .is_some_and(|entry| matches!(entry.phase, Phase::Open) && !entry.gone);
        match share.pending.take() {
            Some(pending) if open => {
                self.done.push_back((pending, Outcome::reply(gpadl.0)));
            }
            pending => {
                if let Some(pending) = pending {
                    self.done.push_back((pending, failure(Error::NotOpen)));
                }
                // A rescinded channel's GPADLs the client tears down itself.
                let _ = self.client.teardown_gpadl(share.channel, gpadl);
            }
        }
    }

    /// The host has opened a channel: the driver is given it, or, if the driver has gone,
    /// it is closed again.
    fn opened(&mut self, channel: ChannelId) {
        let Some(entry) = self.channels.get_mut(&channel) else {
            return;
        };
        match core::mem::replace(&mut entry.phase, Phase::Open) {
            Phase::Opening(Some(pending)) => {
                let outcome = entry
                    .given
                    .take()
                    .map_or(failure(Error::NoResources), Outcome::reply);
                self.done.push_back((pending, outcome));
            }
            Phase::Opening(None) => {
                entry.given = None;
                entry.phase = Phase::Closing(None);
                let gone = entry.gone;
                if !gone {
                    let _ = self.begin_close(channel);
                }
            }
            other => entry.phase = other,
        }
    }

    /// Tears down the memory shared on `channel`, and closes it.
    fn begin_close(&mut self, channel: ChannelId) -> Result<(), moruna_vmbus::Error> {
        let shared: Vec<GpadlId> = self
            .shares
            .iter()
            .filter(|(_, share)| share.channel == channel && share.pending.is_none())
            .map(|(gpadl, _)| *gpadl)
            .collect();
        for gpadl in shared {
            let _ = self.client.teardown_gpadl(channel, gpadl);
        }
        self.client.close(channel)
    }

    /// `offers`, for a connection of `class`.
    pub fn offers(&mut self, cx: &mut Context<'_>, class: Class, interface: Id) -> Outcome {
        match self.contact {
            Contact::Connected => Outcome::reply(self.list(class, interface)),
            Contact::Connecting => {
                self.waiting.push((cx.defer(), class, interface));
                Outcome::Deferred
            }
            Contact::Failed => failure(Error::NotConnected),
        }
    }

    /// `open`, for a connection of `class`: only an offer of the class's interface.
    pub fn open(
        &mut self,
        cx: &mut Context<'_>,
        class: Class,
        channel: u32,
        pages: u32,
        outgoing: u32,
    ) -> Outcome {
        let id = ChannelId(channel);
        if self.contact != Contact::Connected {
            return failure(Error::NotConnected);
        }
        let interface = class.interface().0;
        if !self
            .client
            .offers()
            .any(|offer| offer.channel == id && offer.interface == interface)
        {
            return failure(Error::NoSuchChannel);
        }
        if self.channels.contains_key(&id) {
            return failure(Error::Busy);
        }
        if !rings_fit(pages, outgoing) {
            return failure(Error::BadRings);
        }
        let Some(connection_id) = self.client.offer(id).map(|offer| offer.connection_id) else {
            return failure(Error::NoSuchChannel);
        };
        let (rings, memory) = match self.host.rings(pages) {
            Ok(rings) => rings,
            Err(error) => return failure(error),
        };
        let signal = match self.host.signal(connection_id) {
            Ok(signal) => signal,
            Err(error) => {
                self.host.release(memory);
                return failure(error);
            }
        };
        let Ok((wake, driver_wake)) = Wake::create() else {
            self.host.release(memory);
            return failure(Error::NoResources);
        };
        let ring = RingPair {
            memory,
            outgoing_pages: outgoing,
        };
        if let Err(error) = self.client.open(&mut self.host, id, ring) {
            self.host.release(memory);
            return failure(refusal(error));
        }
        #[cfg(feature = "trace-first-frames")]
        if class == Class::Net
            && let Some(trace) = self.trace.as_mut()
        {
            trace.follow(id);
        }
        self.channels.insert(
            id,
            Channel {
                owner: cx.connection(),
                phase: Phase::Opening(Some(cx.defer())),
                rings: memory,
                wake: Some(wake),
                given: Some(Opened {
                    rings,
                    signal,
                    wake: driver_wake.0,
                }),
                gone: false,
            },
        );
        self.pump();
        Outcome::Deferred
    }

    /// The channel `channel` if it is open and the caller's.
    fn own(&self, cx: &Context<'_>, channel: ChannelId) -> Result<&Channel, Error> {
        let entry = self.channels.get(&channel).ok_or(Error::NotOpen)?;
        if entry.owner != cx.connection() {
            return Err(Error::Busy);
        }
        if entry.gone {
            return Err(Error::NoSuchChannel);
        }
        if !matches!(entry.phase, Phase::Open) {
            return Err(Error::NotOpen);
        }
        Ok(entry)
    }

    /// `share`.
    pub fn share(&mut self, cx: &mut Context<'_>, channel: u32, memory: Handle) -> Outcome {
        let id = ChannelId(channel);
        if let Err(error) = self.own(cx, id) {
            return failure(error);
        }
        let memory = match self.host.pin(memory) {
            Ok(memory) => memory,
            Err(error) => return failure(error),
        };
        let gpadl = match self.client.create_gpadl(&mut self.host, id, memory) {
            Ok(gpadl) => gpadl,
            Err(error) => {
                self.host.release(memory);
                return failure(refusal(error));
            }
        };
        self.shares.insert(
            gpadl,
            Share {
                channel: id,
                owner: cx.connection(),
                memory,
                pending: Some(cx.defer()),
            },
        );
        self.pump();
        Outcome::Deferred
    }

    /// `close`.
    pub fn close(&mut self, cx: &mut Context<'_>, channel: u32) -> Outcome {
        let id = ChannelId(channel);
        if let Err(error) = self.own(cx, id) {
            return failure(error);
        }
        if let Err(error) = self.begin_close(id) {
            return failure(refusal(error));
        }
        if let Some(entry) = self.channels.get_mut(&id) {
            entry.phase = Phase::Closing(Some(cx.defer()));
        }
        self.pump();
        Outcome::Deferred
    }

    /// A driver's connection has gone: its channels are closed, and nothing waits for it.
    pub fn disconnected(&mut self, connection: ConnectionId) {
        self.waiting
            .retain(|(pending, _, _)| pending.connection() != connection);
        for share in self.shares.values_mut() {
            if share.owner == connection {
                share.pending = None;
            }
        }
        let owned: Vec<ChannelId> = self
            .channels
            .iter()
            .filter(|(_, entry)| entry.owner == connection)
            .map(|(id, _)| *id)
            .collect();
        for id in owned {
            let Some(entry) = self.channels.get_mut(&id) else {
                continue;
            };
            let close = matches!(entry.phase, Phase::Open) && !entry.gone;
            entry.phase = match entry.phase {
                // Closed once the host has opened it.
                Phase::Opening(_) => Phase::Opening(None),
                Phase::Open | Phase::Closing(_) => Phase::Closing(None),
            };
            if close {
                let _ = self.begin_close(id);
            }
        }
        self.pump();
    }
}

/// One driver's connection: it serves `moruna.driver.vmbus` from the process's state, for
/// the offers of its class.
pub struct Connection<H: Host> {
    vmbus: Rc<RefCell<Vmbus<H>>>,
    class: Class,
    id: Option<ConnectionId>,
}

/// The factory of `class`'s listener: each driver that connects there is served from
/// `vmbus`, and reaches the offers of that class only.
pub fn factory<H: Host + 'static>(vmbus: Rc<RefCell<Vmbus<H>>>, class: Class) -> Factory {
    Box::new(move || {
        Box::new(Connection {
            vmbus: vmbus.clone(),
            class,
            id: None,
        }) as Box<dyn Handler>
    })
}

/// A request's arguments, every byte and handle of them.
fn arguments<T: Wire>(body: &mut Decoder<'_>) -> Option<T> {
    let value = T::decode(body).ok()?;
    body.finish().ok()?;
    Some(value)
}

impl<H: Host> Handler for Connection<H> {
    fn protocol(&self) -> ProtocolId {
        vmbus::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, body: &mut Decoder<'_>) -> Outcome {
        self.id = Some(cx.connection());
        let mut vmbus = self.vmbus.borrow_mut();
        let class = self.class;
        let outcome = match ordinal {
            1 => arguments(body).map(|interface| vmbus.offers(cx, class, interface)),
            2 => arguments(body)
                .map(|(channel, pages, outgoing)| vmbus.open(cx, class, channel, pages, outgoing)),
            3 => arguments(body).map(|(channel, memory)| vmbus.share(cx, channel, memory)),
            4 => arguments(body).map(|channel| vmbus.close(cx, channel)),
            _ => Some(Outcome::Status(Status::UnknownMethod)),
        };
        while let Some((pending, answer)) = vmbus.finished() {
            cx.complete(pending, answer);
        }
        outcome.unwrap_or(Outcome::Status(Status::BadMessage))
    }

    fn closed(&mut self) {
        if let Some(id) = self.id {
            self.vmbus.borrow_mut().disconnected(id);
        }
    }
}

/// What the status of `hyperv_post` means to the client: posted, busy (the same message
/// is posted again later), or refused.
pub fn post_result(status: u64) -> Result<(), PostError> {
    if status == 0 {
        return Ok(());
    }
    let error = u16::try_from(status)
        .ok()
        .and_then(NonZeroU16::new)
        .map_or(HvError::InvalidParameter, HvError::from);
    if error == HvError::InsufficientBuffers {
        Err(PostError::Busy)
    } else {
        Err(PostError::Hv(error))
    }
}

// The SynIC message header, as `hyperv_take` copies it out (TLFS, "SynIC Message
// Buffer"): the type, the payload's length, the flags, a reserved word, the sender.
const _: () = {
    assert!(offset_of!(HvMessageHeader, typ) == 0);
    assert!(offset_of!(HvMessageHeader, len) == 4);
    assert!(offset_of!(HvMessageHeader, flags) == 5);
    assert!(offset_of!(HvMessageHeader, rsvd) == 6);
    assert!(offset_of!(HvMessageHeader, id) == 8);
    assert!(HV_MESSAGE_SIZE == 16 + HV_MESSAGE_PAYLOAD_SIZE);
};

/// A SynIC message from the bytes `hyperv_take` copied out of SINT 2's slot, as Hyper-V
/// wrote them: the client checks the header itself.
pub fn message(bytes: &[u8; HV_MESSAGE_SIZE]) -> HvMessage {
    let word =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let mut id = [0; 8];
    id.copy_from_slice(&bytes[8..16]);
    let mut payload_buffer = [0; HV_MESSAGE_PAYLOAD_SIZE];
    payload_buffer.copy_from_slice(&bytes[16..]);
    HvMessage {
        header: HvMessageHeader {
            typ: HvMessageType(word(0)),
            len: bytes[4],
            flags: HvMessageFlags::from_bits(bytes[5]),
            rsvd: u16::from_le_bytes([bytes[6], bytes[7]]),
            id: u64::from_le_bytes(id),
        },
        payload_buffer,
    }
}

/// The guest page numbers of pinned memory, from the addresses `memory_pin` wrote. With
/// no IOMMU between the core and Hyper-V's devices, those are guest physical addresses
/// (system design 1F: "A GPADL's guest page numbers come from the DMA grant"). `None` if
/// an address is not a page's.
pub fn page_numbers(addresses: &[u64]) -> Option<Vec<u64>> {
    const PAGE: u64 = moruna_vmbus::PAGE_SIZE as u64;
    addresses
        .iter()
        .map(|address| address.is_multiple_of(PAGE).then_some(address / PAGE))
        .collect()
}
