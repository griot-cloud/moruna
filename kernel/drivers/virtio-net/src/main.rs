//! The virtio network driver (SDD 29, f.3): serves `moruna.driver.net` on the
//! listener its supervisor gave it, for the one virtio-net card whose windows it
//! holds.
//!
//! It negotiates the card's address (required), its link status, its MTU and, when
//! the card has a control queue and several queue pairs, as many pairs as there are
//! CPUs (at most eight), which it sets through the control queue; no offload is
//! negotiated, so every frame goes out and comes in as the client and the wire made
//! it. Each pair is a receive queue of 128 posted one-page buffers and a transmit
//! queue of 128 one-page buffers in the driver's own pinned memory.
//!
//! The protocol is served by `net_protocol::Server`, as every network driver serves
//! it; the card is its `Ring`: a frame is copied with its header into a free transmit
//! page of the queue its flow hashes to, and sends wait, in order, for pages. A buffer
//! the card returns on a receive queue is checked (its length, its header) before its
//! frame is handed to the hub, which queues it for every subscribed connection, and
//! the buffer is posted again. A link change (the configuration interrupt) answers the
//! connections waiting for one. When every client has gone the driver resets the
//! card and ends.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{boxed::Box, collections::BTreeMap, format, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use moruna_pci::dma::DmaBuffer;
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Handle,
    Handler, Interrupt, Outcome, ProtocolId, Signals, Watch, now, println, start,
};
use moruna_virtio::{
    Buffer,
    device::{Device, VECTOR, VirtQueue},
    kind,
    net::{self, NetConfig, feature},
};
use net_protocol::{Info, Ring, Server, flow_hash, net as proto};

moruna_svc::main!(run);

/// Buffers a queue has, one page each.
const BUFFERS: usize = 128;
const PAGE: usize = 4096;
/// The largest frame one page holds after the header.
const PAGE_FRAME: usize = PAGE - net::HEADER;
/// The most queue pairs the driver uses.
const PAIRS_MAX: u16 = 8;
/// How long the card has to carry out a control command.
const CTRL_NS: u64 = 5_000_000_000;

struct Rx {
    queue: VirtQueue,
    memory: DmaBuffer,
    /// The buffer each chain in flight is.
    heads: BTreeMap<u16, usize>,
}

struct Tx {
    queue: VirtQueue,
    memory: DmaBuffer,
    free: Vec<usize>,
    heads: BTreeMap<u16, usize>,
    /// Chains added since the last notify.
    dirty: bool,
}

/// The card: its device and its queue pairs.
struct Card {
    device: Device,
    rx: Vec<Rx>,
    tx: Vec<Tx>,
    status: bool,
    max_frame: usize,
}

impl Card {
    fn post_rx(&mut self, pair: usize, buffer: usize) -> Result<(), String> {
        let rx = &mut self.rx[pair];
        let head = rx
            .queue
            .queue
            .add(&[Buffer {
                addr: rx.memory.addr(buffer * PAGE),
                len: PAGE as u32,
                device_writes: true,
            }])
            .map_err(|e| format!("receive queue {pair}: {e:?}"))?;
        rx.heads.insert(head, buffer);
        Ok(())
    }
}

impl Ring for Card {
    /// Copies the frame, with its header, into a free transmit page of the queue its
    /// flow hashes to.
    fn put(&mut self, frame: &[u8]) -> Result<bool, String> {
        let pair = (flow_hash(frame) as usize) % self.tx.len();
        let tx = &mut self.tx[pair];
        let Some(buffer) = tx.free.pop() else {
            return Ok(false);
        };
        let at = buffer * PAGE;
        net::write_header(&tx.memory.map, at)
            .and_then(|()| tx.memory.map.write(at + net::HEADER, frame))
            .map_err(|e| format!("transmit page: {}", e.name()))?;
        let head = tx
            .queue
            .queue
            .add(&[Buffer {
                addr: tx.memory.addr(at),
                len: (net::HEADER + frame.len()) as u32,
                device_writes: false,
            }])
            .map_err(|e| format!("transmit queue {pair}: {e:?}"))?;
        tx.heads.insert(head, buffer);
        tx.dirty = true;
        Ok(true)
    }

    fn flush(&mut self) -> Result<(), String> {
        for tx in self.tx.iter_mut().filter(|t| t.dirty) {
            tx.dirty = false;
            self.device.notify(&tx.queue)?;
        }
        Ok(())
    }
}

struct Net {
    card: Card,
    server: Server,
}

impl Net {
    /// Takes everything the card has returned: the link, frames received, transmit
    /// pages done; then puts waiting frames on the pages freed.
    fn service(&mut self) -> Result<(), String> {
        let Net { card, server } = self;
        if card.status {
            let up = card
                .device
                .transport
                .device_config(|c| net::link_up(c))
                .map_err(|e| format!("link status: {e}"))?;
            server.link(up, 0);
        }
        for pair in 0..card.rx.len() {
            let mut reposted = false;
            loop {
                let used = card.rx[pair]
                    .queue
                    .queue
                    .pop_used()
                    .map_err(|e| format!("receive queue {pair}: {e:?}"))?;
                let Some(used) = used else { break };
                let rx = &mut card.rx[pair];
                let buffer = rx
                    .heads
                    .remove(&used.head)
                    .ok_or("the card returned a receive chain not in flight")?;
                let at = buffer * PAGE;
                match net::received(&rx.memory.map, at, PAGE, used.len, card.max_frame) {
                    Ok(range) => {
                        let mut frame = alloc::vec![0u8; range.len()];
                        rx.memory
                            .map
                            .read(range.start, &mut frame)
                            .map_err(|e| format!("receive page: {}", e.name()))?;
                        server.hub.deliver(&frame);
                    }
                    // A malformed buffer is the card's error: counted, and dropped.
                    Err(_) => server.hub.rx_errors += 1,
                }
                card.post_rx(pair, buffer)?;
                reposted = true;
            }
            if reposted {
                card.device.notify(&card.rx[pair].queue)?;
            }
        }
        server.delivered();
        for pair in 0..card.tx.len() {
            loop {
                let used = card.tx[pair]
                    .queue
                    .queue
                    .pop_used()
                    .map_err(|e| format!("transmit queue {pair}: {e:?}"))?;
                let Some(used) = used else { break };
                let tx = &mut card.tx[pair];
                let buffer = tx
                    .heads
                    .remove(&used.head)
                    .ok_or("the card returned a transmit chain not in flight")?;
                tx.free.push(buffer);
            }
        }
        server.transmit(card)
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
        let Net { card, server } = &mut *n;
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
        // Acknowledged before the rings are read, so a buffer returned while they are
        // read raises the line again.
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

/// Sets the card's queue pairs through its control queue, and waits for its ack.
fn set_pairs(device: &Device, ctrl: &mut VirtQueue, pairs: u16) -> Result<(), String> {
    let page = DmaBuffer::new(&device.dma, PAGE as u64)?;
    net::write_pairs_command(&page.map, 0, pairs).map_err(|e| e.name())?;
    page.map.write8(64, 0xff).map_err(|e| e.name())?;
    let head = ctrl
        .queue
        .add(&[
            Buffer {
                addr: page.addr(0),
                len: 4,
                device_writes: false,
            },
            Buffer {
                addr: page.addr(64),
                len: 1,
                device_writes: true,
            },
        ])
        .map_err(|e| format!("control queue: {e:?}"))?;
    device.notify(ctrl)?;
    let deadline = now().saturating_add(CTRL_NS);
    loop {
        match ctrl.queue.pop_used() {
            Ok(Some(used)) if used.head == head => break,
            Ok(Some(_)) => return Err(String::from("the control queue returned another chain")),
            Ok(None) if now() < deadline => core::hint::spin_loop(),
            Ok(None) => return Err(String::from("the card did not answer its control queue")),
            Err(e) => return Err(format!("control queue: {e:?}")),
        }
    }
    match page.map.read8(64).map_err(|e| e.name())? {
        net::ctrl::OK => Ok(()),
        ack => Err(format!("the card refused {pairs} queue pairs (ack {ack})")),
    }
}

fn open() -> Result<(Net, Interrupt), String> {
    let wanted = feature::MAC | feature::STATUS | feature::MTU | feature::CTRL_VQ | feature::MQ;
    let (device, line) = Device::open(kind::NET, wanted, feature::MAC)?;
    let features = device.features;
    let raw = device
        .transport
        .device_config(|c| net::read_raw(c, features))
        .map_err(|e| format!("device configuration: {e}"))?;
    let config: NetConfig =
        net::check_config(raw).map_err(|e| format!("device configuration: {e:?}"))?;
    let mq = features & feature::MQ != 0 && features & feature::CTRL_VQ != 0;
    let pairs = if mq {
        config
            .max_pairs
            .min(start::cpus().max(1) as u16)
            .min(PAIRS_MAX)
    } else {
        1
    };
    let status = features & feature::STATUS != 0;
    if status {
        device
            .transport
            .set_config_vector(VECTOR)
            .map_err(|e| format!("configuration vector: {e}"))?;
    }
    // Every queue is set up before the driver says it is ready (virtio 1.2, 3.1.1).
    let mut ctrl = if pairs > 1 {
        Some(device.queue(net::ctrl_queue(config.max_pairs, true), 64)?)
    } else {
        None
    };
    let mut rx = Vec::new();
    let mut tx = Vec::new();
    for pair in 0..pairs {
        rx.push(Rx {
            queue: device.queue(net::rx_queue(pair), BUFFERS as u16)?,
            memory: DmaBuffer::new(&device.dma, (BUFFERS * PAGE) as u64)?,
            heads: BTreeMap::new(),
        });
        let queue = device.queue(net::tx_queue(pair), BUFFERS as u16)?;
        let n = (queue.queue.size() as usize).min(BUFFERS);
        tx.push(Tx {
            queue,
            memory: DmaBuffer::new(&device.dma, (BUFFERS * PAGE) as u64)?,
            free: (0..n).rev().collect(),
            heads: BTreeMap::new(),
            dirty: false,
        });
    }
    let mtu = (config.mtu as usize).min(PAGE_FRAME - net_protocol::HEADER) as u32;
    let info = Info {
        mac: config.mac,
        mtu,
        max_frame: mtu + net_protocol::HEADER as u32,
        offloads: 0,
        queues: pairs as u32,
    };
    let mut card = Card {
        device,
        rx,
        tx,
        status,
        max_frame: info.max_frame as usize,
    };
    for pair in 0..card.rx.len() {
        let n = (card.rx[pair].queue.queue.size() as usize).min(BUFFERS);
        for buffer in 0..n {
            card.post_rx(pair, buffer)?;
        }
    }
    card.device.ready()?;
    if let Some(ctrl) = ctrl.as_mut() {
        set_pairs(&card.device, ctrl, pairs)?;
    }
    for pair in 0..card.rx.len() {
        card.device.notify(&card.rx[pair].queue)?;
    }
    let card = Net {
        card,
        server: Server::new(info, config.link_up, 0),
    };
    Ok((card, line))
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
    let (card, line) = match open() {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KE1 {}: FAIL: {}", name, e);
            return 1;
        }
    };
    let m = net_protocol::octets(card.server.info.mac);
    println!(
        out,
        "KE1 {}: virtio-net up: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, MTU {}, {} queue pairs for {} CPUs, link {}, no offloads",
        name,
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5],
        card.server.info.mtu,
        card.server.info.queues,
        start::cpus(),
        if card.server.hub.link().up {
            "up"
        } else {
            "down"
        }
    );
    let card = Rc::new(RefCell::new(card));
    let mut dispatcher = Dispatcher::new();
    let shared = card.clone();
    let factory: moruna_svc::Factory = Box::new(move || {
        Box::new(Connection {
            net: shared.clone(),
            id: None,
        }) as Box<dyn Handler>
    });
    let setup = dispatcher.listen(listener, factory).and_then(|()| {
        dispatcher.watch(Box::new(Irq {
            line,
            net: card.clone(),
        }))
    });
    if let Err(e) = setup {
        println!(out, "KE1 {}: FAIL: dispatcher: {}", name, e.name());
        return 1;
    }
    loop {
        {
            let mut n = card.borrow_mut();
            if let Some(e) = n.server.failed.take() {
                println!(out, "KE1 {}: FAIL: the card: {}", name, e);
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
    let n = card.borrow();
    // The card is reset, so it uses none of the driver's pages once it has ended.
    if let Err(e) = n.card.device.transport.reset() {
        println!(out, "KE1 {}: FAIL: resetting the card: {}", name, e);
        return 1;
    }
    let s = n.server.hub.stats(u64::MAX);
    println!(
        out,
        "KE1 {}: served until every client left: {} frames out ({} bytes), {} in ({} bytes), {} malformed from the card",
        name,
        s.tx_frames,
        s.tx_bytes,
        s.rx_frames,
        s.rx_bytes,
        s.rx_errors
    );
    0
}
