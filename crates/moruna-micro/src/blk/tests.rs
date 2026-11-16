//! The queue engine and the disk's thread against a model virtio-blk device: the model reads
//! the descriptor chains the engine offers off the same memory, moves the sectors of an
//! in-memory disk to and from the addresses they name, and returns each chain on the used
//! ring, as QEMU's device does.

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use moruna_abi::Error;
use moruna_mmio::Region;
use moruna_virtio::Queue;
use moruna_virtio::queue::{DESC, DEVICE, DRIVER};

use super::disk::{Bell, Disk, Parked, Port};
use super::engine::{Engine, ID_BYTES, Kind, Op, SLOT, Slots, T_FLUSH, T_GET_ID, T_IN, T_OUT};
use super::plan;

/// Plain memory both the engine and the model reach, at a device address of its own.
#[derive(Clone)]
pub(crate) struct Mem {
    bytes: Arc<Mutex<Vec<u8>>>,
    base: u64,
}

impl Mem {
    pub(crate) fn new(len: usize, base: u64) -> Mem {
        Mem {
            bytes: Arc::new(Mutex::new(vec![0; len])),
            base,
        }
    }

    fn at(&self, offset: usize, n: usize) -> Result<usize, Error> {
        if !offset.is_multiple_of(n) {
            return Err(Error::InvalidArgs);
        }
        match offset.checked_add(n) {
            Some(end) if end <= self.bytes.lock().unwrap().len() => Ok(offset),
            _ => Err(Error::OutOfRange),
        }
    }

    fn load<const N: usize>(&self, offset: usize) -> Result<[u8; N], Error> {
        let at = self.at(offset, N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes.lock().unwrap()[at..at + N]);
        Ok(out)
    }

    fn store(&self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
        let at = self.at(offset, bytes.len())?;
        self.bytes.lock().unwrap()[at..at + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.base + self.bytes.lock().unwrap().len() as u64
    }
}

impl Region for Mem {
    fn len(&self) -> usize {
        self.bytes.lock().unwrap().len()
    }
    fn read8(&self, offset: usize) -> Result<u8, Error> {
        self.load::<1>(offset).map(|b| b[0])
    }
    fn read16(&self, offset: usize) -> Result<u16, Error> {
        self.load(offset).map(u16::from_le_bytes)
    }
    fn read32(&self, offset: usize) -> Result<u32, Error> {
        self.load(offset).map(u32::from_le_bytes)
    }
    fn read64(&self, offset: usize) -> Result<u64, Error> {
        self.load(offset).map(u64::from_le_bytes)
    }
    fn write8(&self, offset: usize, value: u8) -> Result<(), Error> {
        self.store(offset, &[value])
    }
    fn write16(&self, offset: usize, value: u16) -> Result<(), Error> {
        self.store(offset, &value.to_le_bytes())
    }
    fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
        self.store(offset, &value.to_le_bytes())
    }
    fn write64(&self, offset: usize, value: u64) -> Result<(), Error> {
        self.store(offset, &value.to_le_bytes())
    }
}

/// Memory at its own address, in runs that end at every 4 KiB page, except a range no device
/// may reach.
pub(crate) struct Identity {
    pub(crate) refused: Option<(u64, u64)>,
}

impl super::engine::Translate for Identity {
    fn runs(&self, addr: u64, len: u64, out: &mut Vec<(u64, u32)>) -> Result<(), String> {
        if let Some((from, to)) = self.refused
            && addr < to
            && addr + len > from
        {
            return Err(format!("{addr:#x} is not pinned"));
        }
        let mut at = addr;
        while at < addr + len {
            let n = (4096 - at % 4096).min(addr + len - at);
            out.push((at, n as u32));
            at += n;
        }
        Ok(())
    }
}

/// A virtio-blk device over an in-memory disk.
pub(crate) struct Model {
    queue: Mem,
    slots: Mem,
    size: u16,
    seen: u16,
    used: u16,
    pub(crate) disk: Vec<u8>,
    serial: Vec<u8>,
    /// A request touching this sector ends with status 1.
    pub(crate) bad_sector: Option<u64>,
    /// The next used entry names a chain never offered.
    pub(crate) garbage: bool,
    /// Requests are left on the ring until this is cleared.
    pub(crate) hold: bool,
    pub(crate) resets: u32,
}

impl Model {
    fn desc(&self, i: u16) -> (u64, u32, u16, u16) {
        let at = DESC + i as usize * 16;
        (
            self.queue.read64(at).unwrap(),
            self.queue.read32(at + 8).unwrap(),
            self.queue.read16(at + 12).unwrap(),
            self.queue.read16(at + 14).unwrap(),
        )
    }

    fn read_mem(&self, addr: u64, out: &mut [u8]) {
        if self.slots.contains(addr) {
            self.slots
                .read_bytes((addr - self.slots.base) as usize, out)
                .unwrap();
        } else {
            // SAFETY: test memory the engine handed over by address and keeps alive.
            unsafe { std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len()) }
        }
    }

    fn write_mem(&self, addr: u64, bytes: &[u8]) {
        if self.slots.contains(addr) {
            self.slots
                .write_bytes((addr - self.slots.base) as usize, bytes)
                .unwrap();
        } else {
            // SAFETY: as in `read_mem`.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) }
        }
    }

    /// Serves every chain on the driver ring.
    pub(crate) fn process(&mut self) {
        if self.hold {
            return;
        }
        let avail = self.queue.read16(DRIVER + 2).unwrap();
        while self.seen != avail {
            let slot = (self.seen % self.size) as usize;
            let head = self.queue.read16(DRIVER + 4 + slot * 2).unwrap();
            self.seen = self.seen.wrapping_add(1);
            let mut chain = Vec::new();
            let mut i = head;
            loop {
                let (addr, len, flags, next) = self.desc(i);
                chain.push((addr, len, flags & 2 != 0));
                if flags & 1 == 0 {
                    break;
                }
                i = next;
            }
            let mut header = [0u8; 16];
            self.read_mem(chain[0].0, &mut header);
            let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
            let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());
            let data = &chain[1..chain.len() - 1];
            let bytes: u64 = data.iter().map(|d| u64::from(d.1)).sum();
            let mut status = 0u8;
            let span = sector * 512..sector * 512 + bytes;
            if self.bad_sector.is_some_and(|b| span.contains(&(b * 512))) {
                status = 1;
            } else {
                let mut at = span.start as usize;
                for &(addr, len, writes) in data {
                    let len = len as usize;
                    match kind {
                        T_IN => {
                            assert!(writes, "a read's data is the device's to write");
                            let src = self.disk[at..at + len].to_vec();
                            self.write_mem(addr, &src);
                        }
                        T_OUT => {
                            assert!(!writes, "a write's data is the device's to read");
                            let mut src = vec![0u8; len];
                            self.read_mem(addr, &mut src);
                            self.disk[at..at + len].copy_from_slice(&src);
                        }
                        T_GET_ID => {
                            let mut id = self.serial.clone();
                            id.resize(len, 0);
                            self.write_mem(addr, &id);
                        }
                        T_FLUSH => {}
                        other => panic!("request type {other}"),
                    }
                    at += len;
                }
            }
            let (status_addr, _, _) = chain[chain.len() - 1];
            self.write_mem(status_addr, &[status]);
            let id = if self.garbage {
                self.garbage = false;
                (head + 1) % self.size
            } else {
                head
            };
            let entry = DEVICE + 4 + (self.used % self.size) as usize * 8;
            self.queue.write32(entry, u32::from(id)).unwrap();
            self.queue.write32(entry + 4, bytes as u32).unwrap();
            self.used = self.used.wrapping_add(1);
            self.queue.write16(DEVICE + 2, self.used).unwrap();
        }
    }
}

pub(crate) const SLOTS_BASE: u64 = 0x7000_0000_0000;

/// An engine of `slots` slots on a queue of `size` over a model disk of `sectors` sectors
/// holding a pattern, and the model.
pub(crate) fn rig(
    size: u16,
    slots: usize,
    sectors: usize,
    max_body: u64,
    writable: bool,
) -> (Engine<Mem, Mem>, Arc<Mutex<Model>>) {
    let queue = Mem::new(moruna_virtio::queue::BYTES, 0x6000_0000_0000);
    let slot_mem = Mem::new(slots * SLOT, SLOTS_BASE);
    let model = Model {
        queue: queue.clone(),
        slots: slot_mem.clone(),
        size,
        seen: 0,
        used: 0,
        disk: (0..sectors * 512).map(|i| (i * 7 % 251) as u8).collect(),
        serial: b"123456789".to_vec(),
        bad_sector: None,
        garbage: false,
        hold: false,
        resets: 0,
    };
    let engine = Engine::new(
        Queue::new(queue, size).unwrap(),
        Slots {
            memory: slot_mem,
            addrs: (0..slots).map(|i| SLOTS_BASE + (i * SLOT) as u64).collect(),
        },
        Arc::new(Identity { refused: None }),
        max_body,
        (sectors * 512) as u64,
        writable,
        true,
    );
    (engine, Arc::new(Mutex::new(model)))
}

type Outcome = Arc<Mutex<Vec<Result<(), String>>>>;

fn op(kind: Kind, offset: u64, memory: &mut [u8], outcome: &Outcome) -> Op {
    let outcome = Arc::clone(outcome);
    Op {
        kind,
        offset,
        addr: memory.as_mut_ptr() as u64,
        len: memory.len() as u64,
        done: Box::new(move |r| outcome.lock().unwrap().push(r)),
    }
}

/// Pumps and polls until nothing waits or is in flight.
fn settle(engine: &mut Engine<Mem, Mem>, model: &Arc<Mutex<Model>>) {
    for _ in 0..10_000 {
        engine.pump(|| {
            model.lock().unwrap().process();
            Ok(())
        });
        model.lock().unwrap().process();
        engine.poll();
        if engine.in_flight() == 0 && engine.waiting() == 0 {
            return;
        }
    }
    panic!("the engine did not settle");
}

#[test]
fn reads_of_every_shape_land_whole_by_dma_and_their_edges_by_bounce() {
    let (mut engine, model) = rig(32, 8, 64, 4096, false);
    let disk = model.lock().unwrap().disk.clone();
    let outcome: Outcome = Arc::default();
    let mut expected_bounce = 0;
    for (offset, len) in [
        (0, 512),
        (1, 10),
        (100, 9000),
        (511, 2),
        (4096, 12_288),
        (513, 20_000),
        (30_000, 2_768),
    ] {
        let mut dst = vec![0u8; len];
        engine.submit(op(Kind::Read, offset, &mut dst, &outcome));
        settle(&mut engine, &model);
        assert_eq!(
            dst,
            disk[offset as usize..offset as usize + len],
            "{offset} {len}"
        );
        expected_bounce += plan::read(offset, len as u64, 4096)
            .iter()
            .flat_map(|p| [p.head, p.tail])
            .flatten()
            .map(|e| e.len as u64)
            .sum::<u64>();
    }
    let results = outcome.lock().unwrap();
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    let stats = engine.stats();
    assert_eq!(stats.ops, 7);
    assert_eq!(stats.bounced_bytes, expected_bounce);
    assert!(stats.dma_bytes > 40_000);
    assert!(stats.notifies >= 1 && stats.requests >= 7);
}

#[test]
fn more_requests_than_room_wait_and_go_in_order() {
    // Two slots and a queue of 8: a 64 KiB read in 4 KiB bodies is sixteen requests.
    let (mut engine, model) = rig(8, 2, 256, 4096, false);
    let disk = model.lock().unwrap().disk.clone();
    let outcome: Outcome = Arc::default();
    let mut a = vec![0u8; 65_536];
    let mut b = vec![0u8; 700];
    engine.submit(op(Kind::Read, 1024, &mut a, &outcome));
    engine.submit(op(Kind::Read, 77, &mut b, &outcome));
    model.lock().unwrap().hold = true;
    assert_eq!(engine.pump(|| Ok(())), 2, "two slots");
    assert_eq!(engine.in_flight(), 2);
    assert!(engine.waiting() > 0);
    assert_eq!(engine.pump(|| Ok(())), 0, "no room");
    model.lock().unwrap().hold = false;
    settle(&mut engine, &model);
    assert_eq!(a, disk[1024..1024 + 65_536]);
    assert_eq!(b, disk[77..777]);
    assert_eq!(outcome.lock().unwrap().len(), 2);
}

#[test]
fn writes_are_whole_sectors_on_a_writable_disk() {
    let (mut engine, model) = rig(16, 4, 64, 4096, true);
    let outcome: Outcome = Arc::default();
    let mut src: Vec<u8> = (0..10_240).map(|i| (i % 13) as u8).collect();
    engine.submit(op(Kind::Write, 2048, &mut src, &outcome));
    settle(&mut engine, &model);
    assert_eq!(model.lock().unwrap().disk[2048..2048 + 10_240], src[..]);
    let mut odd = vec![1u8; 100];
    engine.submit(op(Kind::Write, 0, &mut odd, &outcome));
    let mut past = vec![1u8; 512];
    engine.submit(op(Kind::Write, 64 * 512, &mut past, &outcome));
    engine.submit(op(Kind::Flush, 0, &mut [], &outcome));
    settle(&mut engine, &model);
    let results = outcome.lock().unwrap();
    assert!(results[0].is_ok());
    assert!(results[1].as_ref().unwrap_err().contains("whole sectors"));
    assert!(results[2].as_ref().unwrap_err().contains("past the disk"));
    assert!(results[3].is_ok(), "{:?}", results[3]);
}

#[test]
fn a_read_only_disk_refuses_writes_and_reads_past_its_end() {
    let (mut engine, model) = rig(16, 4, 8, 4096, false);
    let outcome: Outcome = Arc::default();
    let mut buf = vec![0u8; 512];
    engine.submit(op(Kind::Write, 0, &mut buf, &outcome));
    engine.submit(op(Kind::Read, 8 * 512 - 100, &mut buf, &outcome));
    engine.submit(op(Kind::Read, u64::MAX - 10, &mut buf, &outcome));
    let mut nothing = Vec::new();
    engine.submit(op(Kind::Read, 3, &mut nothing, &outcome));
    settle(&mut engine, &model);
    let results = outcome.lock().unwrap();
    assert!(results[0].as_ref().unwrap_err().contains("read only"));
    assert!(results[1].as_ref().unwrap_err().contains("past the disk"));
    assert!(results[2].is_err());
    assert!(results[3].is_ok(), "an empty read moves nothing");
    assert_eq!(engine.capacity(), 8 * 512);
}

#[test]
fn the_serial_comes_through_a_bounce() {
    let (mut engine, model) = rig(16, 4, 8, 4096, false);
    let outcome: Outcome = Arc::default();
    let mut id = vec![0xaa; ID_BYTES];
    engine.submit(op(Kind::Id, 0, &mut id, &outcome));
    let mut short = vec![0u8; 4];
    engine.submit(op(Kind::Id, 0, &mut short, &outcome));
    settle(&mut engine, &model);
    assert_eq!(&id[..9], b"123456789");
    assert!(id[9..].iter().all(|b| *b == 0));
    // The short one is refused as it is submitted, before the other ends.
    let results = outcome.lock().unwrap();
    assert!(results[0].as_ref().unwrap_err().contains("20 bytes"));
    assert!(results[1].is_ok());
}

#[test]
fn a_flush_on_a_disk_with_no_cache_is_done_at_once() {
    let (queue_engine, _model) = rig(16, 4, 8, 4096, true);
    drop(queue_engine);
    let queue = Mem::new(moruna_virtio::queue::BYTES, 0x6000_0000_0000);
    let slots = Mem::new(SLOT, SLOTS_BASE);
    let mut engine = Engine::new(
        Queue::new(queue, 8).unwrap(),
        Slots {
            memory: slots,
            addrs: vec![SLOTS_BASE],
        },
        Arc::new(Identity { refused: None }),
        4096,
        4096,
        true,
        false,
    );
    let outcome: Outcome = Arc::default();
    engine.submit(op(Kind::Flush, 0, &mut [], &outcome));
    assert_eq!(outcome.lock().unwrap().len(), 1);
    assert_eq!(engine.waiting(), 0);
}

#[test]
fn a_failed_request_fails_its_operation_and_no_other() {
    let (mut engine, model) = rig(32, 8, 64, 4096, false);
    model.lock().unwrap().bad_sector = Some(20);
    let outcome: Outcome = Arc::default();
    let mut a = vec![0u8; 4096];
    let mut b = vec![0u8; 20_000];
    engine.submit(op(Kind::Read, 0, &mut a, &outcome));
    engine.submit(op(Kind::Read, 5000, &mut b, &outcome));
    settle(&mut engine, &model);
    let results = outcome.lock().unwrap();
    assert!(results[0].is_ok());
    assert!(results[1].as_ref().unwrap_err().contains("status 1"));
    assert_eq!(engine.stats().failed, 1);
}

#[test]
fn memory_no_device_reaches_fails_its_operation_before_it_goes() {
    let (engine, model) = rig(32, 8, 64, 4096, false);
    let outcome: Outcome = Arc::default();
    let mut a = vec![0u8; 3 * 4096];
    let start = a.as_ptr() as u64;
    let queue = Mem::new(moruna_virtio::queue::BYTES, 0x6000_0000_0000);
    drop(queue);
    // Swap in a translation that refuses the middle of `a`.
    let disk = model.lock().unwrap().disk.clone();
    let mut fresh = Engine::new(
        Queue::new(Mem::new(moruna_virtio::queue::BYTES, 0x6000_0000_0000), 32).unwrap(),
        Slots {
            memory: Mem::new(8 * SLOT, SLOTS_BASE),
            addrs: (0..8).map(|i| SLOTS_BASE + (i * SLOT) as u64).collect(),
        },
        Arc::new(Identity {
            refused: Some((start + 5000, start + 6000)),
        }),
        4096,
        disk.len() as u64,
        false,
        true,
    );
    fresh.submit(op(Kind::Read, 0, &mut a, &outcome));
    // Parts before the refused range go; the refused part and those after are dropped.
    let sent = fresh.pump(|| Ok(()));
    assert!(sent <= 2, "{sent}");
    assert_eq!(fresh.waiting(), 0);
    // The model never sees those; the engine abandons what is in flight once reset.
    fresh.break_down("closed".into());
    fresh.abandon();
    let results = outcome.lock().unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].as_ref().unwrap_err().contains("not pinned"));
    drop(engine);
}

#[test]
fn a_device_that_returns_a_chain_never_offered_breaks_the_queue() {
    let (mut engine, model) = rig(16, 4, 64, 4096, false);
    let outcome: Outcome = Arc::default();
    let mut a = vec![0u8; 512];
    let mut b = vec![0u8; 512];
    model.lock().unwrap().garbage = true;
    engine.submit(op(Kind::Read, 0, &mut a, &outcome));
    engine.pump(|| {
        model.lock().unwrap().process();
        Ok(())
    });
    engine.poll();
    assert!(engine.broken().is_some());
    engine.submit(op(Kind::Read, 0, &mut b, &outcome));
    engine.abandon();
    let results = outcome.lock().unwrap();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results.iter().all(Result::is_err));
}

#[test]
fn a_doorbell_that_fails_breaks_the_queue_and_waiting_operations_end() {
    let (mut engine, _model) = rig(8, 1, 64, 4096, false);
    let outcome: Outcome = Arc::default();
    let mut a = vec![0u8; 512];
    let mut b = vec![0u8; 512];
    engine.submit(op(Kind::Read, 0, &mut a, &outcome));
    engine.submit(op(Kind::Read, 512, &mut b, &outcome));
    engine.pump(|| Err("gone".into()));
    assert!(engine.broken().unwrap().contains("doorbell"));
    // The waiting one ended at once; the one in flight ends when abandoned.
    assert_eq!(outcome.lock().unwrap().len(), 1);
    engine.abandon();
    assert_eq!(outcome.lock().unwrap().len(), 2);
}

/// A port onto the model: the doorbell serves the ring at once.
struct ModelPort {
    model: Arc<Mutex<Model>>,
    waits: Arc<AtomicU64>,
}

impl Port for ModelPort {
    fn ring(&self) -> Result<(), String> {
        self.model.lock().unwrap().process();
        Ok(())
    }
    fn wait_interrupt(&self, _ns: u64) {
        self.waits.fetch_add(1, Ordering::SeqCst);
        self.model.lock().unwrap().process();
    }
    fn reset(&self) {
        self.model.lock().unwrap().resets += 1;
    }
}

#[test]
fn the_disk_thread_serves_many_threads_and_sleeps_between() {
    let (engine, model) = rig(64, 16, 2048, 64 * 1024, true);
    let disk_bytes = model.lock().unwrap().disk.clone();
    let waits = Arc::new(AtomicU64::new(0));
    let bell: Arc<dyn Bell> = Arc::new(Parked::new());
    let disk = Disk::start(
        "disk.in.t",
        engine,
        ModelPort {
            model: Arc::clone(&model),
            waits: Arc::clone(&waits),
        },
        bell,
        1_000_000,
    )
    .unwrap();
    assert_eq!(disk.name(), "disk.in.t");
    assert_eq!(disk.capacity(), 2048 * 512);
    assert_eq!(disk.length(), 1_000_000);
    let threads: Vec<_> = (0..4u64)
        .map(|t| {
            let disk = disk.clone();
            let expect = disk_bytes.clone();
            std::thread::spawn(move || {
                for i in 0..25u64 {
                    let offset = (t * 100_003 + i * 7_919) % 900_000;
                    let len = 1 + ((i * 4_099) % 50_000) as usize;
                    let mut dst = vec![0u8; len];
                    let (tx, rx) = mpsc::channel();
                    disk.submit(Op {
                        kind: Kind::Read,
                        offset,
                        addr: dst.as_mut_ptr() as u64,
                        len: len as u64,
                        done: Box::new(move |r| tx.send(r).unwrap()),
                    });
                    rx.recv().unwrap().unwrap();
                    assert_eq!(dst, expect[offset as usize..offset as usize + len]);
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let mut page = vec![9u8; 4096];
    disk.run(Op {
        kind: Kind::Write,
        offset: 8192,
        addr: page.as_mut_ptr() as u64,
        len: 4096,
        done: Box::new(|_| {}),
    })
    .unwrap();
    assert_eq!(model.lock().unwrap().disk[8192..12288], page[..]);
    assert_eq!(disk.stats().ops, 101);
    disk.close();
    assert_eq!(model.lock().unwrap().resets, 1);
    let closed = disk.run(Op {
        kind: Kind::Read,
        offset: 0,
        addr: page.as_mut_ptr() as u64,
        len: 512,
        done: Box::new(|_| {}),
    });
    assert!(closed.unwrap_err().contains("closed"));
    // Debug names the disk.
    assert!(format!("{disk:?}").contains("disk.in.t"));
}

#[test]
fn a_disk_whose_queue_breaks_refuses_from_then_on() {
    let (engine, model) = rig(16, 4, 64, 4096, false);
    model.lock().unwrap().garbage = true;
    let disk = Disk::start(
        "disk.in.b",
        engine,
        ModelPort {
            model: Arc::clone(&model),
            waits: Arc::default(),
        },
        Arc::new(Parked::default()),
        64 * 512,
    )
    .unwrap();
    let mut a = vec![0u8; 512];
    let first = disk.run(Op {
        kind: Kind::Read,
        offset: 0,
        addr: a.as_mut_ptr() as u64,
        len: 512,
        done: Box::new(|_| {}),
    });
    assert!(first.is_err());
    // The thread has ended; a later operation is refused with why.
    let second = disk.run(Op {
        kind: Kind::Read,
        offset: 0,
        addr: a.as_mut_ptr() as u64,
        len: 512,
        done: Box::new(|_| {}),
    });
    let second = second.unwrap_err();
    assert!(
        second.contains("disk.in.b") && second.contains("used ring"),
        "{second}"
    );
    assert_eq!(model.lock().unwrap().resets, 1);
    disk.close();
}

#[test]
fn a_slow_device_is_waited_for_by_its_interrupt() {
    let (engine, model) = rig(16, 4, 64, 4096, false);
    let waits = Arc::new(AtomicU64::new(0));
    model.lock().unwrap().hold = true;
    let disk = Disk::start(
        "disk.in.s",
        engine,
        ModelPort {
            model: Arc::clone(&model),
            waits: Arc::clone(&waits),
        },
        Arc::new(Parked::default()),
        64 * 512,
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let mut a = vec![0u8; 512];
    disk.submit(Op {
        kind: Kind::Read,
        offset: 0,
        addr: a.as_mut_ptr() as u64,
        len: 512,
        done: Box::new(move |r| tx.send(r).unwrap()),
    });
    while waits.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    model.lock().unwrap().hold = false;
    rx.recv().unwrap().unwrap();
    disk.close();
}

mod reactor_and_layer {
    use std::path::Path;

    use moruna_arena::{Arena, ArenaConfig};
    use moruna_kernel::{
        Allocator, Buffer, BufferView, Completion, CopyDst, CopySrc, Guarantee, IoPaths,
        MorunaError, ObjectMeta, ObjectMetadata, Reactor, Tier,
    };

    use super::*;
    use crate::blk::layer;
    use crate::blk::reactor::DiskReactor;

    fn arena() -> Arc<Arena> {
        Arena::new(ArenaConfig {
            host_bytes: 32 << 20,
            huge_pages: Guarantee::Absent,
            memlock: Guarantee::Absent,
            ..ArenaConfig::default()
        })
        .unwrap()
    }

    /// The reactor a `DiskReactor` wraps: it names each call it is given.
    #[derive(Default)]
    struct Inner {
        calls: Mutex<Vec<&'static str>>,
    }

    impl Inner {
        fn called(&self, what: &'static str) {
            self.calls.lock().unwrap().push(what);
        }
        fn no<T: Send + 'static>(&self, what: &'static str) -> Completion<T> {
            self.called(what);
            Completion::resolved(Err(MorunaError::Unsupported("inner")))
        }
    }

    impl Reactor for Inner {
        fn read_file(&self, _: &Path, _: u64, _: Buffer) -> Completion<Buffer> {
            self.no("read_file")
        }
        fn read_file_opt(
            &self,
            _: &Path,
            _: u64,
            _: Buffer,
            _: bool,
        ) -> Completion<(Buffer, usize)> {
            self.no("read_file_opt")
        }
        fn write_file(&self, _: &Path, _: u64, _: BufferView) -> Completion<()> {
            self.no("write_file")
        }
        fn read_object(&self, _: &str, _: u64, _: Buffer) -> Completion<Buffer> {
            self.no("read_object")
        }
        fn write_object(&self, _: &str, _: BufferView) -> Completion<()> {
            self.no("write_object")
        }
        fn copy(&self, _: CopySrc, _: CopyDst) -> Completion<Option<Buffer>> {
            self.no("copy")
        }
        fn register_segment(&self, _: u32, _: &Path) -> moruna_kernel::Result<()> {
            self.called("register_segment");
            Ok(())
        }
        fn unregister_segment(&self, _: u32) {
            self.called("unregister_segment");
        }
        fn delete_object(&self, _: &str) -> Completion<()> {
            self.no("delete_object")
        }
        fn abort_multipart(&self, _: &str, _: &str) -> Completion<()> {
            self.no("abort_multipart")
        }
        fn paths(&self) -> IoPaths {
            self.called("paths");
            IoPaths::default()
        }
        fn shutdown(&self) {
            self.called("shutdown");
        }
    }

    impl ObjectMetadata for Inner {
        fn head_object(&self, _: &str) -> Completion<ObjectMeta> {
            self.no("head_object")
        }
        fn list_prefix(&self, _: &str) -> Completion<Vec<ObjectMeta>> {
            self.no("list_prefix")
        }
    }

    fn disk(name: &str, sectors: usize, writable: bool) -> (Disk, Arc<Mutex<Model>>) {
        let (engine, model) = rig(64, 16, sectors, 64 * 1024, writable);
        let disk = Disk::start(
            name,
            engine,
            ModelPort {
                model: Arc::clone(&model),
                waits: Arc::default(),
            },
            Arc::new(Parked::default()),
            (sectors * 512) as u64,
        )
        .unwrap();
        (disk, model)
    }

    #[test]
    fn disk_urls_are_read_from_the_disk_and_every_other_goes_to_the_reactor() {
        let arena = arena();
        let (input, model) = disk("disk.in.nyc", 4096, false);
        input.set_length(1_000_000);
        let (output, _) = disk("disk.out", 64, true);
        let inner = Arc::new(Inner::default());
        let reactor = DiskReactor::new(
            inner.clone(),
            Some(inner.clone()),
            [input.clone(), output.clone()],
        )
        .unwrap();
        assert_eq!(reactor.urls(), vec!["disk://nyc".to_string()]);

        let head = reactor.head_object("disk://nyc").wait().unwrap();
        assert_eq!((head.url.as_str(), head.size), ("disk://nyc", 1_000_000));
        assert_eq!(reactor.list_prefix("disk://").wait().unwrap().len(), 1);
        assert_eq!(reactor.list_prefix("disk://ny").wait().unwrap().len(), 1);
        assert!(reactor.list_prefix("disk://x").wait().unwrap().is_empty());

        let disk_bytes = model.lock().unwrap().disk.clone();
        let dst = arena.alloc(70_000, Tier::Host).unwrap();
        let got = reactor
            .read_object("disk://nyc", 123_457, dst)
            .wait()
            .unwrap();
        assert_eq!(got[..], disk_bytes[123_457..123_457 + 70_000]);
        let past = arena.alloc(1000, Tier::Host).unwrap();
        let past = reactor.read_object("disk://nyc", 999_500, past).wait();
        assert!(matches!(past, Err(MorunaError::Io { .. })));

        let held = Arc::new(arena.alloc(512, Tier::Host).unwrap());
        assert!(
            reactor
                .write_object("disk://nyc", held.view())
                .wait()
                .is_err()
        );
        assert!(reactor.delete_object("disk://nyc").wait().is_err());

        // Everything else is the wrapped reactor's.
        let other = arena.alloc(10, Tier::Host).unwrap();
        assert!(reactor.read_object("s3://b/k", 0, other).wait().is_err());
        assert!(
            reactor
                .write_object("s3://b/k", held.view())
                .wait()
                .is_err()
        );
        assert!(reactor.delete_object("s3://b/k").wait().is_err());
        assert!(reactor.head_object("s3://b/k").wait().is_err());
        assert!(reactor.list_prefix("s3://b/").wait().is_err());
        let p = Path::new("/x");
        assert!(
            reactor
                .read_file(p, 0, arena.alloc(1, Tier::Host).unwrap())
                .wait()
                .is_err()
        );
        assert!(
            reactor
                .read_file_opt(p, 0, arena.alloc(1, Tier::Host).unwrap(), true)
                .wait()
                .is_err()
        );
        assert!(reactor.write_file(p, 0, held.view()).wait().is_err());
        assert!(reactor.abort_multipart("s3://b/k", "u").wait().is_err());
        reactor.register_segment(1, p).unwrap();
        reactor.unregister_segment(1);
        let _ = reactor.paths();
        reactor.shutdown();
        assert_eq!(
            model.lock().unwrap().resets,
            0,
            "the disk is the process's, not the run's"
        );
        input.close();
        assert_eq!(model.lock().unwrap().resets, 1);
        assert_eq!(
            inner.calls.lock().unwrap().clone(),
            vec![
                "read_object",
                "write_object",
                "delete_object",
                "head_object",
                "list_prefix",
                "read_file",
                "read_file_opt",
                "write_file",
                "abort_multipart",
                "register_segment",
                "unregister_segment",
                "paths",
                "shutdown"
            ]
        );
        output.close();
    }

    #[test]
    fn with_no_store_behind_it_an_object_url_is_refused() {
        let (input, _) = disk("disk.in.a", 64, false);
        let reactor = DiskReactor::new(Arc::new(Inner::default()), None, [input.clone()]).unwrap();
        assert!(reactor.head_object("s3://b/k").wait().is_err());
        assert!(reactor.list_prefix("s3://b/").wait().is_err());
        input.close();
    }

    #[test]
    fn the_layer_is_written_from_the_first_sector_and_flushed() {
        let arena = arena();
        let (out, model) = disk("disk.out", 16 * 1024, true);
        let dir = std::env::temp_dir().join(format!("moruna-layer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.layer");
        let bytes: Vec<u8> = (0..3 * (1 << 20) + 123).map(|i| (i % 241) as u8).collect();
        std::fs::write(&file, &bytes).unwrap();

        let written = layer::write_file(&out, &file, arena.as_ref(), Tier::Host, layer::PIECE)
            .expect("written");
        assert_eq!(written.bytes, bytes.len() as u64);
        assert_eq!(written.on_disk, (bytes.len() as u64).next_multiple_of(512));
        assert_eq!(written.pieces, 4);
        {
            let m = model.lock().unwrap();
            assert_eq!(m.disk[..bytes.len()], bytes[..]);
            assert!(
                m.disk[bytes.len()..written.on_disk as usize]
                    .iter()
                    .all(|b| *b == 0)
            );
        }
        assert!(arena.stats().host_in_use == 0, "both buffers went back");

        let big = dir.join("big.layer");
        std::fs::write(&big, vec![1u8; 16 * 1024 * 512 + 1]).unwrap();
        let e =
            layer::write_file(&out, &big, arena.as_ref(), Tier::Host, layer::PIECE).unwrap_err();
        assert!(e.contains("do not fit"), "{e}");
        assert!(layer::write_file(&out, &file, arena.as_ref(), Tier::Host, 100).is_err());
        assert!(
            layer::write_file(
                &out,
                &dir.join("none"),
                arena.as_ref(),
                Tier::Host,
                layer::PIECE
            )
            .is_err()
        );
        // A disk that refuses writes refuses the layer.
        let (input, _) = disk("disk.in.ro", 16 * 1024, false);
        assert!(
            layer::write_file(&input, &file, arena.as_ref(), Tier::Host, layer::PIECE).is_err()
        );
        input.close();
        out.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_was_granted_wraps_the_reactor_and_says_what_it_moved() {
        assert!(
            crate::blk::Granted::take().unwrap().is_none(),
            "off the kernel"
        );
        let (input, _) = disk("disk.in.g", 64, false);
        let (output, _) = disk("disk.out", 64, true);
        let objects = crate::arena::Objects::with_max(1 << 20);
        let none = crate::blk::Granted::new(objects.clone(), Vec::new(), None);
        let inner = Arc::new(Inner::default());
        let (r, m) = none.reactor(inner.clone(), None).unwrap();
        assert!(m.is_none());
        r.shutdown();
        assert_eq!(inner.calls.lock().unwrap().as_slice(), &["shutdown"]);
        assert_eq!(none.backing().region_max(), 1 << 20);
        assert!(none.output().is_none());

        let granted = crate::blk::Granted::new(objects, vec![input.clone()], Some(output.clone()));
        assert_eq!(granted.inputs().len(), 1);
        assert_eq!(granted.output().unwrap().name(), "disk.out");
        let (r, m) = granted.reactor(inner, None).unwrap();
        assert_eq!(
            m.unwrap().head_object("disk://g").wait().unwrap().size,
            64 * 512
        );
        let notes = granted.notes();
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[0].contains("0 granted memory objects"));
        assert!(notes[1].contains("disk.in.g") && notes[2].contains("disk.out"));
        r.shutdown();
        output.close();
    }
}
