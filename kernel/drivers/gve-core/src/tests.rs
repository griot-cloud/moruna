//! NT-T10 to NT-T15 (SDD 29, k): gVNIC against a model of the device, the only proof
//! there is short of Google Cloud (KE3). The model keeps the registers of BAR 0 and
//! the doorbells of BAR 2 in windows, executes admin commands, reads transmit
//! descriptors and writes receive descriptors through the addresses (or queue page
//! list offsets) the driver gave it, as the device does by DMA, and misbehaves on
//! demand.

use alloc::{collections::BTreeMap, rc::Rc, vec, vec::Vec};
use core::cell::{Cell, RefCell};

use moruna_abi::Error;
use moruna_mmio::{Clock, Fake, Region};

use crate::{
    Dma, GveError, PAGE,
    admin::{AdminQueue, COMMAND, Command, opcode, status},
    be32,
    describe::{Description, Format, option},
    device::{Gve, RX_CONT, RX_ERR, Settings, next_seqno},
    put_be32, regs,
};

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

/// Device memory: allocations, each a run of pages at consecutive addresses (which a
/// contiguous allocation needs and any other may have).
#[derive(Default)]
struct Memory {
    runs: RefCell<BTreeMap<u64, Rc<Fake>>>,
    next: Cell<u64>,
    contiguous: Cell<u32>,
}

impl Memory {
    fn run(&self, addr: u64) -> (Rc<Fake>, usize) {
        let runs = self.runs.borrow();
        let (base, run) = runs
            .range(..=addr)
            .next_back()
            .unwrap_or_else(|| panic!("the driver gave the device {addr:#x}, which is no memory"));
        let at = (addr - base) as usize;
        assert!(at < run.len(), "{addr:#x} is past its allocation");
        (run.clone(), at)
    }
    fn read(&self, addr: u64, n: usize) -> Vec<u8> {
        let (run, at) = self.run(addr);
        run.peek(at, n)
    }
    fn write(&self, addr: u64, bytes: &[u8]) {
        let (run, at) = self.run(addr);
        run.poke(at, bytes);
    }
}

struct TestDma(Rc<Memory>);

impl Dma for TestDma {
    type Region = Page;
    fn alloc(&mut self, bytes: usize, contiguous: bool) -> Result<(Page, Vec<u64>), GveError> {
        let m = &self.0;
        let pages = bytes.div_ceil(PAGE).max(1);
        // A gap between allocations, so an address past one is no memory.
        let base = 0x100_0000 + m.next.get() * PAGE as u64;
        m.next.set(m.next.get() + pages as u64 + 1);
        if contiguous {
            m.contiguous.set(m.contiguous.get() + 1);
        }
        let run = Rc::new(Fake::new(pages * PAGE));
        m.runs.borrow_mut().insert(base, run.clone());
        Ok((
            Page(run),
            (0..pages).map(|i| base + (i * PAGE) as u64).collect(),
        ))
    }
}

fn be32_of(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("4"))
}

fn be64_of(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().expect("8"))
}

#[derive(Clone, Copy, Default)]
struct Faults {
    /// The admin command with this opcode fails, invalid argument.
    fail: Option<u32>,
    /// The device never finishes a reset.
    stuck_reset: bool,
    /// Admin commands are never executed.
    ignore_admin: bool,
    /// The event counter jumps past what was issued.
    counter_jump: bool,
    /// The transmit event counter runs ahead of what was sent.
    tx_ahead: bool,
    /// Doorbell indices outside BAR 2.
    bad_doorbell: bool,
}

struct TxModel {
    ring: u64,
    size: u32,
    qpl: u32,
    counter: u32,
    db: u32,
    taken: u32,
}

struct RxModel {
    descriptors: u64,
    data: u64,
    size: u32,
    qpl: u32,
    db: u32,
    count: u32,
    seqno: u8,
}

struct State {
    aq: Option<u64>,
    executed: u32,
    descriptor: Vec<u8>,
    counters: u64,
    blocks: u32,
    format: u8,
    lists: BTreeMap<u32, Vec<u64>>,
    tx: BTreeMap<u32, TxModel>,
    rx: BTreeMap<u32, RxModel>,
    sent: Vec<Vec<u8>>,
    faults: Faults,
    commands: Vec<u32>,
}

struct Model {
    bar0: Rc<Fake>,
    bar2: Rc<Fake>,
    mem: Rc<Memory>,
    revision: u8,
    s: RefCell<State>,
}

const MAC: [u8; 6] = [0x42, 0x01, 0x0a, 0x80, 0x00, 0x02];

/// A descriptor: rings of 256, MTU 1460, 64 counters, 64 transmit pages a list, and
/// `options` (id, required features) after it.
fn descriptor(options: &[(u16, u32)]) -> Vec<u8> {
    let mut b = vec![0u8; PAGE];
    b[0..8].copy_from_slice(&100_000u64.to_be_bytes());
    b[10..12].copy_from_slice(&256u16.to_be_bytes());
    b[12..14].copy_from_slice(&256u16.to_be_bytes());
    b[14..16].copy_from_slice(&4u16.to_be_bytes());
    b[16..18].copy_from_slice(&1460u16.to_be_bytes());
    b[18..20].copy_from_slice(&64u16.to_be_bytes());
    b[20..22].copy_from_slice(&64u16.to_be_bytes());
    b[22..24].copy_from_slice(&256u16.to_be_bytes());
    b[24..30].copy_from_slice(&MAC);
    b[30..32].copy_from_slice(&(options.len() as u16).to_be_bytes());
    let mut at = 40;
    for (id, required) in options {
        b[at..at + 2].copy_from_slice(&id.to_be_bytes());
        b[at + 2..at + 4].copy_from_slice(&4u16.to_be_bytes());
        b[at + 4..at + 8].copy_from_slice(&required.to_be_bytes());
        at += 12;
    }
    b[32..34].copy_from_slice(&(at as u16).to_be_bytes());
    b
}

impl Model {
    fn new(mem: Rc<Memory>, revision: u8, options: &[(u16, u32)]) -> Rc<Model> {
        let bar0 = Rc::new(Fake::new(0x100));
        put_be32(&*bar0, regs::MAX_TX_QUEUES, 16).expect("max tx");
        put_be32(&*bar0, regs::MAX_RX_QUEUES, 16).expect("max rx");
        put_be32(&*bar0, regs::DEVICE_STATUS, regs::device_status::LINK).expect("link");
        Rc::new(Model {
            bar0,
            bar2: Rc::new(Fake::new(0x1000)),
            mem,
            revision,
            s: RefCell::new(State {
                aq: None,
                executed: 0,
                descriptor: descriptor(options),
                counters: 0,
                blocks: 0,
                format: 0,
                lists: BTreeMap::new(),
                tx: BTreeMap::new(),
                rx: BTreeMap::new(),
                sent: Vec::new(),
                faults: Faults::default(),
                commands: Vec::new(),
            }),
        })
    }

    fn reg(&self, at: usize) -> u32 {
        be32(&*self.bar0, at).expect("register")
    }

    fn set(&self, at: usize, v: u32) {
        put_be32(&*self.bar0, at, v).expect("register");
    }

    fn bell(&self, index: u32) -> u32 {
        be32(&*self.bar2, index as usize * 4).expect("doorbell")
    }

    fn forget(&self, s: &mut State) {
        s.aq = None;
        s.executed = 0;
        s.tx.clear();
        s.rx.clear();
        s.lists.clear();
        self.set(regs::ADMINQ_EVENT_COUNTER, 0);
        self.set(regs::ADMINQ_DOORBELL, 0);
        for at in (0..self.bar2.len()).step_by(4) {
            self.bar2.poke(at, &[0; 4]);
        }
    }

    fn step(&self) {
        let mut s = self.s.borrow_mut();
        if self.revision == 0 {
            match self.reg(regs::ADMINQ_PFN) {
                0 if s.aq.is_some() => self.forget(&mut s),
                0 => {}
                pfn => s.aq = Some(pfn as u64 * PAGE as u64),
            }
        } else {
            let driver = self.reg(regs::DRIVER_STATUS);
            let device = self.reg(regs::DEVICE_STATUS);
            if driver & regs::driver_status::RESET != 0 {
                if !s.faults.stuck_reset {
                    self.forget(&mut s);
                    self.set(regs::DEVICE_STATUS, device | regs::device_status::IS_RESET);
                }
            } else if driver & regs::driver_status::RUN != 0 && s.aq.is_none() {
                let hi = self.reg(regs::ADMINQ_BASE_HI) as u64;
                let lo = self.reg(regs::ADMINQ_BASE_LO) as u64;
                s.aq = Some(hi << 32 | lo);
                self.set(regs::DEVICE_STATUS, device & !regs::device_status::IS_RESET);
            }
        }
        let Some(aq) = s.aq else { return };
        let issued = self.reg(regs::ADMINQ_DOORBELL);
        while s.executed != issued && !s.faults.ignore_admin {
            let at = aq + (s.executed % 64) as u64 * COMMAND as u64;
            let cmd = self.mem.read(at, COMMAND);
            let op = be32_of(&cmd, 0);
            s.commands.push(op);
            let result = if s.faults.fail == Some(op) {
                0xffff_fff7
            } else {
                self.command(&mut s, op, &cmd)
            };
            self.mem.write(at + 4, &result.to_be_bytes());
            s.executed += 1;
        }
        let shown = if s.faults.counter_jump {
            s.executed + 5
        } else {
            s.executed
        };
        self.set(regs::ADMINQ_EVENT_COUNTER, shown);
        self.transmit(&mut s);
    }

    fn command(&self, s: &mut State, op: u32, c: &[u8]) -> u32 {
        let bad_db = s.faults.bad_doorbell;
        match op {
            opcode::DESCRIBE_DEVICE => {
                self.mem.write(be64_of(c, 8), &s.descriptor.clone());
            }
            opcode::CONFIGURE_DEVICE_RESOURCES => {
                s.counters = be64_of(c, 8);
                let irq = be64_of(c, 16);
                s.blocks = be32_of(c, 28);
                s.format = c[40];
                for i in 0..s.blocks {
                    let index = if bad_db { 0x4000 } else { 0x200 + i * 2 };
                    self.mem.write(irq + i as u64 * 64, &index.to_be_bytes());
                }
            }
            opcode::REGISTER_PAGE_LIST => {
                let (id, n, list) = (be32_of(c, 8), be32_of(c, 12), be64_of(c, 16));
                let bytes = self.mem.read(list, n as usize * 8);
                let pages = (0..n as usize).map(|i| be64_of(&bytes, i * 8)).collect();
                s.lists.insert(id, pages);
            }
            opcode::UNREGISTER_PAGE_LIST => {
                s.lists.remove(&be32_of(c, 8));
            }
            opcode::CREATE_TX_QUEUE => {
                let id = be32_of(c, 8);
                let res = be64_of(c, 16);
                let db = if bad_db { 0x5000 } else { 0x10 + id };
                self.mem.write(res, &db.to_be_bytes());
                self.mem.write(res + 4, &id.to_be_bytes());
                s.tx.insert(
                    id,
                    TxModel {
                        ring: be64_of(c, 24),
                        qpl: be32_of(c, 32),
                        size: u16::from_be_bytes([c[48], c[49]]) as u32,
                        counter: id,
                        db,
                        taken: 0,
                    },
                );
            }
            opcode::CREATE_RX_QUEUE => {
                let id = be32_of(c, 8);
                let res = be64_of(c, 24);
                let db = 0x40 + id;
                self.mem.write(res, &db.to_be_bytes());
                s.rx.insert(
                    id,
                    RxModel {
                        descriptors: be64_of(c, 32),
                        data: be64_of(c, 40),
                        qpl: be32_of(c, 48),
                        size: u16::from_be_bytes([c[52], c[53]]) as u32,
                        db,
                        count: 0,
                        seqno: 1,
                    },
                );
            }
            opcode::DESTROY_TX_QUEUE => {
                s.tx.remove(&be32_of(c, 8));
            }
            opcode::DESTROY_RX_QUEUE => {
                s.rx.remove(&be32_of(c, 8));
            }
            opcode::DECONFIGURE_DEVICE_RESOURCES => s.blocks = 0,
            _ => return status::UNIMPLEMENTED,
        }
        status::PASSED
    }

    /// Where a queue page list offset or a raw address points.
    fn locate(&self, s: &State, qpl: u32, addr: u64) -> u64 {
        if qpl == crate::admin::RAW_ADDRESSING_QPL {
            addr
        } else {
            s.lists[&qpl][addr as usize / PAGE] + addr % PAGE as u64
        }
    }

    fn transmit(&self, s: &mut State) {
        let ids: Vec<u32> = s.tx.keys().copied().collect();
        for id in ids {
            let req = self.bell(s.tx[&id].db);
            while s.tx[&id].taken != req {
                let t = &s.tx[&id];
                let d = self.mem.read(t.ring + (t.taken % t.size) as u64 * 16, 16);
                assert_eq!(d[3], 1, "one descriptor a packet");
                let len = u16::from_be_bytes([d[4], d[5]]) as usize;
                assert_eq!(u16::from_be_bytes([d[6], d[7]]) as usize, len);
                // A frame in a queue page list may cross into its next page.
                let mut frame = Vec::new();
                let mut off = be64_of(&d, 8);
                while frame.len() < len {
                    let here = self.locate(s, t.qpl, off);
                    let n = (PAGE - (here as usize % PAGE)).min(len - frame.len());
                    frame.extend(self.mem.read(here, n));
                    off += n as u64;
                }
                s.sent.push(frame);
                let t = s.tx.get_mut(&id).expect("tx");
                t.taken = t.taken.wrapping_add(1);
            }
            let t = &s.tx[&id];
            let shown = if s.faults.tx_ahead {
                t.taken + 3
            } else {
                t.taken
            };
            self.mem
                .write(s.counters + t.counter as u64 * 4, &shown.to_be_bytes());
        }
    }

    /// The device receives `frame` on queue `q`, in fragments of at most `frag`
    /// bytes, with `flags` on the first; false when the driver has posted no buffer.
    fn receive(&self, q: u32, frame: &[u8], frag: usize, flags: u16) -> bool {
        let mut s = self.s.borrow_mut();
        let mut first = true;
        let mut rest = frame;
        loop {
            let fill = self.bell(s.rx[&q].db);
            let r = &s.rx[&q];
            if r.count == fill {
                return false;
            }
            let slot = (r.count % r.size) as u64;
            let entry = be64_of(&self.mem.read(r.data + slot * 8, 8), 0);
            let buffer = self.locate(&s, r.qpl, entry);
            let pad = if first { 2 } else { 0 };
            let n = rest.len().min(frag - pad);
            self.mem.write(buffer + pad as u64, &rest[..n]);
            rest = &rest[n..];
            let mut f = r.seqno as u16;
            if first {
                f |= flags;
            }
            if !rest.is_empty() {
                f |= RX_CONT;
            }
            let d = r.descriptors + slot * 64;
            self.mem.write(d + 60, &((n + pad) as u16).to_be_bytes());
            self.mem.write(d + 62, &f.to_be_bytes());
            let r = s.rx.get_mut(&q).expect("rx");
            r.count += 1;
            r.seqno = next_seqno(r.seqno);
            first = false;
            if rest.is_empty() {
                return true;
            }
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
    model: Rc<Model>,
    clock: TestClock,
    dma: TestDma,
    mem: Rc<Memory>,
}

fn rig(revision: u8, options: &[(u16, u32)]) -> Rig {
    let mem = Rc::new(Memory::default());
    let model = Model::new(mem.clone(), revision, options);
    Rig {
        clock: TestClock {
            model: model.clone(),
            t: Cell::new(0),
        },
        dma: TestDma(mem.clone()),
        model,
        mem,
    }
}

fn settings(revision: u8) -> Settings {
    Settings {
        queues: 2,
        vectors: 9,
        revision,
        mtu_max: 9000,
        timeout_ns: 1_000_000_000,
    }
}

type Card = Gve<Page, Page, Page>;

fn start(r: &mut Rig, s: Settings) -> Result<Card, GveError> {
    Gve::start(
        Page(r.model.bar0.clone()),
        Page(r.model.bar2.clone()),
        &mut r.dma,
        &r.clock,
        s,
        &mut || Ok(()),
    )
}

fn frame(n: usize, seed: u8) -> Vec<u8> {
    let mut f: Vec<u8> = (0..n).map(|i| (i as u8).wrapping_mul(7) ^ seed).collect();
    f[6..12].copy_from_slice(&MAC);
    f
}

#[test]
fn nt_t10_the_descriptor_is_checked_and_a_format_chosen() {
    let d = Description::parse(&descriptor(&[])).expect("legacy");
    assert_eq!(d.format, Format::GqiQpl);
    assert_eq!(
        (d.tx_entries, d.rx_entries, d.mtu, d.counters),
        (256, 256, 1460, 64)
    );
    assert_eq!(d.mac, 0x4201_0a80_0002);
    assert_eq!(d.default_queues, 4);
    assert_eq!(d.max_registered_pages, 100_000);
    let qpl = Description::parse(&descriptor(&[(option::GQI_QPL, 0), (option::DQO_RDA, 0)]))
        .expect("both");
    assert_eq!(qpl.format, Format::GqiQpl);
    assert_eq!(qpl.options, "0x3,0x4");
    let rda = Description::parse(&descriptor(&[
        (option::GQI_RDA, 0),
        (option::JUMBO_FRAMES, 0),
    ]))
    .expect("rda");
    assert_eq!(rda.format, Format::GqiRda);
    assert!(matches!(
        Description::parse(&descriptor(&[(option::DQO_RDA, 0)])),
        Err(GveError::Unsupported(w)) if w.contains("DQO")
    ));
    assert!(matches!(
        Description::parse(&descriptor(&[(option::GQI_QPL, 1)])),
        Err(GveError::Unsupported(_))
    ));
    let bad = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut d = descriptor(&[(option::GQI_QPL, 0)]);
        edit(&mut d);
        Description::parse(&d)
    };
    for (what, edit) in [
        (
            "total too short",
            &(|d: &mut Vec<u8>| d[32..34].copy_from_slice(&20u16.to_be_bytes()))
                as &dyn Fn(&mut Vec<u8>),
        ),
        ("total past a page", &|d: &mut Vec<u8>| {
            d[32..34].copy_from_slice(&5000u16.to_be_bytes())
        }),
        ("an option past the end", &|d: &mut Vec<u8>| {
            d[42..44].copy_from_slice(&900u16.to_be_bytes())
        }),
        ("more options than room", &|d: &mut Vec<u8>| {
            d[30..32].copy_from_slice(&9u16.to_be_bytes())
        }),
        ("a ring not a power of two", &|d: &mut Vec<u8>| {
            d[10..12].copy_from_slice(&300u16.to_be_bytes())
        }),
        ("a ring of one", &|d: &mut Vec<u8>| {
            d[12..14].copy_from_slice(&1u16.to_be_bytes())
        }),
        ("a ring too large", &|d: &mut Vec<u8>| {
            d[12..14].copy_from_slice(&8192u16.to_be_bytes())
        }),
        ("a small MTU", &|d: &mut Vec<u8>| {
            d[16..18].copy_from_slice(&40u16.to_be_bytes())
        }),
        ("a group address", &|d: &mut Vec<u8>| d[24] = 0x01),
        ("no address", &|d: &mut Vec<u8>| d[24..30].fill(0)),
        ("no counters", &|d: &mut Vec<u8>| d[18..20].fill(0)),
        ("no transmit pages", &|d: &mut Vec<u8>| d[20..22].fill(0)),
    ] {
        assert!(matches!(bad(edit), Err(GveError::Malformed(_))), "{what}");
    }
    assert!(Description::parse(&[0u8; 10]).is_err());
}

#[test]
fn nt_t11_the_admin_queue_checks_what_the_device_says() {
    let mut r = rig(1, &[]);
    let (page, addr) = r.dma.alloc(PAGE, false).expect("page");
    put_be32(&*r.model.bar0, regs::ADMINQ_BASE_LO, addr[0] as u32).expect("lo");
    put_be32(
        &*r.model.bar0,
        regs::DRIVER_STATUS,
        regs::driver_status::RUN,
    )
    .expect("run");
    let mut aq = AdminQueue::new(page, addr[0]);
    let bar = Page(r.model.bar0.clone());
    let (desc, at) = r.dma.alloc(PAGE, false).expect("descriptor page");
    let describe = Command::DescribeDevice {
        addr: at[0],
        length: PAGE as u32,
    };
    aq.execute(&bar, &r.clock, 1_000_000_000, &describe)
        .expect("describe");
    assert_eq!(desc.0.peek(24, 6), MAC);
    r.model.s.borrow_mut().faults.fail = Some(opcode::DESCRIBE_DEVICE);
    assert_eq!(
        aq.execute(&bar, &r.clock, 1_000_000_000, &describe),
        Err(GveError::Command {
            opcode: opcode::DESCRIBE_DEVICE,
            status: 0xffff_fff7
        })
    );
    assert!(
        alloc::format!(
            "{}",
            GveError::Command {
                opcode: 1,
                status: 0xffff_fff7
            }
        )
        .contains("invalid argument")
    );
    r.model.s.borrow_mut().faults.fail = None;
    r.model.s.borrow_mut().faults.ignore_admin = true;
    assert_eq!(
        aq.execute(&bar, &r.clock, 1_000_000_000, &describe),
        Err(GveError::Timeout("execute an admin command"))
    );
    // The device's counter now trails the commands issued: the next command sees it.
    r.model.s.borrow_mut().faults.ignore_admin = false;
    assert!(matches!(
        aq.execute(&bar, &r.clock, 1_000_000_000, &describe),
        Err(GveError::Malformed(_))
    ));
    let mut r = rig(1, &[]);
    r.model.s.borrow_mut().faults.counter_jump = true;
    assert!(matches!(
        start(&mut r, settings(1)),
        Err(GveError::Malformed(_))
    ));
    let cmd = Command::CreateRxQueue {
        id: 3,
        ntfy: 7,
        resources: 0x1000,
        descriptors: 0x2000,
        data: 0x3000,
        qpl: 9,
        size: 512,
        buffer: 2048,
    };
    let f = Fake::new(COMMAND);
    cmd.write(&f, 0).expect("write");
    let b = f.peek(0, COMMAND);
    assert_eq!(be32_of(&b, 0), opcode::CREATE_RX_QUEUE);
    assert_eq!(
        (be32_of(&b, 8), be32_of(&b, 12), be32_of(&b, 20)),
        (3, 3, 7)
    );
    assert_eq!(
        (be64_of(&b, 24), be64_of(&b, 32), be64_of(&b, 40)),
        (0x1000, 0x2000, 0x3000)
    );
    assert_eq!(be32_of(&b, 48), 9);
    assert_eq!(
        (
            u16::from_be_bytes([b[52], b[53]]),
            u16::from_be_bytes([b[54], b[55]])
        ),
        (512, 2048)
    );
}

#[test]
fn nt_t12_the_device_is_brought_up_in_either_revision_and_format() {
    for (revision, options, format) in [
        (1, vec![(option::GQI_QPL, 0)], Format::GqiQpl),
        (0, vec![], Format::GqiQpl),
        (1, vec![(option::GQI_RDA, 0)], Format::GqiRda),
    ] {
        let mut r = rig(revision, &options);
        let g = start(&mut r, settings(revision)).expect("start");
        assert_eq!(g.description.format, format);
        assert_eq!(g.queues(), 2);
        assert_eq!(g.mtu, 1460);
        assert_eq!(g.max_frame(), 1474);
        assert_eq!(g.link_up(), Ok(true));
        assert_eq!(g.counters(), 64);
        let s = r.model.s.borrow();
        assert_eq!(s.blocks, 4);
        assert_eq!(s.format, format.code());
        assert_eq!(s.tx.len(), 2);
        assert_eq!(s.rx.len(), 2);
        assert_eq!(s.lists.len(), if format == Format::GqiQpl { 4 } else { 0 });
        // Every receive buffer posted; every block unmasked.
        for rx in s.rx.values() {
            assert_eq!(r.model.bell(rx.db), 256);
        }
        for i in 0..4 {
            assert_eq!(r.model.bell(0x200 + i * 2), 0);
        }
        drop(s);
        // The rings the device reads as one block are contiguous.
        assert!(r.mem.contiguous.get() >= 6);
        let version: Vec<u8> = r.model.bar0.peek(regs::DRIVER_VERSION, 1);
        assert_eq!(version, b"\n");
    }
    // Queues: at most what the device has, the interrupts allow and the default.
    let mut r = rig(1, &[]);
    assert_eq!(
        start(
            &mut r,
            Settings {
                queues: 16,
                ..settings(1)
            }
        )
        .expect("start")
        .queues(),
        4
    );
    let mut r = rig(1, &[]);
    assert_eq!(
        start(
            &mut r,
            Settings {
                vectors: 5,
                ..settings(1)
            }
        )
        .expect("start")
        .queues(),
        2
    );
    let mut r = rig(1, &[]);
    assert!(matches!(
        start(
            &mut r,
            Settings {
                vectors: 2,
                ..settings(1)
            }
        ),
        Err(GveError::Unsupported(_))
    ));
    let mut r = rig(1, &[]);
    assert_eq!(
        start(
            &mut r,
            Settings {
                mtu_max: 1000,
                ..settings(1)
            }
        )
        .expect("start")
        .mtu,
        1000
    );
    let mut r = rig(1, &[]);
    r.model.s.borrow_mut().faults.bad_doorbell = true;
    assert!(
        matches!(start(&mut r, settings(1)), Err(GveError::Malformed(w)) if w.contains("doorbell"))
    );
    let mut r = rig(1, &[]);
    r.model.s.borrow_mut().faults.stuck_reset = true;
    assert_eq!(
        start(&mut r, settings(1)).map(|_| ()),
        Err(GveError::Timeout("finish a reset"))
    );
    let mut r = rig(1, &[]);
    r.model.s.borrow_mut().faults.fail = Some(opcode::CREATE_TX_QUEUE);
    assert!(matches!(
        start(&mut r, settings(1)),
        Err(GveError::Command { .. })
    ));
    let mut r = rig(1, &[(option::DQO_RDA, 0)]);
    assert!(matches!(
        start(&mut r, settings(1)),
        Err(GveError::Unsupported(_))
    ));
}

#[test]
fn nt_t13_frames_go_out_and_their_slots_come_back() {
    for (revision, options) in [(1, vec![]), (1, vec![(option::GQI_RDA, 0)])] {
        let mut r = rig(revision, &options);
        let mut g = start(&mut r, settings(revision)).expect("start");
        let frames: Vec<Vec<u8>> = (0..5).map(|i| frame(60 + i * 300, i as u8)).collect();
        for f in &frames {
            g.send(0, f).expect("send");
        }
        g.send(1, &frame(1474, 9)).expect("the largest frame");
        assert!(matches!(
            g.send(0, &frame(1475, 1)),
            Err(GveError::Unsupported(_))
        ));
        assert!(matches!(g.send(0, &[0; 13]), Err(GveError::Unsupported(_))));
        assert!(
            r.model.s.borrow().sent.is_empty(),
            "nothing goes before the doorbell"
        );
        g.flush().expect("flush");
        r.model.step();
        let sent = r.model.s.borrow().sent.clone();
        assert_eq!(sent.len(), 6);
        for (want, got) in frames.iter().zip(&sent) {
            assert_eq!(want, got);
        }
        assert_eq!(sent[5], frame(1474, 9));
        assert_eq!(g.pending(), Ok(true));
        assert_eq!(g.complete(), Ok(6));
        assert_eq!(g.pending(), Ok(false));
        // A queue fills at its slots, and frees them as the device finishes.
        let mut n = 0;
        while g.has_room(0) {
            g.send(0, &frame(100, n as u8)).expect("send");
            n += 1;
        }
        assert_eq!(g.send(0, &frame(100, 0)), Err(GveError::Full));
        assert!(n >= 64, "{n}");
        g.flush().expect("flush");
        r.model.step();
        assert_eq!(g.complete(), Ok(n));
        assert!(g.has_room(0));
        // A counter past what was sent is the device's error.
        r.model.s.borrow_mut().faults.tx_ahead = true;
        r.model.step();
        assert!(matches!(g.complete(), Err(GveError::Malformed(_))));
    }
    // A jumbo frame in a queue page list crosses pages.
    let mut r = rig(1, &[]);
    r.model.s.borrow_mut().descriptor[16..18].copy_from_slice(&8896u16.to_be_bytes());
    let mut g = start(&mut r, settings(1)).expect("start");
    assert_eq!(g.mtu, 8896);
    for i in 0..3 {
        g.send(0, &frame(8910, i)).expect("jumbo");
    }
    g.flush().expect("flush");
    r.model.step();
    assert_eq!(r.model.s.borrow().sent[2], frame(8910, 2));
}

#[test]
fn nt_t14_frames_come_in_whole_or_are_dropped_and_counted() {
    let mut r = rig(1, &[]);
    let mut g = start(&mut r, settings(1)).expect("start");
    let mut got = Vec::new();
    assert_eq!(g.receive(&mut |f| got.push(f.to_vec())), Ok(0));
    assert!(got.is_empty());
    assert!(r.model.receive(0, &frame(60, 1), 2048, 0));
    assert!(r.model.receive(1, &frame(1474, 2), 2048, 0));
    // A frame in three fragments of 600 bytes.
    assert!(r.model.receive(0, &frame(1400, 3), 600, 0));
    // A frame the device marks in error, and a runt, are dropped.
    assert!(r.model.receive(0, &frame(200, 4), 2048, RX_ERR));
    assert!(r.model.receive(0, &[0x55; 10], 2048, 0));
    assert_eq!(g.pending(), Ok(true));
    assert_eq!(g.receive(&mut |f| got.push(f.to_vec())), Ok(2));
    assert_eq!(got, [frame(60, 1), frame(1400, 3), frame(1474, 2)]);
    // Each buffer taken is posted again.
    let s = r.model.s.borrow();
    assert_eq!(r.model.bell(s.rx[&0].db), 256 + 6);
    assert_eq!(r.model.bell(s.rx[&1].db), 256 + 1);
    drop(s);
    // Around the ring many times: the sequence numbers wrap 1 to 7.
    for i in 0..1000u32 {
        assert!(r.model.receive(0, &frame(64, i as u8), 2048, 0));
        let mut one = Vec::new();
        assert_eq!(g.receive(&mut |f| one.push(f.to_vec())), Ok(0));
        assert_eq!(one, [frame(64, i as u8)]);
    }
    // A frame longer than the largest is dropped, all its fragments taken.
    assert!(r.model.receive(0, &frame(3000, 7), 1000, 0));
    assert_eq!(
        g.receive(&mut |_| panic!("delivered an oversized frame")),
        Ok(1)
    );
    // A fragment longer than a buffer, and a packet the device stops in the middle
    // of, are the device's errors.
    assert!(r.model.receive(0, &frame(2100, 8), 4096, 0));
    assert!(matches!(
        g.receive(&mut |_| {}),
        Err(GveError::Malformed(_))
    ));
    let mut r = rig(1, &[]);
    let mut g = start(&mut r, settings(1)).expect("start");
    {
        // Two fragments written, the second's descriptor never published.
        assert!(r.model.receive(0, &frame(1000, 9), 600, 0));
        let s = r.model.s.borrow();
        let rx = &s.rx[&0];
        r.mem.write(rx.descriptors + 64 + 62, &0u16.to_be_bytes());
    }
    assert!(matches!(g.receive(&mut |_| {}), Err(GveError::Malformed(w)) if w.contains("middle")));
}

#[test]
fn nt_t15_shutdown_releases_everything_and_the_status_is_read() {
    for revision in [0, 1] {
        let mut r = rig(revision, &[]);
        let mut g = start(&mut r, settings(revision)).expect("start");
        g.shutdown(&r.clock).expect("shutdown");
        // A device of revision 0 lets its admin queue go when it next looks.
        r.model.step();
        let s = r.model.s.borrow();
        assert!(s.tx.is_empty() && s.rx.is_empty() && s.lists.is_empty());
        assert_eq!(s.blocks, 0);
        assert!(s.aq.is_none());
        assert!(s.commands.contains(&opcode::DECONFIGURE_DEVICE_RESOURCES));
    }
    let mut r = rig(1, &[]);
    let g = start(&mut r, settings(1)).expect("start");
    put_be32(
        &*r.model.bar0,
        regs::DEVICE_STATUS,
        regs::device_status::RESET,
    )
    .expect("status");
    assert_eq!(g.status(), Ok(regs::device_status::RESET));
    assert_eq!(g.link_up(), Ok(false));
    g.ack().expect("ack");
    assert_eq!(r.model.bell(0x200), regs::irq::ACK | regs::irq::EVENT);
}
