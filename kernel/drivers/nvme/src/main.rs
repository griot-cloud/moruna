//! The NVMe driver (SDD 25, f.15): serves `moruna.driver.block` on the listener its
//! supervisor gave it, for the first active namespace of the one NVMe controller whose
//! windows it holds. It is the virtio-blk driver's shape over `moruna-nvme` instead
//! of virtio, so every client of the block protocol (the image filesystem, the
//! engine's spill) works on NVMe unchanged.
//!
//! It resets the controller before it lets it master the bus and before it releases
//! its DMA resource's quarantine, so the controller uses no memory a previous driver
//! gave it; then it brings up one I/O queue pair per CPU, as many as the controller
//! grants and the driver has interrupts for, each completion queue on an MSI-X entry
//! of its own (or shared, round robin, when there are fewer).
//!
//! A read or a write pins the client's memory object and hands its pages to the
//! controller as PRP entries, so data moves between the namespace and the client's
//! memory with no copy; a registered buffer (protocol 1.1) is pinned once. A command
//! the controller fails is answered `Io`, and its status printed. A command past its
//! deadline, a completion that does not check, or the controller's fatal status reset
//! the controller: every command it held is answered `Io`, the queues come up again,
//! and waiting requests go on. A controller that needs more than three resets, or
//! cannot be brought back, ends the driver, whose connections close; the device
//! manager starts it again. When the last client has gone and nothing is in flight
//! the driver deletes its queues and shuts the controller down, so its volatile write
//! cache reaches the media.

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
use moruna_mmio::TimerClock;
use moruna_nvme::{
    Engine, Io, IoOp, NvmeError, Settings, Status,
    engine::{Dma, Overdue},
    identify,
};
use moruna_pci::{command, dma::DmaBuffer, function::BarWindow, function::Function};
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Handle,
    Handler, Interrupt, Memory, Outcome, Pending, Pin, ProtocolError, ProtocolId, Signals,
    Status as SvcStatus, Watch, Wire, dma_release, now, println, start,
};

moruna_svc::main!(run);

/// How long a command may take before the controller is reset.
const TIMEOUT_NS: u64 = 30_000_000_000;
/// Resets the driver makes before it gives up on the controller.
const RECOVERIES_MAX: u32 = 3;
/// Failed commands printed one by one; the rest are counted.
const ERROR_LINES: u64 = 16;
/// The most interrupts the driver uses, and so the most I/O queues it creates.
const VECTORS_MAX: u64 = 16;
/// PCI class, subclass and interface of an NVM Express controller.
const CLASS_NVME: (u8, u8, u8) = (0x01, 0x08, 0x02);

struct Pages<'a>(&'a Handle);

impl Dma for Pages<'_> {
    type Region = DmaBuffer;
    fn page(&mut self) -> Result<(DmaBuffer, u64), NvmeError> {
        let buffer = DmaBuffer::new(self.0, 4096).map_err(NvmeError::Unsupported)?;
        let addr = buffer.addr(0);
        Ok((buffer, addr))
    }
}

/// A client's buffer, pinned once for every transfer into or out of it.
struct Registration {
    _pin: Pin,
    pages: Vec<u64>,
}

/// What a command keeps alive until the controller returns it.
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
struct Tag {
    hold: Hold,
    pending: Option<Pending>,
    lease: Option<LeaseTag>,
}

struct Nvme {
    name: &'static str,
    out: Rc<Console>,
    engine: Engine<BarWindow, DmaBuffer, Tag>,
    clock: TimerClock,
    function: Function,
    dma: Handle,
    waiting: VecDeque<Io<Tag>>,
    /// Answers ready to send, sent by the main loop after each turn.
    ready: Vec<(Pending, Outcome)>,
    info: Info,
    write_cache: bool,
    writer: lease::Lease,
    release_pending: Option<Pending>,
    closed: Rc<RefCell<Vec<u64>>>,
    served: u64,
    errors: u64,
    recoveries: u32,
    /// Why the controller can no longer be used; the driver ends with it.
    failed: Option<String>,
}

impl Nvme {
    fn reset_done(function: &Function, dma: &Handle) -> Result<(), NvmeError> {
        dma_release(dma)
            .map_err(|e| NvmeError::Unsupported(format!("dma_release: {}", e.name())))?;
        function.enable_bus_master().map_err(NvmeError::Unsupported)
    }

    /// Refuses a transfer the namespace cannot make: a count of zero or above the
    /// largest, blocks past the end, a write to a read-only namespace.
    fn check(&self, write: bool, sector: u64, count: u32) -> Result<(), block::Error> {
        if count == 0 || count > self.info.max_sectors {
            return Err(block::Error::BadCount);
        }
        if sector
            .checked_add(count as u64)
            .is_none_or(|end| end > self.info.sectors)
        {
            return Err(block::Error::OutOfRange);
        }
        if write && self.info.read_only {
            return Err(block::Error::ReadOnly);
        }
        Ok(())
    }

    /// Hands the controller as many waiting requests as its queues take.
    fn submit_waiting(&mut self) {
        while self.failed.is_none() && self.engine.has_room() {
            let Some(io) = self.waiting.pop_front() else {
                break;
            };
            match self.engine.submit(io, now()) {
                Ok(()) => {}
                Err((NvmeError::Full, io)) => {
                    self.waiting.push_front(io);
                    break;
                }
                Err((e, io)) => {
                    println!(
                        self.out,
                        "KE1 {}: a request refused at submission: {}", self.name, e
                    );
                    self.reject_tag(io.tag);
                }
            }
        }
    }

    fn queue(&mut self, io: Io<Tag>) -> Outcome {
        self.waiting.push_back(io);
        self.submit_waiting();
        Outcome::Deferred
    }

    fn transfer(
        &mut self,
        cx: &mut Context<'_>,
        write: bool,
        token: Option<u64>,
        sector: u64,
        count: u32,
        buffer: Memory,
    ) -> Outcome {
        let fail = |e: block::Error| Outcome::Failure(e.code());
        if write && let Err(error) = self.writer.authorize(cx.connection(), token.unwrap_or(0)) {
            return fail(error);
        }
        if let Err(e) = self.check(write, sector, count) {
            return fail(e);
        }
        if !write && let Err(error) = self.writer.can_read(cx.connection()) {
            return Outcome::Failure(error.code());
        }
        let bytes = block_protocol::bytes(&self.info, count);
        let Ok(n) = Pin::pages(&self.dma, &buffer.0, !write) else {
            return fail(block::Error::BadBuffer);
        };
        if (n as u64) * 4096 < bytes {
            return fail(block::Error::BadBuffer);
        }
        let mut pages = vec![0u64; n];
        let Ok((pin, _)) = Pin::create(&self.dma, &buffer.0, !write, &mut pages) else {
            return fail(block::Error::BadBuffer);
        };
        pages.truncate(bytes.div_ceil(4096) as usize);
        let lease = if write {
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
        let io = Io {
            op: if write { IoOp::Write } else { IoOp::Read },
            lba: sector,
            blocks: count,
            pages,
            tag: Tag {
                hold: Hold::Buffer(buffer, pin),
                pending: Some(cx.defer()),
                lease,
            },
        };
        self.queue(io)
    }

    fn register(&mut self, buffer: &Memory) -> Result<Registration, block::Error> {
        let n = Pin::pages(&self.dma, &buffer.0, true).map_err(|_| block::Error::BadBuffer)?;
        let mut pages = vec![0u64; n];
        let (pin, _) = Pin::create(&self.dma, &buffer.0, true, &mut pages)
            .map_err(|_| block::Error::BadBuffer)?;
        Ok(Registration { _pin: pin, pages })
    }

    #[allow(clippy::too_many_arguments)]
    fn transfer_at(
        &mut self,
        cx: &mut Context<'_>,
        write: bool,
        token: Option<u64>,
        registration: Rc<Registration>,
        page: u32,
        sector: u64,
        count: u32,
    ) -> Outcome {
        if write && let Err(error) = self.writer.authorize(cx.connection(), token.unwrap_or(0)) {
            return Outcome::Failure(error.code());
        }
        if let Err(e) = self.check(write, sector, count) {
            return Outcome::Failure(e.code());
        }
        if !write && let Err(error) = self.writer.can_read(cx.connection()) {
            return Outcome::Failure(error.code());
        }
        let bytes = block_protocol::bytes(&self.info, count);
        let first = page as usize;
        let n = bytes.div_ceil(4096) as usize;
        let Some(pages) = first
            .checked_add(n)
            .and_then(|end| registration.pages.get(first..end))
        else {
            return Outcome::Failure(block::Error::BadBuffer.code());
        };
        let lease = if write {
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
        let io = Io {
            op: if write { IoOp::Write } else { IoOp::Read },
            lba: sector,
            blocks: count,
            pages: pages.to_vec(),
            tag: Tag {
                hold: Hold::Registered(registration),
                pending: Some(cx.defer()),
                lease,
            },
        };
        self.queue(io)
    }

    fn flush(&mut self, cx: &mut Context<'_>, token: u64) -> Outcome {
        if let Err(error) = self.writer.authorize(cx.connection(), token) {
            return Outcome::Failure(error.code());
        }
        if !self.write_cache {
            // A controller without a volatile write cache has nothing to flush.
            return Outcome::reply(());
        }
        let lease = match self.writer.flush(cx.connection(), token) {
            Ok(io) => Some(LeaseTag::Original(io)),
            Err(error) => return Outcome::Failure(error.code()),
        };
        let io = Io {
            op: IoOp::Flush,
            lba: 0,
            blocks: 0,
            pages: Vec::new(),
            tag: Tag {
                hold: Hold::None,
                pending: Some(cx.defer()),
                lease,
            },
        };
        self.queue(io)
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
    fn reject_tag(&mut self, mut tag: Tag) {
        self.finish_lease(tag.lease.take(), false);
        if let Some(pending) = tag.pending {
            self.ready
                .push((pending, Outcome::Failure(block::Error::Io.code())));
        }
        self.advance_lease();
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
            if !release.needs_flush() || !self.write_cache {
                self.finish_lease(Some(LeaseTag::Release(release)), true);
            } else {
                let io = Io {
                    op: IoOp::Flush,
                    lba: 0,
                    blocks: 0,
                    pages: Vec::new(),
                    tag: Tag {
                        hold: Hold::None,
                        pending: None,
                        lease: Some(LeaseTag::Release(release)),
                    },
                };
                self.queue(io);
            }
        }
    }
    fn answer(&mut self, mut tag: Tag, result: Result<(), Status>) {
        self.finish_lease(tag.lease.take(), result.is_ok());
        let outcome = match result {
            Ok(()) => match tag.hold {
                // The pin goes to the quarantine; the memory goes back.
                Hold::Buffer(memory, pin) => {
                    drop(pin);
                    Outcome::reply(memory)
                }
                // The registration's pin goes to the quarantine only if it was ended
                // while this transfer was in flight.
                Hold::Registered(registration) => {
                    drop(registration);
                    Outcome::reply(())
                }
                Hold::None => Outcome::reply(()),
            },
            Err(status) => {
                self.errors += 1;
                if self.errors <= ERROR_LINES {
                    println!(self.out, "KE1 {}: a command failed: {}", self.name, status);
                }
                Outcome::Failure(block::Error::Io.code())
            }
        };
        if let Some(pending) = tag.pending {
            self.ready.push((pending, outcome));
        }
        self.served += 1;
        self.advance_lease();
    }

    /// Answers every command the queues of `vector` have completed; resets the
    /// controller if a completion does not check or it reports a fatal status.
    fn interrupt(&mut self, vector: u16) {
        let mut done = Vec::new();
        let taken = self.engine.complete(Some(vector), &mut done);
        let finished = !done.is_empty();
        for (tag, result) in done {
            self.answer(tag, result);
        }
        if let Err(e) = taken {
            self.recover(&format!("{e}"));
            return;
        }
        if self.engine.fatal().unwrap_or(true) {
            self.recover("the controller reports a fatal status (CSTS.CFS)");
            return;
        }
        if finished {
            self.release_quarantine();
        }
        self.submit_waiting();
    }

    /// Releases the quarantine: every pin in it is one the controller has finished
    /// with (pins of commands in flight are still open).
    fn release_quarantine(&mut self) {
        if let Err(e) = dma_release(&self.dma) {
            self.failed
                .get_or_insert(format!("dma_release: {}", e.name()));
        }
    }

    /// Releases the quarantine when no command is in flight, as for a registration
    /// just ended.
    fn release_idle(&mut self) {
        if self.engine.in_flight() == 0 {
            self.release_quarantine();
        }
    }

    /// Checks deadlines: a command past its own resets the controller.
    fn tick(&mut self) {
        if let Some(Overdue {
            queue,
            cid,
            op,
            lba,
            late_ns,
        }) = self.engine.overdue(now())
        {
            self.recover(&format!(
                "{op:?} at block {lba} (queue {queue}, command {cid}) is {} ms past its {} s deadline",
                late_ns / 1_000_000,
                TIMEOUT_NS / 1_000_000_000
            ));
        }
    }

    /// Resets the controller and answers every command it held `Io`.
    fn recover(&mut self, why: &str) {
        self.recoveries += 1;
        println!(
            self.out,
            "KE1 {}: resetting the controller ({} of {}): {}",
            self.name,
            self.recoveries,
            RECOVERIES_MAX,
            why
        );
        if self.recoveries > RECOVERIES_MAX {
            self.failed.get_or_insert(format!(
                "the controller needed more than {RECOVERIES_MAX} resets; the last: {why}"
            ));
            return;
        }
        let (function, dma) = (&self.function, &self.dma);
        match self
            .engine
            .recover(&self.clock, &mut || Nvme::reset_done(function, dma))
        {
            Ok(failed) => {
                let n = failed.len();
                for tag in failed {
                    self.reject_tag(tag);
                }
                println!(
                    self.out,
                    "KE1 {}: the controller is back; {} commands it held were answered Io",
                    self.name,
                    n
                );
                self.submit_waiting();
            }
            Err(e) => {
                self.failed
                    .get_or_insert(format!("the controller could not be reset: {e}"));
            }
        }
    }
}

struct Connection {
    nvme: Rc<RefCell<Nvme>>,
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
        let nvme = self.nvme.clone();
        let mut n = nvme.borrow_mut();
        let bad = Outcome::Status(SvcStatus::BadMessage);
        match ordinal {
            1 => match d.finish() {
                Ok(()) => Outcome::reply(n.info),
                Err(_) => bad,
            },
            2 | 3 => {
                let token = if ordinal == 3 {
                    let Some(token) = decode::<u64>(d) else {
                        return bad;
                    };
                    Some(token)
                } else {
                    None
                };
                let (Some(sector), Some(count), Some(buffer)) =
                    (decode::<u64>(d), decode::<u32>(d), decode::<Memory>(d))
                else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                n.transfer(cx, ordinal == 3, token, sector, count, buffer)
            }
            4 => {
                let Some(token) = decode::<u64>(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                n.flush(cx, token)
            }
            5 => {
                let Some(buffer) = decode::<Memory>(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                if self.registrations.len() >= block_protocol::REGISTRATIONS_MAX {
                    return Outcome::Failure(block::Error::TooManyRegistrations.code());
                }
                match n.register(&buffer) {
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
                        return bad;
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
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                let Some(registration) = self.registrations.get(&id).cloned() else {
                    return Outcome::Failure(block::Error::NoRegistration.code());
                };
                n.transfer_at(cx, ordinal == 7, token, registration, page, sector, count)
            }
            8 => {
                let Some(id) = decode::<u32>(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                match self.registrations.remove(&id) {
                    Some(registration) => {
                        drop(registration);
                        n.release_idle();
                        Outcome::reply(())
                    }
                    None => Outcome::Failure(block::Error::NoRegistration.code()),
                }
            }
            9 => {
                if d.finish().is_err() {
                    return bad;
                }
                if n.info.read_only {
                    return Outcome::Failure(block::Error::ReadOnly.code());
                }
                match n.writer.claim(cx.connection()) {
                    Ok(token) => Outcome::reply(token),
                    Err(error) => Outcome::Failure(error.code()),
                }
            }
            10 => {
                let Some(token) = decode::<u64>(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                if let Err(error) = n.writer.close(cx.connection(), token) {
                    return Outcome::Failure(error.code());
                }
                n.release_pending = Some(cx.defer());
                n.advance_lease();
                Outcome::Deferred
            }
            _ => Outcome::Status(SvcStatus::UnknownMethod),
        }
    }

    fn closed(&mut self) {
        self.registrations.clear();
        if let Some(connection) = self.connection.take() {
            self.closed.borrow_mut().push(connection);
        }
        if let Ok(mut n) = self.nvme.try_borrow_mut() {
            n.release_idle();
        }
    }
}

struct Irq {
    line: Interrupt,
    vector: u16,
    nvme: Rc<RefCell<Nvme>>,
}

impl Watch for Irq {
    fn handle(&self) -> &Handle {
        &self.line.0
    }
    fn signals(&self) -> Signals {
        Signals::SIGNALED
    }
    fn signaled(&mut self, _: Signals, _: &mut Completions) -> bool {
        let mut n = self.nvme.borrow_mut();
        // Acknowledged before the queues are read, so a completion that lands while
        // they are read raises the line again.
        if let Err(e) = self.line.ack() {
            n.failed
                .get_or_insert(format!("interrupt_ack: {}", e.name()));
            return false;
        }
        n.interrupt(self.vector);
        true
    }
}

/// Opens the controller: its function, its interrupts, and the engine over BAR 0.
fn open(name: &'static str, out: Rc<Console>) -> Result<(Nvme, Vec<(Interrupt, u16)>), String> {
    let mut function = Function::open()?;
    let class = function.config.class().map_err(|e| e.name())?;
    if class != CLASS_NVME {
        return Err(format!(
            "the function's class is {:02x}:{:02x}:{:02x}, not an NVM Express controller (01:08:02)",
            class.0, class.1, class.2
        ));
    }
    function
        .config
        .enable(command::MEMORY | command::INTX_DISABLE)
        .map_err(|e| format!("command register: {}", e.name()))?;
    let msix = function
        .config
        .msix()
        .map_err(|e| format!("MSI-X capability: {}", e.name()))?
        .ok_or("the controller has no MSI-X, and Moruna has no line-based interrupts for it")?;
    let cpus = start::cpus().max(1) as u64;
    let vectors = function
        .interrupts()
        .min(msix.entries as u64)
        .min(cpus)
        .min(VECTORS_MAX);
    if vectors == 0 {
        return Err(String::from("the supervisor gave no MSI slot"));
    }
    let mut lines = Vec::new();
    {
        let table = function
            .bar(msix.table_bar)
            .ok_or_else(|| format!("no window for the MSI-X table's bar{}", msix.table_bar))?;
        for v in 0..vectors {
            let (line, message) = function.interrupt(v)?;
            msix.program(table, v as u16, message.address, message.data)
                .map_err(|e| format!("MSI-X table entry {v}: {}", e.name()))?;
            lines.push((line, v as u16));
        }
        msix.enable(&function.config)
            .map_err(|e| format!("MSI-X enable: {}", e.name()))?;
    }
    let dma = function.take_dma()?;
    let bar = function.take_bar(0).ok_or("no window for BAR 0")?;
    let clock = TimerClock::new().map_err(|e| format!("timer_create: {}", e.name()))?;
    let settings = Settings {
        queues: cpus as u16,
        vectors: vectors as u16,
        timeout_ns: TIMEOUT_NS,
    };
    let engine = Engine::start(bar, &mut Pages(&dma), &clock, settings, &mut || {
        Nvme::reset_done(&function, &dma)
    })
    .map_err(|e| format!("{e}"))?;
    let block = identify::block_bytes(&engine.ns);
    let pages = engine.id.max_pages;
    let info = Info {
        sector_size: block,
        sectors: engine.ns.blocks,
        max_sectors: (pages * 4096 / block).max(1),
        read_only: engine.ns.read_only,
    };
    let write_cache = engine.id.write_cache;
    // The interrupts no I/O queue uses are let go.
    lines.truncate(engine.vectors_used() as usize);
    Ok((
        Nvme {
            name,
            out,
            engine,
            clock,
            function,
            dma,
            waiting: VecDeque::new(),
            ready: Vec::new(),
            info,
            write_cache,
            writer: lease::Lease::default(),
            release_pending: None,
            closed: Rc::new(RefCell::new(Vec::new())),
            served: 0,
            errors: 0,
            recoveries: 0,
            failed: None,
        },
        lines,
    ))
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
    // The driver and its loop print through one console: a driver's console has no
    // right to be duplicated.
    let out = Rc::new(out);
    let (nvme, lines) = match open(name, out.clone()) {
        Ok(n) => n,
        Err(e) => {
            println!(out, "KE1 {}: FAIL: {}", name, e);
            return 1;
        }
    };
    {
        let e = &nvme.engine;
        let (major, minor, _) = e.ctrl.version().unwrap_or((0, 0, 0));
        println!(
            out,
            "KE1 {}: nvme up: {} (serial {}, firmware {}), NVMe {}.{}; namespace {} of {} active: {} blocks of {} bytes, {} blocks a transfer{}; {} I/O queue pairs over {} interrupts for {} CPUs; {}",
            name,
            e.id.model,
            e.id.serial,
            e.id.firmware,
            major,
            minor,
            e.nsid,
            e.namespaces.len(),
            nvme.info.sectors,
            nvme.info.sector_size,
            nvme.info.max_sectors,
            if nvme.info.read_only {
                ", read only"
            } else {
                ""
            },
            e.queues(),
            lines.len(),
            start::cpus(),
            if nvme.write_cache {
                "a volatile write cache, which flush writes out"
            } else {
                "no volatile write cache"
            }
        );
    }
    let nvme = Rc::new(RefCell::new(nvme));
    let mut dispatcher = Dispatcher::new();
    let shared = nvme.clone();
    let closed = nvme.borrow().closed.clone();
    let factory: moruna_svc::Factory = Box::new(move || {
        Box::new(Connection {
            nvme: shared.clone(),
            registrations: BTreeMap::new(),
            next: 1,
            connection: None,
            closed: closed.clone(),
        }) as Box<dyn Handler>
    });
    if let Err(e) = dispatcher.listen(listener, factory) {
        println!(out, "KE1 {}: FAIL: dispatcher: {}", name, e.name());
        return 1;
    }
    for (line, vector) in lines {
        let watched = dispatcher.watch(Box::new(Irq {
            line,
            vector,
            nvme: nvme.clone(),
        }));
        if let Err(e) = watched {
            println!(out, "KE1 {}: FAIL: dispatcher: {}", name, e.name());
            return 1;
        }
    }
    loop {
        let (busy, deadline) = {
            let mut n = nvme.borrow_mut();
            n.advance_lease();
            if let Some(e) = n.failed.take() {
                println!(out, "KE1 {}: FAIL: the controller: {}", name, e);
                return 1;
            }
            for (pending, outcome) in core::mem::take(&mut n.ready) {
                // An answer whose client has gone is dropped with what it carries.
                let _ = dispatcher.complete(pending, outcome);
            }
            let busy = n.engine.in_flight() > 0 || !n.waiting.is_empty();
            (busy, n.engine.next_deadline().unwrap_or(DEADLINE_INFINITE))
        };
        if dispatcher.channels() == 0 && !busy {
            break;
        }
        if let Err(e) = dispatcher.turn(deadline) {
            println!(out, "KE1 {}: FAIL: wait: {}", name, e.name());
            return 1;
        }
        nvme.borrow_mut().tick();
    }
    let mut n = nvme.borrow_mut();
    let n = &mut *n;
    if let Err(e) = n.engine.shutdown(&n.clock) {
        println!(
            out,
            "KE1 {}: FAIL: shutting the controller down: {}", name, e
        );
        return 1;
    }
    println!(
        out,
        "KE1 {}: served {} commands ({} failed, {} controller resets) until every client left; the controller is shut down",
        name,
        n.served,
        n.errors,
        n.recoveries
    );
    0
}
