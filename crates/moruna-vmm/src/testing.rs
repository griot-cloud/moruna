//! Test support: scratch directories, guest memory, and a virtqueue driver that plays the
//! guest's side of a split virtqueue through a device's virtio-mmio registers.
//!
//! Test code only (preamble 6.7: a test writes to a scratch directory unique to its process).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use vm_memory::{Bytes, GuestAddress};

use crate::devices::Irq;
use crate::devices::mmio::regs;
use crate::devices::{Device, Mem};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A fresh directory unique to this process and call. Under `/tmp` and short, so that Unix
/// socket paths inside it stay under `sun_path`'s limit.
pub fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tag: String = tag.chars().take(8).collect();
    let dir = PathBuf::from("/tmp").join(format!("mvmm-{}-{n}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Guest memory of `size` bytes at address 0.
pub fn guest_memory(size: usize) -> Mem {
    Mem::from_ranges(&[(GuestAddress(0), size)]).expect("guest memory")
}

/// An interrupt line that counts its triggers.
#[derive(Default)]
pub struct CountingIrq {
    /// Triggers so far.
    pub count: AtomicU32,
}

impl Irq for CountingIrq {
    fn trigger(&self) -> std::io::Result<()> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl CountingIrq {
    /// Triggers so far.
    pub fn get(&self) -> u32 {
        self.count.load(Ordering::SeqCst)
    }
}

/// Read a 32-bit register of a device.
pub fn rd(dev: &mut dyn Device, off: u64) -> u32 {
    let mut b = [0u8; 4];
    dev.read(off, &mut b);
    u32::from_le_bytes(b)
}

/// Write a 32-bit register of a device.
pub fn wr(dev: &mut dyn Device, off: u64, v: u32) {
    dev.write(off, &v.to_le_bytes());
}

/// Descriptor flag: the chain continues.
pub const F_NEXT: u16 = 1;
/// Descriptor flag: device writes this buffer.
pub const F_WRITE: u16 = 2;

/// The guest side of one split virtqueue in guest memory.
pub struct DriverQueue {
    /// Queue size.
    pub size: u16,
    /// Descriptor table address.
    pub desc: u64,
    /// Available ring address.
    pub avail: u64,
    /// Used ring address.
    pub used: u64,
    next_desc: u16,
    avail_idx: u16,
    used_seen: u16,
}

impl DriverQueue {
    /// Lay a queue of `size` out at `base`.
    pub fn new(base: u64, size: u16) -> Self {
        let desc = base;
        let avail = desc + 16 * size as u64;
        let used = (avail + 6 + 2 * size as u64 + 3) & !3;
        DriverQueue {
            size,
            desc,
            avail,
            used,
            next_desc: 0,
            avail_idx: 0,
            used_seen: 0,
        }
    }

    /// Program this queue as queue `index` of `dev` through its mmio registers.
    pub fn attach(&self, dev: &mut dyn Device, index: u32) {
        wr(dev, regs::QUEUE_SEL, index);
        wr(dev, regs::QUEUE_NUM, self.size as u32);
        wr(dev, regs::QUEUE_DESC_LOW, self.desc as u32);
        wr(dev, regs::QUEUE_DESC_HIGH, (self.desc >> 32) as u32);
        wr(dev, regs::QUEUE_DRIVER_LOW, self.avail as u32);
        wr(dev, regs::QUEUE_DRIVER_HIGH, (self.avail >> 32) as u32);
        wr(dev, regs::QUEUE_DEVICE_LOW, self.used as u32);
        wr(dev, regs::QUEUE_DEVICE_HIGH, (self.used >> 32) as u32);
        wr(dev, regs::QUEUE_READY, 1);
    }

    /// Offer a chain of `(addr, len, device_writable)` buffers; returns its head index.
    pub fn add(&mut self, mem: &Mem, bufs: &[(u64, u32, bool)]) -> u16 {
        let head = self.next_desc;
        for (i, (addr, len, w)) in bufs.iter().enumerate() {
            let idx = (self.next_desc + i as u16) % self.size;
            let mut flags = if *w { F_WRITE } else { 0 };
            let next = (idx + 1) % self.size;
            if i + 1 < bufs.len() {
                flags |= F_NEXT;
            }
            let at = self.desc + 16 * idx as u64;
            mem.write_obj(*addr, GuestAddress(at)).unwrap();
            mem.write_obj(*len, GuestAddress(at + 8)).unwrap();
            mem.write_obj(flags, GuestAddress(at + 12)).unwrap();
            mem.write_obj(next, GuestAddress(at + 14)).unwrap();
        }
        self.next_desc = (self.next_desc + bufs.len() as u16) % self.size;
        let slot = self.avail + 4 + 2 * (self.avail_idx % self.size) as u64;
        mem.write_obj(head, GuestAddress(slot)).unwrap();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        mem.write_obj(self.avail_idx, GuestAddress(self.avail + 2))
            .unwrap();
        head
    }

    /// Used elements the device has returned since the last call: `(head, len)`.
    pub fn used(&mut self, mem: &Mem) -> Vec<(u32, u32)> {
        let idx: u16 = mem.read_obj(GuestAddress(self.used + 2)).unwrap();
        let mut out = Vec::new();
        while self.used_seen != idx {
            let at = self.used + 4 + 8 * (self.used_seen % self.size) as u64;
            let id: u32 = mem.read_obj(GuestAddress(at)).unwrap();
            let len: u32 = mem.read_obj(GuestAddress(at + 4)).unwrap();
            out.push((id, len));
            self.used_seen = self.used_seen.wrapping_add(1);
        }
        out
    }
}

/// Bring a virtio-mmio device to DRIVER_OK the way the Linux driver does: reset, ACK, DRIVER,
/// accept every offered feature, FEATURES_OK, program the queues, DRIVER_OK.
pub fn bring_up(dev: &mut dyn Device, queues: &[&DriverQueue]) -> u64 {
    use crate::devices::virtio::status;
    wr(dev, regs::STATUS, 0);
    wr(dev, regs::STATUS, status::ACKNOWLEDGE);
    wr(dev, regs::STATUS, status::ACKNOWLEDGE | status::DRIVER);
    wr(dev, regs::DEVICE_FEATURES_SEL, 0);
    let lo = rd(dev, regs::DEVICE_FEATURES);
    wr(dev, regs::DEVICE_FEATURES_SEL, 1);
    let hi = rd(dev, regs::DEVICE_FEATURES);
    wr(dev, regs::DRIVER_FEATURES_SEL, 0);
    wr(dev, regs::DRIVER_FEATURES, lo);
    wr(dev, regs::DRIVER_FEATURES_SEL, 1);
    wr(dev, regs::DRIVER_FEATURES, hi);
    let s = status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK;
    wr(dev, regs::STATUS, s);
    assert_eq!(
        rd(dev, regs::STATUS) & status::FEATURES_OK,
        status::FEATURES_OK
    );
    for (i, q) in queues.iter().enumerate() {
        q.attach(dev, i as u32);
    }
    wr(dev, regs::STATUS, s | status::DRIVER_OK);
    ((hi as u64) << 32) | lo as u64
}

/// An `Arc<CountingIrq>` and the same line as the trait object devices take.
pub fn counting_irq() -> (Arc<CountingIrq>, Arc<dyn Irq>) {
    let c = Arc::new(CountingIrq::default());
    (c.clone(), c)
}
