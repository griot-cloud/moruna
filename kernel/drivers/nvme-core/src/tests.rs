//! DR-T20 to DR-T24 (SDD 25, k): the controller's registers, commands and statuses,
//! queue pairs, PRP lists and Identify, and the engine, against a model controller
//! that keeps its registers in a window and reads and writes "device memory" through
//! the addresses the driver gave it, as a controller does by DMA. The model can be
//! told to misbehave in each way a device can: never become ready, report a fatal
//! status, fail or swallow a command, name a command or a queue that is not in flight,
//! report a submission head outside the ring, grant more queues than asked.

use alloc::{collections::BTreeMap, rc::Rc, vec, vec::Vec};
use core::cell::{Cell, RefCell};

use moruna_abi::Error;
use moruna_mmio::{Clock, Fake, Region};

use crate::{
    CQE, Command, Completion, NvmeError, PAGE, QUEUE_ENTRIES, QueuePair, SQE, Status,
    command::{admin, cns, nvm},
    controller::{Cap, Controller, cc, csts, reg},
    engine::{Dma, Engine, Io, IoOp, Settings},
    identify::{ControllerId, NamespaceId, active_namespaces, block_bytes},
    prp,
};

/// A page of "device memory", shared by the driver's mapping and the model.
#[derive(Clone)]
struct Page(Rc<Fake>);

impl Region for Page {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read8(&self, o: usize) -> Result<u8, Error> {
        self.0.read8(o)
    }
    fn read16(&self, o: usize) -> Result<u16, Error> {
        self.0.read16(o)
    }
    fn read32(&self, o: usize) -> Result<u32, Error> {
        self.0.read32(o)
    }
    fn read64(&self, o: usize) -> Result<u64, Error> {
        self.0.read64(o)
    }
    fn write8(&self, o: usize, v: u8) -> Result<(), Error> {
        self.0.write8(o, v)
    }
    fn write16(&self, o: usize, v: u16) -> Result<(), Error> {
        self.0.write16(o, v)
    }
    fn write32(&self, o: usize, v: u32) -> Result<(), Error> {
        self.0.write32(o, v)
    }
    fn write64(&self, o: usize, v: u64) -> Result<(), Error> {
        self.0.write64(o, v)
    }
}

/// The device's address space: pages by address.
#[derive(Default)]
struct Memory {
    pages: RefCell<BTreeMap<u64, Rc<Fake>>>,
    next: Cell<u64>,
}

impl Memory {
    fn alloc(&self) -> (Page, u64) {
        let addr = 0x10_0000 + self.next.get() * PAGE as u64;
        self.next.set(self.next.get() + 1);
        let page = Rc::new(Fake::new(PAGE));
        self.pages.borrow_mut().insert(addr, page.clone());
        (Page(page), addr)
    }

    fn page(&self, addr: u64) -> Rc<Fake> {
        self.pages
            .borrow()
            .get(&(addr & !0xfff))
            .cloned()
            .unwrap_or_else(|| panic!("the driver gave the device {addr:#x}, which is no page"))
    }

    fn read(&self, addr: u64, n: usize) -> Vec<u8> {
        self.page(addr).peek((addr & 0xfff) as usize, n)
    }

    fn write(&self, addr: u64, bytes: &[u8]) {
        self.page(addr).poke((addr & 0xfff) as usize, bytes);
    }
}

struct TestDma(Rc<Memory>);

impl Dma for TestDma {
    type Region = Page;
    fn page(&mut self) -> Result<(Page, u64), NvmeError> {
        Ok(self.0.alloc())
    }
}

#[derive(Clone, Copy)]
struct ModelSq {
    addr: u64,
    size: u16,
    cq: u16,
    head: u16,
}

#[derive(Clone, Copy)]
struct ModelCq {
    addr: u64,
    size: u16,
    tail: u16,
    phase: bool,
    vector: u16,
}

/// How the model misbehaves.
#[derive(Clone, Copy, Default)]
struct Faults {
    never_ready: bool,
    fatal_on_enable: bool,
    /// Admin commands are never completed.
    ignore_admin: bool,
    /// I/O commands are taken off the queue and never completed.
    swallow_io: bool,
    /// The next I/O completion names command identifier + 7.
    wrong_cid: bool,
    /// The next I/O completion names another submission queue.
    wrong_sq: bool,
    /// The next I/O completion reports a submission head past the tail.
    bad_head: bool,
    /// I/O commands fail with this status code (generic type).
    fail_io: Option<u8>,
    /// Admin opcode that fails with Invalid Field.
    fail_admin: Option<u8>,
    /// Set Features grants this many queues (zero based in both halves), whatever was
    /// asked.
    grant: Option<u32>,
}

struct State {
    enabled: bool,
    shut_down: bool,
    sqs: BTreeMap<u16, ModelSq>,
    cqs: BTreeMap<u16, ModelCq>,
    disk: Vec<u8>,
    block_shift: u8,
    flushes: usize,
    faults: Faults,
    max_queues: u32,
    /// MSI-X entries raised, in order.
    raised: Vec<u16>,
    controller_page: Vec<u8>,
    namespace_page: Vec<u8>,
}

struct Model {
    bar: Rc<Fake>,
    mem: Rc<Memory>,
    s: RefCell<State>,
}

const BAR: usize = 0x2000;
const STRIDE: u8 = 1;
const BLOCKS: u64 = 2048;

fn controller_page(write_cache: bool) -> Vec<u8> {
    let mut b = vec![0u8; PAGE];
    b[0..2].copy_from_slice(&0x1b36u16.to_le_bytes());
    b[4..24].copy_from_slice(b"MODEL-SERIAL-0001   ");
    let mut model = [b' '; 40];
    model[..11].copy_from_slice(b"Model NVMe ");
    b[24..64].copy_from_slice(&model);
    b[64..72].copy_from_slice(b"1.0     ");
    b[77] = 5; // 32 pages a transfer
    b[111] = 1;
    b[512] = 0x66;
    b[513] = 0x44;
    b[516..520].copy_from_slice(&4u32.to_le_bytes());
    b[525] = write_cache as u8;
    b
}

fn namespace_page(blocks: u64, shift: u8) -> Vec<u8> {
    let mut b = vec![0u8; PAGE];
    b[0..8].copy_from_slice(&blocks.to_le_bytes());
    b[8..16].copy_from_slice(&blocks.to_le_bytes());
    b[16..24].copy_from_slice(&blocks.to_le_bytes());
    b[25] = 1; // two formats
    b[26] = 1; // the second in use
    b[128..132].copy_from_slice(&(9u32 << 16).to_le_bytes());
    b[132..136].copy_from_slice(&((shift as u32) << 16).to_le_bytes());
    b
}

impl Model {
    fn new(mem: Rc<Memory>) -> Rc<Model> {
        let bar = Rc::new(Fake::new(BAR));
        // MQES 255, CQR, TO 1 (500 ms), DSTRD 1, CSS NVM, MPSMIN 0.
        let cap: u64 = 255 | 1 << 16 | 1 << 24 | (STRIDE as u64) << 32 | 1 << 37;
        bar.poke(reg::CAP, &cap.to_le_bytes());
        bar.poke(reg::VS, &0x0001_0400u32.to_le_bytes());
        let disk = (0..BLOCKS as usize * 512)
            .map(|i| (i % 251) as u8)
            .collect();
        Rc::new(Model {
            bar,
            mem,
            s: RefCell::new(State {
                enabled: false,
                shut_down: false,
                sqs: BTreeMap::new(),
                cqs: BTreeMap::new(),
                disk,
                block_shift: 9,
                flushes: 0,
                faults: Faults::default(),
                max_queues: 64,
                raised: Vec::new(),
                controller_page: controller_page(true),
                namespace_page: namespace_page(BLOCKS, 9),
            }),
        })
    }

    fn reg32(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.bar.peek(at, 4).try_into().expect("4"))
    }

    fn set_csts(&self, value: u32) {
        self.bar.poke(reg::CSTS, &value.to_le_bytes());
    }

    fn doorbell(qid: u16, cq: bool) -> usize {
        reg::DOORBELLS + (2 * qid as usize + cq as usize) * (4 << STRIDE)
    }

    /// Does what the controller would have done by now: follows CC and takes every
    /// command its submission doorbells publish.
    fn step(&self) {
        let cc = self.reg32(reg::CC);
        let mut s = self.s.borrow_mut();
        if cc & cc::EN != 0 && !s.enabled {
            if s.faults.fatal_on_enable {
                self.set_csts(csts::CFS);
                return;
            }
            if s.faults.never_ready {
                return;
            }
            let aqa = self.reg32(reg::AQA);
            let asq = self.reg32(reg::ASQ) as u64 | (self.reg32(reg::ASQ + 4) as u64) << 32;
            let acq = self.reg32(reg::ACQ) as u64 | (self.reg32(reg::ACQ + 4) as u64) << 32;
            let sq_size = (aqa & 0xfff) as u16 + 1;
            let cq_size = ((aqa >> 16) & 0xfff) as u16 + 1;
            s.sqs.insert(
                0,
                ModelSq {
                    addr: asq,
                    size: sq_size,
                    cq: 0,
                    head: 0,
                },
            );
            s.cqs.insert(
                0,
                ModelCq {
                    addr: acq,
                    size: cq_size,
                    tail: 0,
                    phase: true,
                    vector: 0,
                },
            );
            // A reset controller's doorbells read zero.
            for at in (reg::DOORBELLS..BAR).step_by(4) {
                self.bar.poke(at, &[0; 4]);
            }
            s.enabled = true;
            self.set_csts(csts::RDY);
        } else if cc & cc::EN == 0 && s.enabled {
            s.enabled = false;
            s.sqs.clear();
            s.cqs.clear();
            self.set_csts(0);
            return;
        }
        if !s.enabled {
            return;
        }
        if cc & cc::SHN_MASK != 0 {
            s.shut_down = true;
            self.set_csts(csts::RDY | csts::SHST_COMPLETE);
            return;
        }
        let ids: Vec<u16> = s.sqs.keys().copied().collect();
        for qid in ids {
            let tail = self.reg32(Self::doorbell(qid, false)) as u16;
            // A queue deleted by an admin command this step takes nothing more.
            while let Some(sq) = s.sqs.get(&qid).copied() {
                if sq.head == tail || tail >= sq.size {
                    break;
                }
                let entry = self.mem.read(sq.addr + sq.head as u64 * SQE as u64, SQE);
                let head = (sq.head + 1) % sq.size;
                s.sqs.get_mut(&qid).expect("sq").head = head;
                let dw =
                    |i: usize| u32::from_le_bytes(entry[i * 4..i * 4 + 4].try_into().expect("4"));
                let (opcode, cid) = (dw(0) as u8, (dw(0) >> 16) as u16);
                let result = if qid == 0 {
                    if s.faults.ignore_admin {
                        continue;
                    }
                    self.admin(&mut s, opcode, &dw)
                } else {
                    if s.faults.swallow_io {
                        continue;
                    }
                    self.io(&mut s, opcode, &dw)
                };
                let (dw0, status) = match result {
                    Ok(dw0) => (dw0, 0u16),
                    Err(sc) => (0, sc),
                };
                self.post(&mut s, qid, sq.cq, cid, head, dw0, status);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn post(&self, s: &mut State, qid: u16, cqid: u16, cid: u16, head: u16, dw0: u32, status: u16) {
        let (mut cid, mut sq_id, mut head) = (cid, qid, head);
        if qid != 0 {
            if core::mem::take(&mut s.faults.wrong_cid) {
                cid += 7;
            }
            if core::mem::take(&mut s.faults.wrong_sq) {
                sq_id += 1;
            }
            if core::mem::take(&mut s.faults.bad_head) {
                head = (head + 5) % s.sqs[&qid].size;
            }
        }
        let cq = s.cqs.get_mut(&cqid).expect("cq");
        let mut e = [0u8; CQE];
        e[0..4].copy_from_slice(&dw0.to_le_bytes());
        e[8..10].copy_from_slice(&head.to_le_bytes());
        e[10..12].copy_from_slice(&sq_id.to_le_bytes());
        let dw3 = cid as u32 | (cq.phase as u32) << 16 | (status as u32) << 17;
        e[12..16].copy_from_slice(&dw3.to_le_bytes());
        self.mem.write(cq.addr + cq.tail as u64 * CQE as u64, &e);
        cq.tail += 1;
        if cq.tail == cq.size {
            cq.tail = 0;
            cq.phase = !cq.phase;
        }
        let vector = cq.vector;
        s.raised.push(vector);
    }

    fn admin(&self, s: &mut State, opcode: u8, dw: &dyn Fn(usize) -> u32) -> Result<u32, u16> {
        if s.faults.fail_admin == Some(opcode) {
            return Err(0x02);
        }
        let prp1 = dw(6) as u64 | (dw(7) as u64) << 32;
        match opcode {
            admin::IDENTIFY => {
                let page = match (dw(10), dw(1)) {
                    (cns::CONTROLLER, _) => s.controller_page.clone(),
                    (cns::ACTIVE_NAMESPACES, _) => {
                        let mut p = vec![0u8; PAGE];
                        p[0..4].copy_from_slice(&1u32.to_le_bytes());
                        p[4..8].copy_from_slice(&3u32.to_le_bytes());
                        p
                    }
                    (cns::NAMESPACE, 1) => s.namespace_page.clone(),
                    _ => return Err(0x0b),
                };
                self.mem.write(prp1, &page);
                Ok(0)
            }
            admin::SET_FEATURES if dw(10) == crate::command::FEATURE_QUEUES => {
                let asked = dw(11) & 0xffff;
                let n = s.faults.grant.unwrap_or(asked.min(s.max_queues - 1));
                Ok(n << 16 | n)
            }
            admin::CREATE_CQ => {
                let qid = dw(10) as u16;
                s.cqs.insert(
                    qid,
                    ModelCq {
                        addr: prp1,
                        size: (dw(10) >> 16) as u16 + 1,
                        tail: 0,
                        phase: true,
                        vector: (dw(11) >> 16) as u16,
                    },
                );
                Ok(0)
            }
            admin::CREATE_SQ => {
                let qid = dw(10) as u16;
                let cq = (dw(11) >> 16) as u16;
                if !s.cqs.contains_key(&cq) {
                    return Err(0x100);
                }
                s.sqs.insert(
                    qid,
                    ModelSq {
                        addr: prp1,
                        size: (dw(10) >> 16) as u16 + 1,
                        cq,
                        head: 0,
                    },
                );
                Ok(0)
            }
            admin::DELETE_SQ => {
                s.sqs.remove(&(dw(10) as u16));
                Ok(0)
            }
            admin::DELETE_CQ => {
                s.cqs.remove(&(dw(10) as u16));
                Ok(0)
            }
            _ => Err(0x01),
        }
    }

    /// The pages a command's PRP entries name, for `bytes`.
    fn pages(&self, dw: &dyn Fn(usize) -> u32, bytes: usize) -> Vec<u64> {
        let prp1 = dw(6) as u64 | (dw(7) as u64) << 32;
        let prp2 = dw(8) as u64 | (dw(9) as u64) << 32;
        let n = bytes.div_ceil(PAGE);
        match n {
            1 => vec![prp1],
            2 => vec![prp1, prp2],
            _ => {
                let list = self.mem.read(prp2, (n - 1) * 8);
                let mut pages = vec![prp1];
                pages.extend(
                    list.chunks(8)
                        .map(|c| u64::from_le_bytes(c.try_into().expect("8"))),
                );
                pages
            }
        }
    }

    fn io(&self, s: &mut State, opcode: u8, dw: &dyn Fn(usize) -> u32) -> Result<u32, u16> {
        if let Some(sc) = s.faults.fail_io {
            return Err(sc as u16);
        }
        if dw(1) != 1 {
            return Err(0x0b);
        }
        let block = 1usize << s.block_shift;
        let lba = dw(10) as u64 | (dw(11) as u64) << 32;
        let blocks = (dw(12) & 0xffff) as usize + 1;
        let (at, bytes) = (lba as usize * block, blocks * block);
        match opcode {
            nvm::FLUSH => {
                s.flushes += 1;
                Ok(0)
            }
            nvm::READ | nvm::WRITE => {
                if at + bytes > s.disk.len() {
                    return Err(0x80);
                }
                for (i, page) in self.pages(dw, bytes).into_iter().enumerate() {
                    let from = at + i * PAGE;
                    let n = PAGE.min(at + bytes - from);
                    if opcode == nvm::READ {
                        self.mem.write(page, &s.disk[from..from + n]);
                    } else {
                        let data = self.mem.read(page, n);
                        s.disk[from..from + n].copy_from_slice(&data);
                    }
                }
                Ok(0)
            }
            _ => Err(0x01),
        }
    }
}

struct TestClock {
    model: Rc<Model>,
    t: Cell<u64>,
}

impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.t.get()
    }
    fn pause(&self) {
        self.t.set(self.t.get() + 1_000_000);
        self.model.step();
    }
}

struct Rig {
    mem: Rc<Memory>,
    model: Rc<Model>,
    clock: TestClock,
    dma: TestDma,
}

fn rig() -> Rig {
    let mem = Rc::new(Memory::default());
    let model = Model::new(mem.clone());
    Rig {
        clock: TestClock {
            model: model.clone(),
            t: Cell::new(0),
        },
        dma: TestDma(mem.clone()),
        mem,
        model,
    }
}

const SETTINGS: Settings = Settings {
    queues: 4,
    vectors: 2,
    timeout_ns: 30_000_000_000,
};

fn start(r: &mut Rig, settings: Settings) -> Result<(Engine<Page, Page, u32>, u32), NvmeError> {
    let resets = Rc::new(Cell::new(0u32));
    let counted = resets.clone();
    let e = Engine::start(
        Page(r.model.bar.clone()),
        &mut r.dma,
        &r.clock,
        settings,
        &mut || {
            counted.set(counted.get() + 1);
            Ok(())
        },
    )?;
    Ok((e, resets.get()))
}

/// Buffers of `pages` pages in device memory.
fn buffer(r: &Rig, pages: usize) -> Vec<u64> {
    (0..pages).map(|_| r.mem.alloc().1).collect()
}

fn run(r: &Rig, e: &mut Engine<Page, Page, u32>) -> Vec<(u32, Result<(), Status>)> {
    r.model.step();
    let mut out = Vec::new();
    e.complete(None, &mut out).expect("completions");
    out
}

#[test]
fn dr_t20_capabilities_and_the_controllers_refusals() {
    let cap = Cap::parse(0x0020_0020_0f01_03ff);
    assert_eq!(cap.max_entries, 1024);
    assert!(cap.contiguous);
    assert_eq!(cap.timeout_ns, 15 * 500_000_000);
    assert_eq!(cap.stride, 0);
    assert!(cap.nvm);
    assert_eq!(cap.mps_min, 0);
    assert_eq!(cap.mps_max, 2);
    let bar = |cap: u64, len: usize| {
        let f = Fake::new(len);
        f.poke(reg::CAP, &cap.to_le_bytes());
        f
    };
    let nvm = 1u64 << 37 | 63;
    let c = Controller::new(bar(nvm, BAR)).expect("controller");
    assert_eq!(c.doorbell(0, false), Ok(0x1000));
    assert_eq!(c.doorbell(3, true), Ok(0x1000 + 7 * 4));
    assert!(matches!(
        c.doorbell(1000, true),
        Err(NvmeError::Unsupported(_))
    ));
    let wide = Controller::new(bar(nvm | 2 << 32, BAR)).expect("stride");
    assert_eq!(wide.doorbell(1, false), Ok(0x1000 + 2 * 16));
    assert!(matches!(
        Controller::new(bar(63, BAR)),
        Err(NvmeError::Unsupported(s)) if s.contains("NVM command set")
    ));
    assert!(matches!(
        Controller::new(bar(nvm | 1 << 48, BAR)),
        Err(NvmeError::Unsupported(s)) if s.contains("8192")
    ));
    assert!(matches!(
        Controller::new(bar(nvm, 0x1000)),
        Err(NvmeError::Unsupported(s)) if s.contains("too small")
    ));
    assert!(matches!(
        Controller::new(bar(1 << 37, BAR)),
        Err(NvmeError::Malformed(_))
    ));
    let v = bar(nvm, BAR);
    v.poke(reg::VS, &0x0002_0001u32.to_le_bytes());
    assert_eq!(Controller::new(v).expect("v").version(), Ok((2, 0, 1)));
}

#[test]
fn dr_t20_commands_encode_and_statuses_decode() {
    let sq = Fake::new(SQE);
    Command::create_cq(3, 64, 0x1234_5000, 2)
        .write(&sq, 0, 9)
        .expect("write");
    assert_eq!(sq.read32(0), Ok(admin::CREATE_CQ as u32 | 9 << 16));
    assert_eq!(sq.read32(24), Ok(0x1234_5000));
    assert_eq!(sq.read32(40), Ok(63 << 16 | 3));
    assert_eq!(sq.read32(44), Ok(2 << 16 | 3));
    let c = Command::create_sq(3, 64, 0x9000, 3);
    assert_eq!(c.cdw11, 3 << 16 | 0b011);
    let t = Command::transfer(true, 1, 0x1_0000_0002, 8, (0xa000, 0xb000));
    assert_eq!(
        (t.opcode, t.cdw10, t.cdw11, t.cdw12, t.prp2),
        (nvm::WRITE, 2, 1, 7, 0xb000)
    );
    assert_eq!(Command::transfer(false, 1, 0, 1, (0, 0)).opcode, nvm::READ);
    assert_eq!(Command::set_queue_count(4).cdw11, 3 << 16 | 3);
    assert_eq!(Command::set_queue_count(0).cdw11, 0);
    assert_eq!(Command::identify(cns::NAMESPACE, 1, 0x7000).nsid, 1);
    assert_eq!(Command::flush(1).opcode, nvm::FLUSH);
    let ok = Status::from_dword(0x0001_0005);
    assert!(ok.ok());
    let lba = Status::from_dword(0x80 << 17 | 1 << 31);
    assert_eq!((lba.sct, lba.sc, lba.dnr), (0, 0x80, true));
    assert_eq!(lba.name(), "LBA out of range");
    assert!(alloc::format!("{lba}").contains("do not retry"));
    let media = Status::from_dword((0x281u32) << 17);
    assert_eq!((media.sct, media.sc), (2, 0x81));
    assert_eq!(media.name(), "unrecovered read error");
    assert_eq!(Status::from_dword(0x7ff << 17).name(), "unnamed status");
    let cq = Fake::new(CQE);
    cq.poke(0, &0xdead_beefu32.to_le_bytes());
    cq.poke(8, &(5u32 << 16 | 12).to_le_bytes());
    cq.poke(12, &(1u32 << 16 | 33).to_le_bytes());
    assert_eq!(Completion::phase(&cq, 0), Ok(true));
    let c = Completion::read(&cq, 0).expect("read");
    assert_eq!((c.dw0, c.sq_head, c.sq_id, c.cid), (0xdead_beef, 12, 5, 33));
}

/// The controller's side of a queue pair, for queue tests: completes entries with
/// whatever the test says.
fn complete(cq: &Fake, slot: u16, phase: bool, sq_id: u16, head: u16, cid: u16) {
    let at = slot as usize * CQE;
    cq.poke(at + 8, &((sq_id as u32) << 16 | head as u32).to_le_bytes());
    cq.poke(at + 12, &((phase as u32) << 16 | cid as u32).to_le_bytes());
}

fn pair(size: u16) -> (QueuePair<Page>, Rc<Fake>, Rc<Fake>) {
    let (sq, cq) = (Rc::new(Fake::new(PAGE)), Rc::new(Fake::new(PAGE)));
    let q = QueuePair::new(2, size, Page(sq.clone()), Page(cq.clone())).expect("pair");
    (q, sq, cq)
}

#[test]
fn dr_t21_a_queue_pair_submits_completes_wraps_and_fills() {
    let (mut q, sq, cq) = pair(4);
    assert!(QueuePair::new(1, 65, Page(sq.clone()), Page(cq.clone())).is_err());
    assert!(QueuePair::new(1, 1, Page(sq.clone()), Page(cq.clone())).is_err());
    assert_eq!(q.pop(), Ok(None));
    let mut phase = true;
    let mut slot = 0;
    for round in 0..10u16 {
        let (cid, tail) = q.submit(&Command::flush(1)).expect("submit");
        assert_eq!(tail, (round + 1) % 4);
        assert_eq!(sq.read32(round as usize % 4 * SQE), Ok((cid as u32) << 16));
        complete(&cq, slot, phase, 2, tail, cid);
        let c = q.pop().expect("pop").expect("one");
        assert_eq!(c.cid, cid);
        assert_eq!(q.pop(), Ok(None));
        slot += 1;
        if slot == 4 {
            slot = 0;
            phase = !phase;
        }
        assert_eq!(q.cq_head(), slot);
    }
    // Three commands fill a queue of four.
    for _ in 0..3 {
        q.submit(&Command::flush(1)).expect("submit");
    }
    assert!(!q.has_room());
    assert_eq!(q.in_flight(), 3);
    assert_eq!(q.submit(&Command::flush(1)), Err(NvmeError::Full));
    q.reset().expect("reset");
    assert_eq!(q.in_flight(), 0);
    assert_eq!(q.next_cid(), Some(0));
}

#[test]
fn dr_t21_a_completion_is_checked_before_it_is_believed() {
    let cases: [(&str, u16, u16); 4] = [
        ("submission queue 3", 3, 1),
        ("not in flight", 2, 99),
        ("not in flight", 2, 2),
        ("submission head", 2, 0),
    ];
    for (words, sq_id, cid_or_head) in cases {
        let (mut q, _, cq) = pair(8);
        let (cid, tail) = q.submit(&Command::flush(1)).expect("submit");
        let (cid, head) = match words {
            "not in flight" => (cid_or_head, tail),
            // A head past the tail: the device says it read what was never written.
            "submission head" => (cid, tail + 3),
            _ => (cid, tail),
        };
        complete(&cq, 0, true, sq_id, head, cid);
        match q.pop() {
            Err(NvmeError::Malformed(why)) => assert!(why.contains(words), "{why}"),
            other => panic!("{words}: {other:?}"),
        }
    }
    let (mut q, _, cq) = pair(8);
    let (cid, _) = q.submit(&Command::flush(1)).expect("submit");
    complete(&cq, 0, true, 2, 40, cid);
    assert!(matches!(q.pop(), Err(NvmeError::Malformed(w)) if w.contains("40")));
    // The same identifier twice: the second names a command no longer in flight.
    let (mut q, _, cq) = pair(8);
    let (cid, tail) = q.submit(&Command::flush(1)).expect("submit");
    complete(&cq, 0, true, 2, tail, cid);
    complete(&cq, 1, true, 2, tail, cid);
    assert!(q.pop().expect("first").is_some());
    assert!(matches!(q.pop(), Err(NvmeError::Malformed(_))));
}

#[test]
fn dr_t22_prp_entries_name_every_page() {
    let list = Fake::new(PAGE);
    assert_eq!(prp::entries(&[0x1000], &list, 0x9000), Ok((0x1000, 0)));
    assert_eq!(
        prp::entries(&[0x1000, 0x5000], &list, 0x9000),
        Ok((0x1000, 0x5000))
    );
    let pages: Vec<u64> = (0..256).map(|i| 0x10_0000 + i * 0x3000).collect();
    assert_eq!(prp::entries(&pages, &list, 0x9000), Ok((0x10_0000, 0x9000)));
    for i in 0..255 {
        assert_eq!(list.read64(i * 8), Ok(pages[i + 1]));
    }
    assert!(prp::entries(&[], &list, 0x9000).is_err());
    assert!(prp::entries(&[0x1004], &list, 0x9000).is_err());
    let many: Vec<u64> = (0..257).map(|i| i * 0x1000).collect();
    assert!(prp::entries(&many, &list, 0x9000).is_err());
    assert!(prp::entries(&pages[..3], &list, 0x9010).is_err());
    assert!(prp::entries(&pages[..3], &Fake::new(8), 0x9000).is_err());
}

#[test]
fn dr_t22_identify_is_checked() {
    let c = ControllerId::parse(&controller_page(true)).expect("controller");
    assert_eq!(c.vendor, 0x1b36);
    assert_eq!(c.serial, "MODEL-SERIAL-0001");
    assert_eq!(c.model, "Model NVMe");
    assert_eq!(c.firmware, "1.0");
    assert_eq!(c.max_pages, 32);
    assert!(c.write_cache);
    assert_eq!(c.namespaces, 4);
    let with = |at: usize, v: u8| {
        let mut p = controller_page(false);
        p[at] = v;
        ControllerId::parse(&p)
    };
    assert_eq!(with(77, 0).expect("mdts 0").max_pages, 256);
    assert_eq!(with(77, 12).expect("mdts 12").max_pages, 256);
    assert_eq!(with(77, 8).expect("mdts 8").max_pages, 256);
    assert_eq!(with(77, 1).expect("mdts 1").max_pages, 2);
    assert!(!with(77, 1).expect("cache").write_cache);
    assert!(matches!(with(111, 2), Err(NvmeError::Unsupported(_))));
    assert!(matches!(with(512, 0x77), Err(NvmeError::Unsupported(_))));
    assert!(matches!(with(513, 0x33), Err(NvmeError::Unsupported(_))));
    assert!(ControllerId::parse(&[0u8; 100]).is_err());
    let mut weird = controller_page(false);
    weird[4] = 0x01;
    assert!(
        ControllerId::parse(&weird)
            .expect("text")
            .serial
            .starts_with('?')
    );

    let mut list = vec![0u8; PAGE];
    for (i, id) in [1u32, 2, 7].iter().enumerate() {
        list[i * 4..i * 4 + 4].copy_from_slice(&id.to_le_bytes());
    }
    assert_eq!(active_namespaces(&list, 8), Ok(vec![1, 2, 7]));
    assert!(matches!(
        active_namespaces(&list, 4),
        Err(NvmeError::Malformed(_))
    ));
    list[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert!(matches!(
        active_namespaces(&list, 8),
        Err(NvmeError::Malformed(_))
    ));
    assert_eq!(active_namespaces(&[0u8; PAGE], 8), Ok(vec![]));

    let n = NamespaceId::parse(&namespace_page(1000, 12)).expect("namespace");
    assert_eq!((n.blocks, n.block_shift, n.read_only), (1000, 12, false));
    assert_eq!(block_bytes(&n), 4096);
    let ns = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut p = namespace_page(1000, 9);
        edit(&mut p);
        NamespaceId::parse(&p)
    };
    assert!(ns(&|p| p[99] = 1).expect("ro").read_only);
    assert!(matches!(
        ns(&|p| p[0..8].fill(0)),
        Err(NvmeError::Unsupported(_))
    ));
    assert!(matches!(ns(&|p| p[8] = 0xff), Err(NvmeError::Malformed(_))));
    assert!(matches!(ns(&|p| p[26] = 5), Err(NvmeError::Malformed(_))));
    assert!(
        matches!(ns(&|p| p[132] = 8), Err(NvmeError::Unsupported(w)) if w.contains("metadata"))
    );
    assert!(matches!(
        ns(&|p| p[134] = 13),
        Err(NvmeError::Unsupported(_))
    ));
    assert!(matches!(
        ns(&|p| p[134] = 8),
        Err(NvmeError::Unsupported(_))
    ));
    // Formats past 16 take FLBAS bits 5 and 6 for the upper index.
    let high = ns(&|p| {
        p[25] = 19;
        p[26] = 0b0010_0001;
        p[128 + 4 * 17..128 + 4 * 18].copy_from_slice(&(10u32 << 16).to_le_bytes());
    });
    assert_eq!(high.expect("format 17").block_shift, 10);
    let huge = ns(&|p| {
        p[0..8].copy_from_slice(&u64::MAX.to_le_bytes());
        p[8..16].copy_from_slice(&1u64.to_le_bytes());
    });
    assert!(matches!(huge, Err(NvmeError::Malformed(_))));
}

#[test]
fn dr_t23_the_engine_brings_the_controller_up_and_moves_data() {
    let mut r = rig();
    let (mut e, resets) = start(&mut r, SETTINGS).expect("start");
    assert_eq!(resets, 1);
    assert_eq!(e.queues(), 4);
    assert_eq!(e.vectors_used(), 2);
    assert_eq!(e.namespaces, [1, 3]);
    assert_eq!(e.nsid, 1);
    assert_eq!(e.ns.blocks, BLOCKS);
    assert_eq!(e.id.max_pages, 32);
    {
        let s = r.model.s.borrow();
        assert_eq!(s.sqs.len(), 5);
        let vectors: Vec<u16> = (1..=4).map(|q| s.cqs[&q].vector).collect();
        assert_eq!(vectors, [0, 1, 0, 1]);
        assert!(s.sqs.values().all(|q| q.size == QUEUE_ENTRIES));
    }
    // A read of 1, 2 and 8 pages: PRP1 alone, PRP1 and PRP2, and a list.
    for (tag, pages) in [(1u32, 1usize), (2, 2), (3, 8)] {
        let buf = buffer(&r, pages);
        let blocks = (pages * PAGE / 512) as u32;
        e.submit(
            Io {
                op: IoOp::Read,
                lba: 16,
                blocks,
                pages: buf.clone(),
                tag,
            },
            0,
        )
        .map_err(|(e, _)| e)
        .expect("submit");
        let done = run(&r, &mut e);
        assert_eq!(done, [(tag, Ok(()))]);
        let disk = r.model.s.borrow().disk.clone();
        for (i, page) in buf.iter().enumerate() {
            let at = 16 * 512 + i * PAGE;
            assert_eq!(r.mem.read(*page, PAGE), disk[at..at + PAGE], "page {i}");
        }
    }
    // A write lands on the disk; a flush reaches the controller.
    let buf = buffer(&r, 3);
    for (i, p) in buf.iter().enumerate() {
        r.mem.write(*p, &[0xa0 + i as u8; PAGE]);
    }
    e.submit(
        Io {
            op: IoOp::Write,
            lba: 100,
            blocks: 24,
            pages: buf,
            tag: 9,
        },
        0,
    )
    .map_err(|(e, _)| e)
    .expect("write");
    e.submit(
        Io {
            op: IoOp::Flush,
            lba: 0,
            blocks: 0,
            pages: Vec::new(),
            tag: 10,
        },
        0,
    )
    .map_err(|(e, _)| e)
    .expect("flush");
    let mut done = run(&r, &mut e);
    done.sort_by_key(|d| d.0);
    assert_eq!(done, [(9, Ok(())), (10, Ok(()))]);
    let s = r.model.s.borrow();
    assert_eq!(s.flushes, 1);
    assert!(
        s.disk[100 * 512..100 * 512 + PAGE]
            .iter()
            .all(|b| *b == 0xa0)
    );
    assert!(
        s.disk[100 * 512 + 2 * PAGE..100 * 512 + 3 * PAGE]
            .iter()
            .all(|b| *b == 0xa2)
    );
    drop(s);
    // Commands spread over the queues with the fewest in flight; completions come
    // back by the interrupt of each queue.
    r.model.s.borrow_mut().raised.clear();
    for tag in 20..28 {
        let buf = buffer(&r, 1);
        e.submit(
            Io {
                op: IoOp::Read,
                lba: 0,
                blocks: 8,
                pages: buf,
                tag,
            },
            0,
        )
        .map_err(|(e, _)| e)
        .expect("submit");
    }
    assert_eq!(e.in_flight(), 8);
    r.model.step();
    assert_eq!(r.model.s.borrow().raised.len(), 8);
    let mut first = Vec::new();
    e.complete(Some(0), &mut first).expect("vector 0");
    assert_eq!(first.len(), 4);
    let mut second = Vec::new();
    e.complete(Some(1), &mut second).expect("vector 1");
    assert_eq!(second.len(), 4);
    assert_eq!(e.in_flight(), 0);
    // A failed command hands its status back.
    r.model.s.borrow_mut().faults.fail_io = Some(0x81);
    let buf = buffer(&r, 1);
    e.submit(
        Io {
            op: IoOp::Read,
            lba: 0,
            blocks: 8,
            pages: buf,
            tag: 30,
        },
        0,
    )
    .map_err(|(e, _)| e)
    .expect("submit");
    let done = run(&r, &mut e);
    assert!(matches!(done[..], [(30, Err(s))] if s.sc == 0x81));
    // A transfer the namespace cannot hold, or whose pages do not cover it, is
    // refused before it reaches a queue.
    for (lba, blocks, pages) in [(BLOCKS - 1, 8u32, 1usize), (0, 9, 1), (0, 8, 2), (0, 0, 1)] {
        let io = Io {
            op: IoOp::Read,
            lba,
            blocks,
            pages: buffer(&r, pages),
            tag: 40,
        };
        assert!(matches!(
            e.submit(io, 0),
            Err((NvmeError::Unsupported(_), _))
        ));
    }
    // A normal shutdown deletes the queues and completes.
    e.shutdown(&r.clock).expect("shutdown");
    assert!(r.model.s.borrow().shut_down);
    assert_eq!(r.model.s.borrow().sqs.len(), 1);
}

#[test]
fn dr_t23_queues_fill_and_grants_are_bounded() {
    let mut r = rig();
    let one = Settings {
        queues: 1,
        vectors: 1,
        ..SETTINGS
    };
    let (mut e, _) = start(&mut r, one).expect("start");
    for tag in 0..63 {
        let io = Io {
            op: IoOp::Flush,
            lba: 0,
            blocks: 0,
            pages: Vec::new(),
            tag,
        };
        e.submit(io, 0).map_err(|(e, _)| e).expect("room");
    }
    assert!(!e.has_room());
    let io = Io {
        op: IoOp::Flush,
        lba: 0,
        blocks: 0,
        pages: Vec::new(),
        tag: 99,
    };
    match e.submit(io, 0) {
        Err((NvmeError::Full, back)) => assert_eq!(back.tag, 99),
        _ => panic!("a full queue took a command"),
    }
    assert_eq!(run(&r, &mut e).len(), 63);
    // A controller that grants more queues than asked gets only what was asked; one
    // that grants fewer gets what it granted.
    let mut r = rig();
    r.model.s.borrow_mut().faults.grant = Some(40);
    assert_eq!(start(&mut r, SETTINGS).expect("start").0.queues(), 4);
    let mut r = rig();
    r.model.s.borrow_mut().faults.grant = Some(1);
    assert_eq!(start(&mut r, SETTINGS).expect("start").0.queues(), 2);
    assert!(
        start(
            &mut rig(),
            Settings {
                vectors: 0,
                ..SETTINGS
            }
        )
        .is_err()
    );
}

#[test]
fn dr_t24_a_controller_that_fails_to_start_is_named() {
    let fault = |f: Faults| {
        let mut r = rig();
        r.model.s.borrow_mut().faults = f;
        start(&mut r, SETTINGS).map(|_| ()).expect_err("fails")
    };
    assert_eq!(
        fault(Faults {
            never_ready: true,
            ..Faults::default()
        }),
        NvmeError::Timeout("become ready")
    );
    assert_eq!(
        fault(Faults {
            fatal_on_enable: true,
            ..Faults::default()
        }),
        NvmeError::Fatal
    );
    assert_eq!(
        fault(Faults {
            ignore_admin: true,
            ..Faults::default()
        }),
        NvmeError::Timeout("Identify Controller")
    );
    assert!(matches!(
        fault(Faults {
            fail_admin: Some(admin::CREATE_SQ),
            ..Faults::default()
        }),
        NvmeError::Command("Create I/O Submission Queue", s) if s.sc == 2
    ));
    let mut r = rig();
    r.model.s.borrow_mut().namespace_page = namespace_page(BLOCKS, 13);
    assert!(matches!(
        start(&mut r, SETTINGS),
        Err(NvmeError::Unsupported(_))
    ));
    let mut r = rig();
    r.model.s.borrow_mut().controller_page[111] = 3;
    assert!(matches!(
        start(&mut r, SETTINGS),
        Err(NvmeError::Unsupported(_))
    ));
    // A controller left enabled by a previous driver is reset first.
    let mut r = rig();
    drop(start(&mut r, SETTINGS).expect("first"));
    let (e, resets) = start(&mut r, SETTINGS).expect("second");
    assert_eq!(resets, 1);
    assert_eq!(e.queues(), 4);
}

fn read(r: &Rig, tag: u32) -> Io<u32> {
    Io {
        op: IoOp::Read,
        lba: 8,
        blocks: 8,
        pages: buffer(r, 1),
        tag,
    }
}

#[test]
fn dr_t24_a_misbehaving_controller_is_caught_and_recovered() {
    for fault in ["cid", "sq", "head"] {
        let mut r = rig();
        let (mut e, _) = start(&mut r, SETTINGS).expect("start");
        {
            let f = &mut r.model.s.borrow_mut().faults;
            match fault {
                "cid" => f.wrong_cid = true,
                "sq" => f.wrong_sq = true,
                _ => f.bad_head = true,
            }
        }
        e.submit(read(&r, 1), 0)
            .map_err(|(e, _)| e)
            .expect("submit");
        r.model.step();
        let mut out = Vec::new();
        assert!(
            matches!(e.complete(None, &mut out), Err(NvmeError::Malformed(_))),
            "{fault}"
        );
        assert!(out.is_empty());
        // Recovery resets the controller, fails what was in flight, and serves again.
        let mut released = 0;
        let failed = e
            .recover(&r.clock, &mut || {
                released += 1;
                Ok(())
            })
            .expect("recover");
        assert_eq!((failed, released), (vec![1], 1));
        e.submit(read(&r, 2), 0)
            .map_err(|(e, _)| e)
            .expect("submit");
        assert_eq!(run(&r, &mut e), [(2, Ok(()))]);
    }
    // A command the controller never completes is overdue at its deadline.
    let mut r = rig();
    let (mut e, _) = start(&mut r, SETTINGS).expect("start");
    r.model.s.borrow_mut().faults.swallow_io = true;
    e.submit(read(&r, 5), 1_000)
        .map_err(|(e, _)| e)
        .expect("submit");
    e.submit(read(&r, 6), 2_000)
        .map_err(|(e, _)| e)
        .expect("submit");
    assert_eq!(run(&r, &mut e), []);
    assert_eq!(e.next_deadline(), Some(1_000 + SETTINGS.timeout_ns));
    assert_eq!(e.overdue(SETTINGS.timeout_ns), None);
    let late = e.overdue(SETTINGS.timeout_ns + 5_000).expect("overdue");
    assert_eq!((late.op, late.lba, late.late_ns), (IoOp::Read, 8, 4_000));
    r.model.s.borrow_mut().faults.swallow_io = false;
    let mut failed = e.recover(&r.clock, &mut || Ok(())).expect("recover");
    failed.sort();
    assert_eq!(failed, [5, 6]);
    assert_eq!(e.in_flight(), 0);
    assert_eq!(e.next_deadline(), None);
    e.submit(read(&r, 7), 0)
        .map_err(|(e, _)| e)
        .expect("submit");
    assert_eq!(run(&r, &mut e), [(7, Ok(()))]);
    // A fatal status is read, and a recovery that cannot make the controller ready
    // says so.
    r.model.set_csts(csts::CFS | csts::RDY);
    assert_eq!(e.fatal(), Ok(true));
    r.model.s.borrow_mut().faults.never_ready = true;
    assert_eq!(
        e.recover(&r.clock, &mut || Ok(())).map(|_| ()),
        Err(NvmeError::Timeout("become ready"))
    );
}
