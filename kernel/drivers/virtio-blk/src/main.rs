//! The virtio block driver (SDD 25, f.9): serves `moruna.driver.block` on the listener
//! its supervisor gave it, for the one virtio-blk device whose windows it holds.
//!
//! A read or a write pins the client's memory object through the driver's DMA
//! resource and hands its pages to the device as a descriptor chain (a header, one part
//! for each run of pages the device sees one after another, a status byte), so the data
//! moves between the disk and the client's memory with no copy, and a transfer on large
//! pages takes a few descriptors, however long. A transfer of more parts than the queue
//! chains is cut into several chains, each whole sectors (`moruna_virtio::sg`). It is
//! answered when the device has returned every chain; its pin then goes to the
//! quarantine, which the driver releases since the device is done with it. Chains that
//! find the queue full wait, in order, for room.
//!
//! A registered buffer (protocol 1.1) is pinned once, when the client registers it,
//! and its pages' device addresses kept with the connection; `read_at` and `write_at`
//! hand the device some of those pages and pin nothing. Each transfer holds the
//! registration while it is in flight, so a registration ended (or a connection
//! closed) under a transfer keeps its pin until the device returns the chain, and
//! only then does the pin go to the quarantine. `read_to` and `write_from` (protocol 2.2)
//! start a transfer at any byte of a registration.
//!
//! At start the driver asks the device its serial (`GET_ID`) and reads its GPT header: the
//! disk whose header names the GUID of the node's image (`disk.guid`, which the device
//! manager passes on from the boot payload) is the disk the node booted from, never a
//! volume (`architecture/moruna-os-system.md` 3C, item 3.3).

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
use moruna_disk::Guid;
use moruna_mmio::Region;
use moruna_pci::dma::DmaBuffer;
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Handle,
    Handler, Interrupt, Memory, Outcome, Pending, Pin, ProtocolError, ProtocolId, Signals, Status,
    Watch, Wire, deadline_in, dma_release, map, println, start, wait_one,
};
use moruna_virtio::{
    Buffer,
    device::{Device, VirtQueue},
    kind, sg,
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
const T_GET_ID: u32 = 8;
const S_OK: u8 = 0;
/// A slot of the header page: the 16-byte request header, then the status byte.
const SLOT: usize = 32;
const SLOTS: usize = 4096 / SLOT;
/// The largest transfer: 1 MiB, whatever the queue's size, which bounds only how many
/// parts one chain has.
const MAX_BYTES: u64 = 1 << 20;
/// The bytes of the sectors the device addresses (virtio 1.2, 5.2.4).
const SECTOR: u64 = 512;
/// How long the device has to answer the requests made at start.
const START_NS: u64 = 30_000_000_000;

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
    /// The data, as device ranges.
    ranges: Vec<(u64, u32)>,
    hold: Hold,
    pending: Option<Pending>,
    lease: Option<LeaseTag>,
}

/// A request on its way: the chains the device has yet to return, and whether every one
/// returned so far succeeded.
struct Transfer {
    request: Request,
    left: usize,
    ok: bool,
}

/// One chain of a transfer: its first sector, relative to the transfer's, and its data.
struct Piece {
    transfer: u64,
    sector: u64,
    ranges: Vec<(u64, u32)>,
}

struct Block {
    device: Device,
    queue: VirtQueue,
    headers: DmaBuffer,
    free_slots: Vec<usize>,
    /// Each chain the device holds, by its head: its header slot and its transfer.
    in_flight: BTreeMap<u16, (usize, u64)>,
    waiting: VecDeque<Piece>,
    transfers: BTreeMap<u64, Transfer>,
    next_transfer: u64,
    /// The most data parts one chain has: the queue's size less the header and the
    /// status, and no more than the device's `seg_max`.
    parts: usize,
    info: Info,
    flush: bool,
    writer: lease::Lease,
    release_pending: Option<Pending>,
    closed: Rc<RefCell<Vec<u64>>>,
    ready: Vec<(Pending, Outcome)>,
    served: u64,
    /// Why the device can no longer be used; the driver ends with it.
    failed: Option<String>,
    /// What the device said its serial is (`GET_ID`), when it said one.
    serial: Option<String>,
    /// Whether it is the disk the node booted from.
    boot: bool,
}

impl Block {
    /// Hands the device as many waiting chains as the queue takes.
    fn submit_waiting(&mut self) -> Result<(), String> {
        let mut notify = false;
        while let Some(piece) = self.waiting.front() {
            let parts = piece.ranges.len() + 2;
            if self.free_slots.is_empty() || self.queue.queue.free() < parts {
                break;
            }
            let Some(piece) = self.waiting.pop_front() else {
                break;
            };
            self.submit(piece)?;
            notify = true;
        }
        if notify {
            self.device.notify(&self.queue)?;
        }
        Ok(())
    }

    fn submit(&mut self, piece: Piece) -> Result<(), String> {
        let request = &self
            .transfers
            .get(&piece.transfer)
            .ok_or("a chain of no transfer")?
            .request;
        let slot = self.free_slots.pop().ok_or("no header slot")?;
        let at = slot * SLOT;
        let (kind, writes) = match request.op {
            Op::Read => (T_IN, true),
            Op::Write => (T_OUT, false),
            Op::Flush => (T_FLUSH, false),
        };
        let sector = request.sector + piece.sector;
        let h = &self.headers.map;
        let w = |e: moruna_svc::Error| format!("header: {}", e.name());
        h.write32(at, kind).map_err(w)?;
        h.write32(at + 4, 0).map_err(w)?;
        h.write64(at + 8, sector).map_err(w)?;
        h.write8(at + 16, 0xff).map_err(w)?;
        let chain = sg::chain(
            Buffer {
                addr: self.headers.addr(at),
                len: 16,
                device_writes: false,
            },
            &piece.ranges,
            writes,
            Buffer {
                addr: self.headers.addr(at + 16),
                len: 1,
                device_writes: true,
            },
        );
        let head = self
            .queue
            .queue
            .add(&chain)
            .map_err(|e| format!("queue: {e:?}"))?;
        self.in_flight.insert(head, (slot, piece.transfer));
        Ok(())
    }

    /// Answers every request the device has returned all the chains of.
    fn complete(&mut self, done: &mut Completions) -> Result<(), String> {
        let mut finished = 0;
        while let Some(used) = self
            .queue
            .queue
            .pop_used()
            .map_err(|e| format!("used ring: {e:?}"))?
        {
            let (slot, id) = self
                .in_flight
                .remove(&used.head)
                .ok_or("the device returned a chain not in flight")?;
            let status = self
                .headers
                .map
                .read8(slot * SLOT + 16)
                .map_err(|e| format!("status: {}", e.name()))?;
            self.free_slots.push(slot);
            finished += 1;
            let transfer = self
                .transfers
                .get_mut(&id)
                .ok_or("the device returned a chain of no transfer")?;
            transfer.left -= 1;
            transfer.ok &= status == S_OK;
            if transfer.left > 0 {
                continue;
            }
            let Some(Transfer {
                mut request, ok, ..
            }) = self.transfers.remove(&id)
            else {
                continue;
            };
            self.finish_lease(request.lease.take(), ok);
            let outcome = if !ok {
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
        if matches!(op, Op::Write)
            && let Err(error) = self.writer.authorize(cx.connection(), token.unwrap_or(0))
        {
            return fail(error);
        }
        if let Err(e) = self.check(&op, sector, count) {
            return fail(e);
        }
        let read = matches!(op, Op::Read);
        if matches!(op, Op::Read)
            && let Err(error) = self.writer.can_read(cx.connection())
        {
            return Outcome::Failure(error.code());
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
        let Ok(ranges) = sg::ranges(&pages, 0, bytes) else {
            return fail(block::Error::BadBuffer);
        };
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
            ranges,
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

    /// A transfer between the disk and a registration, from its byte `offset`.
    #[expect(
        clippy::too_many_arguments,
        reason = "The protocol keeps connection authority, registration pages and disk range distinct"
    )]
    fn transfer_at(
        &mut self,
        cx: &mut Context<'_>,
        op: Op,
        token: Option<u64>,
        registration: Rc<Registration>,
        offset: u64,
        sector: u64,
        count: u32,
    ) -> Outcome {
        if matches!(op, Op::Write)
            && let Err(error) = self.writer.authorize(cx.connection(), token.unwrap_or(0))
        {
            return Outcome::Failure(error.code());
        }
        if let Err(e) = self.check(&op, sector, count) {
            return Outcome::Failure(e.code());
        }
        if matches!(op, Op::Read)
            && let Err(error) = self.writer.can_read(cx.connection())
        {
            return Outcome::Failure(error.code());
        }
        let bytes = block_protocol::bytes(&self.info, count);
        let Ok(ranges) = sg::ranges(&registration.pages, offset, bytes) else {
            return Outcome::Failure(block::Error::BadBuffer.code());
        };
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
            ranges,
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
                    ranges: Vec::new(),
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

    /// Queues a request for the device, as the chains the queue takes. If the device
    /// cannot take it, the driver records why and ends after this turn, which closes
    /// every connection.
    fn queue_request(&mut self, mut request: Request) -> Outcome {
        let pieces = if request.ranges.is_empty() {
            vec![sg::Request {
                sector: 0,
                ranges: Vec::new(),
            }]
        } else {
            match sg::requests(&request.ranges, SECTOR, self.parts) {
                Ok(pieces) => pieces,
                Err(_) => {
                    self.finish_lease(request.lease.take(), false);
                    return Outcome::Failure(block::Error::BadBuffer.code());
                }
            }
        };
        let id = self.next_transfer;
        self.next_transfer += 1;
        self.transfers.insert(
            id,
            Transfer {
                request,
                left: pieces.len(),
                ok: true,
            },
        );
        for piece in pieces {
            self.waiting.push_back(Piece {
                transfer: id,
                sector: piece.sector,
                ranges: piece.ranges,
            });
        }
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
                    ranges: Vec::new(),
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
                let offset = u64::from(page) * 4096;
                b.transfer_at(cx, op, token, registration, offset, sector, count)
            }
            11 | 12 => {
                let token = if ordinal == 12 {
                    let Some(token) = decode::<u64>(d) else {
                        return Outcome::Status(Status::BadMessage);
                    };
                    Some(token)
                } else {
                    None
                };
                let (Some(id), Some(offset), Some(sector), Some(count)) = (
                    decode::<u32>(d),
                    decode::<u64>(d),
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
                let op = if ordinal == 11 { Op::Read } else { Op::Write };
                b.transfer_at(cx, op, token, registration, offset, sector, count)
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

/// Makes one request at start, before the driver serves: `kind` for `sector`, its data the
/// `len` bytes of `data` at `at`, and waits for the device's answer. Whether it succeeded.
#[expect(
    clippy::too_many_arguments,
    reason = "One request's every part, made once at start"
)]
fn ask(
    device: &Device,
    queue: &mut VirtQueue,
    line: &Interrupt,
    headers: &DmaBuffer,
    kind: u32,
    sector: u64,
    data: &DmaBuffer,
    at: usize,
    len: u32,
) -> Result<bool, String> {
    let h = &headers.map;
    let w = |e: moruna_svc::Error| format!("header: {}", e.name());
    h.write32(0, kind).map_err(w)?;
    h.write32(4, 0).map_err(w)?;
    h.write64(8, sector).map_err(w)?;
    h.write8(16, 0xff).map_err(w)?;
    let chain = sg::chain(
        Buffer {
            addr: headers.addr(0),
            len: 16,
            device_writes: false,
        },
        &[(data.addr(at), len)],
        true,
        Buffer {
            addr: headers.addr(16),
            len: 1,
            device_writes: true,
        },
    );
    queue
        .queue
        .add(&chain)
        .map_err(|e| format!("queue: {e:?}"))?;
    device.notify(queue)?;
    let deadline = deadline_in(START_NS);
    loop {
        if queue
            .queue
            .pop_used()
            .map_err(|e| format!("used ring: {e:?}"))?
            .is_some()
        {
            break;
        }
        wait_one(&line.0, Signals::SIGNALED, deadline)
            .map_err(|e| format!("the device did not answer at start: {}", e.name()))?;
        line.ack()
            .map_err(|e| format!("interrupt_ack: {}", e.name()))?;
    }
    let status = h.read8(16).map_err(|e| format!("status: {}", e.name()))?;
    dma_release(&device.dma).map_err(|e| format!("dma_release: {}", e.name()))?;
    Ok(status == S_OK)
}

/// The device's serial, when it states one (`GET_ID`), and whether it is the disk the node
/// booted from: its logical block 1 a GPT header naming `image`.
fn identify(
    device: &Device,
    queue: &mut VirtQueue,
    line: &Interrupt,
    headers: &DmaBuffer,
    block: u64,
    sectors: u64,
    image: Option<&Guid>,
) -> Result<(Option<String>, bool), String> {
    let probe = DmaBuffer::new(&device.dma, 2 * 4096)?;
    let serial = if ask(
        device,
        queue,
        line,
        headers,
        T_GET_ID,
        0,
        &probe,
        0,
        moruna_disk::VIRTIO_ID_BYTES as u32,
    )? {
        let mut id = [0u8; moruna_disk::VIRTIO_ID_BYTES];
        probe
            .map
            .read(0, &mut id)
            .map_err(|e| format!("the serial: {}", e.name()))?;
        moruna_disk::serial(&id)
    } else {
        None
    };
    let header_sector = block / SECTOR;
    let mut boot = false;
    if let Some(image) = image
        && block <= 4096
        && sectors >= 2 * header_sector
        && ask(
            device,
            queue,
            line,
            headers,
            T_IN,
            header_sector,
            &probe,
            4096,
            block as u32,
        )?
    {
        let mut header = vec![0u8; block as usize];
        probe
            .map
            .read(4096, &mut header)
            .map_err(|e| format!("the GPT header: {}", e.name()))?;
        boot = moruna_disk::is_boot(&header, image);
    }
    Ok((serial, boot))
}

fn open(image: Option<&Guid>) -> Result<(Block, Interrupt), String> {
    let (device, line) = Device::open(kind::BLOCK, F_SEG_MAX | F_RO | F_BLK_SIZE | F_FLUSH, 0)?;
    let features = device.features;
    // virtio-blk addresses 512-byte sectors whatever logical block size it advertises
    // (virtio 1.2, 5.2.4), so the protocol's sector is 512 bytes; the logical block is
    // where the GPT header is.
    let (sectors, seg_max, blk_size) = device
        .transport
        .device_config(|c| Ok((c.read64(0)?, c.read32(12)?, c.read32(20)?)))
        .map_err(|e| format!("device configuration: {e}"))?;
    let block = if features & F_BLK_SIZE != 0 && blk_size >= 512 && blk_size.is_power_of_two() {
        u64::from(blk_size)
    } else {
        SECTOR
    };
    let mut queue = device.queue(0, 256)?;
    let mut parts = queue.queue.size() as usize - 2;
    if features & F_SEG_MAX != 0 && seg_max != 0 {
        parts = parts.min(seg_max as usize);
    }
    if parts < 2 {
        return Err(format!("a queue whose chains hold {parts} parts of data"));
    }
    let headers = DmaBuffer::new(&device.dma, 4096)?;
    let slots = SLOTS.min(queue.queue.size() as usize);
    device.ready()?;
    let (serial, boot) = identify(&device, &mut queue, &line, &headers, block, sectors, image)?;
    Ok((
        Block {
            device,
            queue,
            headers,
            free_slots: (0..slots).rev().collect(),
            in_flight: BTreeMap::new(),
            waiting: VecDeque::new(),
            transfers: BTreeMap::new(),
            next_transfer: 0,
            parts,
            info: Info {
                sector_size: SECTOR as u32,
                sectors,
                max_sectors: (MAX_BYTES / SECTOR) as u32,
                read_only: features & F_RO != 0,
            },
            flush: features & F_FLUSH != 0,
            writer: lease::Lease::default(),
            release_pending: None,
            closed: Rc::new(RefCell::new(Vec::new())),
            ready: Vec::new(),
            served: 0,
            failed: None,
            serial,
            boot,
        },
        line,
    ))
}

/// The GUID of the node's image's disk, from the boot payload's `disk.guid`, which the device
/// manager passes on when the boot gave it one.
fn image_guid() -> Result<Option<Guid>, String> {
    let Some(handle) = start::take("disk.guid") else {
        return Ok(None);
    };
    let mut page = [0u8; 4096];
    Memory(handle)
        .map(0, 4096, map::READ)
        .and_then(|m| m.read(0, &mut page))
        .map_err(|e| format!("reading disk.guid: {}", e.name()))?;
    Guid::from_file(&page)
        .map(Some)
        .ok_or_else(|| String::from("disk.guid holds no GUID"))
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
    let opened = image_guid().and_then(|image| open(image.as_ref()));
    let (block, line) = match opened {
        Ok(b) => b,
        Err(e) => {
            println!(out, "KB3 {}: FAIL: {}", name, e);
            return 1;
        }
    };
    let info = block.info;
    let serial = match &block.serial {
        Some(serial) => format!(", serial {serial}"),
        None => String::from(", no serial"),
    };
    let boot = if block.boot { ", the boot disk" } else { "" };
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
        "KB3 {}: virtio-blk up: {} sectors of {} bytes, {} sectors a transfer{}{}{}",
        name,
        info.sectors,
        info.sector_size,
        info.max_sectors,
        if info.read_only { ", read only" } else { "" },
        serial,
        boot
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
