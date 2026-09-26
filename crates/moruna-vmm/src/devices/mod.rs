//! The device model (MH 4.8.2): one [`Device`] trait, one implementation per device, and a
//! [`Bus`] that routes a guest access to the device that owns the address.
//!
//! The guest's devices are exactly: `virtio-blk` (one per disk), `virtio-vsock`, `virtio-mem`,
//! the 8250 serial console, and on x86_64 the CPU hot-plug controller and the two legacy ports
//! a Linux guest uses to power off and reset. All virtio devices sit on `virtio-mmio`. There is
//! no network device: [`DeviceKind`] has no variant for one and the virtio device ids the
//! monitor can construct exclude `VIRTIO_ID_NET` (MH H10, `vm_t2_no_network_device_kind`).

pub mod block;
pub mod cpu_hotplug;
pub mod legacy;
pub mod mem;
pub mod mmio;
pub mod serial;
pub mod virtio;
pub mod vsock;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{Result, VmmError};

/// The guest's memory, as every device sees it.
pub type Mem = vm_memory::GuestMemoryMmap<()>;

/// A device on a bus: the guest reads and writes bytes at offsets within the device's window.
///
/// This is the one trait every device implements (MH 4.8.2: "a trait per device from day
/// one"); a later device, a shared-memory window or a PCI function, implements it too and
/// touches no other device (MH 4.8.6).
pub trait Device: Send {
    /// A short name for diagnostics.
    fn name(&self) -> &'static str;
    /// The guest reads `data.len()` bytes at `offset` within the device's window.
    fn read(&mut self, offset: u64, data: &mut [u8]);
    /// The guest writes `data` at `offset` within the device's window.
    fn write(&mut self, offset: u64, data: &[u8]);
}

/// Every kind of device the monitor can attach. There is no network kind (MH H10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceKind {
    /// `virtio-blk`, one per disk.
    Block,
    /// `virtio-vsock`, the only channel to the host.
    Vsock,
    /// `virtio-mem`, memory hot-plug.
    Mem,
    /// The 8250 serial console.
    Serial,
    /// The x86_64 ACPI CPU hot-plug controller.
    CpuHotplug,
    /// The x86_64 i8042 reset port and ACPI sleep/reset registers.
    Legacy,
}

impl DeviceKind {
    /// Every kind, for the exhaustive H10 test.
    pub const ALL: [DeviceKind; 6] = [
        DeviceKind::Block,
        DeviceKind::Vsock,
        DeviceKind::Mem,
        DeviceKind::Serial,
        DeviceKind::CpuHotplug,
        DeviceKind::Legacy,
    ];

    /// The virtio device id, for the kinds that are virtio devices.
    pub fn virtio_id(self) -> Option<u32> {
        match self {
            DeviceKind::Block => Some(virtio::ID_BLOCK),
            DeviceKind::Vsock => Some(virtio::ID_VSOCK),
            DeviceKind::Mem => Some(virtio::ID_MEM),
            DeviceKind::Serial | DeviceKind::CpuHotplug | DeviceKind::Legacy => None,
        }
    }
}

/// An interrupt line into the guest. On Linux an `eventfd` registered with `KVM_IRQFD`; in
/// tests a counter.
pub trait Irq: Send + Sync {
    /// Raise the line (edge).
    fn trigger(&self) -> std::io::Result<()>;
}

/// Why the guest stopped, as the monitor learnt it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The guest powered off (ACPI S5, PSCI `SYSTEM_OFF`).
    PowerOff,
    /// The guest reset (i8042, the ACPI reset register, PSCI `SYSTEM_RESET`, a triple fault).
    /// A one-run monitor never reboots: a reset ends the run.
    Reset,
    /// KVM reported a guest crash (`KVM_SYSTEM_EVENT_CRASH`).
    Crash,
    /// A peer on the control socket asked for the guest to be stopped.
    Killed,
    /// The monitor or the hypervisor failed.
    MonitorError(String),
}

/// A one-shot latch: the first reason recorded wins, later ones are ignored, and every thread
/// can ask whether the guest has stopped.
#[derive(Default)]
pub struct StopSignal {
    stopped: AtomicBool,
    reason: Mutex<Option<StopReason>>,
}

impl StopSignal {
    /// Record `reason` if nothing was recorded before; true if this call recorded it.
    pub fn request(&self, reason: StopReason) -> bool {
        let mut r = self.reason.lock().unwrap_or_else(|p| p.into_inner());
        if r.is_some() {
            return false;
        }
        *r = Some(reason);
        self.stopped.store(true, Ordering::SeqCst);
        true
    }

    /// True once a reason has been recorded.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// The recorded reason.
    pub fn reason(&self) -> Option<StopReason> {
        self.reason
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// A shared, lockable device.
pub type SharedDevice = Arc<Mutex<dyn Device>>;

/// An address space (MMIO or port IO) mapping ranges to devices. Ranges never overlap:
/// [`Bus::insert`] refuses an overlapping range.
#[derive(Default, Clone)]
pub struct Bus {
    ranges: BTreeMap<u64, (u64, SharedDevice)>,
}

impl Bus {
    /// Attach `dev` at `[base, base + len)`.
    pub fn insert(&mut self, base: u64, len: u64, dev: SharedDevice) -> Result<()> {
        if len == 0 {
            return Err(VmmError::device("bus", "zero-length range"));
        }
        let end = base
            .checked_add(len)
            .ok_or_else(|| VmmError::device("bus", "range overflows"))?;
        let clash = self
            .ranges
            .range(..end)
            .next_back()
            .is_some_and(|(b, (l, _))| b + l > base);
        if clash {
            return Err(VmmError::device(
                "bus",
                format!("range {base:#x}+{len:#x} overlaps an attached device"),
            ));
        }
        self.ranges.insert(base, (len, dev));
        Ok(())
    }

    /// The device owning `addr` and the offset of `addr` within it.
    pub fn find(&self, addr: u64) -> Option<(SharedDevice, u64)> {
        let (base, (len, dev)) = self.ranges.range(..=addr).next_back()?;
        (addr < base + len).then(|| (dev.clone(), addr - base))
    }

    /// Route a guest read; an access to no device reads as all ones (as on real hardware)
    /// and returns false.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        match self.find(addr) {
            Some((dev, off)) => {
                dev.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .read(off, data);
                true
            }
            None => {
                data.fill(0xff);
                false
            }
        }
    }

    /// Route a guest write; a write to no device is dropped and returns false.
    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.find(addr) {
            Some((dev, off)) => {
                dev.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .write(off, data);
                true
            }
            None => false,
        }
    }

    /// Number of attached devices.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// True when nothing is attached.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo {
        last: Vec<(u64, Vec<u8>)>,
    }

    impl Device for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn read(&mut self, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }
        fn write(&mut self, offset: u64, data: &[u8]) {
            self.last.push((offset, data.to_vec()));
        }
    }

    #[test]
    fn vm_t31_bus_routes_and_refuses_overlaps() {
        let a = Arc::new(Mutex::new(Echo { last: vec![] }));
        let b = Arc::new(Mutex::new(Echo { last: vec![] }));
        let mut bus = Bus::default();
        assert!(bus.is_empty());
        bus.insert(0x1000, 0x100, a.clone()).unwrap();
        bus.insert(0x1100, 0x100, b.clone()).unwrap();
        assert!(bus.insert(0x10ff, 2, a.clone()).is_err());
        assert!(bus.insert(0x0f00, 0x101, a.clone()).is_err());
        assert!(bus.insert(0x1050, 1, a.clone()).is_err());
        assert!(bus.insert(0x2000, 0, a.clone()).is_err());
        assert!(bus.insert(u64::MAX, 2, a.clone()).is_err());
        assert_eq!(bus.len(), 2);

        let mut d = [0u8; 2];
        assert!(bus.read(0x1004, &mut d));
        assert_eq!(d, [4, 4]);
        assert!(bus.write(0x1101, &[9]));
        assert_eq!(b.lock().unwrap().last, vec![(1, vec![9])]);
        assert!(!bus.read(0x1200, &mut d));
        assert_eq!(d, [0xff, 0xff]);
        assert!(!bus.write(0x0fff, &[1]));
        assert!(bus.find(0x10ff).is_some());
        assert!(bus.find(0x1200).is_none());
        assert_eq!(a.lock().unwrap().name(), "echo");
    }

    #[test]
    fn vm_t31_stop_signal_keeps_the_first_reason() {
        let s = StopSignal::default();
        assert!(!s.is_stopped());
        assert_eq!(s.reason(), None);
        assert!(s.request(StopReason::PowerOff));
        assert!(!s.request(StopReason::Reset));
        assert!(s.is_stopped());
        assert_eq!(s.reason(), Some(StopReason::PowerOff));
    }

    /// H10: no device kind is a network device, and no virtio id the monitor can construct
    /// is `VIRTIO_ID_NET` (1).
    #[test]
    fn vm_t2_no_network_device_kind() {
        for k in DeviceKind::ALL {
            let name = format!("{k:?}").to_ascii_lowercase();
            assert!(!name.contains("net") && !name.contains("nic"), "{name}");
            assert_ne!(
                k.virtio_id(),
                Some(virtio_bindings::virtio_ids::VIRTIO_ID_NET)
            );
        }
        let ids: Vec<u32> = DeviceKind::ALL
            .iter()
            .filter_map(|k| k.virtio_id())
            .collect();
        assert_eq!(
            ids,
            vec![virtio::ID_BLOCK, virtio::ID_VSOCK, virtio::ID_MEM]
        );
    }
}
