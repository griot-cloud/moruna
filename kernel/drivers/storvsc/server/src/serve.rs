//! Serving the disks: `moruna.driver.block` for each, on its controller's channel.
//!
//! A read or a write pins the client's memory object (the [`Node`]'s DMA grant on a node)
//! and names its pages to the host in the request, GPA direct, so the data moves between the
//! disk and the client's memory with no copy. The request is answered when the host
//! completes it; its pin then goes to the grant's quarantine, which is released once the
//! host has finished with every pin closed so far. Requests that find the ring or the
//! in-flight limit full wait, in order, for room. A registered buffer (protocol 1.1) is
//! pinned once, and `read_at`, `write_at`, `read_to` and `write_from` name some of its pages,
//! from any byte of the first.
//!
//! The writer lease, the checks and the answers are virtio-blk's
//! (`kernel/drivers/virtio-blk/src/main.rs`), so a client sees the same driver whatever the
//! disk is.

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    format,
    rc::Rc,
    string::String,
    vec::Vec,
};
use core::{cell::RefCell, fmt};

use block_protocol::{Info, REGISTRATIONS_MAX, block, lease};
use moruna_storvsc::{Channel, Completion, Disk, Error, Event, PAGE, Storvsc};
use moruna_svc::{
    ConnectionId, Context, Decoder, Factory, Handler, Memory, Outcome, Pending, ProtocolError,
    ProtocolId, Status, Wire,
};

use crate::naming::Name;

/// `read_to(id, offset, sector, count)`: block protocol minor 2 (item 3.3), whole sectors
/// into registration `id` from its byte `offset`. Served by its ordinal until that minor is
/// in `block-protocol`.
pub const READ_TO: u16 = 11;

/// `write_from(lease, id, offset, sector, count)`: block protocol minor 2's write from
/// registration `id` at its byte `offset`, under the writer lease as every write is.
pub const WRITE_FROM: u16 = 12;

/// The most failed transfers said on the console; the rest are only counted.
const FAILURE_LINES: u64 = 16;

/// What the driver needs of the node besides its channels.
pub trait Node {
    /// Memory pinned for the host to read or write: dropped, it goes to the DMA grant's
    /// quarantine until [`release`](Node::release).
    type Pin;

    /// Pins `memory` for the host, to write into it with `device_writes`, or to read it:
    /// the pin, and the guest page number of each of its pages. `None` when it is not
    /// memory the host may reach, or lacks the right the transfer needs.
    fn pin(&mut self, memory: &Memory, device_writes: bool) -> Option<(Self::Pin, Vec<u64>)>;

    /// The host is about to read `pin`'s pages (`to_device`), or has written them. Nothing
    /// to do where the host reaches guest memory itself, as Hyper-V does; a node that
    /// reaches it through copies, as a host test's fake does, makes them here.
    fn sync(&mut self, _pin: &Self::Pin, _to_device: bool) {}

    /// The host has finished with every pin dropped so far: the quarantine is released.
    fn release(&mut self);

    /// Writes a line on the node's console.
    fn say(&mut self, line: fmt::Arguments<'_>);
}

/// A disk the driver serves: its controller, where and how large it is, and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Served {
    /// Its controller, by index into the driver's.
    pub controller: usize,
    /// Its address and size.
    pub disk: Disk,
    /// Its serial, and whether it is served read-only and is the boot disk.
    pub name: Name,
    /// The window of the disk it is, in the disk's blocks: the boot disk's `moruna-system`
    /// partition (item 3.3), which no request reaches past. `None`: the whole disk.
    pub window: Option<moruna_disk::Slice>,
}

impl Served {
    /// Its blocks: its window's, or the disk's.
    pub fn blocks(&self) -> u64 {
        self.window.map_or(self.disk.blocks(), |w| w.sectors)
    }

    /// The disk's block of its block `block`.
    fn first(&self) -> u64 {
        self.window.map_or(0, |w| w.first)
    }
}

enum Op {
    Read,
    Write,
    Flush,
}

/// A client's buffer, pinned once for every transfer into or out of it.
struct Registration<P> {
    pin: P,
    pages: Vec<u64>,
}

/// What a transfer keeps alive until the host completes it.
enum Hold<P> {
    None,
    /// A buffer pinned for this transfer, handed back in the reply.
    Buffer(Memory, P),
    /// A registration, which keeps its pin.
    Registered(Rc<Registration<P>>),
}

impl<P> Hold<P> {
    fn pin(&self) -> Option<&P> {
        match self {
            Hold::None => None,
            Hold::Buffer(_, pin) => Some(pin),
            Hold::Registered(registration) => Some(&registration.pin),
        }
    }
}

enum LeaseTag {
    Original(lease::Io),
    Release(lease::Release),
}

struct Transfer<P> {
    disk: usize,
    op: Op,
    block: u64,
    count: u32,
    /// The data's first byte in the first page.
    offset: usize,
    pages: Vec<u64>,
    hold: Hold<P>,
    pending: Option<Pending>,
    lease: Option<LeaseTag>,
}

struct DiskState {
    served: Served,
    writer: lease::Lease,
    release_pending: Option<Pending>,
    /// Connections gone since the lease last heard of it.
    closed: Vec<ConnectionId>,
}

/// The driver: its controllers, its disks, and the transfers in flight and waiting.
pub struct Driver<C: Channel, N: Node> {
    node: N,
    controllers: Vec<Storvsc<C>>,
    disks: Vec<DiskState>,
    in_flight: BTreeMap<(usize, u64), Transfer<N::Pin>>,
    waiting: Vec<VecDeque<Transfer<N::Pin>>>,
    ready: Vec<(Pending, Outcome)>,
    failures: u64,
    served: u64,
    failed: Option<String>,
}

fn failure(error: block::Error) -> Outcome {
    Outcome::Failure(error.code())
}

impl<C: Channel, N: Node> Driver<C, N> {
    /// The driver over `node`, serving `disks` on `controllers`, each ready.
    pub fn new(node: N, controllers: Vec<Storvsc<C>>, disks: Vec<Served>) -> Driver<C, N> {
        let waiting = controllers.iter().map(|_| VecDeque::new()).collect();
        Driver {
            node,
            controllers,
            disks: disks
                .into_iter()
                .map(|served| DiskState {
                    served,
                    writer: lease::Lease::default(),
                    release_pending: None,
                    closed: Vec::new(),
                })
                .collect(),
            in_flight: BTreeMap::new(),
            waiting,
            ready: Vec::new(),
            failures: 0,
            served: 0,
            failed: None,
        }
    }

    /// The node the driver runs on.
    pub fn node(&self) -> &N {
        &self.node
    }

    /// The node the driver runs on.
    pub fn node_mut(&mut self) -> &mut N {
        &mut self.node
    }

    /// The disks served, in the order their listeners are.
    pub fn disks(&self) -> impl Iterator<Item = &Served> {
        self.disks.iter().map(|state| &state.served)
    }

    /// Why a controller can no longer be used, if one cannot: the process ends with it.
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Transfers the host has, and transfers waiting for room.
    pub fn busy(&self) -> usize {
        self.in_flight.len() + self.waiting.iter().map(VecDeque::len).sum::<usize>()
    }

    /// Transfers answered so far.
    pub fn served(&self) -> u64 {
        self.served
    }

    /// The answers that are ready, for the dispatcher to send.
    pub fn take_ready(&mut self) -> Vec<(Pending, Outcome)> {
        core::mem::take(&mut self.ready)
    }

    /// What `info` answers for disk `disk`. The largest transfer leaves a page of the
    /// largest a request can name for a registration's byte offset.
    pub fn info(&self, disk: usize) -> Info {
        let served = &self.disks[disk].served;
        let size = served.disk.block_size();
        let room = self.controllers[served.controller]
            .max_transfer_bytes()
            .saturating_sub(PAGE as u32);
        Info {
            sector_size: size,
            sectors: served.blocks(),
            max_sectors: (room / size).max(1),
            read_only: served.name.read_only,
        }
    }

    fn fail(&mut self, why: String) {
        self.failed.get_or_insert(why);
    }

    /// Reads what every controller has, answers each completed transfer, and sends what
    /// waits; then asks each controller's host to signal, and does it all again while
    /// there is more. The driver waits only once this has returned.
    pub fn service(&mut self) {
        loop {
            self.poll();
            if self.failed.is_some() {
                return;
            }
            let mut more = false;
            for c in 0..self.controllers.len() {
                match self.controllers[c].arm() {
                    Ok(ready) => more |= ready,
                    Err(e) => {
                        self.fail(format!("controller {c}: {e}"));
                        return;
                    }
                }
            }
            if !more {
                return;
            }
        }
    }

    /// Answers every transfer the controllers have completed, then sends what waits.
    fn poll(&mut self) {
        let mut finished = 0;
        for c in 0..self.controllers.len() {
            loop {
                match self.controllers[c].poll() {
                    Ok(Some(Event::Completed(completion))) => {
                        self.complete(c, completion);
                        finished += 1;
                    }
                    Ok(Some(Event::Ready)) => {}
                    Ok(None) => break,
                    Err(e @ (Error::Ring(_) | Error::Failed | Error::Channel(_))) => {
                        self.fail(format!("controller {c}: {e}"));
                        return;
                    }
                    // One packet, which is gone: nothing in flight is lost by it.
                    Err(e) => {
                        self.failures += 1;
                        if self.failures <= FAILURE_LINES {
                            self.node
                                .say(format_args!("storvsc: controller {c}: refused {e}"));
                        }
                    }
                }
            }
        }
        if finished > 0 {
            // The host has finished with every pin closed so far.
            self.node.release();
        }
        self.advance_leases();
        for c in 0..self.controllers.len() {
            self.submit(c);
        }
    }

    /// The end of one transfer: its answer, and its lease's.
    fn complete(&mut self, controller: usize, completion: Completion) {
        let Some(mut transfer) = self.in_flight.remove(&(controller, completion.id)) else {
            return;
        };
        let read = matches!(transfer.op, Op::Read);
        self.finish_lease(
            transfer.disk,
            transfer.lease.take(),
            completion.result.is_ok(),
        );
        let outcome = match completion.result {
            Ok(_) => {
                if read && let Some(pin) = transfer.hold.pin() {
                    self.node.sync(pin, false);
                }
                match transfer.hold {
                    Hold::Buffer(memory, pin) => {
                        drop(pin);
                        Outcome::reply(memory)
                    }
                    Hold::Registered(_) | Hold::None => Outcome::reply(()),
                }
            }
            Err(why) => {
                self.failures += 1;
                if self.failures <= FAILURE_LINES {
                    let serial = &self.disks[transfer.disk].served.name.serial;
                    self.node.say(format_args!(
                        "storvsc: {serial}: a transfer at sector {} failed: {why:?}",
                        transfer.block
                    ));
                }
                failure(block::Error::Io)
            }
        };
        if let Some(pending) = transfer.pending {
            self.ready.push((pending, outcome));
        }
        self.served += 1;
    }

    /// Hands controller `c` as many waiting transfers as it takes.
    fn submit(&mut self, c: usize) {
        self.submit_waiting(c);
        if self.failed.is_some() {
            return;
        }
        // What was sent goes to the host together: one commit, at most one signal.
        if let Err(e) = self.controllers[c].commit() {
            self.fail(format!("controller {c}: {e}"));
        }
    }

    /// Sends the transfers waiting on controller `c`, in order, until one finds no room.
    fn submit_waiting(&mut self, c: usize) {
        while let Some(transfer) = self.waiting[c].front() {
            if matches!(transfer.op, Op::Write)
                && let Some(pin) = transfer.hold.pin()
            {
                self.node.sync(pin, true);
            }
            let disk = self.disks[transfer.disk].served.disk;
            // A window's block is the disk's that many blocks on: `check` held it inside.
            let block = transfer.block + self.disks[transfer.disk].served.first();
            let device = &mut self.controllers[c];
            let sent = match transfer.op {
                Op::Read => device.read_offset(
                    &disk,
                    block,
                    transfer.count,
                    transfer.offset,
                    &transfer.pages,
                ),
                Op::Write => device.write_offset(
                    &disk,
                    block,
                    transfer.count,
                    transfer.offset,
                    &transfer.pages,
                ),
                Op::Flush => device.flush(disk.address()),
            };
            match sent {
                Ok(id) => {
                    if let Some(transfer) = self.waiting[c].pop_front() {
                        self.in_flight.insert((c, id), transfer);
                    }
                }
                Err(Error::Full) => return,
                Err(e @ (Error::Ring(_) | Error::Failed | Error::Channel(_))) => {
                    self.fail(format!("controller {c}: {e}"));
                    return;
                }
                // The library refused it: an answer the checks before it should have
                // given, as a failed transfer.
                Err(_) => {
                    if let Some(mut transfer) = self.waiting[c].pop_front() {
                        self.finish_lease(transfer.disk, transfer.lease.take(), false);
                        if let Some(pending) = transfer.pending {
                            self.ready.push((pending, failure(block::Error::Io)));
                        }
                    }
                }
            }
        }
    }

    fn queue(&mut self, transfer: Transfer<N::Pin>) -> Outcome {
        let c = self.disks[transfer.disk].served.controller;
        self.waiting[c].push_back(transfer);
        self.submit(c);
        Outcome::Deferred
    }

    /// Refuses a transfer the disk cannot make: a count of zero or above the largest,
    /// sectors past the end, a write to a read-only disk.
    fn check(&self, disk: usize, write: bool, sector: u64, count: u32) -> Result<(), block::Error> {
        let info = self.info(disk);
        if count == 0 || count > info.max_sectors {
            return Err(block::Error::BadCount);
        }
        if sector
            .checked_add(count as u64)
            .is_none_or(|end| end > info.sectors)
        {
            return Err(block::Error::OutOfRange);
        }
        if write && info.read_only {
            return Err(block::Error::ReadOnly);
        }
        Ok(())
    }

    /// The lease's say on a transfer: a write needs the writer, and a read the writer's
    /// connection does is counted.
    fn admit(
        &mut self,
        cx: &Context<'_>,
        disk: usize,
        write: bool,
        token: Option<u64>,
    ) -> Result<Option<LeaseTag>, block::Error> {
        let writer = &mut self.disks[disk].writer;
        if write {
            writer
                .write(cx.connection(), token.unwrap_or(0))
                .map(|io| Some(LeaseTag::Original(io)))
        } else {
            writer
                .read(cx.connection())
                .map(|io| io.map(LeaseTag::Original))
        }
    }

    /// The checks before a transfer, in virtio-blk's order.
    fn allowed(
        &mut self,
        cx: &Context<'_>,
        disk: usize,
        write: bool,
        token: Option<u64>,
        sector: u64,
        count: u32,
    ) -> Result<(), block::Error> {
        if write {
            self.disks[disk]
                .writer
                .authorize(cx.connection(), token.unwrap_or(0))?;
        }
        self.check(disk, write, sector, count)?;
        if !write {
            self.disks[disk].writer.can_read(cx.connection())?;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The protocol keeps connection authority, the buffer and disk range distinct"
    )]
    fn transfer(
        &mut self,
        cx: &mut Context<'_>,
        disk: usize,
        write: bool,
        token: Option<u64>,
        sector: u64,
        count: u32,
        buffer: Memory,
    ) -> Outcome {
        if let Err(e) = self.allowed(cx, disk, write, token, sector, count) {
            return failure(e);
        }
        let bytes = self.info(disk).sector_size as usize * count as usize;
        let Some((pin, mut pages)) = self.node.pin(&buffer, !write) else {
            return failure(block::Error::BadBuffer);
        };
        let needed = bytes.div_ceil(PAGE);
        if pages.len() < needed {
            drop(pin);
            self.release_idle();
            return failure(block::Error::BadBuffer);
        }
        pages.truncate(needed);
        let lease = match self.admit(cx, disk, write, token) {
            Ok(lease) => lease,
            Err(e) => {
                drop(pin);
                self.release_idle();
                return failure(e);
            }
        };
        self.queue(Transfer {
            disk,
            op: if write { Op::Write } else { Op::Read },
            block: sector,
            count,
            offset: 0,
            pages,
            hold: Hold::Buffer(buffer, pin),
            pending: Some(cx.defer()),
            lease,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The protocol keeps connection authority, registration bytes and disk range distinct"
    )]
    fn transfer_at(
        &mut self,
        cx: &mut Context<'_>,
        disk: usize,
        write: bool,
        token: Option<u64>,
        registration: Rc<Registration<N::Pin>>,
        offset: u64,
        sector: u64,
        count: u32,
    ) -> Outcome {
        if let Err(e) = self.allowed(cx, disk, write, token, sector, count) {
            return failure(e);
        }
        let bytes = self.info(disk).sector_size as u64 * count as u64;
        let first = offset / PAGE as u64;
        let within = (offset % PAGE as u64) as usize;
        let needed = (within as u64 + bytes).div_ceil(PAGE as u64);
        let pages = usize::try_from(first)
            .ok()
            .zip(usize::try_from(needed).ok())
            .and_then(|(first, needed)| registration.pages.get(first..first.checked_add(needed)?));
        let Some(pages) = pages.map(<[u64]>::to_vec) else {
            return failure(block::Error::BadBuffer);
        };
        let lease = match self.admit(cx, disk, write, token) {
            Ok(lease) => lease,
            Err(e) => return failure(e),
        };
        self.queue(Transfer {
            disk,
            op: if write { Op::Write } else { Op::Read },
            block: sector,
            count,
            offset: within,
            pages,
            hold: Hold::Registered(registration),
            pending: Some(cx.defer()),
            lease,
        })
    }

    fn flush(&mut self, cx: &mut Context<'_>, disk: usize, token: u64) -> Outcome {
        let writer = &mut self.disks[disk].writer;
        if let Err(e) = writer.authorize(cx.connection(), token) {
            return failure(e);
        }
        let lease = match writer.flush(cx.connection(), token) {
            Ok(io) => Some(LeaseTag::Original(io)),
            Err(e) => return failure(e),
        };
        self.queue(Transfer {
            disk,
            op: Op::Flush,
            block: 0,
            count: 0,
            offset: 0,
            pages: Vec::new(),
            hold: Hold::None,
            pending: Some(cx.defer()),
            lease,
        })
    }

    fn release_write(&mut self, cx: &mut Context<'_>, disk: usize, token: u64) -> Outcome {
        if let Err(e) = self.disks[disk].writer.close(cx.connection(), token) {
            return failure(e);
        }
        self.disks[disk].release_pending = Some(cx.defer());
        self.advance_leases();
        Outcome::Deferred
    }

    fn finish_lease(&mut self, disk: usize, tag: Option<LeaseTag>, success: bool) {
        let state = &mut self.disks[disk];
        let result = match tag {
            Some(LeaseTag::Original(io)) => state.writer.finish(io),
            Some(LeaseTag::Release(release)) => {
                let result = state.writer.released(release, success);
                if let Some(pending) = state.release_pending.take() {
                    let outcome = match result {
                        Ok(()) => Outcome::reply(()),
                        Err(_) => failure(block::Error::LeaseFailed),
                    };
                    self.ready.push((pending, outcome));
                }
                result
            }
            None => Ok(()),
        };
        if result.is_err() {
            let serial = &self.disks[disk].served.name.serial;
            self.failed.get_or_insert(format!(
                "{serial}: the writer's completion or its final flush failed"
            ));
        }
    }

    /// Hands each disk's lease the connections that have gone, and sends the final flush
    /// of a writer whose original transfers are all done.
    fn advance_leases(&mut self) {
        if self.failed.is_some() {
            return;
        }
        let mut released = false;
        for disk in 0..self.disks.len() {
            let state = &mut self.disks[disk];
            for connection in core::mem::take(&mut state.closed) {
                state.writer.disconnect(connection);
                released = true;
            }
            let Some(release) = state.writer.releasing() else {
                continue;
            };
            if release.needs_flush() {
                self.queue(Transfer {
                    disk,
                    op: Op::Flush,
                    block: 0,
                    count: 0,
                    offset: 0,
                    pages: Vec::new(),
                    hold: Hold::None,
                    pending: None,
                    lease: Some(LeaseTag::Release(release)),
                });
            } else {
                self.finish_lease(disk, Some(LeaseTag::Release(release)), true);
            }
        }
        if released {
            self.release_idle();
        }
    }

    /// Releases the quarantine when no transfer is in flight: every pin in it is then one
    /// the host has finished with, such as a registration just ended.
    fn release_idle(&mut self) {
        if self.in_flight.is_empty() {
            self.node.release();
        }
    }
}

/// One client's connection to one disk.
pub struct Connection<C: Channel, N: Node> {
    driver: Rc<RefCell<Driver<C, N>>>,
    disk: usize,
    registrations: BTreeMap<u32, Rc<Registration<N::Pin>>>,
    next: u32,
    connection: Option<ConnectionId>,
}

/// The factory of disk `disk`'s listener: each client that connects there is served it.
pub fn factory<C: Channel + 'static, N: Node + 'static>(
    driver: Rc<RefCell<Driver<C, N>>>,
    disk: usize,
) -> Factory {
    Box::new(move || {
        Box::new(Connection {
            driver: driver.clone(),
            disk,
            registrations: BTreeMap::new(),
            next: 1,
            connection: None,
        }) as Box<dyn Handler>
    })
}

/// A request's arguments, every byte and handle of them.
fn arguments<T: Wire>(d: &mut Decoder<'_>) -> Option<T> {
    let value = T::decode(d).ok()?;
    d.finish().ok()?;
    Some(value)
}

impl<C: Channel, N: Node> Connection<C, N> {
    fn registration(&self, id: u32) -> Result<Rc<Registration<N::Pin>>, Outcome> {
        self.registrations
            .get(&id)
            .cloned()
            .ok_or(failure(block::Error::NoRegistration))
    }

    fn request(
        &mut self,
        driver: &mut Driver<C, N>,
        cx: &mut Context<'_>,
        ordinal: u16,
        d: &mut Decoder<'_>,
    ) -> Option<Outcome> {
        let disk = self.disk;
        Some(match ordinal {
            1 => {
                d.finish().ok()?;
                Outcome::reply(driver.info(disk))
            }
            2 => {
                let (sector, count, buffer) = arguments(d)?;
                driver.transfer(cx, disk, false, None, sector, count, buffer)
            }
            3 => {
                let (token, sector, count, buffer) = arguments(d)?;
                driver.transfer(cx, disk, true, Some(token), sector, count, buffer)
            }
            4 => driver.flush(cx, disk, arguments(d)?),
            5 => {
                let buffer: Memory = arguments(d)?;
                if self.registrations.len() >= REGISTRATIONS_MAX {
                    return Some(failure(block::Error::TooManyRegistrations));
                }
                let Some((pin, pages)) = driver.node.pin(&buffer, true) else {
                    return Some(failure(block::Error::BadBuffer));
                };
                let id = self.next;
                self.next = self.next.wrapping_add(1);
                let count = pages.len() as u32;
                self.registrations
                    .insert(id, Rc::new(Registration { pin, pages }));
                Outcome::reply((id, count))
            }
            6 | READ_TO => {
                let (id, at, sector, count): (u32, u64, u64, u32) = if ordinal == 6 {
                    let (id, page, sector, count): (u32, u32, u64, u32) = arguments(d)?;
                    (id, page as u64 * PAGE as u64, sector, count)
                } else {
                    arguments(d)?
                };
                match self.registration(id) {
                    Ok(registration) => {
                        driver.transfer_at(cx, disk, false, None, registration, at, sector, count)
                    }
                    Err(refused) => refused,
                }
            }
            7 | WRITE_FROM => {
                let (token, id, at, sector, count): (u64, u32, u64, u64, u32) = if ordinal == 7 {
                    let (token, id, page, sector, count): (u64, u32, u32, u64, u32) = arguments(d)?;
                    (token, id, page as u64 * PAGE as u64, sector, count)
                } else {
                    arguments(d)?
                };
                match self.registration(id) {
                    Ok(registration) => driver.transfer_at(
                        cx,
                        disk,
                        true,
                        Some(token),
                        registration,
                        at,
                        sector,
                        count,
                    ),
                    Err(refused) => refused,
                }
            }
            8 => {
                let id: u32 = arguments(d)?;
                match self.registrations.remove(&id) {
                    Some(registration) => {
                        drop(registration);
                        driver.release_idle();
                        Outcome::reply(())
                    }
                    None => failure(block::Error::NoRegistration),
                }
            }
            9 => {
                d.finish().ok()?;
                if driver.info(disk).read_only {
                    return Some(failure(block::Error::ReadOnly));
                }
                match driver.disks[disk].writer.claim(cx.connection()) {
                    Ok(token) => Outcome::reply(token),
                    Err(e) => failure(e),
                }
            }
            10 => driver.release_write(cx, disk, arguments(d)?),
            _ => Outcome::Status(Status::UnknownMethod),
        })
    }
}

impl<C: Channel, N: Node> Handler for Connection<C, N> {
    fn protocol(&self) -> ProtocolId {
        block::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        self.connection = Some(cx.connection());
        let driver = self.driver.clone();
        let mut driver = driver.borrow_mut();
        let outcome = self.request(&mut driver, cx, ordinal, d);
        // What this request's turn finished, its own answer among them maybe.
        driver.service();
        for (pending, answer) in driver.take_ready() {
            cx.complete(pending, answer);
        }
        outcome.unwrap_or(Outcome::Status(Status::BadMessage))
    }

    fn closed(&mut self) {
        self.registrations.clear();
        let Ok(mut driver) = self.driver.try_borrow_mut() else {
            return;
        };
        if let Some(connection) = self.connection.take() {
            driver.disks[self.disk].closed.push(connection);
        }
        driver.advance_leases();
        driver.release_idle();
    }
}
