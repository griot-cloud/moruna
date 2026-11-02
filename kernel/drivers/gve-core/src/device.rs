//! A gVNIC brought up and driven in the GQI queue format (SDD 29, f.4): everything a
//! driver does with the device that is not a kernel call, so a host test drives it
//! against a model as the driver drives the device.
//!
//! [`Gve::start`] follows the Linux driver's order: reset whatever a previous driver
//! left (and let the caller release its quarantine and turn bus mastering on), write
//! the driver's version, give the device its admin queue, describe it, configure its
//! counters and notification blocks, register the queue page lists, create the
//! transmit and receive queues, post every receive buffer and unmask the interrupts.
//! Transmit queue `i` notifies through block `i`, receive queue `i` through block
//! `queues + i`, and the management interrupt is the MSI-X entry after the blocks.
//!
//! A frame sent is copied into a slot of its queue's buffers (its queue page list, or
//! pages the driver hands the device raw) and described by one packet descriptor; the
//! device's event counter says how many descriptors it has finished. A frame received
//! is one descriptor, or several with `PKT_CONT`, each naming the buffer of its slot,
//! its data two bytes in on the first; the descriptor's sequence number says it is new.

use alloc::{format, vec, vec::Vec};
use core::sync::atomic::{Ordering, fence};

use moruna_mmio::{Clock, Region};

use crate::{
    Dma, GveError, PAGE,
    admin::{AdminQueue, Command, RAW_ADDRESSING_QPL},
    be16, be32,
    describe::{Description, Format},
    put_be16, put_be32, put_be64, regs,
};

/// What the driver asks of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Queue pairs wanted: one per CPU.
    pub queues: u16,
    /// MSI-X entries the driver has programmed, from entry 0.
    pub vectors: u16,
    /// The function's PCI revision: 0 has the admin queue's page frame register, 1 or
    /// later its address and length registers.
    pub revision: u8,
    /// The largest MTU the driver serves.
    pub mtu_max: u16,
    /// How long the device has for an admin command or a reset.
    pub timeout_ns: u64,
}

/// Bytes of a receive buffer, and of the padding before a packet's first byte.
pub const RX_BUFFER: usize = 2048;
pub const RX_PAD: usize = 2;
/// Bytes of a transmit descriptor and of a receive descriptor.
pub const TX_DESC: usize = 16;
pub const RX_DESC: usize = 64;
/// The most queue pairs an engine creates.
pub const MAX_QUEUES: u16 = 8;
/// Receive descriptor flags, in the host order of `flags_seq`.
pub const RX_ERR: u16 = 1 << 11;
pub const RX_CONT: u16 = 1 << 13;

/// The next receive sequence number: 1 to 7, around.
pub fn next_seqno(s: u8) -> u8 {
    if s >= 7 { 1 } else { s + 1 }
}

struct TxQueue<M: Region> {
    ring: M,
    entries: u32,
    req: u32,
    done: u32,
    buffers: M,
    /// Each buffer page's device address (raw addressing).
    pages: Vec<u64>,
    slot_bytes: usize,
    free: Vec<u32>,
    /// The slot each descriptor in flight carries.
    slot_of: Vec<u32>,
    _resources: M,
    doorbell: usize,
    counter: usize,
    dirty: bool,
}

struct RxQueue<M: Region> {
    descriptors: M,
    _data: M,
    entries: u32,
    count: u32,
    fill: u32,
    seqno: u8,
    buffers: M,
    _resources: M,
    doorbell: usize,
}

pub struct Gve<R: Region, B: Region, M: Region> {
    bar0: R,
    bar2: B,
    admin: AdminQueue<M>,
    pub description: Description,
    pub mtu: u16,
    tx: Vec<TxQueue<M>>,
    rx: Vec<RxQueue<M>>,
    counters: M,
    num_counters: usize,
    irq_doorbells: Vec<usize>,
    _irq_indices: M,
    _lists: Vec<M>,
    qpls: Vec<u32>,
    settings: Settings,
    /// A packet being assembled: its bytes, whether it is dropped.
    partial: Vec<u8>,
    dropping: bool,
}

/// Resets the device: it releases everything a driver gave it, the admin queue
/// included (`gve_adminq_release`).
fn reset<R: Region + ?Sized>(bar0: &R, clock: &dyn Clock, s: &Settings) -> Result<(), GveError> {
    let deadline = clock.now().saturating_add(s.timeout_ns);
    if s.revision == 0 {
        put_be32(bar0, regs::ADMINQ_PFN, 0)?;
        while be32(bar0, regs::ADMINQ_PFN)? != 0 {
            if clock.now() >= deadline {
                return Err(GveError::Timeout("release its admin queue on a reset"));
            }
            clock.pause();
        }
    } else {
        put_be32(bar0, regs::DRIVER_STATUS, regs::driver_status::RESET)?;
        while be32(bar0, regs::DEVICE_STATUS)? & regs::device_status::IS_RESET == 0 {
            if clock.now() >= deadline {
                return Err(GveError::Timeout("finish a reset"));
            }
            clock.pause();
        }
    }
    Ok(())
}

/// A doorbell index the device gave, as an offset in BAR 2, checked to lie inside it.
fn doorbell<B: Region + ?Sized>(bar2: &B, index: u32, what: &str) -> Result<usize, GveError> {
    let at = (index as usize)
        .checked_mul(4)
        .filter(|at| at + 4 <= bar2.len());
    at.ok_or_else(|| {
        GveError::Malformed(format!(
            "{what} doorbell index {index}, outside BAR 2's {} bytes",
            bar2.len()
        ))
    })
}

impl<R: Region, B: Region, M: Region> Gve<R, B, M> {
    /// Brings the device up (see the module's description).
    pub fn start<D: Dma<Region = M>>(
        bar0: R,
        bar2: B,
        dma: &mut D,
        clock: &dyn Clock,
        settings: Settings,
        reset_done: &mut dyn FnMut() -> Result<(), GveError>,
    ) -> Result<Gve<R, B, M>, GveError> {
        if bar0.len() < regs::SIZE || bar2.len() < 4 {
            return Err(GveError::Unsupported(format!(
                "BAR 0 of {} bytes and BAR 2 of {}",
                bar0.len(),
                bar2.len()
            )));
        }
        reset(&bar0, clock, &settings)?;
        reset_done()?;
        for b in regs::VERSION {
            bar0.write8(regs::DRIVER_VERSION, *b)?;
        }
        let (page, addrs) = dma.alloc(PAGE, false)?;
        let addr = addrs[0];
        let mut admin = AdminQueue::new(page, addr);
        if settings.revision == 0 {
            let pfn = u32::try_from(addr / PAGE as u64).map_err(|_| {
                GveError::Unsupported(format!("an admin queue at {addr:#x}, past a 32-bit frame"))
            })?;
            put_be32(&bar0, regs::ADMINQ_PFN, pfn)?;
        } else {
            put_be16(&bar0, regs::ADMINQ_LENGTH, PAGE as u16)?;
            put_be32(&bar0, regs::ADMINQ_BASE_HI, (addr >> 32) as u32)?;
            put_be32(&bar0, regs::ADMINQ_BASE_LO, addr as u32)?;
            put_be32(&bar0, regs::DRIVER_STATUS, regs::driver_status::RUN)?;
        }
        let (described, at) = dma.alloc(PAGE, false)?;
        admin.execute(
            &bar0,
            clock,
            settings.timeout_ns,
            &Command::DescribeDevice {
                addr: at[0],
                length: PAGE as u32,
            },
        )?;
        let mut bytes = vec![0u8; PAGE];
        described.read_bytes(0, &mut bytes)?;
        let description = Description::parse(&bytes)?;
        let max_tx = be32(&bar0, regs::MAX_TX_QUEUES)?;
        let max_rx = be32(&bar0, regs::MAX_RX_QUEUES)?;
        let mut queues = (settings.queues as u32)
            .min(max_tx)
            .min(max_rx)
            .min((settings.vectors.saturating_sub(1) / 2) as u32)
            .min(MAX_QUEUES as u32);
        if description.default_queues > 0 {
            queues = queues.min(description.default_queues as u32);
        }
        if queues == 0 {
            return Err(GveError::Unsupported(format!(
                "no queue pair: the device has {max_tx} and {max_rx}, the driver {} interrupts",
                settings.vectors
            )));
        }
        let q = queues as usize;
        let blocks = 2 * q;
        let num_counters = description.counters as usize;
        let (counters, counter_addr) =
            dma.alloc((num_counters * 4).next_multiple_of(PAGE), true)?;
        let (irq_indices, irq_addr) = dma.alloc((blocks * 64).next_multiple_of(PAGE), true)?;
        let format = description.format;
        admin.execute(
            &bar0,
            clock,
            settings.timeout_ns,
            &Command::ConfigureDeviceResources {
                counters: counter_addr[0],
                num_counters: num_counters as u32,
                irq_dbs: irq_addr[0],
                num_irq_dbs: blocks as u32,
                format: format.code(),
            },
        )?;
        let mut irq_doorbells = Vec::with_capacity(blocks);
        for i in 0..blocks {
            let index = be32(&irq_indices, i * 64)?;
            irq_doorbells.push(doorbell(&bar2, index, "an interrupt")?);
        }
        let mtu = description.mtu.min(settings.mtu_max);
        let max_frame = mtu as usize + 14;
        let slot_bytes = max_frame.next_multiple_of(RX_BUFFER);
        let (tx_entries, rx_entries) =
            (description.tx_entries as u32, description.rx_entries as u32);
        let tx_pages = match format {
            Format::GqiQpl => description.tx_pages_per_qpl as usize,
            Format::GqiRda => (tx_entries.min(256) as usize * slot_bytes).div_ceil(PAGE),
        };
        let qpl = format == Format::GqiQpl;
        if qpl {
            let registered = (q * (tx_pages + rx_entries as usize)) as u64;
            if registered > description.max_registered_pages {
                return Err(GveError::Unsupported(format!(
                    "{q} queue pairs need {registered} registered pages, more than the device's {}",
                    description.max_registered_pages
                )));
            }
        }
        let slots = (tx_pages * PAGE / slot_bytes).min(tx_entries as usize);
        if slots == 0 {
            return Err(GveError::Unsupported(format!(
                "{tx_pages} transmit pages a queue hold no {slot_bytes}-byte slot"
            )));
        }
        let mut lists = Vec::new();
        let mut qpls = Vec::new();
        let mut register = |dma: &mut D,
                            admin: &mut AdminQueue<M>,
                            id: u32,
                            pages: &[u64]|
         -> Result<(), GveError> {
            let (list, list_addr) = dma.alloc((pages.len() * 8).next_multiple_of(PAGE), true)?;
            for (i, p) in pages.iter().enumerate() {
                put_be64(&list, i * 8, *p)?;
            }
            admin.execute(
                &bar0,
                clock,
                settings.timeout_ns,
                &Command::RegisterPageList {
                    id,
                    pages: pages.len() as u32,
                    list: list_addr[0],
                },
            )?;
            lists.push(list);
            qpls.push(id);
            Ok(())
        };
        let mut tx = Vec::with_capacity(q);
        let mut rx = Vec::with_capacity(q);
        for i in 0..q as u32 {
            let contiguous = !qpl && slot_bytes > PAGE;
            let (buffers, pages) = dma.alloc(tx_pages * PAGE, contiguous)?;
            if qpl {
                register(dma, &mut admin, i, &pages)?;
            }
            let (ring, ring_addr) =
                dma.alloc((tx_entries as usize * TX_DESC).next_multiple_of(PAGE), true)?;
            let (resources, res_addr) = dma.alloc(PAGE, false)?;
            admin.execute(
                &bar0,
                clock,
                settings.timeout_ns,
                &Command::CreateTxQueue {
                    id: i,
                    resources: res_addr[0],
                    ring: ring_addr[0],
                    qpl: if qpl { i } else { RAW_ADDRESSING_QPL },
                    ntfy: i,
                    size: tx_entries as u16,
                },
            )?;
            let db = doorbell(&bar2, be32(&resources, 0)?, "a transmit queue's")?;
            let counter = be32(&resources, 4)? as usize;
            if counter >= num_counters {
                return Err(GveError::Malformed(format!(
                    "transmit queue {i}'s event counter {counter}, of {num_counters}"
                )));
            }
            tx.push(TxQueue {
                ring,
                entries: tx_entries,
                req: 0,
                done: 0,
                buffers,
                pages,
                slot_bytes,
                free: (0..slots as u32).rev().collect(),
                slot_of: vec![0; tx_entries as usize],
                _resources: resources,
                doorbell: db,
                counter,
                dirty: false,
            });
        }
        for i in 0..q as u32 {
            let (buffers, pages) = dma.alloc(rx_entries as usize * PAGE, false)?;
            let id = queues + i;
            if qpl {
                register(dma, &mut admin, id, &pages)?;
            }
            let (descriptors, desc_addr) =
                dma.alloc((rx_entries as usize * RX_DESC).next_multiple_of(PAGE), true)?;
            let (data, data_addr) =
                dma.alloc((rx_entries as usize * 8).next_multiple_of(PAGE), true)?;
            for (slot, page) in pages.iter().enumerate() {
                let entry = if qpl { (slot * PAGE) as u64 } else { *page };
                put_be64(&data, slot * 8, entry)?;
            }
            let (resources, res_addr) = dma.alloc(PAGE, false)?;
            admin.execute(
                &bar0,
                clock,
                settings.timeout_ns,
                &Command::CreateRxQueue {
                    id: i,
                    ntfy: queues + i,
                    resources: res_addr[0],
                    descriptors: desc_addr[0],
                    data: data_addr[0],
                    qpl: if qpl { id } else { RAW_ADDRESSING_QPL },
                    size: rx_entries as u16,
                    buffer: RX_BUFFER as u16,
                },
            )?;
            let db = doorbell(&bar2, be32(&resources, 0)?, "a receive queue's")?;
            rx.push(RxQueue {
                descriptors,
                _data: data,
                entries: rx_entries,
                count: 0,
                fill: rx_entries,
                seqno: 1,
                buffers,
                _resources: resources,
                doorbell: db,
            });
        }
        let gve = Gve {
            bar0,
            bar2,
            admin,
            description,
            mtu,
            tx,
            rx,
            counters,
            num_counters,
            irq_doorbells,
            _irq_indices: irq_indices,
            _lists: lists,
            qpls,
            settings,
            partial: Vec::new(),
            dropping: false,
        };
        fence(Ordering::SeqCst);
        for r in &gve.rx {
            put_be32(&gve.bar2, r.doorbell, r.fill)?;
        }
        for at in &gve.irq_doorbells {
            put_be32(&gve.bar2, *at, 0)?;
        }
        Ok(gve)
    }

    pub fn queues(&self) -> usize {
        self.tx.len()
    }

    /// The largest frame: the MTU and an Ethernet header.
    pub fn max_frame(&self) -> usize {
        self.mtu as usize + 14
    }

    /// The device status register.
    pub fn status(&self) -> Result<u32, GveError> {
        Ok(be32(&self.bar0, regs::DEVICE_STATUS)?)
    }

    pub fn link_up(&self) -> Result<bool, GveError> {
        Ok(self.status()? & regs::device_status::LINK != 0)
    }

    /// Whether transmit queue `q` can take a frame.
    pub fn has_room(&self, q: usize) -> bool {
        self.tx
            .get(q)
            .is_some_and(|t| !t.free.is_empty() && t.req.wrapping_sub(t.done) < t.entries)
    }

    /// Puts `frame` on transmit queue `q`; the device learns of it at [`Gve::flush`].
    pub fn send(&mut self, q: usize, frame: &[u8]) -> Result<(), GveError> {
        if frame.len() < 14 || frame.len() > self.max_frame() {
            return Err(GveError::Unsupported(format!(
                "a frame of {} bytes",
                frame.len()
            )));
        }
        if !self.has_room(q) {
            return Err(GveError::Full);
        }
        let qpl = self.description.format == Format::GqiQpl;
        let t = &mut self.tx[q];
        let slot = t.free.pop().ok_or(GveError::Full)?;
        let at = slot as usize * t.slot_bytes;
        t.buffers.write_bytes(at, frame)?;
        let addr = if qpl {
            at as u64
        } else {
            t.pages[at / PAGE] + (at % PAGE) as u64
        };
        let index = (t.req % t.entries) as usize;
        let d = index * TX_DESC;
        // A standard packet descriptor, no offload, one descriptor for the packet.
        t.ring.write32(d, 1 << 24)?;
        put_be16(&t.ring, d + 4, frame.len() as u16)?;
        put_be16(&t.ring, d + 6, frame.len() as u16)?;
        put_be64(&t.ring, d + 8, addr)?;
        t.slot_of[index] = slot;
        t.req = t.req.wrapping_add(1);
        t.dirty = true;
        Ok(())
    }

    /// Rings the doorbell of every transmit queue that has new descriptors.
    pub fn flush(&mut self) -> Result<(), GveError> {
        fence(Ordering::SeqCst);
        for t in self.tx.iter_mut().filter(|t| t.dirty) {
            put_be32(&self.bar2, t.doorbell, t.req)?;
            t.dirty = false;
        }
        Ok(())
    }

    /// Frees the slots of every descriptor the device has finished; returns how many.
    pub fn complete(&mut self) -> Result<u32, GveError> {
        let mut freed = 0;
        for (i, t) in self.tx.iter_mut().enumerate() {
            let counter = be32(&self.counters, t.counter * 4)?;
            let n = counter.wrapping_sub(t.done);
            if n > t.req.wrapping_sub(t.done) {
                return Err(GveError::Malformed(format!(
                    "transmit queue {i}'s counter at {counter}, with {} sent and {} done",
                    t.req, t.done
                )));
            }
            for _ in 0..n {
                let index = (t.done % t.entries) as usize;
                t.free.push(t.slot_of[index]);
                t.done = t.done.wrapping_add(1);
            }
            freed += n;
        }
        Ok(freed)
    }

    /// Takes every packet the device has received, handing each whole one to
    /// `deliver`; returns how many it dropped (marked in error, or of a length out of
    /// bounds). A descriptor that says more than a well-behaved device does is an
    /// error.
    pub fn receive(&mut self, deliver: &mut dyn FnMut(&[u8])) -> Result<u64, GveError> {
        let max_frame = self.max_frame();
        let mut dropped = 0;
        for (i, r) in self.rx.iter_mut().enumerate() {
            let mut took = false;
            loop {
                let index = (r.count % r.entries) as usize;
                let d = index * RX_DESC;
                let flags = be16(&r.descriptors, d + 62)?;
                if (flags & 0x7) as u8 != r.seqno {
                    break;
                }
                fence(Ordering::SeqCst);
                let len = be16(&r.descriptors, d + 60)? as usize;
                let pad = if self.partial.is_empty() && !self.dropping {
                    RX_PAD
                } else {
                    0
                };
                if len > RX_BUFFER || len < pad {
                    return Err(GveError::Malformed(format!(
                        "a receive fragment of {len} bytes on queue {i}"
                    )));
                }
                if flags & RX_ERR != 0 {
                    self.dropping = true;
                }
                if !self.dropping && self.partial.len() + len - pad <= max_frame {
                    let start = self.partial.len();
                    self.partial.resize(start + len - pad, 0);
                    r.buffers
                        .read_bytes(index * PAGE + pad, &mut self.partial[start..])?;
                } else {
                    self.dropping = true;
                }
                r.count = r.count.wrapping_add(1);
                r.fill = r.fill.wrapping_add(1);
                r.seqno = next_seqno(r.seqno);
                took = true;
                if flags & RX_CONT == 0 {
                    if self.dropping || self.partial.len() < 14 {
                        dropped += 1;
                    } else {
                        deliver(&self.partial);
                    }
                    self.partial.clear();
                    self.dropping = false;
                }
            }
            if !self.partial.is_empty() || self.dropping {
                return Err(GveError::Malformed(format!(
                    "receive queue {i} stopped in the middle of a packet"
                )));
            }
            if took {
                fence(Ordering::SeqCst);
                put_be32(&self.bar2, r.doorbell, r.fill)?;
            }
        }
        Ok(dropped)
    }

    /// Acknowledges every notification block and unmasks it.
    pub fn ack(&self) -> Result<(), GveError> {
        for at in &self.irq_doorbells {
            put_be32(&self.bar2, *at, regs::irq::ACK | regs::irq::EVENT)?;
        }
        fence(Ordering::SeqCst);
        Ok(())
    }

    /// Whether the device has done something since it was last looked at: a packet
    /// received or a descriptor finished.
    pub fn pending(&self) -> Result<bool, GveError> {
        for r in &self.rx {
            let d = (r.count % r.entries) as usize * RX_DESC;
            if (be16(&r.descriptors, d + 62)? & 0x7) as u8 == r.seqno {
                return Ok(true);
            }
        }
        for t in &self.tx {
            if be32(&self.counters, t.counter * 4)? != t.done {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Destroys the queues, unregisters the page lists, deconfigures the resources and
    /// resets the device, so it holds nothing of the driver's.
    pub fn shutdown(&mut self, clock: &dyn Clock) -> Result<(), GveError> {
        let t = self.settings.timeout_ns;
        for id in 0..self.tx.len() as u32 {
            self.admin
                .execute(&self.bar0, clock, t, &Command::DestroyTxQueue { id })?;
        }
        for id in 0..self.rx.len() as u32 {
            self.admin
                .execute(&self.bar0, clock, t, &Command::DestroyRxQueue { id })?;
        }
        for id in core::mem::take(&mut self.qpls) {
            self.admin
                .execute(&self.bar0, clock, t, &Command::UnregisterPageList { id })?;
        }
        self.admin
            .execute(&self.bar0, clock, t, &Command::DeconfigureDeviceResources)?;
        reset(&self.bar0, clock, &self.settings)?;
        self.admin.reset()
    }

    /// The event counters' count, for a driver's line.
    pub fn counters(&self) -> usize {
        self.num_counters
    }
}
