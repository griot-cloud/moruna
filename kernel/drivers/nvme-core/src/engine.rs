//! A controller brought up and driven (SDD 25, f.15): everything a block driver does
//! with an NVMe controller that is not a kernel call, so a host test drives it against
//! a model controller as the driver drives it against a device.
//!
//! - [`Engine::start`] resets the controller, lets the driver release what a previous
//!   driver left (the caller's `reset_done`, which calls `dma_release` and turns bus
//!   mastering on), gives it an admin queue pair, enables it, reads Identify for the
//!   controller, the active namespaces and the first of them, asks for one I/O queue
//!   pair per CPU (no more than the driver has interrupts for and the controller
//!   grants), and creates them, queue `q` interrupting through MSI-X entry
//!   `(q - 1) % vectors`.
//! - [`Engine::submit`] puts a read, a write or a flush on the I/O queue with the fewest
//!   commands in flight, its pages as PRP entries (a list in a page of the queue's for
//!   three or more), with a deadline.
//! - [`Engine::complete`] takes the completions of one interrupt's queues, each checked
//!   (`queue`), and hands back each command's tag with its status.
//! - [`Engine::overdue`] names a command past its deadline; [`Engine::fatal`] reads the
//!   controller's fatal status; on either the driver calls [`Engine::recover`], which
//!   resets the controller (after which it uses no memory it was given), hands back
//!   every command in flight to be failed, and brings the queues up again.
//! - [`Engine::shutdown`] deletes the I/O queues and shuts the controller down, so its
//!   volatile cache reaches the media.

use alloc::{format, string::String, vec::Vec};

use moruna_mmio::{Clock, Region};

use crate::{
    Command, Completion, NvmeError, PAGE, QUEUE_ENTRIES, QueuePair, Status,
    command::cns,
    controller::Controller,
    identify::{self, ControllerId, NamespaceId},
    prp,
};

/// Memory a controller reads and writes: zeroed pages, mapped, with device addresses.
pub trait Dma {
    type Region: Region;
    /// One zeroed page and its device address.
    fn page(&mut self) -> Result<(Self::Region, u64), NvmeError>;
}

/// What the driver asks of the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// I/O queue pairs wanted: one per CPU.
    pub queues: u16,
    /// MSI-X entries the driver has programmed, from entry 0.
    pub vectors: u16,
    /// How long a command may take before the controller is reset.
    pub timeout_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoOp {
    Read,
    Write,
    Flush,
}

/// A transfer or a flush, and the driver's tag for it.
pub struct Io<T> {
    pub op: IoOp,
    pub lba: u64,
    pub blocks: u32,
    /// The device address of each page the transfer moves, in order.
    pub pages: Vec<u64>,
    pub tag: T,
}

struct Slot<T> {
    tag: T,
    op: IoOp,
    lba: u64,
    deadline: u64,
}

struct IoQueue<M: Region, T> {
    pair: QueuePair<M>,
    sq_bell: usize,
    cq_bell: usize,
    vector: u16,
    /// One PRP list page for each command identifier.
    lists: Vec<(M, u64)>,
    slots: Vec<Option<Slot<T>>>,
    sq_addr: u64,
    cq_addr: u64,
}

/// A command past its deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overdue {
    pub queue: u16,
    pub cid: u16,
    pub op: IoOp,
    pub lba: u64,
    pub late_ns: u64,
}

pub struct Engine<R: Region, M: Region, T> {
    pub ctrl: Controller<R>,
    admin: QueuePair<M>,
    admin_addrs: (u64, u64),
    admin_bells: (usize, usize),
    /// The page Identify writes into.
    scratch: (M, u64),
    queues: Vec<IoQueue<M, T>>,
    pub id: ControllerId,
    /// Every active namespace; the driver serves the first.
    pub namespaces: Vec<u32>,
    pub nsid: u32,
    pub ns: NamespaceId,
    settings: Settings,
    /// Each I/O queue's MSI-X entry, as created.
    vectors: u16,
}

/// The most I/O queue pairs an engine creates.
pub const MAX_QUEUES: u16 = 16;

impl<R: Region, M: Region, T> Engine<R, M, T> {
    /// Brings the controller on `bar` up (see the module's description).
    pub fn start<D: Dma<Region = M>>(
        bar: R,
        dma: &mut D,
        clock: &dyn Clock,
        settings: Settings,
        reset_done: &mut dyn FnMut() -> Result<(), NvmeError>,
    ) -> Result<Engine<R, M, T>, NvmeError> {
        if settings.vectors == 0 || settings.queues == 0 {
            return Err(NvmeError::Unsupported(format!(
                "{} I/O queues over {} interrupts",
                settings.queues, settings.vectors
            )));
        }
        let ctrl = Controller::new(bar)?;
        ctrl.disable(clock)?;
        reset_done()?;
        let size = QUEUE_ENTRIES.min(ctrl.cap.max_entries.min(u16::MAX as u32) as u16);
        let (sq, sq_addr) = dma.page()?;
        let (cq, cq_addr) = dma.page()?;
        let admin = QueuePair::new(0, size, sq, cq)?;
        let admin_bells = (ctrl.doorbell(0, false)?, ctrl.doorbell(0, true)?);
        ctrl.enable(clock, sq_addr, cq_addr, size)?;
        let scratch = dma.page()?;
        let mut e = Engine {
            ctrl,
            admin,
            admin_addrs: (sq_addr, cq_addr),
            admin_bells,
            scratch,
            queues: Vec::new(),
            id: ControllerId {
                vendor: 0,
                serial: String::new(),
                model: String::new(),
                firmware: String::new(),
                max_pages: 1,
                write_cache: false,
                namespaces: 0,
            },
            namespaces: Vec::new(),
            nsid: 0,
            ns: NamespaceId {
                blocks: 0,
                block_shift: 9,
                read_only: true,
            },
            settings,
            vectors: settings.vectors,
        };
        let page = e.identify(clock, cns::CONTROLLER, 0, "Identify Controller")?;
        e.id = ControllerId::parse(&page)?;
        let page = e.identify(
            clock,
            cns::ACTIVE_NAMESPACES,
            0,
            "Identify active namespaces",
        )?;
        e.namespaces = identify::active_namespaces(&page, e.id.namespaces)?;
        e.nsid = *e
            .namespaces
            .first()
            .ok_or_else(|| NvmeError::Unsupported(String::from("no active namespace")))?;
        let page = e.identify(clock, cns::NAMESPACE, e.nsid, "Identify Namespace")?;
        e.ns = NamespaceId::parse(&page)?;
        let wanted = settings.queues.min(MAX_QUEUES);
        let c = e.admin(
            clock,
            &Command::set_queue_count(wanted),
            "Set Features (queues)",
        )?;
        // Both counts are zero based; the controller may grant more than asked.
        let granted = ((c.dw0 & 0xffff).min(c.dw0 >> 16) + 1).min(wanted as u32) as u16;
        for q in 1..=granted {
            let (sq, sq_addr) = dma.page()?;
            let (cq, cq_addr) = dma.page()?;
            let pair = QueuePair::new(q, size, sq, cq)?;
            let mut lists = Vec::with_capacity(size as usize);
            for _ in 0..size {
                lists.push(dma.page()?);
            }
            let mut slots = Vec::with_capacity(size as usize);
            slots.resize_with(size as usize, || None);
            e.queues.push(IoQueue {
                pair,
                sq_bell: e.ctrl.doorbell(q, false)?,
                cq_bell: e.ctrl.doorbell(q, true)?,
                vector: (q - 1) % settings.vectors,
                lists,
                slots,
                sq_addr,
                cq_addr,
            });
        }
        e.create_queues(clock)?;
        Ok(e)
    }

    fn create_queues(&mut self, clock: &dyn Clock) -> Result<(), NvmeError> {
        for i in 0..self.queues.len() {
            let (q, size, cq_addr, sq_addr, vector) = {
                let io = &self.queues[i];
                (
                    io.pair.id,
                    io.pair.size(),
                    io.cq_addr,
                    io.sq_addr,
                    io.vector,
                )
            };
            self.admin(
                clock,
                &Command::create_cq(q, size, cq_addr, vector),
                "Create I/O Completion Queue",
            )?;
            self.admin(
                clock,
                &Command::create_sq(q, size, sq_addr, q),
                "Create I/O Submission Queue",
            )?;
        }
        Ok(())
    }

    /// Runs an admin command and waits for its completion.
    pub fn admin(
        &mut self,
        clock: &dyn Clock,
        command: &Command,
        what: &'static str,
    ) -> Result<Completion, NvmeError> {
        let (cid, tail) = self.admin.submit(command)?;
        self.ctrl.ring(self.admin_bells.0, tail)?;
        let deadline = clock.now().saturating_add(self.settings.timeout_ns);
        loop {
            if let Some(c) = self.admin.pop()? {
                self.ctrl.ring(self.admin_bells.1, self.admin.cq_head())?;
                if c.cid != cid {
                    return Err(NvmeError::Malformed(format!(
                        "the completion of admin command {} while {cid} ({what}) was the one in flight",
                        c.cid
                    )));
                }
                if !c.status.ok() {
                    return Err(NvmeError::Command(what, c.status));
                }
                return Ok(c);
            }
            if self.ctrl.fatal()? {
                return Err(NvmeError::Fatal);
            }
            if clock.now() >= deadline {
                return Err(NvmeError::Timeout(what));
            }
            clock.pause();
        }
    }

    fn identify(
        &mut self,
        clock: &dyn Clock,
        cns: u32,
        nsid: u32,
        what: &'static str,
    ) -> Result<Vec<u8>, NvmeError> {
        for at in (0..PAGE).step_by(8) {
            self.scratch.0.write64(at, 0)?;
        }
        self.admin(clock, &Command::identify(cns, nsid, self.scratch.1), what)?;
        let mut page = alloc::vec![0u8; PAGE];
        self.scratch.0.read_bytes(0, &mut page)?;
        Ok(page)
    }

    /// The I/O queue pairs created.
    pub fn queues(&self) -> usize {
        self.queues.len()
    }

    /// The MSI-X entries the I/O queues interrupt through: `0 .. vectors_used`.
    pub fn vectors_used(&self) -> u16 {
        (self.queues.len() as u16).min(self.vectors)
    }

    /// Commands in flight on every I/O queue.
    pub fn in_flight(&self) -> usize {
        self.queues.iter().map(|q| q.pair.in_flight()).sum()
    }

    /// Whether some I/O queue can take a command.
    pub fn has_room(&self) -> bool {
        self.queues.iter().any(|q| q.pair.has_room())
    }

    /// Submits `io`, due by `now` plus the command timeout. `Full` hands it back.
    pub fn submit(&mut self, io: Io<T>, now: u64) -> Result<(), (NvmeError, Io<T>)> {
        let block = identify::block_bytes(&self.ns) as u64;
        if io.op != IoOp::Flush {
            let bytes = io.blocks as u64 * block;
            let fits = io.blocks >= 1
                && io.blocks <= 65536
                && (io.pages.len() as u64) * PAGE as u64 >= bytes
                && io.pages.len() as u64 == bytes.div_ceil(PAGE as u64)
                && io
                    .lba
                    .checked_add(io.blocks as u64)
                    .is_some_and(|end| end <= self.ns.blocks);
            if !fits {
                let why = format!(
                    "a transfer of {} blocks at {} over {} pages",
                    io.blocks,
                    io.lba,
                    io.pages.len()
                );
                return Err((NvmeError::Unsupported(why), io));
            }
        }
        let Some(index) = (0..self.queues.len())
            .filter(|i| self.queues[*i].pair.has_room())
            .min_by_key(|i| self.queues[*i].pair.in_flight())
        else {
            return Err((NvmeError::Full, io));
        };
        let nsid = self.nsid;
        let q = &mut self.queues[index];
        let Some(cid) = q.pair.next_cid() else {
            return Err((NvmeError::Full, io));
        };
        let command = match io.op {
            IoOp::Flush => Command::flush(nsid),
            op => {
                let (list, list_addr) = &q.lists[cid as usize];
                let entries = match prp::entries(&io.pages, list, *list_addr) {
                    Ok(e) => e,
                    Err(e) => return Err((e, io)),
                };
                Command::transfer(op == IoOp::Write, nsid, io.lba, io.blocks, entries)
            }
        };
        let tail = match q.pair.submit(&command) {
            Ok((given, tail)) if given == cid => tail,
            Ok((given, _)) => {
                let why = format!("queue {} gave command {given}, not {cid}", q.pair.id);
                return Err((NvmeError::Malformed(why), io));
            }
            Err(e) => return Err((e, io)),
        };
        q.slots[cid as usize] = Some(Slot {
            tag: io.tag,
            op: io.op,
            lba: io.lba,
            deadline: now.saturating_add(self.settings.timeout_ns),
        });
        // A doorbell that cannot be written leaves the command on the queue: the
        // deadline then resets the controller.
        let _ = self.ctrl.ring(q.sq_bell, tail);
        Ok(())
    }

    /// Takes the completions of the I/O queues that interrupt through `vector` (every
    /// queue for `None`), handing each command's tag back with its status.
    pub fn complete(
        &mut self,
        vector: Option<u16>,
        out: &mut Vec<(T, Result<(), Status>)>,
    ) -> Result<(), NvmeError> {
        for q in self
            .queues
            .iter_mut()
            .filter(|q| vector.is_none_or(|v| q.vector == v))
        {
            let mut took = false;
            while let Some(c) = q.pair.pop()? {
                took = true;
                let slot = q.slots[c.cid as usize].take().ok_or_else(|| {
                    NvmeError::Malformed(format!(
                        "a completion on queue {} for command {}, whose slot is empty",
                        q.pair.id, c.cid
                    ))
                })?;
                let result = if c.status.ok() { Ok(()) } else { Err(c.status) };
                out.push((slot.tag, result));
            }
            if took {
                self.ctrl.ring(q.cq_bell, q.pair.cq_head())?;
            }
        }
        Ok(())
    }

    /// The earliest deadline of a command in flight.
    pub fn next_deadline(&self) -> Option<u64> {
        self.queues
            .iter()
            .flat_map(|q| q.slots.iter().flatten())
            .map(|s| s.deadline)
            .min()
    }

    /// The command furthest past its deadline at `now`, if any is.
    pub fn overdue(&self, now: u64) -> Option<Overdue> {
        let mut worst: Option<Overdue> = None;
        for q in &self.queues {
            for (cid, slot) in q.slots.iter().enumerate() {
                let Some(s) = slot else { continue };
                if s.deadline > now {
                    continue;
                }
                let late = now - s.deadline;
                if worst.as_ref().is_none_or(|w| late > w.late_ns) {
                    worst = Some(Overdue {
                        queue: q.pair.id,
                        cid: cid as u16,
                        op: s.op,
                        lba: s.lba,
                        late_ns: late,
                    });
                }
            }
        }
        worst
    }

    /// Whether the controller reports a fatal status.
    pub fn fatal(&self) -> Result<bool, NvmeError> {
        self.ctrl.fatal()
    }

    /// Resets the controller, hands back the tag of every command it held, and brings
    /// the admin and I/O queues up again in the same memory. `reset_done` runs once the
    /// controller is reset, when no memory given to it is in use.
    pub fn recover(
        &mut self,
        clock: &dyn Clock,
        reset_done: &mut dyn FnMut() -> Result<(), NvmeError>,
    ) -> Result<Vec<T>, NvmeError> {
        self.ctrl.disable(clock)?;
        let mut failed = Vec::new();
        for q in &mut self.queues {
            failed.extend(q.slots.iter_mut().filter_map(Option::take).map(|s| s.tag));
            q.pair.reset()?;
        }
        self.admin.reset()?;
        reset_done()?;
        self.ctrl.enable(
            clock,
            self.admin_addrs.0,
            self.admin_addrs.1,
            self.admin.size(),
        )?;
        let wanted = self.queues.len() as u16;
        let c = self.admin(
            clock,
            &Command::set_queue_count(wanted),
            "Set Features (queues)",
        )?;
        let granted = ((c.dw0 & 0xffff).min(c.dw0 >> 16) + 1) as usize;
        if granted < self.queues.len() {
            return Err(NvmeError::Unsupported(format!(
                "after a reset the controller grants {granted} I/O queues, not {}",
                self.queues.len()
            )));
        }
        self.create_queues(clock)?;
        Ok(failed)
    }

    /// Deletes the I/O queues and shuts the controller down. Commands still in flight
    /// are the caller's to have waited for.
    pub fn shutdown(&mut self, clock: &dyn Clock) -> Result<(), NvmeError> {
        for i in (0..self.queues.len()).rev() {
            let q = self.queues[i].pair.id;
            self.admin(clock, &Command::delete_sq(q), "Delete I/O Submission Queue")?;
            self.admin(clock, &Command::delete_cq(q), "Delete I/O Completion Queue")?;
        }
        self.ctrl.shutdown(clock)
    }
}
