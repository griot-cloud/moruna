//! The virtio console driver (SDD 25, f.10): serves `moruna.driver.console` on the
//! first port of one virtio console (no multiport), on the listener its supervisor
//! gave it.
//!
//! Output: one write at a time is copied into a pinned buffer and handed to the
//! transmit queue as a chain of pages; it is answered when the device returns it, and
//! the next write starts. Input: eight receive buffers stay posted; what arrives is
//! cut into lines, and each `read_line` is answered with the next whole line, in the
//! order the requests came.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{boxed::Box, collections::VecDeque, format, rc::Rc, string::String, vec, vec::Vec};
use core::cell::RefCell;

use console_protocol::{LINE_MAX, console};
use moruna_mmio::Region;
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Handle,
    Handler, Interrupt, Outcome, Pending, ProtocolError, ProtocolId, Signals, Status, Watch, Wire,
    println, start,
};
use moruna_virtio::{
    Buffer,
    device::{Device, DmaBuffer, VirtQueue},
    kind,
};

moruna_svc::main!(run);

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
/// Receive buffers: eight of 512 bytes in one page.
const RX_BUFFERS: usize = 8;
const RX_BYTES: usize = 512;
/// The transmit buffer: the largest write a message carries.
const TX_PAGES: usize = 16;

struct Port {
    device: Device,
    rx: VirtQueue,
    tx: VirtQueue,
    rx_memory: DmaBuffer,
    tx_memory: DmaBuffer,
    /// Which receive buffer each chain in flight is.
    rx_heads: [Option<u16>; RX_BUFFERS],
    input: Vec<u8>,
    lines: VecDeque<Vec<u8>>,
    readers: VecDeque<Pending>,
    writes: VecDeque<(Pending, Vec<u8>)>,
    writing: Option<(Pending, u32)>,
    written: u64,
    failed: Option<String>,
}

impl Port {
    fn post_rx(&mut self, buffer: usize) -> Result<(), String> {
        let head = self
            .rx
            .queue
            .add(&[Buffer {
                addr: self.rx_memory.addr(buffer * RX_BYTES),
                len: RX_BYTES as u32,
                device_writes: true,
            }])
            .map_err(|e| format!("receive queue: {e:?}"))?;
        self.rx_heads[buffer] = Some(head);
        Ok(())
    }

    fn start_write(&mut self) -> Result<(), String> {
        if self.writing.is_some() {
            return Ok(());
        }
        let Some((pending, text)) = self.writes.pop_front() else {
            return Ok(());
        };
        self.tx_memory
            .map
            .write_bytes(0, &text)
            .map_err(|e| format!("transmit buffer: {}", e.name()))?;
        let chain: Vec<Buffer> = (0..text.len().div_ceil(4096))
            .map(|page| Buffer {
                addr: self.tx_memory.addr(page * 4096),
                len: (text.len() - page * 4096).min(4096) as u32,
                device_writes: false,
            })
            .collect();
        self.tx
            .queue
            .add(&chain)
            .map_err(|e| format!("transmit queue: {e:?}"))?;
        self.device.notify(&self.tx)?;
        self.writing = Some((pending, text.len() as u32));
        Ok(())
    }

    fn take_input(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                let mut line = core::mem::take(&mut self.input);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.lines.push_back(line);
            } else {
                self.input.push(b);
                if self.input.len() == LINE_MAX {
                    self.lines.push_back(core::mem::take(&mut self.input));
                }
            }
        }
    }

    fn answer_readers(&mut self, done: &mut Completions) {
        while !self.readers.is_empty() && !self.lines.is_empty() {
            if let (Some(reader), Some(line)) = (self.readers.pop_front(), self.lines.pop_front()) {
                done.complete(reader, Outcome::reply(line));
            }
        }
    }

    fn service(&mut self, done: &mut Completions) -> Result<(), String> {
        let mut reposted = false;
        while let Some(used) = self
            .rx
            .queue
            .pop_used()
            .map_err(|e| format!("receive ring: {e:?}"))?
        {
            let buffer = self
                .rx_heads
                .iter()
                .position(|h| *h == Some(used.head))
                .ok_or("the device returned a receive buffer not posted")?;
            let len = (used.len as usize).min(RX_BYTES);
            let mut bytes = vec![0u8; len];
            self.rx_memory
                .map
                .read_bytes(buffer * RX_BYTES, &mut bytes)
                .map_err(|e| format!("receive buffer: {}", e.name()))?;
            self.take_input(&bytes);
            self.post_rx(buffer)?;
            reposted = true;
        }
        if reposted {
            self.device.notify(&self.rx)?;
        }
        self.answer_readers(done);
        while self
            .tx
            .queue
            .pop_used()
            .map_err(|e| format!("transmit ring: {e:?}"))?
            .is_some()
        {
            let (pending, len) = self
                .writing
                .take()
                .ok_or("the device returned a write not in flight")?;
            self.written += len as u64;
            done.complete(pending, Outcome::reply(len));
        }
        self.start_write()
    }
}

struct Connection(Rc<RefCell<Port>>);

impl Handler for Connection {
    fn protocol(&self) -> ProtocolId {
        console::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        let mut port = self.0.borrow_mut();
        match ordinal {
            1 => {
                let Ok(text) = Vec::<u8>::decode(d) else {
                    return Outcome::Status(Status::BadMessage);
                };
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                if text.is_empty() {
                    return Outcome::Failure(console::Error::Empty.code());
                }
                let pending = cx.defer();
                port.writes.push_back((pending, text));
                if let Err(e) = port.start_write() {
                    port.failed.get_or_insert(e);
                }
                Outcome::Deferred
            }
            2 => {
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                match port.lines.pop_front() {
                    Some(line) if port.readers.is_empty() => Outcome::reply(line),
                    other => {
                        if let Some(line) = other {
                            port.lines.push_front(line);
                        }
                        let pending = cx.defer();
                        port.readers.push_back(pending);
                        Outcome::Deferred
                    }
                }
            }
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }
}

struct Irq {
    line: Interrupt,
    port: Rc<RefCell<Port>>,
}

impl Watch for Irq {
    fn handle(&self) -> &Handle {
        &self.line.0
    }
    fn signals(&self) -> Signals {
        Signals::SIGNALED
    }
    fn signaled(&mut self, _: Signals, done: &mut Completions) -> bool {
        let mut port = self.port.borrow_mut();
        let result = self
            .line
            .ack()
            .map_err(|e| format!("interrupt_ack: {}", e.name()))
            .and_then(|()| port.service(done));
        match result {
            Ok(()) => true,
            Err(e) => {
                port.failed.get_or_insert(e);
                false
            }
        }
    }
}

fn open() -> Result<(Port, Interrupt), String> {
    let (device, line) = Device::open(kind::CONSOLE, 0, 0)?;
    let rx = device.queue(RX_QUEUE, 64)?;
    let tx = device.queue(TX_QUEUE, 64)?;
    let rx_memory = DmaBuffer::new(&device.dma, (RX_BUFFERS * RX_BYTES) as u64)?;
    let tx_memory = DmaBuffer::new(&device.dma, (TX_PAGES * 4096) as u64)?;
    device.ready()?;
    let mut port = Port {
        device,
        rx,
        tx,
        rx_memory,
        tx_memory,
        rx_heads: [None; RX_BUFFERS],
        input: Vec::new(),
        lines: VecDeque::new(),
        readers: VecDeque::new(),
        writes: VecDeque::new(),
        writing: None,
        written: 0,
        failed: None,
    };
    for buffer in 0..RX_BUFFERS {
        port.post_rx(buffer)?;
    }
    port.device.notify(&port.rx)?;
    Ok((port, line))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let name = start::process_name();
    let Some(listener) = start::take("listen").map(Channel) else {
        println!(out, "KB3 {}: FAIL: no listener", name);
        return 1;
    };
    let (port, line) = match open() {
        Ok(p) => p,
        Err(e) => {
            println!(out, "KB3 {}: FAIL: {}", name, e);
            return 1;
        }
    };
    let port = Rc::new(RefCell::new(port));
    let mut dispatcher = Dispatcher::new();
    let shared = port.clone();
    let factory: moruna_svc::Factory =
        Box::new(move || Box::new(Connection(shared.clone())) as Box<dyn Handler>);
    let setup = dispatcher.listen(listener, factory).and_then(|()| {
        dispatcher.watch(Box::new(Irq {
            line,
            port: port.clone(),
        }))
    });
    if let Err(e) = setup {
        println!(out, "KB3 {}: FAIL: dispatcher: {}", name, e.name());
        return 1;
    }
    println!(out, "KB3 {}: virtio-console up", name);
    while dispatcher.channels() > 0 {
        if let Some(e) = port.borrow_mut().failed.take() {
            println!(out, "KB3 {}: FAIL: the device: {}", name, e);
            return 1;
        }
        if let Err(e) = dispatcher.turn(DEADLINE_INFINITE) {
            println!(out, "KB3 {}: FAIL: wait: {}", name, e.name());
            return 1;
        }
    }
    println!(
        out,
        "KB3 {}: wrote {} bytes until every client left",
        name,
        port.borrow().written
    );
    0
}
