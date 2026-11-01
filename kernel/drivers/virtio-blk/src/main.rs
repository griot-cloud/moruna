//! The virtio block driver (SDD 25, f.9): serves `moruna.driver.block` on the listener
//! its supervisor gave it, for the one virtio-blk device whose windows it holds.
//!
//! A read or a write pins the client's memory object through the driver's DMA
//! resource and hands its pages to the device as one descriptor chain (a header, one
//! part per page, a status byte), so the data moves between the disk and the client's
//! memory with no copy. The request is answered when the device returns the chain;
//! its pin then goes to the quarantine, which the driver releases since the device is
//! done with it. Requests that find the queue full wait, in order, for room.
//!
//! A registered buffer (protocol 1.1) is pinned once, when the client registers it,
//! and its pages' device addresses kept with the connection; `read_at` and `write_at`
//! hand the device some of those pages and pin nothing. Each transfer holds the
//! registration while it is in flight, so a registration ended (or a connection
//! closed) under a transfer keeps its pin until the device returns the chain, and
//! only then does the pin go to the quarantine.

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

use block_protocol::{Info, block, lease};
use moruna_mmio::Region;
use moruna_pci::dma::DmaBuffer;
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Handle,
    Handler, Interrupt, Memory, Outcome, Pending, Pin, ProtocolError, ProtocolId, Signals, Status,
    Watch, Wire, dma_release, println, start,
};
use moruna_virtio::{
    Buffer,
    device::{Device, VirtQueue},
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

/// A client's buffer, pinned once for every transfer into or out of it.
struct Registration {
    _pin: Pin,
    pages: Vec<u64>,
}

/// What a transfer keeps alive until the device returns it.
enum Hold {
    None,
    /// A buffer pinned for this transfer, handed back in the reply.
    Buffer(Memory, Pin),
    /// A registration, which keeps its pin.
    Registered(Rc<Registration>),
}

enum LeaseTag {
    Original(lease::Io),
    Release(lease::Release),
}
struct Request {
    op: Op,
    sector: u64,
    pages: Vec<u64>,
    bytes: u64,
    hold: Hold,
    pending: Option<Pending>,
    lease: Option<LeaseTag>,
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
    writer: lease::Lease,
    release_pending: Option<Pending>,
    closed: Rc<RefCell<Vec<u64>>>,
    ready: Vec<(Pending, Outcome)>,
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
            let (slot, mut request) = self
                .in_flight
                .remove(&used.head)
                .ok_or("the device returned a chain not in flight")?;
            let status = self
                .headers
                .map
                .read8(slot * SLOT + 16)
                .map_err(|e| format!("status: {}", e.name()))?;
            self.free_slots.push(slot);
            self.finish_lease(request.lease.take(), status == S_OK);
            let outcome = if status != S_OK {
                Outcome::Failure(block::Error::Io.code())
            } else {
                match request.hold {
                    // The pin goes to the quarantine; the memory goes back.
                    Hold::Buffer(memory, pin) => {
                        drop(pin);
                        Outcome::reply(memory)
                    }
                    // The registration's pin goes to the quarantine only if it was
                    // ended while this transfer was in flight.
                    Hold::Registered(registration) => {
                        drop(registration);
                        Outcome::reply(())
                    }
                    Hold::None => Outcome::reply(()),
                }
            };
            if let Some(pending) = request.pending {
                done.complete(pending, outcome);
            }
            self.served += 1;
            finished += 1;
        }
        if finished > 0 {
            // The device has finished with every pin closed so far.
            dma_release(&self.device.dma).map_err(|e| format!("dma_release: {}", e.name()))?;
        }
        self.advance_lease();
        self.submit_waiting()
    }

    /// Refuses a transfer the device cannot make: a count of zero or above the
    /// largest, sectors past the end, a write to a read-only device.
    fn check(&self, op: &Op, sector: u64, count: u32) -> Result<(), block::Error> {
        if count == 0 || count > self.info.max_sectors {
            return Err(block::Error::BadCount);
        }
        if sector
            .checked_add(count as u64)
            .is_none_or(|end| end > self.info.sectors)
        {
            return Err(block::Error::OutOfRange);
        }
        if matches!(op, Op::Write) && self.info.read_only {
            return Err(block::Error::ReadOnly);
        }
        Ok(())
    }

    fn transfer(
        &mut self,
        cx: &mut Context<'_>,
        op: Op,
        token: Option<u64>,
        sector: u64,
        count: u32,
        buffer: Memory,
    ) -> Outcome {
        let fail = |e: block::Error| Outcome::Failure(e.code());
        if matches!(op, Op::Write) {
            if let Err(error) = self.writer.authorize(cx.connection(), token.unwrap_or(0)) {
                return fail(error);
            }
        }
        if let Err(e) = self.check(&op, sector, count) {
            return fail(e);
        }
        let read = matches!(op, Op::Read);
        if matches!(op, Op::Read) {
            if let Err(error) = self.writer.can_read(cx.connection()) {
                return Outcome::Failure(error.code());
            }
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
        let lease = if matches!(op, Op::Write) {
            match self.writer.write(cx.connection(), token.unwrap_or(0)) {
                Ok(io) => Some(LeaseTag::Original(io)),
                Err(error) => return fail(error),
            }
        } else {
            match self.writer.read(cx.connection()) {
                Ok(io) => io.map(LeaseTag::Original),
                Err(error) => return Outcome::Failure(error.code()),
            }
        };
        self.queue_request(Request {
            op,
            sector,
            pages,
            bytes,
            hold: Hold::Buffer(buffer, pin),
            pending: Some(cx.defer()),
            lease,
        })
    }

    /// Pins `buffer` for the connection's registrations (protocol 1.1).
    fn register(&mut self, buffer: &Memory) -> Result<Registration, block::Error> {
        let n =
            Pin::pages(&self.device.dma, &buffer.0, true).map_err(|_| block::Error::BadBuffer)?;
        let mut pages = vec![0u64; n];
        let (pin, _) = Pin::create(&self.device.dma, &buffer.0, true, &mut pages)
            .map_err(|_| block::Error::BadBuffer)?;
        Ok(Registration { _pin: pin, pages })
    }

    /// A transfer between the disk and pages of a registration.
    fn transfer_at(
        &mut self,
        cx: &mut Context<'_>,
        op: Op,
        token: Option<u64>,
        registration: Rc<Registration>,
        page: u32,
        sector: u64,
        count: u32,
    ) -> Outcome {
        if matches!(op, Op::Write) {
            if let Err(error) = self.writer.authorize(cx.connection(), token.unwrap_or(0)) {
                return Outcome::Failure(error.code());
            }
        }
        if let Err(e) = self.check(&op, sector, count) {
            return Outcome::Failure(e.code());
        }
        if matches!(op, Op::Read) {
            if let Err(error) = self.writer.can_read(cx.connection()) {
                return Outcome::Failure(error.code());
            }
        }
        let bytes = block_protocol::bytes(&self.info, count);
        let first = page as usize;
        let n = bytes.div_ceil(4096) as usize;
        let Some(pages) = registration.pages.get(first..first + n) else {
            return Outcome::Failure(block::Error::BadBuffer.code());
        };
        let pages = pages.to_vec();
        let lease = if matches!(op, Op::Write) {
            match self.writer.write(cx.connection(), token.unwrap_or(0)) {
                Ok(io) => Some(LeaseTag::Original(io)),
                Err(error) => return Outcome::Failure(error.code()),
            }
        } else {
            match self.writer.read(cx.connection()) {
                Ok(io) => io.map(LeaseTag::Original),
                Err(error) => return Outcome::Failure(error.code()),
            }
        };
        self.queue_request(Request {
            op,
            sector,
            pages,
            bytes,
            hold: Hold::Registered(registration),
            pending: Some(cx.defer()),
            lease,
        })
    }

    fn finish_lease(&mut self, tag: Option<LeaseTag>, success: bool) {
        let result = match tag {
            Some(LeaseTag::Original(io)) => self.writer.finish(io),
            Some(LeaseTag::Release(release)) => {
                let result = self.writer.released(release, success);
                if let Some(pending) = self.release_pending.take() {
                    self.ready.push((
                        pending,
                        if result.is_ok() {
                            Outcome::reply(())
                        } else {
                            Outcome::Failure(block::Error::LeaseFailed.code())
                        },
                    ));
                }
                result
            }
            None => Ok(()),
        };
        if result.is_err() {
            self.failed.get_or_insert(String::from(
                "namespace writer completion/final durability barrier failed",
            ));
        }
    }
    fn advance_lease(&mut self) {
        if self.failed.is_some() {
            return;
        }
        let closed = core::mem::take(&mut *self.closed.borrow_mut());
        let had_closed = !closed.is_empty();
        for connection in closed {
            self.writer.disconnect(connection);
        }
        if had_closed {
            self.release_idle();
        }
        if let Some(release) = self.writer.releasing() {
            if !release.needs_flush() || !self.flush {
                self.finish_lease(Some(LeaseTag::Release(release)), true);
            } else {
                self.queue_request(Request {
                    op: Op::Flush,
                    sector: 0,
                    pages: Vec::new(),
                    bytes: 0,
                    hold: Hold::None,
                    pending: None,
                    lease: Some(LeaseTag::Release(release)),
                });
            }
        }
    }
    /// Releases the quarantine when no transfer is in flight: every pin in it then is
    /// one the device has finished with, such as a registration just ended.
    fn release_idle(&mut self) {
        if self.in_flight.is_empty()
            && let Err(e) = dma_release(&self.device.dma)
        {
            self.failed
                .get_or_insert(format!("dma_release: {}", e.name()));
        }
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

struct Connection {
    block: Rc<RefCell<Block>>,
    registrations: BTreeMap<u32, Rc<Registration>>,
    next: u32,
    connection: Option<u64>,
    closed: Rc<RefCell<Vec<u64>>>,
}

fn decode<T: Wire>(d: &mut Decoder<'_>) -> Option<T> {
    T::decode(d).ok()
}

impl Handler for Connection {
    fn protocol(&self) -> ProtocolId {
        block::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        self.connection = Some(cx.connection());
        let block = self.block.clone();
        let mut b = block.borrow_mut();
        match ordinal {
            1 => match d.finish() {
                Ok(()) => Outcome::reply(b.info),
                Err(_) => Outcome::Status(Status::BadMessage),
            },
            2 | 3 => {
                let token = if ordinal == 3 {
                    let Some(token) = decode::<u64>(d) else {
                        return Outcome::Status(Status::BadMessage);
                    };
                    Some(token)
                } else {
                    None
                };
                let (Some(sector), Some(count), Some(buffer)) =
                    (decode::<u64>(d), decode::<u32>(d), decode::<Memory>(d))
                else {
                    return Outcome::Status(Status::BadMessage);
                };
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                let op = if ordinal == 2 { Op::Read } else { Op::Write };
                b.transfer(cx, op, token, sector, count, buffer)
            }
            4 => {
                let Some(token) = decode::<u64>(d) else {
                    return Outcome::Status(Status::BadMessage);
                };
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                if let Err(error) = b.writer.authorize(cx.connection(), token) {
                    return Outcome::Failure(error.code());
                }
                if !b.flush {
                    // A device without a write cache has nothing to flush.
                    return Outcome::reply(());
                }
                let lease = match b.writer.flush(cx.connection(), token) {
                    Ok(io) => Some(LeaseTag::Original(io)),
                    Err(error) => return Outcome::Failure(error.code()),
                };
                let pending = Some(cx.defer());
                b.queue_request(Request {
                    op: Op::Flush,
                    sector: 0,
                    pages: Vec::new(),
                    bytes: 0,
                    hold: Hold::None,
                    pending,
                    lease,
                })
            }
            5 => {
                let Some(buffer) = decode::<Memory>(d) else {
                    return Outcome::Status(Status::BadMessage);
                };
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                if self.registrations.len() >= block_protocol::REGISTRATIONS_MAX {
                    return Outcome::Failure(block::Error::TooManyRegistrations.code());
                }
                match b.register(&buffer) {
                    Ok(registration) => {
                        let id = self.next;
                        self.next = self.next.wrapping_add(1);
                        let pages = registration.pages.len() as u32;
                        self.registrations.insert(id, Rc::new(registration));
                        Outcome::reply((id, pages))
                    }
                    Err(e) => Outcome::Failure(e.code()),
                }
            }
            6 | 7 => {
                let token = if ordinal == 7 {
                    let Some(token) = decode::<u64>(d) else {
                        return Outcome::Status(Status::BadMessage);
                    };
                    Some(token)
                } else {
                    None
                };
                let (Some(id), Some(page), Some(sector), Some(count)) = (
                    decode::<u32>(d),
                    decode::<u32>(d),
                    decode::<u64>(d),
                    decode::<u32>(d),
                ) else {
                    return Outcome::Status(Status::BadMessage);
                };
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                let Some(registration) = self.registrations.get(&id).cloned() else {
                    return Outcome::Failure(block::Error::NoRegistration.code());
                };
                let op = if ordinal == 6 { Op::Read } else { Op::Write };
                b.transfer_at(cx, op, token, registration, page, sector, count)
            }
            8 => {
                let Some(id) = decode::<u32>(d) else {
                    return Outcome::Status(Status::BadMessage);
                };
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                match self.registrations.remove(&id) {
                    Some(registration) => {
                        drop(registration);
                        b.release_idle();
                        Outcome::reply(())
                    }
                    None => Outcome::Failure(block::Error::NoRegistration.code()),
                }
            }
            9 => {
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                if b.info.read_only {
                    return Outcome::Failure(block::Error::ReadOnly.code());
                }
                match b.writer.claim(cx.connection()) {
                    Ok(token) => Outcome::reply(token),
                    Err(error) => Outcome::Failure(error.code()),
                }
            }
            10 => {
                let Some(token) = decode::<u64>(d) else {
                    return Outcome::Status(Status::BadMessage);
                };
                if d.finish().is_err() {
                    return Outcome::Status(Status::BadMessage);
                }
                if let Err(error) = b.writer.close(cx.connection(), token) {
                    return Outcome::Failure(error.code());
                }
                b.release_pending = Some(cx.defer());
                b.advance_lease();
                Outcome::Deferred
            }
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }

    fn closed(&mut self) {
        self.registrations.clear();
        if let Some(connection) = self.connection.take() {
            self.closed.borrow_mut().push(connection);
        }
        if let Ok(mut b) = self.block.try_borrow_mut() {
            b.release_idle();
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
            writer: lease::Lease::default(),
            release_pending: None,
            closed: Rc::new(RefCell::new(Vec::new())),
            ready: Vec::new(),
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
    let closed = block.borrow().closed.clone();
    let factory: moruna_svc::Factory = Box::new(move || {
        Box::new(Connection {
            block: shared.clone(),
            registrations: BTreeMap::new(),
            next: 1,
            connection: None,
            closed: closed.clone(),
        }) as Box<dyn Handler>
    });
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
    loop {
        let (busy, ready, failure) = {
            let mut block = block.borrow_mut();
            block.advance_lease();
            (
                !block.in_flight.is_empty() || !block.waiting.is_empty(),
                core::mem::take(&mut block.ready),
                block.failed.take(),
            )
        };
        for (pending, outcome) in ready {
            let _peer_already_closed = dispatcher.complete(pending, outcome);
        }
        if let Some(e) = failure {
            println!(out, "KB3 {}: FAIL: the device: {}", name, e);
            return 1;
        }
        if dispatcher.channels() == 0 && !busy {
            break;
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
