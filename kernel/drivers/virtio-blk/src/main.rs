//! The virtio block driver (SDD 25, f.9): serves `moruna.driver.block` on the listener
//! its supervisor gave it, for the one virtio-blk device whose windows it holds.
//!
//! A read or a write pins the client's memory object through the driver's DMA
//! resource and hands its pages to the device as one descriptor chain (a header, one
//! part per page, a status byte), so the data moves between the disk and the client's
//! memory with no copy. The request is answered when the device returns the chain;
//! its pin then goes to the quarantine, which the driver releases since the device is
//! done with it. Requests that find the queue full wait, in order, for room.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    format,
    rc::Rc,
    string::String,
    vec,
    vec::Vec,
};
use core::cell::RefCell;

use block_protocol::{Info, block};
use moruna_mmio::Region;
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Handle,
    Handler, Interrupt, Memory, Outcome, Pending, Pin, ProtocolError, ProtocolId, Signals, Status,
    Watch, Wire, dma_release, println, start,
};
use moruna_virtio::{
    Buffer,
    device::{Device, DmaBuffer, VirtQueue},
    kind,
};

moruna_svc::main!(run);

/// virtio-blk feature bits (virtio 1.2, 5.2.3).
const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;
/// Request types and statuses (virtio 1.2, 5.2.6).
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const S_OK: u8 = 0;
/// A slot of the header page: the 16-byte request header, then the status byte.
const SLOT: usize = 32;
const SLOTS: usize = 4096 / SLOT;
/// The largest transfer, in pages: one descriptor each, plus the header and status.
const MAX_PAGES: u32 = 256;

enum Op {
    Read,
    Write,
    Flush,
}

struct Request {
    op: Op,
    sector: u64,
    pages: Vec<u64>,
    bytes: u64,
    buffer: Option<(Memory, Pin)>,
    pending: Pending,
}

struct Block {
    device: Device,
    queue: VirtQueue,
    headers: DmaBuffer,
    free_slots: Vec<usize>,
    in_flight: BTreeMap<u16, (usize, Request)>,
    waiting: VecDeque<Request>,
    info: Info,
    flush: bool,
    served: u64,
    /// Why the device can no longer be used; the driver ends with it.
    failed: Option<String>,
}

impl Block {
    /// Hands the device as many waiting requests as the queue takes.
    fn submit_waiting(&mut self) -> Result<(), String> {
        let mut notify = false;
        while let Some(request) = self.waiting.front() {
            let parts = request.pages.len() + 2;
            if self.free_slots.is_empty() || self.queue.queue.free() < parts {
                break;
            }
            let Some(request) = self.waiting.pop_front() else {
                break;
            };
            self.submit(request)?;
            notify = true;
        }
        if notify {
            self.device.notify(&self.queue)?;
        }
        Ok(())
    }

    fn submit(&mut self, request: Request) -> Result<(), String> {
        let slot = self.free_slots.pop().ok_or("no header slot")?;
        let at = slot * SLOT;
        let (kind, writes) = match request.op {
            Op::Read => (T_IN, true),
            Op::Write => (T_OUT, false),
            Op::Flush => (T_FLUSH, false),
        };
        let h = &self.headers.map;
        let w = |e: moruna_svc::Error| format!("header: {}", e.name());
        h.write32(at, kind).map_err(w)?;
        h.write32(at + 4, 0).map_err(w)?;
        h.write64(at + 8, request.sector).map_err(w)?;
        h.write8(at + 16, 0xff).map_err(w)?;
        let mut chain = Vec::with_capacity(request.pages.len() + 2);
        chain.push(Buffer {
            addr: self.headers.addr(at),
            len: 16,
            device_writes: false,
        });
        let mut left = request.bytes;
        for page in &request.pages {
            let len = left.min(4096);
            chain.push(Buffer {
                addr: *page,
                len: len as u32,
                device_writes: writes,
            });
            left -= len;
        }
        chain.push(Buffer {
            addr: self.headers.addr(at + 16),
            len: 1,
            device_writes: true,
        });
        let head = self
            .queue
            .queue
            .add(&chain)
            .map_err(|e| format!("queue: {e:?}"))?;
        self.in_flight.insert(head, (slot, request));
        Ok(())
    }

    /// Answers every request the device has returned.
    fn complete(&mut self, done: &mut Completions) -> Result<(), String> {
        let mut finished = 0;
        while let Some(used) = self
            .queue
            .queue
            .pop_used()
            .map_err(|e| format!("used ring: {e:?}"))?
        {
            let (slot, request) = self
                .in_flight
                .remove(&used.head)
                .ok_or("the device returned a chain not in flight")?;
            let status = self
                .headers
                .map
                .read8(slot * SLOT + 16)
                .map_err(|e| format!("status: {}", e.name()))?;
            self.free_slots.push(slot);
            let outcome = if status != S_OK {
                Outcome::Failure(block::Error::Io.code())
            } else {
                match request.buffer {
                    // The pin goes to the quarantine; the memory goes back.
                    Some((memory, pin)) => {
                        drop(pin);
                        Outcome::reply(memory)
                    }
                    None => Outcome::reply(()),
                }
            };
            done.complete(request.pending, outcome);
            self.served += 1;
            finished += 1;
        }
        if finished > 0 {
            // The device has finished with every pin closed so far.
            dma_release(&self.device.dma).map_err(|e| format!("dma_release: {}", e.name()))?;
        }
        self.submit_waiting()
    }

    fn transfer(
        &mut self,
        cx: &mut Context<'_>,
        op: Op,
        sector: u64,
        count: u32,
        buffer: Memory,
    ) -> Outcome {
        let fail = |e: block::Error| Outcome::Failure(e.code());
        if count == 0 || count > self.info.max_sectors {
            return fail(block::Error::BadCount);
        }
        if sector
            .checked_add(count as u64)
            .is_none_or(|end| end > self.info.sectors)
        {
            return fail(block::Error::OutOfRange);
        }
        let read = matches!(op, Op::Read);
        if !read && self.info.read_only {
            return fail(block::Error::ReadOnly);
        }
        let bytes = block_protocol::bytes(&self.info, count);
        let Ok(n) = Pin::pages(&self.device.dma, &buffer.0, read) else {
            return fail(block::Error::BadBuffer);
        };
        if (n as u64) * 4096 < bytes {
            return fail(block::Error::BadBuffer);
        }
        let mut pages = vec![0u64; n];
        let Ok((pin, _)) = Pin::create(&self.device.dma, &buffer.0, read, &mut pages) else {
            return fail(block::Error::BadBuffer);
        };
        pages.truncate(bytes.div_ceil(4096) as usize);
        self.queue_request(Request {
            op,
            sector,
            pages,
            bytes,
            buffer: Some((buffer, pin)),
            pending: cx.defer(),
        })
    }

    /// Queues a request for the device. If the device cannot take it, the driver
    /// records why and ends after this turn, which closes every connection.
    fn queue_request(&mut self, request: Request) -> Outcome {
        self.waiting.push_back(request);
        if let Err(e) = self.submit_waiting() {
            self.failed.get_or_insert(e);
        }
        Outcome::Deferred
    }
}

struct Connection(Rc<RefCell<Block>>);

fn decode<T: Wire>(d: &mut Decoder<'_>) -> Option<T> {
    T::decode(d).ok()
}

impl Handler for Connection {
    fn protocol(&self) -> ProtocolId {
        block::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        let mut b = self.0.borrow_mut();
        match ordinal {
            1 => match d.finish() {
                Ok(()) => Outcome::reply(b.info),
                Err(_) => Outcome::Status(Status::BadMessage),
            },
            2 | 3 => {
                let (Some(sector), Some(count), Some(buffer)) =
                    (decode::<u64>(d), decode::<u32>(d), decode::<Memory>(d))
                else {
                    return Outcome::Status(Status::BadMessage);
                };
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                let op = if ordinal == 2 { Op::Read } else { Op::Write };
                b.transfer(cx, op, sector, count, buffer)
            }
            4 => {
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                if !b.flush {
                    // A device without a write cache has nothing to flush.
                    return Outcome::reply(());
                }
                let pending = cx.defer();
                b.queue_request(Request {
                    op: Op::Flush,
                    sector: 0,
                    pages: Vec::new(),
                    bytes: 0,
                    buffer: None,
                    pending,
                })
            }
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }
}

struct Irq {
    line: Interrupt,
    block: Rc<RefCell<Block>>,
}

impl Watch for Irq {
    fn handle(&self) -> &Handle {
        &self.line.0
    }
    fn signals(&self) -> Signals {
        Signals::SIGNALED
    }
    fn signaled(&mut self, _: Signals, done: &mut Completions) -> bool {
        // Acknowledged before the ring is read, so a completion that lands while it is
        // read raises the line again.
        let mut b = self.block.borrow_mut();
        let result = self
            .line
            .ack()
            .map_err(|e| format!("interrupt_ack: {}", e.name()))
            .and_then(|()| b.complete(done));
        match result {
            Ok(()) => true,
            Err(e) => {
                b.failed.get_or_insert(e);
                false
            }
        }
    }
}

fn open() -> Result<(Block, Interrupt), String> {
    let (device, line) = Device::open(kind::BLOCK, F_SEG_MAX | F_RO | F_BLK_SIZE | F_FLUSH, 0)?;
    let features = device.features;
    // virtio-blk addresses 512-byte sectors whatever logical block size it advertises
    // (virtio 1.2, 5.2.4), so the protocol's sector is 512 bytes.
    let (sectors, seg_max) = device
        .transport
        .device_config(|c| Ok((c.read64(0)?, c.read32(12)?)))
        .map_err(|e| format!("device configuration: {e}"))?;
    let queue = device.queue(0, 256)?;
    let mut pages = MAX_PAGES.min(queue.queue.size() as u32 - 2);
    if features & F_SEG_MAX != 0 && seg_max != 0 {
        pages = pages.min(seg_max);
    }
    let headers = DmaBuffer::new(&device.dma, 4096)?;
    let slots = SLOTS.min(queue.queue.size() as usize);
    device.ready()?;
    Ok((
        Block {
            device,
            queue,
            headers,
            free_slots: (0..slots).rev().collect(),
            in_flight: BTreeMap::new(),
            waiting: VecDeque::new(),
            info: Info {
                sector_size: 512,
                sectors,
                max_sectors: pages * (4096 / 512),
                read_only: features & F_RO != 0,
            },
            flush: features & F_FLUSH != 0,
            served: 0,
            failed: None,
        },
        line,
    ))
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
    let (block, line) = match open() {
        Ok(b) => b,
        Err(e) => {
            println!(out, "KB3 {}: FAIL: {}", name, e);
            return 1;
        }
    };
    let info = block.info;
    let block = Rc::new(RefCell::new(block));
    let mut dispatcher = Dispatcher::new();
    let shared = block.clone();
    let factory: moruna_svc::Factory =
        Box::new(move || Box::new(Connection(shared.clone())) as Box<dyn Handler>);
    let setup = dispatcher.listen(listener, factory).and_then(|()| {
        dispatcher.watch(Box::new(Irq {
            line,
            block: block.clone(),
        }))
    });
    if let Err(e) = setup {
        println!(out, "KB3 {}: FAIL: dispatcher: {}", name, e.name());
        return 1;
    }
    println!(
        out,
        "KB3 {}: virtio-blk up: {} sectors of {} bytes, {} sectors a transfer{}",
        name,
        info.sectors,
        info.sector_size,
        info.max_sectors,
        if info.read_only { ", read only" } else { "" }
    );
    while dispatcher.channels() > 0 {
        if let Some(e) = block.borrow_mut().failed.take() {
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
        "KB3 {}: served {} transfers until every client left",
        name,
        block.borrow().served
    );
    0
}
