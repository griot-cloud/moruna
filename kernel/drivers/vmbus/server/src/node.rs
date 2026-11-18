//! The process on the node: [`Host`] and [`Hv`] over the core's calls, and the loop.
//!
//! Its start handles: `console`; `root.hyperv`, which the supervisor grants only on
//! Hyper-V; `dma`, the DMA grant it pins memory with; and, when a driver is to reach it,
//! `listener`, where drivers' connections arrive.

use alloc::{boxed::Box, collections::BTreeMap, format, rc::Rc, string::String, vec, vec::Vec};
use core::{cell::RefCell, fmt, sync::atomic::Ordering};

use moruna_svc::{
    Channel, Completions, DEADLINE_INFINITE, Dispatcher, Handle, Signals, Watch, now,
};
use moruna_sys::{
    Console, Interrupt, Mapping, Memory, Pin, Rights, dma_release, futex_wake,
    hyperv::{EventFlags, HvSignal, Hyperv, MESSAGE_SIZE},
    map, println, start,
};
use moruna_vmbus::{Hv, HvError, HvMessage, MemoryId, PostError};
use moruna_vmbus_server::{Host, Vmbus, WakeId, factory, message, page_numbers, post_result};
use vmbus_protocol::{PAGES_MAX, vmbus::Error, wake};

/// How long the process waits before it posts again to a host that had no room.
const RETRY_NS: u64 = 1_000_000;

/// A page, as memory objects are made of them.
const PAGE: u64 = 4096;

/// The node, as the process reaches it.
struct Node {
    console: Console,
    root: Hyperv,
    events: EventFlags,
    dma: Handle,
    /// The client's own signals (`Hv::signal_event`), one handle a connection ID.
    signals: BTreeMap<u32, HvSignal>,
    /// Pinned memory: its pin, and its page numbers.
    pinned: BTreeMap<u64, (Pin, Vec<u64>)>,
    /// Wake words: the memory, kept mapped to be written.
    wakes: BTreeMap<u64, (Memory, Mapping<'static>)>,
    next: u64,
}

impl Node {
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Pins `memory` for the host to read and write, with the process's DMA grant.
    fn pin_memory(&mut self, memory: &Handle) -> Result<MemoryId, Error> {
        let pages = Pin::pages(&self.dma, memory, true).map_err(|_| Error::BadMemory)?;
        if pages > PAGES_MAX as usize {
            return Err(Error::BadMemory);
        }
        let mut addresses = vec![0; pages];
        let (pin, _) =
            Pin::create(&self.dma, memory, true, &mut addresses).map_err(|_| Error::BadMemory)?;
        let Some(numbers) = page_numbers(&addresses) else {
            drop(pin);
            let _ = dma_release(&self.dma);
            return Err(Error::BadMemory);
        };
        let id = self.id();
        self.pinned.insert(id, (pin, numbers));
        Ok(MemoryId(id))
    }
}

impl Hv for Node {
    fn post_message(
        &mut self,
        connection_id: u32,
        message_type: u32,
        payload: &[u8],
    ) -> Result<(), PostError> {
        match self.root.post(connection_id, message_type, payload) {
            Ok(status) => post_result(status),
            // The core refuses only what the client never posts: over 240 bytes.
            Err(_) => Err(PostError::Hv(HvError::InvalidParameter)),
        }
    }

    fn signal_event(&mut self, connection_id: u32, _flag: u16) {
        // The core signals event flag 0, the one every channel is signalled on.
        if !self.signals.contains_key(&connection_id)
            && let Ok(signal) = self.root.signal(connection_id)
        {
            self.signals.insert(connection_id, signal);
        }
        if let Some(signal) = self.signals.get(&connection_id) {
            let _ = signal.signal();
        }
    }

    fn take_message(&mut self) -> Option<HvMessage> {
        let mut bytes = [0; MESSAGE_SIZE];
        matches!(self.root.take(&mut bytes), Ok(true)).then(|| message(&bytes))
    }

    fn test_and_clear_event(&mut self, flag: u16) -> bool {
        self.events.test_and_clear(flag)
    }

    fn page_numbers(&mut self, memory: MemoryId) -> Option<Vec<u64>> {
        self.pinned.get(&memory.0).map(|(_, pages)| pages.clone())
    }
}

impl Host for Node {
    fn rings(&mut self, pages: u32) -> Result<(Handle, MemoryId), Error> {
        let memory = Memory::create_dma(u64::from(pages) * PAGE).map_err(|_| Error::NoResources)?;
        let id = self.pin_memory(&memory.0)?;
        Ok((memory.0, id))
    }

    fn pin(&mut self, memory: Handle) -> Result<MemoryId, Error> {
        self.pin_memory(&memory)
    }

    fn release(&mut self, memory: MemoryId) {
        // The pin goes to the DMA grant's quarantine, which only memory the host has let
        // go of ever enters: so all of it may be released.
        if self.pinned.remove(&memory.0).is_some() {
            let _ = dma_release(&self.dma);
        }
    }

    fn signal(&mut self, connection_id: u32) -> Result<Handle, Error> {
        self.root
            .signal(connection_id)
            .map(|signal| signal.0)
            .map_err(|_| Error::NoResources)
    }

    fn wake(&mut self) -> Result<(Handle, WakeId), Error> {
        let memory = Memory::create(wake::SIZE).map_err(|_| Error::NoResources)?;
        let mapping = memory
            .map_detached(0, wake::SIZE, map::READ | map::WRITE)
            .map_err(|_| Error::NoResources)?;
        // The driver reads the word; only this process writes it.
        let driver = memory
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::DUPLICATE | Rights::TRANSFER)
            .map_err(|_| Error::NoResources)?;
        let id = self.id();
        self.wakes.insert(id, (memory, mapping));
        Ok((driver, WakeId(id)))
    }

    fn bump(&mut self, wake: WakeId, gone: bool) {
        let Some((_, mapping)) = self.wakes.get(&wake.0) else {
            return;
        };
        if gone && let Ok(state) = mapping.atomic32(wake::STATE) {
            state.store(wake::GONE, Ordering::Release);
        }
        if let Ok(counter) = mapping.atomic32(wake::COUNTER) {
            counter.fetch_add(1, Ordering::Release);
            let _ = futex_wake(counter, u32::MAX);
        }
    }

    fn drop_wake(&mut self, wake: WakeId) {
        self.wakes.remove(&wake.0);
    }

    fn say(&mut self, line: fmt::Arguments<'_>) {
        println!(self.console, "{}", line);
    }
}

/// SINT 2's interrupt, in the dispatcher's wait.
struct Sint {
    interrupt: Interrupt,
    vmbus: Rc<RefCell<Vmbus<Node>>>,
}

impl Watch for Sint {
    fn handle(&self) -> &Handle {
        &self.interrupt.0
    }

    fn signals(&self) -> Signals {
        Signals::SIGNALED
    }

    fn signaled(&mut self, _seen: Signals, done: &mut Completions) -> bool {
        // Acknowledged before the messages and flags are read, so that one which comes
        // meanwhile raises the interrupt again.
        let _ = self.interrupt.ack();
        let mut vmbus = self.vmbus.borrow_mut();
        vmbus.interrupted();
        while let Some((pending, outcome)) = vmbus.finished() {
            done.complete(pending, outcome);
        }
        true
    }
}

fn take(name: &str) -> Result<Handle, String> {
    start::take(name).ok_or_else(|| format!("no start handle {name}"))
}

fn serve(console: &Console) -> Result<(), String> {
    let root = Hyperv(take("root.hyperv")?);
    let dma = take("dma")?;
    let events = root
        .events()
        .map_err(|e| format!("event flags: {}", e.name()))?;
    // SINT 2's interrupt, and CPU 0's VP index, which the contact and every channel
    // target.
    let sint = root.sint().map_err(|e| format!("SINT 2: {}", e.name()))?;
    let said = console
        .0
        .duplicate(Rights::WRITE)
        .map_err(|e| format!("console: {}", e.name()))?;
    let node = Node {
        console: Console(said),
        root,
        events,
        dma,
        signals: BTreeMap::new(),
        pinned: BTreeMap::new(),
        wakes: BTreeMap::new(),
        next: 0,
    };
    let vmbus = Rc::new(RefCell::new(Vmbus::new(node, sint.vp_index)));
    let mut dispatcher = Dispatcher::new();
    dispatcher
        .watch(Box::new(Sint {
            interrupt: sint.interrupt,
            vmbus: vmbus.clone(),
        }))
        .map_err(|e| format!("watch: {}", e.name()))?;
    if let Some(listener) = start::take("listener") {
        dispatcher
            .listen(Channel(listener), factory(vmbus.clone()))
            .map_err(|e| format!("listener: {}", e.name()))?;
    }
    vmbus.borrow_mut().start();
    loop {
        let unsent = {
            let mut vmbus = vmbus.borrow_mut();
            while let Some((pending, outcome)) = vmbus.finished() {
                let _ = dispatcher.complete(pending, outcome);
            }
            // The library has said why on the console.
            if vmbus.fatal().is_some() {
                return Err(String::new());
            }
            vmbus.has_unsent()
        };
        let deadline = if unsent {
            now().saturating_add(RETRY_NS)
        } else {
            DEADLINE_INFINITE
        };
        dispatcher
            .turn(deadline)
            .map_err(|e| format!("wait: {}", e.name()))?;
        if unsent {
            vmbus.borrow_mut().retry();
        }
    }
}

/// The process: it serves until the host or Hyper-V refuses it, which ends it.
pub fn run() -> i64 {
    let Some(console) = start::take("console").map(Console) else {
        return 1;
    };
    match serve(&console) {
        Ok(()) => 0,
        Err(reason) => {
            if !reason.is_empty() {
                println!(console, "vmbus: {}", reason);
            }
            1
        }
    }
}
