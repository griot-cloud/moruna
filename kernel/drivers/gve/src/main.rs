//! The gVNIC driver (SDD 29, f.4): serves `moruna.driver.net` on the listener its
//! supervisor gave it, for the one gVNIC whose windows it holds, in the GQI queue
//! format, with one queue pair per CPU (at most what the device has and its
//! interrupts allow). **Not yet proven on hardware**: there is no emulator of the
//! device; `moruna-gve` is tested against a model of it, and KE3 boots this on Google
//! Cloud.
//!
//! The device's BAR 0 holds its registers, BAR 1 its MSI-X table, BAR 2 its doorbells.
//! Every MSI-X entry the driver uses (each notification block's and the management
//! interrupt after them) carries the one message of the driver's first MSI slot, so
//! one interrupt wakes the driver for all of them; it then takes every queue's work,
//! reads the device status (link, and a reset the device asks for), and acknowledges
//! and unmasks every block. The device is reset before it may master the bus and
//! before the quarantine is released, so it uses nothing a previous driver gave it.
//!
//! The protocol is served by `net_protocol::Server`; the device is its `Ring`: a
//! frame is copied into a slot of the transmit queue its flow hashes to. When every
//! client has gone the driver destroys its queues, unregisters its page lists and
//! resets the device. A device that asks for a reset ends the driver, which the device
//! manager starts again from a reset.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{boxed::Box, format, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use moruna_gve::{Dma, Gve, GveError, Settings, regs};
use moruna_mmio::TimerClock;
use moruna_pci::{
    command,
    dma::DmaBuffer,
    function::{BarWindow, Function},
    reg,
};
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Handle,
    Handler, Interrupt, Outcome, ProtocolId, Signals, Watch, dma_release, println, start,
};
use net_protocol::{FRAME_MAX, HEADER, Info, Ring, Server, flow_hash, net as proto};

moruna_svc::main!(run);

/// How long the device has for an admin command or a reset.
const TIMEOUT_NS: u64 = 10_000_000_000;
/// The most MSI-X entries the driver programs: eight queue pairs' blocks and the
/// management interrupt.
const VECTORS_MAX: u16 = 17;
/// Rounds of work one interrupt takes before the driver waits again.
const ROUNDS: usize = 16;

struct Pages<'a>(&'a Handle);

impl Dma for Pages<'_> {
    type Region = DmaBuffer;
    fn alloc(&mut self, bytes: usize, contiguous: bool) -> Result<(DmaBuffer, Vec<u64>), GveError> {
        let buffer = if contiguous {
            DmaBuffer::contiguous(self.0, bytes as u64)
        } else {
            DmaBuffer::new(self.0, bytes as u64)
        }
        .map_err(GveError::Unsupported)?;
        let pages = buffer.pages().to_vec();
        Ok((buffer, pages))
    }
}

/// The device as the server's ring.
struct Card {
    gve: Gve<BarWindow, BarWindow, DmaBuffer>,
}

impl Ring for Card {
    fn put(&mut self, frame: &[u8]) -> Result<bool, String> {
        let q = (flow_hash(frame) as usize) % self.gve.queues();
        if !self.gve.has_room(q) {
            return Ok(false);
        }
        self.gve.send(q, frame).map_err(|e| format!("{e}"))?;
        Ok(true)
    }

    fn flush(&mut self) -> Result<(), String> {
        self.gve.flush().map_err(|e| format!("{e}"))
    }
}

struct Net {
    card: Card,
    server: Server,
    /// The function's claims and its DMA right, held while the driver runs.
    _function: Function,
    _dma: Handle,
}

impl Net {
    /// Takes every queue's work and the device's status, then acknowledges the
    /// blocks; again while the device has more, at most `ROUNDS` times.
    fn service(&mut self) -> Result<(), String> {
        let Net { card, server, .. } = self;
        let e = |e: GveError| format!("{e}");
        for _ in 0..ROUNDS {
            let status = card.gve.status().map_err(e)?;
            if status & regs::device_status::RESET != 0 {
                return Err(String::from("the device asks to be reset"));
            }
            server.link(status & regs::device_status::LINK != 0, 0);
            let hub = &mut server.hub;
            let dropped = card
                .gve
                .receive(&mut |frame| hub.deliver(frame))
                .map_err(e)?;
            hub.rx_errors += dropped;
            server.delivered();
            card.gve.complete().map_err(e)?;
            server.transmit(card)?;
            card.gve.ack().map_err(e)?;
            if !card.gve.pending().map_err(e)? {
                return Ok(());
            }
        }
        Ok(())
    }
}

struct Connection {
    net: Rc<RefCell<Net>>,
    id: Option<u64>,
}

impl Handler for Connection {
    fn protocol(&self) -> ProtocolId {
        proto::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        self.id = Some(cx.connection());
        let net = self.net.clone();
        let mut n = net.borrow_mut();
        let Net { card, server, .. } = &mut *n;
        server.handle(card, cx, ordinal, d)
    }

    fn closed(&mut self) {
        if let (Some(id), Ok(mut n)) = (self.id, self.net.try_borrow_mut()) {
            n.server.closed(id);
        }
    }
}

struct Irq {
    line: Interrupt,
    net: Rc<RefCell<Net>>,
}

impl Watch for Irq {
    fn handle(&self) -> &Handle {
        &self.line.0
    }
    fn signals(&self) -> Signals {
        Signals::SIGNALED
    }
    fn signaled(&mut self, _: Signals, _: &mut Completions) -> bool {
        let mut n = self.net.borrow_mut();
        let result = self
            .line
            .ack()
            .map_err(|e| format!("interrupt_ack: {}", e.name()))
            .and_then(|()| n.service());
        match result {
            Ok(()) => true,
            Err(e) => {
                n.server.failed.get_or_insert(e);
                false
            }
        }
    }
}

fn open() -> Result<(Net, Interrupt, TimerClock), String> {
    let mut function = Function::open()?;
    let (vendor, device) = (
        function.config.vendor().map_err(|e| e.name())?,
        function.config.device().map_err(|e| e.name())?,
    );
    if (vendor, device) != (moruna_gve::VENDOR, moruna_gve::DEVICE) {
        return Err(format!(
            "the function is {vendor:04x}:{device:04x}, not a gVNIC"
        ));
    }
    let revision = function
        .config
        .region()
        .read8(reg::REVISION)
        .map_err(|e| e.name())?;
    function
        .config
        .enable(command::MEMORY | command::INTX_DISABLE)
        .map_err(|e| format!("command register: {}", e.name()))?;
    let msix = function
        .config
        .msix()
        .map_err(|e| format!("MSI-X capability: {}", e.name()))?
        .ok_or("the device has no MSI-X")?;
    let vectors = msix.entries.min(VECTORS_MAX);
    let (line, message) = function.interrupt(0)?;
    {
        let table = function
            .bar(msix.table_bar)
            .ok_or_else(|| format!("no window for the MSI-X table's bar{}", msix.table_bar))?;
        for entry in 0..vectors {
            msix.program(table, entry, message.address, message.data)
                .map_err(|e| format!("MSI-X table entry {entry}: {}", e.name()))?;
        }
        msix.enable(&function.config)
            .map_err(|e| format!("MSI-X enable: {}", e.name()))?;
    }
    let dma = function.take_dma()?;
    let bar0 = function
        .take_bar(moruna_gve::BAR_REGISTERS)
        .ok_or("no window for BAR 0")?;
    let bar2 = function
        .take_bar(moruna_gve::BAR_DOORBELLS)
        .ok_or("no window for BAR 2")?;
    let clock = TimerClock::new().map_err(|e| format!("timer_create: {}", e.name()))?;
    let settings = Settings {
        queues: start::cpus().max(1) as u16,
        vectors,
        revision,
        mtu_max: (FRAME_MAX - HEADER) as u16,
        timeout_ns: TIMEOUT_NS,
    };
    let gve = Gve::start(bar0, bar2, &mut Pages(&dma), &clock, settings, &mut || {
        dma_release(&dma)
            .map_err(|e| GveError::Unsupported(format!("dma_release: {}", e.name())))?;
        function.enable_bus_master().map_err(GveError::Unsupported)
    })
    .map_err(|e| format!("{e}"))?;
    let link = gve.link_up().map_err(|e| format!("{e}"))?;
    let info = Info {
        mac: gve.description.mac,
        mtu: gve.mtu as u32,
        max_frame: gve.max_frame() as u32,
        offloads: 0,
        queues: gve.queues() as u32,
    };
    let net = Net {
        card: Card { gve },
        server: Server::new(info, link, 0),
        _function: function,
        _dma: dma,
    };
    Ok((net, line, clock))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let name = start::process_name();
    let Some(listener) = start::take("listen").map(Channel) else {
        println!(out, "KE1 {}: FAIL: no listener", name);
        return 1;
    };
    let (net, line, clock) = match open() {
        Ok(n) => n,
        Err(e) => {
            println!(out, "KE1 {}: FAIL: {}", name, e);
            return 1;
        }
    };
    {
        let g = &net.card.gve;
        let m = net_protocol::octets(net.server.info.mac);
        println!(
            out,
            "KE1 {}: gve up: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, MTU {}, {:?} format (device options {}), {} queue pairs for {} CPUs, rings of {} and {}, link {}, no offloads",
            name,
            m[0],
            m[1],
            m[2],
            m[3],
            m[4],
            m[5],
            g.mtu,
            g.description.format,
            if g.description.options.is_empty() {
                "none"
            } else {
                &g.description.options
            },
            g.queues(),
            start::cpus(),
            g.description.tx_entries,
            g.description.rx_entries,
            if net.server.hub.link().up {
                "up"
            } else {
                "down"
            }
        );
    }
    let net = Rc::new(RefCell::new(net));
    let mut dispatcher = Dispatcher::new();
    let shared = net.clone();
    let factory: moruna_svc::Factory = Box::new(move || {
        Box::new(Connection {
            net: shared.clone(),
            id: None,
        }) as Box<dyn Handler>
    });
    let setup = dispatcher.listen(listener, factory).and_then(|()| {
        dispatcher.watch(Box::new(Irq {
            line,
            net: net.clone(),
        }))
    });
    if let Err(e) = setup {
        println!(out, "KE1 {}: FAIL: dispatcher: {}", name, e.name());
        return 1;
    }
    loop {
        {
            let mut n = net.borrow_mut();
            if let Some(e) = n.server.failed.take() {
                println!(out, "KE1 {}: FAIL: the device: {}", name, e);
                return 1;
            }
            for (pending, outcome) in core::mem::take(&mut n.server.ready) {
                // An answer whose client has gone is dropped.
                let _ = dispatcher.complete(pending, outcome);
            }
        }
        if dispatcher.channels() == 0 {
            break;
        }
        if let Err(e) = dispatcher.turn(DEADLINE_INFINITE) {
            println!(out, "KE1 {}: FAIL: wait: {}", name, e.name());
            return 1;
        }
    }
    let mut n = net.borrow_mut();
    if let Err(e) = n.card.gve.shutdown(&clock) {
        println!(out, "KE1 {}: FAIL: releasing the device: {}", name, e);
        return 1;
    }
    let s = n.server.hub.stats(u64::MAX);
    println!(
        out,
        "KE1 {}: served until every client left: {} frames out, {} in, {} malformed or dropped by the device's flags; the device is reset",
        name,
        s.tx_frames,
        s.rx_frames,
        s.rx_errors
    );
    0
}
