//! DR-T6 (SDD 25, k): the virtio transport against a model device, and split queues
//! against a model of the device's side of the rings.

use alloc::{vec, vec::Vec};
use core::cell::RefCell;

use moruna_abi::Error;
use moruna_mmio::{Fake, Region};
use moruna_pci::{CONFIG_SIZE, Config, STATUS_CAPABILITIES, cap, reg};

use crate::{
    NO_VECTOR, feature,
    queue::{self, Buffer, Queue, QueueError, Used},
    status,
    transport::{Layout, Transport, VirtioError, Window, cfg_type, common},
};

const COMMON: usize = 0x0000;
const ISR: usize = 0x1000;
const DEVICE: usize = 0x2000;
const NOTIFY: usize = 0x3000;
const MULTIPLIER: u32 = 4;

#[derive(Default)]
struct ModelQueue {
    max: u16,
    size: u16,
    rings: [u64; 3],
    vector: u16,
    enabled: bool,
}

#[derive(Default)]
struct State {
    status: u8,
    offered: u64,
    feature_select: u32,
    driver_select: u32,
    taken: u64,
    queue_select: u16,
    queues: Vec<ModelQueue>,
    msix_config: u16,
    generation: u8,
    /// Reads of the generation left before it changes once (a concurrent update).
    bump_after: Option<u32>,
    config: Vec<u8>,
    notified: Vec<(usize, u16)>,
    isr: u8,
    /// The device never finishes a reset.
    stuck: bool,
    /// The device clears FEATURES_OK.
    refuse_features: bool,
    /// The device takes no MSI-X vector.
    refuse_vectors: bool,
}

/// One BAR of a modern virtio device: common configuration, ISR, device
/// configuration and notify structures, as the device presents them.
struct Model(RefCell<State>);

impl Model {
    fn new(offered: u64, queues: &[u16]) -> Model {
        Model(RefCell::new(State {
            offered,
            queues: queues
                .iter()
                .map(|&max| ModelQueue {
                    max,
                    ..ModelQueue::default()
                })
                .collect(),
            config: vec![0; 64],
            ..State::default()
        }))
    }

    fn read(&self, offset: usize, width: usize) -> Result<u64, Error> {
        let mut s = self.0.borrow_mut();
        let q = s.queue_select as usize;
        Ok(match offset {
            o if o == COMMON + common::DEVICE_FEATURE => {
                (s.offered >> (32 * s.feature_select.min(1))) & 0xffff_ffff
            }
            o if o == COMMON + common::DRIVER_FEATURE => {
                (s.taken >> (32 * s.driver_select.min(1))) & 0xffff_ffff
            }
            o if o == COMMON + common::MSIX_CONFIG => s.msix_config as u64,
            o if o == COMMON + common::NUM_QUEUES => s.queues.len() as u64,
            o if o == COMMON + common::DEVICE_STATUS => s.status as u64,
            o if o == COMMON + common::CONFIG_GENERATION => {
                if let Some(n) = s.bump_after {
                    if n == 0 {
                        s.generation = s.generation.wrapping_add(1);
                        s.bump_after = None;
                    } else {
                        s.bump_after = Some(n - 1);
                    }
                }
                s.generation as u64
            }
            o if o == COMMON + common::QUEUE_SIZE => s.queues.get(q).map_or(0, |q| q.max) as u64,
            o if o == COMMON + common::QUEUE_MSIX_VECTOR => {
                s.queues.get(q).map_or(NO_VECTOR, |q| q.vector) as u64
            }
            o if o == COMMON + common::QUEUE_NOTIFY_OFF => q as u64,
            o if o == ISR => core::mem::take(&mut s.isr) as u64,
            o if (DEVICE..DEVICE + 64).contains(&o) => {
                let at = o - DEVICE;
                let mut word = [0u8; 8];
                word[..width].copy_from_slice(&s.config[at..at + width]);
                u64::from_le_bytes(word)
            }
            _ => 0,
        })
    }

    fn write(&self, offset: usize, value: u64) -> Result<(), Error> {
        let mut s = self.0.borrow_mut();
        let q = s.queue_select as usize;
        match offset {
            o if o == COMMON + common::DEVICE_FEATURE_SELECT => s.feature_select = value as u32,
            o if o == COMMON + common::DRIVER_FEATURE_SELECT => s.driver_select = value as u32,
            o if o == COMMON + common::DRIVER_FEATURE => {
                let shift = 32 * s.driver_select.min(1);
                s.taken = (s.taken & !(0xffff_ffff << shift)) | (value << shift);
            }
            o if o == COMMON + common::MSIX_CONFIG => {
                s.msix_config = if s.refuse_vectors {
                    NO_VECTOR
                } else {
                    value as u16
                };
            }
            o if o == COMMON + common::DEVICE_STATUS => {
                let v = value as u8;
                if v == 0 {
                    if !s.stuck {
                        s.status = 0;
                    }
                } else if v & status::FEATURES_OK != 0 && s.refuse_features {
                    s.status = v & !status::FEATURES_OK;
                } else {
                    s.status = v;
                }
            }
            o if o == COMMON + common::QUEUE_SELECT => s.queue_select = value as u16,
            o if o == COMMON + common::QUEUE_SIZE => s.queues[q].size = value as u16,
            o if o == COMMON + common::QUEUE_MSIX_VECTOR => {
                let refuse = s.refuse_vectors;
                s.queues[q].vector = if refuse { NO_VECTOR } else { value as u16 };
            }
            o if o == COMMON + common::QUEUE_ENABLE => s.queues[q].enabled = value == 1,
            o if (COMMON + common::QUEUE_DESC..COMMON + common::QUEUE_DEVICE + 8).contains(&o) => {
                let ring = (o - COMMON - common::QUEUE_DESC) / 8;
                let shift = if o % 8 == 4 { 32 } else { 0 };
                let r = &mut s.queues[q].rings[ring];
                *r = (*r & !(0xffff_ffff << shift)) | (value << shift);
            }
            o if o >= NOTIFY => s.notified.push((o - NOTIFY, value as u16)),
            _ => {}
        }
        Ok(())
    }
}

impl Region for Model {
    fn len(&self) -> usize {
        0x4000
    }
    fn read8(&self, offset: usize) -> Result<u8, Error> {
        self.read(offset, 1).map(|v| v as u8)
    }
    fn read16(&self, offset: usize) -> Result<u16, Error> {
        self.read(offset, 2).map(|v| v as u16)
    }
    fn read32(&self, offset: usize) -> Result<u32, Error> {
        self.read(offset, 4).map(|v| v as u32)
    }
    fn read64(&self, offset: usize) -> Result<u64, Error> {
        self.read(offset, 8)
    }
    fn write8(&self, offset: usize, value: u8) -> Result<(), Error> {
        self.write(offset, value as u64)
    }
    fn write16(&self, offset: usize, value: u16) -> Result<(), Error> {
        self.write(offset, value as u64)
    }
    fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
        self.write(offset, value as u64)
    }
    fn write64(&self, offset: usize, value: u64) -> Result<(), Error> {
        self.write(offset, value)
    }
}

/// Configuration space with the vendor capabilities a QEMU virtio device has: common,
/// ISR, device and notify in BAR 4, and one capability of an unknown type.
fn config_space(with_device: bool) -> Config<Fake> {
    let f = Fake::new(CONFIG_SIZE);
    f.poke(reg::STATUS, &STATUS_CAPABILITIES.to_le_bytes());
    let mut caps: Vec<(u8, u32, u32)> = vec![
        (cfg_type::COMMON, COMMON as u32, 0x1000),
        (cfg_type::ISR, ISR as u32, 0x1000),
        (cfg_type::NOTIFY, NOTIFY as u32, 0x1000),
        (5, 0, 0),
        (cfg_type::COMMON, 0x3f00, 0x10),
    ];
    if with_device {
        caps.insert(2, (cfg_type::DEVICE, DEVICE as u32, 0x1000));
    }
    let first = 0x40usize;
    f.poke(reg::CAPABILITIES, &[first as u8]);
    for (i, (kind, offset, length)) in caps.iter().enumerate() {
        let at = first + i * 0x14;
        let next = if i + 1 < caps.len() {
            (at + 0x14) as u8
        } else {
            0
        };
        f.poke(at, &[cap::VENDOR, next, 0x14, *kind, 4]);
        f.poke(at + 8, &offset.to_le_bytes());
        f.poke(at + 12, &length.to_le_bytes());
        f.poke(at + 16, &MULTIPLIER.to_le_bytes());
    }
    // An MSI-X capability between them is skipped.
    Config::new(f).expect("config")
}

fn transport(model: Model) -> Transport<Model> {
    let layout = Layout::read(&config_space(true)).expect("layout");
    let mut bars: [Option<(Model, u64)>; 6] = Default::default();
    bars[4] = Some((model, 0));
    Transport::new(bars, layout).expect("transport")
}

#[test]
fn dr_t6_the_layout_is_read_from_the_vendor_capabilities() {
    let layout = Layout::read(&config_space(true)).expect("layout");
    assert_eq!(
        layout.common,
        Window {
            bar: 4,
            offset: 0,
            length: 0x1000
        }
    );
    assert_eq!(layout.isr.offset, ISR as u64);
    assert_eq!(layout.notify.offset, NOTIFY as u64);
    assert_eq!(layout.notify_multiplier, MULTIPLIER);
    assert_eq!(layout.device.map(|w| w.offset), Some(DEVICE as u64));
    assert_eq!(layout.bars(), [4]);
    let without = Layout::read(&config_space(false)).expect("layout");
    assert_eq!(without.device, None);
    let empty = Config::new(Fake::new(CONFIG_SIZE)).expect("config");
    assert_eq!(Layout::read(&empty), Err(VirtioError::Missing("notify")));
}

#[test]
fn dr_t6_a_short_common_structure_or_a_missing_bar_is_refused() {
    let f = config_space(true);
    // Shrink the common structure below the fields the driver uses.
    f.region().poke(0x40 + 12, &0x10u32.to_le_bytes());
    assert_eq!(Layout::read(&f), Err(VirtioError::BadCapability));
    let layout = Layout::read(&config_space(true)).expect("layout");
    let bars: [Option<(Fake, u64)>; 6] = Default::default();
    assert!(matches!(
        Transport::new(bars, layout),
        Err(VirtioError::BadCapability)
    ));
    let mut small: [Option<(Fake, u64)>; 6] = Default::default();
    small[4] = Some((Fake::new(0x1000), 0));
    assert!(matches!(
        Transport::new(small, layout),
        Err(VirtioError::BadCapability)
    ));
}

#[test]
fn dr_t6_a_device_is_reset_negotiated_and_set_up() {
    let offered = feature::VERSION_1 | feature::ACCESS_PLATFORM | feature::EVENT_IDX | (1 << 9) | 1;
    let t = transport(Model::new(offered, &[256, 128]));
    t.reset().expect("reset");
    assert_eq!(t.device_features(), Ok(offered));
    let taken = t.negotiate(1 << 9 | 1 << 5, 1).expect("negotiate");
    assert_eq!(
        taken,
        feature::VERSION_1 | feature::ACCESS_PLATFORM | (1 << 9) | 1
    );
    assert_eq!(
        t.status(),
        Ok(status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK)
    );
    assert_eq!(t.queues(), Ok(2));
    assert_eq!(t.queue_max(1), Ok(128));
    assert_eq!(t.queue_max(2), Err(VirtioError::NoQueue(2)));
    let off = t
        .setup_queue(1, 128, [0x1000_0000, 0x2000_0000, 0x3_0000_0000], 0)
        .expect("queue");
    assert_eq!(off, 1);
    assert_eq!(
        t.setup_queue(0, 512, [0; 3], 0),
        Err(VirtioError::NoQueue(0))
    );
    assert_eq!(t.setup_queue(0, 0, [0; 3], 0), Err(VirtioError::NoQueue(0)));
    t.set_config_vector(NO_VECTOR).expect("no config vector");
    t.set_config_vector(0).expect("config vector");
    t.driver_ok().expect("ok");
    assert_eq!(
        t.status().map(|s| s & status::DRIVER_OK),
        Ok(status::DRIVER_OK)
    );
    t.notify(1, off).expect("notify");
    let model = t_model(&t);
    {
        let s = model.0.borrow();
        let q = &s.queues[1];
        assert_eq!(q.size, 128);
        assert_eq!(q.rings, [0x1000_0000, 0x2000_0000, 0x3_0000_0000]);
        assert_eq!(q.vector, 0);
        assert!(q.enabled);
        assert_eq!(s.notified, [(MULTIPLIER as usize, 1)]);
        assert_eq!(s.msix_config, 0);
    }
    model.0.borrow_mut().isr = 0x3;
    assert_eq!(t.isr(), Ok(0x3));
    assert_eq!(t.isr(), Ok(0));
    t.fail().expect("fail");
    assert_eq!(t.status().map(|s| s & status::FAILED), Ok(status::FAILED));
    assert_eq!(t.layout().notify_multiplier, MULTIPLIER);
}

/// The model behind a transport built by `transport`.
fn t_model(t: &Transport<Model>) -> &Model {
    t.bar(4).expect("bar 4")
}

#[test]
fn dr_t6_negotiation_refuses_a_legacy_device_and_missing_features() {
    let legacy = transport(Model::new(1, &[16]));
    assert_eq!(legacy.negotiate(0, 0), Err(VirtioError::Legacy));
    assert_eq!(
        legacy.status().map(|s| s & status::FAILED),
        Ok(status::FAILED)
    );
    let lacking = transport(Model::new(feature::VERSION_1, &[16]));
    assert_eq!(
        lacking.negotiate(0, 1 << 9),
        Err(VirtioError::MissingFeature(1 << 9))
    );
    let refusing = Model::new(feature::VERSION_1, &[16]);
    refusing.0.borrow_mut().refuse_features = true;
    let refusing = transport(refusing);
    assert_eq!(refusing.negotiate(0, 0), Err(VirtioError::FeaturesRefused));
}

#[test]
fn dr_t6_a_reset_that_never_ends_and_a_refused_vector_are_errors() {
    let stuck = Model::new(feature::VERSION_1, &[16]);
    {
        let mut s = stuck.0.borrow_mut();
        s.stuck = true;
        s.status = status::DRIVER_OK;
    }
    assert_eq!(transport(stuck).reset(), Err(VirtioError::ResetTimeout));
    let refusing = Model::new(feature::VERSION_1, &[16]);
    refusing.0.borrow_mut().refuse_vectors = true;
    let t = transport(refusing);
    assert_eq!(
        t.setup_queue(0, 16, [0; 3], 0),
        Err(VirtioError::VectorRefused)
    );
    assert_eq!(t.set_config_vector(0), Err(VirtioError::VectorRefused));
    assert_eq!(t.setup_queue(0, 16, [0; 3], NO_VECTOR), Ok(0));
}

#[test]
fn dr_t6_device_configuration_is_read_under_one_generation() {
    let model = Model::new(feature::VERSION_1, &[16]);
    model.0.borrow_mut().config[..8].copy_from_slice(&8192u64.to_le_bytes());
    // The generation changes between the first read's before and after.
    model.0.borrow_mut().bump_after = Some(1);
    let t = transport(model);
    let mut reads = 0;
    let capacity = t
        .device_config(|w| {
            reads += 1;
            w.read64(0)
        })
        .expect("config");
    assert_eq!(capacity, 8192);
    assert_eq!(reads, 2);
    let layout = Layout::read(&config_space(false)).expect("layout");
    let mut bars: [Option<(Model, u64)>; 6] = Default::default();
    bars[4] = Some((Model::new(feature::VERSION_1, &[16]), 0));
    let t = Transport::new(bars, layout).expect("transport");
    assert_eq!(
        t.device_config(|w| w.read8(0)).map(|_| ()),
        Err(VirtioError::Missing("device"))
    );
}

#[test]
fn dr_t6_errors_describe_themselves() {
    for e in [
        VirtioError::Kernel(Error::Fault),
        VirtioError::Missing("common"),
        VirtioError::BadCapability,
        VirtioError::Legacy,
        VirtioError::MissingFeature(1),
        VirtioError::FeaturesRefused,
        VirtioError::ResetTimeout,
        VirtioError::NoQueue(3),
        VirtioError::VectorRefused,
    ] {
        assert!(!alloc::format!("{e}").is_empty());
    }
    assert_eq!(
        VirtioError::from(Error::Fault),
        VirtioError::Kernel(Error::Fault)
    );
    assert_eq!(
        QueueError::from(Error::Fault),
        QueueError::Kernel(Error::Fault)
    );
}

/// A chain as the device sees it: its head, and each descriptor's address, length and
/// flags.
type Chain = (u16, Vec<(u64, u32, u16)>);

/// The device's side of a queue: takes what the driver offered and returns it.
struct DeviceSide {
    taken: u16,
    used: u16,
    size: u16,
}

impl DeviceSide {
    /// The chains offered since the last call, each as its descriptors.
    fn take(&mut self, m: &Fake) -> Vec<Chain> {
        let idx = m.read16(queue::DRIVER + 2).expect("idx");
        let mut chains = Vec::new();
        while self.taken != idx {
            let slot = (self.taken % self.size) as usize;
            let head = m.read16(queue::DRIVER + 4 + slot * 2).expect("ring");
            let mut parts = Vec::new();
            let mut d = head;
            loop {
                let at = queue::DESC + d as usize * 16;
                let flags = m.read16(at + 12).expect("flags");
                parts.push((
                    m.read64(at).expect("addr"),
                    m.read32(at + 8).expect("len"),
                    flags,
                ));
                if flags & queue::flags::NEXT == 0 {
                    break;
                }
                d = m.read16(at + 14).expect("next");
            }
            chains.push((head, parts));
            self.taken = self.taken.wrapping_add(1);
        }
        chains
    }

    fn give(&mut self, m: &Fake, id: u32, len: u32) {
        let slot = (self.used % self.size) as usize;
        m.write32(queue::DEVICE + 4 + slot * 8, id).expect("id");
        m.write32(queue::DEVICE + 8 + slot * 8, len).expect("len");
        self.used = self.used.wrapping_add(1);
        m.write16(queue::DEVICE + 2, self.used).expect("idx");
    }
}

#[test]
fn dr_t6_chains_go_out_and_come_back() {
    let memory = Fake::new(queue::BYTES);
    memory.poke(0, &[0xff; 16]);
    let mut q = Queue::new(Sub(&memory), 4).expect("queue");
    assert_eq!(memory.read64(0), Ok(0));
    assert_eq!(q.size(), 4);
    let mut device = DeviceSide {
        taken: 0,
        used: 0,
        size: 4,
    };
    let read = [
        Buffer {
            addr: 0x1000,
            len: 16,
            device_writes: false,
        },
        Buffer {
            addr: 0x2000,
            len: 4096,
            device_writes: true,
        },
        Buffer {
            addr: 0x3000,
            len: 1,
            device_writes: true,
        },
    ];
    let a = q.add(&read).expect("add");
    assert_eq!(q.free(), 1);
    assert_eq!(q.in_flight(), 1);
    assert_eq!(q.add(&read), Err(QueueError::Full));
    let b = q.add(&read[..1]).expect("add");
    assert_eq!(q.pop_used(), Ok(None));
    let chains = device.take(&memory);
    assert_eq!(chains.len(), 2);
    assert_eq!(chains[0].0, a);
    assert_eq!(
        chains[0].1,
        [
            (0x1000, 16, queue::flags::NEXT),
            (0x2000, 4096, queue::flags::NEXT | queue::flags::WRITE),
            (0x3000, 1, queue::flags::WRITE),
        ]
    );
    assert_eq!(chains[1].1, [(0x1000, 16, 0)]);
    // Returned out of order.
    device.give(&memory, b as u32, 0);
    device.give(&memory, a as u32, 4097);
    assert_eq!(q.pop_used(), Ok(Some(Used { head: b, len: 0 })));
    assert_eq!(q.pop_used(), Ok(Some(Used { head: a, len: 4097 })));
    assert_eq!(q.pop_used(), Ok(None));
    assert_eq!(q.free(), 4);
    assert_eq!(q.in_flight(), 0);
    // Around the rings several times.
    for round in 0..10u32 {
        let h = q.add(&read[..2]).expect("add");
        assert_eq!(device.take(&memory).len(), 1);
        device.give(&memory, h as u32, round);
        assert_eq!(
            q.pop_used(),
            Ok(Some(Used {
                head: h,
                len: round
            }))
        );
    }
}

#[test]
fn dr_t6_what_the_device_returns_is_checked() {
    let memory = Fake::new(queue::BYTES);
    let mut q = Queue::new(Sub(&memory), 4).expect("queue");
    let mut device = DeviceSide {
        taken: 0,
        used: 0,
        size: 4,
    };
    let one = [Buffer {
        addr: 0x1000,
        len: 8,
        device_writes: false,
    }];
    let h = q.add(&one).expect("add");
    // An id beyond the queue, then one that is not in flight.
    device.give(&memory, 99, 0);
    assert_eq!(q.pop_used(), Err(QueueError::Malformed));
    let mut q = Queue::new(Sub(&memory), 4).expect("queue");
    let h2 = q.add(&one).expect("add");
    assert_eq!(h, h2);
    let mut device = DeviceSide {
        taken: 0,
        used: 0,
        size: 4,
    };
    device.give(&memory, (h + 1) as u32, 0);
    assert_eq!(q.pop_used(), Err(QueueError::Malformed));
    // An id too wide for a descriptor index.
    let mut q = Queue::new(Sub(&memory), 4).expect("queue");
    let mut device = DeviceSide {
        taken: 0,
        used: 0,
        size: 4,
    };
    q.add(&one).expect("add");
    device.give(&memory, 1 << 20, 0);
    assert_eq!(q.pop_used(), Err(QueueError::Malformed));
}

#[test]
fn dr_t6_bad_queues_and_chains_are_refused() {
    let memory = Fake::new(queue::BYTES);
    for size in [0, 3, 512] {
        assert!(matches!(
            Queue::new(Sub(&memory), size),
            Err(QueueError::Invalid)
        ));
    }
    assert!(matches!(
        Queue::new(Fake::new(4096), 4),
        Err(QueueError::Invalid)
    ));
    assert!(matches!(
        Queue::new(Fake::read_only(queue::BYTES), 4),
        Err(QueueError::Kernel(Error::AccessDenied))
    ));
    let mut q = Queue::new(Sub(&memory), 2).expect("queue");
    assert_eq!(q.add(&[]), Err(QueueError::Invalid));
    let part = Buffer {
        addr: 1,
        len: 1,
        device_writes: false,
    };
    assert_eq!(q.add(&[part; 3]), Err(QueueError::Invalid));
    assert_eq!(
        q.add(&[Buffer { len: 0, ..part }]),
        Err(QueueError::Invalid)
    );
}

/// A borrowed window, so a test keeps the memory to play the device with.
struct Sub<'a>(&'a Fake);

impl Region for Sub<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read8(&self, offset: usize) -> Result<u8, Error> {
        self.0.read8(offset)
    }
    fn read16(&self, offset: usize) -> Result<u16, Error> {
        self.0.read16(offset)
    }
    fn read32(&self, offset: usize) -> Result<u32, Error> {
        self.0.read32(offset)
    }
    fn read64(&self, offset: usize) -> Result<u64, Error> {
        self.0.read64(offset)
    }
    fn write8(&self, offset: usize, value: u8) -> Result<(), Error> {
        self.0.write8(offset, value)
    }
    fn write16(&self, offset: usize, value: u16) -> Result<(), Error> {
        self.0.write16(offset, value)
    }
    fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
        self.0.write32(offset, value)
    }
    fn write64(&self, offset: usize, value: u64) -> Result<(), Error> {
        self.0.write64(offset, value)
    }
}

/// NT-T6 (SDD 29, k): a network device's configuration, the header of a frame sent,
/// what a received buffer holds, and the command that sets the queue pairs, each
/// value the device gives checked.
#[test]
fn nt_t6_virtio_net_configuration_frames_and_commands() {
    use crate::net::{
        self, NetConfig, NetError, ctrl_queue, feature, read_config, received, rx_queue, tx_queue,
        write_header, write_pairs_command,
    };
    let c = Fake::new(16);
    c.poke(0, &[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    c.poke(6, &1u16.to_le_bytes());
    c.poke(8, &4u16.to_le_bytes());
    c.poke(10, &9000u16.to_le_bytes());
    let all = feature::MAC | feature::STATUS | feature::MQ | feature::MTU;
    assert_eq!(
        read_config(&c, all),
        Ok(NetConfig {
            mac: 0x5254_0012_3456,
            link_up: true,
            max_pairs: 4,
            mtu: 9000
        })
    );
    // Without the features, the device is up, has one pair and the default MTU.
    assert_eq!(
        read_config(&c, feature::MAC),
        Ok(NetConfig {
            mac: 0x5254_0012_3456,
            link_up: true,
            max_pairs: 1,
            mtu: net::DEFAULT_MTU
        })
    );
    c.poke(6, &0u16.to_le_bytes());
    assert!(!read_config(&c, all).expect("down").link_up);
    assert_eq!(net::link_up(&c), Ok(false));
    c.poke(8, &0u16.to_le_bytes());
    assert_eq!(read_config(&c, all), Err(NetError::BadPairs(0)));
    c.poke(8, &0x8001u16.to_le_bytes());
    assert_eq!(read_config(&c, all), Err(NetError::BadPairs(0x8001)));
    c.poke(8, &2u16.to_le_bytes());
    c.poke(10, &67u16.to_le_bytes());
    assert_eq!(read_config(&c, all), Err(NetError::BadMtu(67)));
    c.poke(0, &[0x01, 0, 0x5e, 0, 0, 1]);
    assert!(matches!(
        read_config(&c, feature::MAC),
        Err(NetError::BadAddress(_))
    ));
    c.poke(0, &[0; 6]);
    assert_eq!(read_config(&c, feature::MAC), Err(NetError::BadAddress(0)));

    let page = Fake::new(4096);
    page.poke(0, &[0xff; 12]);
    write_header(&page, 0).expect("header");
    assert_eq!(page.peek(0, 12), [0; 12]);
    // A received buffer: header, then the frame.
    let frame = |used| received(&page, 0, 4096, used, 1514).map(|r| r.frame);
    assert_eq!(frame(12 + 60), Ok(12..72));
    assert_eq!(frame(12 + 1514), Ok(12..1526));
    for bad in [0, 11, 12 + 13, 12 + 1515, 4097] {
        assert_eq!(frame(bad), Err(NetError::BadLength(bad)), "{bad}");
    }
    write_pairs_command(&page, 64, 3).expect("command");
    assert_eq!(page.peek(64, 4), [4, 0, 3, 0]);
    assert_eq!(
        (rx_queue(0), tx_queue(0), rx_queue(3), tx_queue(3)),
        (0, 1, 6, 7)
    );
    assert_eq!(ctrl_queue(4, true), 8);
    assert_eq!(ctrl_queue(1, false), 2);
}

/// A frame as a host sends it: Ethernet, IPv4 from `src` to 192.168.77.119 with
/// protocol `proto`, then `l4`. The IPv4 header's checksum is left zero: completing a
/// partial checksum touches only the transport's.
fn ipv4_frame(src: [u8; 4], proto: u8, l4: &[u8]) -> Vec<u8> {
    let total = (20 + l4.len()) as u16;
    let mut f = vec![
        0xfa, 0x16, 0x3e, 0xd8, 0x73, 0xdc, 0xfa, 0x16, 0x3e, 0x00, 0x00, 0x01, 0x08, 0x00,
    ];
    f.extend([0x45, 0]);
    f.extend(total.to_be_bytes());
    f.extend([0x12, 0x34, 0x40, 0x00, 64, proto, 0, 0]);
    f.extend(src);
    f.extend([192, 168, 77, 119]);
    f.extend(l4);
    f
}

/// The pseudo-header's sum, folded, as Linux's TCP and UDP leave it in the checksum
/// field of a frame whose checksum is partial (`~csum_tcpudp_magic`).
fn pseudo_sum(src: [u8; 4], proto: u8, l4_len: usize) -> u16 {
    let mut p = Vec::from(src);
    p.extend([192, 168, 77, 119, 0, proto]);
    p.extend((l4_len as u16).to_be_bytes());
    crate::net::ones_sum(&p)
}

/// Puts a receive buffer at 0 in `page`: a header with `flags`, `gso`, `csum_start`
/// and `csum_offset`, then `frame`; says the bytes the device wrote.
fn put_buffer(page: &Fake, flags: u8, gso: u8, start: u16, offset: u16, frame: &[u8]) -> u32 {
    let mut header = [0u8; 12];
    header[0] = flags;
    header[1] = gso;
    header[6..8].copy_from_slice(&start.to_le_bytes());
    header[8..10].copy_from_slice(&offset.to_le_bytes());
    header[10] = 1;
    page.poke(0, &header);
    page.poke(12, frame);
    (12 + frame.len()) as u32
}

/// NT-T6, item 1.6f (Servercore run 9: 46 buffers refused against 23 frames taken, and
/// the metadata service never answered): a frame whose header has `DATA_VALID` is
/// delivered as it came. Linux's tun writes `DATA_VALID` on every frame whose checksum
/// the host trusts, whatever the guest negotiated (`virtio_net_hdr_from_skb` with
/// `has_data_valid`), and Linux's virtio_net takes it; the stack still checks the
/// checksum itself. Bits the driver does not know, `RSC_INFO` without `NEEDS_CSUM`
/// among them, are ignored, as virtio 1.2, 5.1.6.4.2 says a driver must.
#[test]
fn nt_t6_a_frame_with_data_valid_is_delivered() {
    use crate::net::{Received, flag, received};
    let segment = [
        0, 80, 0xc0, 0x00, 0, 0, 0, 1, 0, 0, 0, 2, 0x50, 0x18, 0xff, 0xff, 0xcd, 0x85, 0, 0,
    ];
    let frame = ipv4_frame([169, 254, 169, 254], 6, &segment);
    let page = Fake::new(4096);
    for flags in [
        flag::DATA_VALID,
        flag::RSC_INFO,
        flag::DATA_VALID | flag::RSC_INFO,
        0x08,
        0x80,
        0xfa,
    ] {
        let used = put_buffer(&page, flags, 0, 0, 0, &frame);
        let got = received(&page, 0, 4096, used, 1514).expect("delivered");
        assert_eq!(
            got,
            Received {
                frame: 12..12 + frame.len(),
                flags,
                partial: None
            },
            "{flags:#x}"
        );
        let mut bytes = page.peek(12, frame.len());
        got.complete(&mut bytes).expect("nothing to complete");
        assert_eq!(bytes, frame, "{flags:#x}: as it came");
    }
}

/// NT-T6, item 1.6f: a frame whose checksum is partial (`NEEDS_CSUM`), which a device
/// should not send without `GUEST_CSUM` but Linux's virtio_net takes whatever was
/// negotiated, has its checksum completed from `csum_start` and `csum_offset`, so the
/// stack checks a whole one. The expected checksums were computed apart, by RFC 1071.
#[test]
fn nt_t6_a_partial_checksum_is_completed() {
    use crate::net::{Partial, Received, flag, ones_sum, received};
    // RFC 1071, section 3's example.
    assert_eq!(
        ones_sum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]),
        0xddf2
    );
    assert_eq!(ones_sum(&[]), 0);
    assert_eq!(ones_sum(&[0xab]), 0xab00);
    let page = Fake::new(4096);

    // TCP from the metadata service, of an odd length: an answer's first line.
    let src = [169, 254, 169, 254];
    let mut segment = vec![
        0, 80, 0xc0, 0x00, 0, 0, 0, 1, 0, 0, 0, 2, 0x50, 0x18, 0xff, 0xff, 0, 0, 0, 0,
    ];
    segment.extend(b"HTTP/1.1 200 OK\r\n");
    let partial = pseudo_sum(src, 6, segment.len());
    assert_eq!(partial, 0x6248);
    segment[16..18].copy_from_slice(&partial.to_be_bytes());
    let frame = ipv4_frame(src, 6, &segment);
    for flags in [flag::NEEDS_CSUM, flag::NEEDS_CSUM | flag::DATA_VALID] {
        let used = put_buffer(&page, flags, 0, 34, 16, &frame);
        let got = received(&page, 0, 4096, used, 1514).expect("taken");
        assert_eq!(
            got,
            Received {
                frame: 12..12 + frame.len(),
                flags,
                partial: Some(Partial {
                    start: 34,
                    offset: 16
                })
            }
        );
        let mut bytes = page.peek(12, frame.len());
        got.complete(&mut bytes).expect("completed");
        assert_eq!(bytes[50..52], [0xcd, 0x85]);
        assert_eq!(bytes[..50], frame[..50]);
        assert_eq!(bytes[52..], frame[52..]);
        // What the stack checks: the pseudo-header and the segment sum to all ones.
        let mut whole = Vec::from(src);
        whole.extend([192, 168, 77, 119, 0, 6, 0, segment.len() as u8]);
        whole.extend(&bytes[34..]);
        assert_eq!(ones_sum(&whole), 0xffff);
    }

    // UDP from the router, its checksum six bytes in.
    let src = [192, 168, 77, 1];
    let mut datagram = vec![0, 53, 0x9c, 0x40, 0, 14];
    let partial = pseudo_sum(src, 17, 14);
    assert_eq!(partial, 0x1be9);
    datagram.extend(partial.to_be_bytes());
    datagram.extend(b"moruna");
    let frame = ipv4_frame(src, 17, &datagram);
    let used = put_buffer(&page, flag::NEEDS_CSUM, 0, 34, 6, &frame);
    let got = received(&page, 0, 4096, used, 1514).expect("taken");
    let mut bytes = page.peek(12, frame.len());
    got.complete(&mut bytes).expect("completed");
    assert_eq!(bytes[40..42], [0xf9, 0x4c]);

    // A sum that complements to zero is stored as 0xffff, which UDP needs.
    let zero = Received {
        frame: 0..18,
        flags: flag::NEEDS_CSUM,
        partial: Some(Partial {
            start: 14,
            offset: 2,
        }),
    };
    let mut bytes = [0u8; 18];
    bytes[14..16].copy_from_slice(&[0xff, 0xff]);
    zero.complete(&mut bytes).expect("completed");
    assert_eq!(bytes[16..], [0xff, 0xff]);
}

/// NT-T6, item 1.6f: what is refused, and why. A GSO type, which needs a feature this
/// driver never offers, is refused whatever the flags; a partial checksum whose field
/// lies past the frame's end, or whose offsets are `RSC_INFO`'s counts, is refused,
/// since it cannot be completed.
#[test]
fn nt_t6_segmentation_and_unusable_checksum_offsets_are_refused() {
    use crate::net::{NetError, Partial, Received, flag, received};
    let page = Fake::new(4096);
    let frame = [0u8; 60];
    for gso in [1, 3, 4, 5, 0x80, 0x81, 0xff] {
        for flags in [0, flag::NEEDS_CSUM, flag::DATA_VALID] {
            let used = put_buffer(&page, flags, gso, 34, 16, &frame);
            assert_eq!(
                received(&page, 0, 4096, used, 1514),
                Err(NetError::Gso(gso)),
                "{gso:#x} {flags:#x}"
            );
        }
    }
    // The field's last byte is the frame's last: taken; one further: refused.
    let used = put_buffer(&page, flag::NEEDS_CSUM, 0, 34, 24, &frame);
    assert!(received(&page, 0, 4096, used, 1514).is_ok());
    for (start, offset) in [(34, 25), (59, 0), (60, 0), (0, 59), (0xffff, 0xffff)] {
        let used = put_buffer(&page, flag::NEEDS_CSUM, 0, start, offset, &frame);
        assert_eq!(
            received(&page, 0, 4096, used, 1514),
            Err(NetError::Checksum {
                flags: flag::NEEDS_CSUM,
                start,
                offset
            }),
            "{start} {offset}"
        );
    }
    let flags = flag::NEEDS_CSUM | flag::RSC_INFO;
    let used = put_buffer(&page, flags, 0, 34, 16, &frame);
    assert_eq!(
        received(&page, 0, 4096, used, 1514),
        Err(NetError::Checksum {
            flags,
            start: 34,
            offset: 16
        })
    );
    // Completing in fewer bytes than the offsets were checked against is refused.
    let got = Received {
        frame: 12..72,
        flags: 3,
        partial: Some(Partial {
            start: 34,
            offset: 16,
        }),
    };
    assert_eq!(
        got.complete(&mut [0u8; 51]),
        Err(NetError::Checksum {
            flags: 3,
            start: 34,
            offset: 16
        })
    );
    assert_eq!(got.complete(&mut [0u8; 52]), Ok(()));
    // A header the driver cannot read is the kernel's error, not the buffer's.
    assert_eq!(
        received(&Fake::new(1), 0, 4096, 100, 1514),
        Err(NetError::Kernel(Error::OutOfRange))
    );
}

/// NT-T6, item 1.6f: the refusals are counted by reason (a length out of bounds, each
/// flags value, each GSO type) and the frames taken by their flags, for the driver's
/// last line; the first refusal is told once, with the bytes written and its header.
#[test]
fn nt_t6_refusals_are_counted_by_reason_and_the_first_is_told() {
    use crate::net::{First, NetError, Reason, RxCounts, header};
    use alloc::string::ToString;
    assert_eq!(Reason::of(&NetError::BadLength(5)), Some(Reason::Length));
    let checksum = NetError::Checksum {
        flags: 5,
        start: 1,
        offset: 2,
    };
    assert_eq!(Reason::of(&checksum), Some(Reason::Flags(5)));
    assert_eq!(Reason::of(&NetError::Gso(4)), Some(Reason::Gso(4)));
    for not_the_buffer in [
        NetError::Kernel(Error::OutOfRange),
        NetError::BadAddress(0),
        NetError::BadPairs(0),
        NetError::BadMtu(0),
    ] {
        assert_eq!(Reason::of(&not_the_buffer), None);
    }

    let mut counts = RxCounts::default();
    let nothing = "0 malformed from the card, 0 taken with flags";
    assert_eq!(counts.to_string(), nothing);
    assert_eq!(counts.first_to_print(), None);
    counts.take(0);
    assert_eq!(counts.to_string(), nothing);
    for flags in [2, 2, 1, 2] {
        counts.take(flags);
    }
    assert_eq!(
        counts.to_string(),
        "0 malformed from the card, 4 taken with flags (0x01: 1, 0x02: 3)"
    );

    let page = Fake::new(64);
    page.poke(0, &[0x05, 0, 0, 0, 0, 0, 0x22, 0, 0x10, 0, 1, 0]);
    let first = header(&page, 0).expect("header");
    assert_eq!(first, [0x05, 0, 0, 0, 0, 0, 0x22, 0, 0x10, 0, 1, 0]);
    assert_eq!(header(&page, 60), Err(Error::OutOfRange));
    counts.refuse(Reason::Flags(5), 74, first);
    counts.refuse(Reason::Gso(1), 1526, [0; 12]);
    counts.refuse(Reason::Length, 4097, [0; 12]);
    counts.refuse(Reason::Gso(1), 1526, [0; 12]);
    assert_eq!(counts.refused(), 4);
    assert_eq!(
        counts.to_string(),
        "4 malformed from the card (length: 1, flags 0x05: 1, gso 0x01: 2), \
         4 taken with flags (0x01: 1, 0x02: 3)"
    );
    let told = counts.first_to_print().expect("the first");
    assert_eq!(
        told,
        First {
            reason: Reason::Flags(5),
            used: 74,
            header: first
        }
    );
    assert_eq!(
        told.to_string(),
        "the first malformed buffer from the card: flags 0x05, 74 bytes written, \
         header 05 00 00 00 00 00 22 00 10 00 01 00"
    );
    // Told once: later refusals are counted, not told.
    assert_eq!(counts.first_to_print(), None);
    counts.refuse(Reason::Length, 3, [0; 12]);
    assert_eq!(counts.first_to_print(), None);
    assert_eq!(counts.refused(), 5);
    assert_eq!(Reason::Length.to_string(), "length");
    assert_eq!(Reason::Gso(0x81).to_string(), "gso 0x81");
}
