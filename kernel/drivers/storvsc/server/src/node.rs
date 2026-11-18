//! The process on the node: the library over the core's calls, `vmbus` and the device
//! manager, and the loop.
//!
//! Its start handles: `console`; `dma`, its own DMA grant, a copy of `root.dma` (1F);
//! `vmbus`, its end of `vmbus`'s `scsi` listener, whose server it is a client of; and
//! `devices`, its end of the device manager's `register-storvsc` listener, which lets it
//! register block devices and nothing else.

use alloc::{boxed::Box, format, rc::Rc, string::String, vec, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    fmt,
    sync::atomic::AtomicU8,
};

use devices_protocol::Block;
use moruna_storvsc::{ChannelError, Storvsc, Wait};
use moruna_storvsc_server::{
    Controller, Driver, Node, OUTGOING_PAGES, RING_PAGES, factory, plan,
    scan::{self, Scratch},
};
use moruna_svc::{
    Channel, Completions, DEADLINE_INFINITE, Dispatcher, Handle, Signals, Watch, connect,
    deadline_in, now, wait_one,
};
use moruna_sys::{
    Console, Mapping, Memory, Pin, dma_release, hyperv::HvSignal, map, println, start,
};
use moruna_vmbus::{GpadlId, MemoryId};
use vmbus_protocol::{Class, Opened, vmbus};
use vmbus_ring::{IncomingRing, OutgoingRing, SingleMappedRingMem};
use zerocopy::FromBytes;

/// A page, as memory objects and guest page numbers count them.
const PAGE: u64 = 4096;

/// How long `vmbus` and the device manager may take to answer.
const ANSWER_NS: u64 = 30_000_000_000;

/// A ring's memory: a control page and its data, in the rings `vmbus` made.
type Ring = SingleMappedRingMem<&'static [AtomicU8]>;

/// A channel `vmbus` opened for the process: its rings, its signal handle, and its wake
/// channel, which the dispatcher takes once the scan is done.
struct Opening {
    incoming: IncomingRing<Ring>,
    outgoing: OutgoingRing<Ring>,
    signal: HvSignal,
    wake: Cell<Option<Channel>>,
}

impl Opening {
    fn new(opened: Opened) -> Result<Opening, String> {
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

impl moruna_storvsc::Channel for Opening {
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

    /// Only the scan waits here; serving, the dispatcher waits on the wake channel.
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

    /// storvsc names guest pages in every request and shares no memory as a GPADL.
    fn share(&mut self, _: MemoryId) -> Result<GpadlId, ChannelError> {
        Err(ChannelError::NotShared)
    }
}

/// The node, as the driver reaches it.
struct Os {
    console: Console,
    dma: Handle,
}

/// The guest page numbers of pinned memory, from the addresses the pin wrote: with no
/// IOMMU they are guest physical addresses (1F).
fn page_numbers(addresses: &[u64]) -> Option<Vec<u64>> {
    addresses
        .iter()
        .map(|address| address.is_multiple_of(PAGE).then_some(address / PAGE))
        .collect()
}

impl Node for Os {
    type Pin = Pin;

    fn pin(&mut self, memory: &Memory, device_writes: bool) -> Option<(Pin, Vec<u64>)> {
        let n = Pin::pages(&self.dma, &memory.0, device_writes).ok()?;
        let mut addresses = vec![0; n];
        let (pin, _) = Pin::create(&self.dma, &memory.0, device_writes, &mut addresses).ok()?;
        Some((pin, page_numbers(&addresses)?))
    }

    fn release(&mut self) {
        let _ = dma_release(&self.dma);
    }

    fn say(&mut self, line: fmt::Arguments<'_>) {
        println!(self.console, "{}", line);
    }
}

/// The scan's page: one page of DMA memory pinned for the host to write, mapped to read.
struct Page {
    _pin: Pin,
    page: u64,
    mapping: Mapping<'static>,
}

impl Page {
    fn new(dma: &Handle) -> Result<Page, String> {
        let memory = Memory::create_dma(PAGE).map_err(|e| format!("DMA memory: {}", e.name()))?;
        let mut address = [0];
        let (pin, _) = Pin::create(dma, &memory.0, true, &mut address)
            .map_err(|e| format!("pinning the scan's page: {}", e.name()))?;
        let page = page_numbers(&address).ok_or("the scan's page is not a page")?[0];
        let mapping = memory
            .map_detached(0, PAGE, 0)
            .map_err(|e| format!("mapping the scan's page: {}", e.name()))?;
        Ok(Page {
            _pin: pin,
            page,
            mapping,
        })
    }
}

impl Scratch for Page {
    fn page(&self) -> u64 {
        self.page
    }

    fn read(&self, out: &mut [u8]) {
        let _ = self.mapping.read(0, out);
    }

    fn now(&self) -> u64 {
        now()
    }
}

type Shared = Rc<RefCell<Driver<Opening, Os>>>;

/// A controller's wake channel, in the dispatcher's wait: the host has data for it.
struct Woken {
    wake: Channel,
    driver: Shared,
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

/// Each SCSI controller `vmbus` offers, opened and scanned: its instance, its device, and
/// what was found on it.
type Scanned = (vmbus_protocol::Guid, Storvsc<Opening>, scan::Scan);

/// Opens and scans every SCSI controller `vmbus` offers. The connection is kept as long as
/// the process lives: `vmbus` closes a connection's channels when it goes.
fn controllers(
    vmbus_listener: &Channel,
    scratch: &Page,
) -> Result<(vmbus::Client, Vec<Scanned>), String> {
    let channel = connect(vmbus_listener).map_err(|e| format!("vmbus: {e:?}"))?;
    let mut vmbus = vmbus::Client::connect(channel, deadline_in(ANSWER_NS))
        .map_err(|e| format!("vmbus: {e:?}"))?;
    vmbus.core().set_timeout(ANSWER_NS);
    let offers = vmbus
        .offers(Class::Scsi.interface())
        .map_err(|e| format!("vmbus's offers: {e:?}"))?;
    let mut found = Vec::new();
    for offer in offers.iter().filter(|offer| offer.subchannel == 0) {
        let instance = offer.instance.0;
        let opened = vmbus
            .open(offer.channel, RING_PAGES, OUTGOING_PAGES)
            .map_err(|e| format!("controller {instance}: open: {e:?}"))?;
        let channel = Opening::new(opened).map_err(|e| format!("controller {instance}: {e}"))?;
        let mut device =
            scan::start(channel, scratch).map_err(|e| format!("controller {instance}: {e}"))?;
        let scanned =
            scan::scan(&mut device, scratch).map_err(|e| format!("controller {instance}: {e}"))?;
        found.push((instance, device, scanned));
    }
    Ok((vmbus, found))
}

fn serve(console: &Console) -> Result<(), String> {
    let dma = take("dma")?;
    let vmbus_listener = Channel(take("vmbus")?);
    let devices = Channel(take("devices")?);
    let scratch = Page::new(&dma)?;
    let (_vmbus, found) = controllers(&vmbus_listener, &scratch)?;
    drop(scratch);
    let scans: Vec<Controller<'_>> = found
        .iter()
        .map(|(instance, _, scan)| Controller {
            instance: *instance,
            scan,
        })
        .collect();
    let plan = plan(&scans);
    for line in &plan.lines {
        println!(console, "{}", line);
    }
    let mut wakes = Vec::new();
    let mut devices_found = Vec::new();
    for (_, device, _) in found {
        wakes.extend(device.channel().take_wake());
        devices_found.push(device);
    }
    let said = console
        .0
        .duplicate(moruna_sys::Rights::WRITE)
        .map_err(|e| format!("console: {}", e.name()))?;
    let os = Os {
        console: Console(said),
        dma,
    };
    let driver: Shared = Rc::new(RefCell::new(Driver::new(os, devices_found, plan.served)));
    let mut dispatcher = Dispatcher::new();
    let gone = Rc::new(Cell::new(false));
    for wake in wakes {
        dispatcher
            .watch(Box::new(Woken {
                wake,
                driver: driver.clone(),
                gone: gone.clone(),
            }))
            .map_err(|e| format!("watching a wake channel: {}", e.name()))?;
    }
    let mut directory = devices_protocol::open_directory(devices, deadline_in(ANSWER_NS))
        .map_err(|e| format!("the device manager: {e:?}"))?;
    directory.core().set_timeout(ANSWER_NS);
    let served: Vec<_> = driver.borrow().disks().cloned().collect();
    for (index, disk) in served.into_iter().enumerate() {
        let (ours, theirs) =
            Channel::create().map_err(|e| format!("a disk's listener: {}", e.name()))?;
        dispatcher
            .listen(ours, factory(driver.clone(), index))
            .map_err(|e| format!("a disk's listener: {}", e.name()))?;
        let block = Block {
            serial: disk.name.serial.clone(),
            sector_size: disk.disk.block_size(),
            sectors: disk.disk.blocks(),
            read_only: disk.name.read_only,
            boot: disk.name.boot,
        };
        if let Err(e) = directory.register(block, theirs) {
            println!(
                console,
                "storvsc: {}: the device manager did not register it: {:?}", disk.name.serial, e
            );
        }
    }
    drop(directory);
    loop {
        {
            let mut driver = driver.borrow_mut();
            driver.service();
            for (pending, outcome) in driver.take_ready() {
                let _peer_gone = dispatcher.complete(pending, outcome);
            }
            if let Some(why) = driver.failed() {
                return Err(String::from(why));
            }
        }
        if gone.get() {
            return Err(String::from("a SCSI controller is gone"));
        }
        dispatcher
            .turn(DEADLINE_INFINITE)
            .map_err(|e| format!("wait: {}", e.name()))?;
    }
}

/// The process: it serves until a controller fails, which ends it, and the node with it.
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
                    stage: "storvsc",
                    reason: &reason
                }
            );
            1
        }
    }
}
