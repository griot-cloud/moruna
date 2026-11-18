//! The process on the node: the library over the core's calls, `vmbus` and the device
//! manager, and the loop.
//!
//! Its start handles: `console`; `vmbus`, its end of `vmbus`'s `net` listener, whose server
//! it is a client of; and `devices`, its end of the device manager's `register-netvsc`
//! listener, which lets it register the card and nothing else. No DMA grant: its two buffers
//! are shared with the host through `vmbus`, which pins them with its own.

use alloc::{boxed::Box, format, rc::Rc, string::String, vec, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    sync::atomic::AtomicU8,
};

use moruna_netvsc::{Buffer, Netvsc, Shared};
use moruna_netvsc_server::{
    Driver, NAME, OUTGOING_PAGES, RECEIVE_BUFFER, RING_PAGES, SEND_BUFFER, describe, factory,
};
use moruna_svc::{
    CallError, Channel, Completions, DEADLINE_INFINITE, Dispatcher, Handle, Signals, Watch,
    connect, deadline_in, wait_one,
};
use moruna_sys::{Console, Mapping, Memory, hyperv::HvSignal, map, println, start};
use moruna_vmbus::{ChannelError, GpadlId, MemoryId, Wait};
use vmbus_protocol::{Class, Opened, vmbus};
use vmbus_ring::{IncomingRing, OutgoingRing, SingleMappedRingMem};
use zerocopy::FromBytes;

/// A page, as memory objects count them.
const PAGE: u64 = 4096;

/// How long `vmbus` and the device manager may take to answer.
const ANSWER_NS: u64 = 30_000_000_000;

/// How long the card's start may take: NVSP and RNDIS, a few of the host's answers.
const START_NS: u64 = 60_000_000_000;

/// A ring's memory: a control page and its data, in the rings `vmbus` made.
type Ring = SingleMappedRingMem<&'static [AtomicU8]>;

/// One of the card's two buffers, mapped for as long as the process lives: the host reads
/// and writes it at any time once it is shared.
#[derive(Clone, Copy)]
struct Mapped(&'static Mapping<'static>);

impl Shared for Mapped {
    fn len(&self) -> usize {
        self.0.len() as usize
    }

    fn read(&self, at: usize, data: &mut [u8]) {
        // The card keeps every access within the buffer.
        let _ = self.0.read(at, data);
    }

    fn write(&self, at: usize, data: &[u8]) {
        let _ = self.0.write(at, data);
    }
}

/// A buffer of `bytes`, DMA memory `vmbus` can pin, mapped: the card's view of it, and the
/// handle `share` hands to `vmbus`.
fn buffer(id: u64, bytes: usize) -> Result<(Buffer<Mapped>, Handle), String> {
    let memory = Memory::create_dma(bytes as u64).map_err(|e| format!("a buffer: {}", e.name()))?;
    let mapping = memory
        .map_detached(0, bytes as u64, map::READ | map::WRITE)
        .map_err(|e| format!("mapping a buffer: {}", e.name()))?;
    let mapping: &'static Mapping<'static> = Box::leak(Box::new(mapping));
    Ok((
        Buffer {
            id: MemoryId(id),
            memory: Mapped(mapping),
        },
        memory.0,
    ))
}

/// The channel `vmbus` opened for the process: its rings, its signal handle, its wake
/// channel, which the dispatcher takes once the card has started, and the connection to
/// `vmbus` that shares the card's buffers on it. The connection is kept as long as the
/// process lives: `vmbus` closes a connection's channels when it goes.
struct Opening {
    incoming: IncomingRing<Ring>,
    outgoing: OutgoingRing<Ring>,
    signal: HvSignal,
    wake: Cell<Option<Channel>>,
    vmbus: vmbus::Client,
    channel: u32,
    /// The buffers not yet shared, by the ID the card names them by.
    unshared: Vec<(MemoryId, Handle)>,
}

impl Opening {
    fn new(
        opened: Opened,
        vmbus: vmbus::Client,
        channel: u32,
        unshared: Vec<(MemoryId, Handle)>,
    ) -> Result<Opening, String> {
        let memory = Memory(opened.rings);
        let bytes = u64::from(RING_PAGES) * PAGE;
        let mapping = memory
            .map_detached(0, bytes, map::READ | map::WRITE)
            .map_err(|e| format!("mapping the rings: {}", e.name()))?;
        // The rings are the process's for as long as it lives: their mapping is never let go.
        let mapping: &'static mut Mapping<'static> = Box::leak(Box::new(mapping));
        let region = mapping
            .shared_region()
            .map_err(|e| format!("the rings' bytes: {}", e.name()))?;
        let atomics = <[AtomicU8]>::mut_from_bytes(region)
            .map_err(|_| String::from("the rings' bytes as atomics"))?;
        let (outgoing, incoming) = atomics.split_at_mut(OUTGOING_PAGES as usize * PAGE as usize);
        let ring = |e: vmbus_ring::Error| format!("a ring: {e}");
        Ok(Opening {
            outgoing: OutgoingRing::new(SingleMappedRingMem(&*outgoing)).map_err(ring)?,
            incoming: IncomingRing::new(SingleMappedRingMem(&*incoming)).map_err(ring)?,
            signal: HvSignal(opened.signal),
            wake: Cell::new(Some(Channel(opened.wake))),
            vmbus,
            channel,
            unshared,
        })
    }

    /// Hands the wake channel to the dispatcher, which waits on it from now on.
    fn take_wake(&self) -> Option<Channel> {
        self.wake.take()
    }
}

/// Reads every message waiting on a wake channel: whether its peer, `vmbus`, has closed it.
fn drain(wake: &Channel) -> bool {
    loop {
        match wake.read(&mut [], &mut []) {
            Ok(_) => {}
            Err(moruna_svc::Error::PeerClosed) => return true,
            Err(_) => return false,
        }
    }
}

impl moruna_vmbus::Channel for Opening {
    type Memory = Ring;

    fn incoming(&self) -> &IncomingRing<Ring> {
        &self.incoming
    }

    fn outgoing(&self) -> &OutgoingRing<Ring> {
        &self.outgoing
    }

    fn signal_host(&mut self) -> Result<(), ChannelError> {
        self.signal
            .signal()
            .map(|_| ())
            .map_err(|_| ChannelError::Closed)
    }

    /// Only the card's start waits here; serving, the dispatcher waits on the wake channel.
    fn wait(&mut self, deadline: Option<u64>) -> Result<Wait, ChannelError> {
        let Some(wake) = self.wake.take() else {
            return Ok(Wait::Signalled);
        };
        let seen = wait_one(
            &wake.0,
            Signals::READABLE | Signals::PEER_CLOSED,
            deadline.unwrap_or(DEADLINE_INFINITE),
        );
        let answer = match seen {
            Ok(_) if drain(&wake) => Err(ChannelError::Closed),
            Ok(_) => Ok(Wait::Signalled),
            Err(moruna_svc::Error::TimedOut) => Ok(Wait::TimedOut),
            Err(_) => Err(ChannelError::Closed),
        };
        self.wake.set(Some(wake));
        answer
    }

    /// One of the card's buffers, shared through `vmbus`, which pins it and makes the
    /// GPADL: once, as the card asks during its start.
    fn share(&mut self, memory: MemoryId) -> Result<GpadlId, ChannelError> {
        let at = self
            .unshared
            .iter()
            .position(|(id, _)| *id == memory)
            .ok_or(ChannelError::NotShared)?;
        let (_, handle) = self.unshared.swap_remove(at);
        match self.vmbus.share(self.channel, handle) {
            Ok(gpadl) => Ok(GpadlId(gpadl)),
            Err(CallError::Remote(vmbus::Error::NoSuchChannel | vmbus::Error::NotOpen))
            | Err(CallError::PeerClosed) => Err(ChannelError::Closed),
            Err(_) => Err(ChannelError::NotShared),
        }
    }
}

type Served = Rc<RefCell<Driver<Opening, Mapped>>>;

/// The card's wake channel, in the dispatcher's wait: the host has sent something, or
/// made room.
struct Woken {
    wake: Channel,
    driver: Served,
    gone: Rc<Cell<bool>>,
}

impl Watch for Woken {
    fn handle(&self) -> &Handle {
        &self.wake.0
    }

    fn signals(&self) -> Signals {
        Signals::READABLE | Signals::PEER_CLOSED
    }

    fn signaled(&mut self, _: Signals, done: &mut Completions) -> bool {
        let closed = drain(&self.wake);
        let mut driver = self.driver.borrow_mut();
        driver.service();
        for (pending, outcome) in driver.take_ready() {
            done.complete(pending, outcome);
        }
        if closed {
            self.gone.set(true);
        }
        !closed
    }
}

fn take(name: &str) -> Result<Handle, String> {
    start::take(name).ok_or_else(|| format!("no start handle {name}"))
}

/// The first synthetic network card `vmbus` offers, opened and started.
fn card(console: &Console, vmbus_listener: &Channel) -> Result<Netvsc<Opening, Mapped>, String> {
    let channel = connect(vmbus_listener).map_err(|e| format!("vmbus: {e:?}"))?;
    let mut vmbus = vmbus::Client::connect(channel, deadline_in(ANSWER_NS))
        .map_err(|e| format!("vmbus: {e:?}"))?;
    vmbus.core().set_timeout(ANSWER_NS);
    let offers = vmbus
        .offers(Class::Net.interface())
        .map_err(|e| format!("vmbus's offers: {e:?}"))?;
    let mut cards = offers.iter().filter(|offer| offer.subchannel == 0);
    let offer = cards
        .next()
        .ok_or("vmbus offers no synthetic network card")?;
    for other in cards {
        println!(
            console,
            "netvsc: card {}: not served, only the first card is", other.instance.0
        );
    }
    let instance = offer.instance.0;
    let opened = vmbus
        .open(offer.channel, RING_PAGES, OUTGOING_PAGES)
        .map_err(|e| format!("card {instance}: open: {e:?}"))?;
    let (receive, receive_memory) = buffer(1, RECEIVE_BUFFER)?;
    let (send, send_memory) = buffer(2, SEND_BUFFER)?;
    let unshared = vec![(receive.id, receive_memory), (send.id, send_memory)];
    let channel = Opening::new(opened, vmbus, offer.channel, unshared)
        .map_err(|e| format!("card {instance}: {e}"))?;
    let card = Netvsc::start(channel, receive, send, Some(deadline_in(START_NS)))
        .map_err(|e| format!("card {instance}: {e}"))?;
    println!(console, "netvsc: card {instance}: {}", describe(&card));
    Ok(card)
}

fn serve(console: &Console) -> Result<(), String> {
    let vmbus_listener = Channel(take("vmbus")?);
    // The registrar's connection first, before the card is up: while it is open, the
    // network server's lookup of `net0` waits for the card rather than finding nothing.
    let devices = Channel(take("devices")?);
    let mut directory = devices_protocol::open_directory(devices, deadline_in(ANSWER_NS))
        .map_err(|e| format!("the device manager: {e:?}"))?;
    directory.core().set_timeout(ANSWER_NS);
    let mut card = card(console, &vmbus_listener)?;
    let wake = card
        .channel()
        .take_wake()
        .ok_or("the card's wake channel")?;
    let driver: Served = Rc::new(RefCell::new(Driver::new(card)));
    let mut dispatcher = Dispatcher::new();
    let gone = Rc::new(Cell::new(false));
    dispatcher
        .watch(Box::new(Woken {
            wake,
            driver: driver.clone(),
            gone: gone.clone(),
        }))
        .map_err(|e| format!("watching the wake channel: {}", e.name()))?;
    let (ours, theirs) =
        Channel::create().map_err(|e| format!("the card's listener: {}", e.name()))?;
    dispatcher
        .listen(ours, factory(driver.clone()))
        .map_err(|e| format!("the card's listener: {}", e.name()))?;
    // Registered once it is served: a lookup of the name connects at once. The connection
    // closes when registering is done, which the device manager's waiting lookups hear.
    match directory.register_net(String::from(NAME), theirs) {
        Ok(()) => println!(console, "netvsc: served as {}", NAME),
        Err(e) => println!(
            console,
            "netvsc: {}: the device manager did not register it: {:?}", NAME, e
        ),
    }
    drop(directory);
    loop {
        {
            let mut driver = driver.borrow_mut();
            driver.service();
            for (pending, outcome) in driver.take_ready() {
                // An answer whose client has gone is dropped.
                let _ = dispatcher.complete(pending, outcome);
            }
            for line in driver.take_lines() {
                println!(console, "{}", line);
            }
            if let Some(why) = driver.failed() {
                return Err(String::from(why));
            }
        }
        if gone.get() {
            return Err(String::from("the network card is gone"));
        }
        dispatcher
            .turn(DEADLINE_INFINITE)
            .map_err(|e| format!("wait: {}", e.name()))?;
    }
}

/// The process: it serves until the card fails or goes, which ends it, and the node with
/// it.
pub fn run() -> i64 {
    let Some(console) = start::take("console").map(Console) else {
        return 1;
    };
    match serve(&console) {
        Ok(()) => 0,
        Err(reason) => {
            println!(
                console,
                "{}",
                moruna_abi::boot_line::Fatal {
                    stage: "netvsc",
                    reason: &reason
                }
            );
            1
        }
    }
}
