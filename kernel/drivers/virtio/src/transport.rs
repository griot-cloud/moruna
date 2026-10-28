//! The virtio PCI transport (virtio 1.2, 4.1): where the configuration structures are
//! and how a driver brings a device up through them.

use alloc::vec::Vec;
use core::fmt;

use moruna_abi::Error;
use moruna_mmio::{Region, Sub};
use moruna_pci::{Config, cap};

use crate::{NO_VECTOR, feature, status};

/// The configuration structure types of a virtio vendor capability.
pub mod cfg_type {
    pub const COMMON: u8 = 1;
    pub const NOTIFY: u8 = 2;
    pub const ISR: u8 = 3;
    pub const DEVICE: u8 = 4;
}

/// Offsets in the common configuration structure (virtio 1.2, 4.1.4.3).
pub mod common {
    pub const DEVICE_FEATURE_SELECT: usize = 0x00;
    pub const DEVICE_FEATURE: usize = 0x04;
    pub const DRIVER_FEATURE_SELECT: usize = 0x08;
    pub const DRIVER_FEATURE: usize = 0x0c;
    pub const MSIX_CONFIG: usize = 0x10;
    pub const NUM_QUEUES: usize = 0x12;
    pub const DEVICE_STATUS: usize = 0x14;
    pub const CONFIG_GENERATION: usize = 0x15;
    pub const QUEUE_SELECT: usize = 0x16;
    pub const QUEUE_SIZE: usize = 0x18;
    pub const QUEUE_MSIX_VECTOR: usize = 0x1a;
    pub const QUEUE_ENABLE: usize = 0x1c;
    pub const QUEUE_NOTIFY_OFF: usize = 0x1e;
    pub const QUEUE_DESC: usize = 0x20;
    pub const QUEUE_DRIVER: usize = 0x28;
    pub const QUEUE_DEVICE: usize = 0x30;
    /// The structure's size up to the last field this code uses.
    pub const SIZE: usize = 0x38;
}

/// Why bringing a device up failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtioError {
    /// A kernel call or a window access failed.
    Kernel(Error),
    /// The function has no capability for this configuration structure.
    Missing(&'static str),
    /// A capability places its structure outside its BAR, or in a BAR with no window.
    BadCapability,
    /// The device does not offer `VERSION_1`: it is a legacy device.
    Legacy,
    /// The device lacks a feature the driver requires.
    MissingFeature(u64),
    /// The device cleared `FEATURES_OK`: it refused the features the driver chose.
    FeaturesRefused,
    /// The device did not finish its reset.
    ResetTimeout,
    /// The device has no queue with this index, or a size of zero.
    NoQueue(u16),
    /// The device refused an MSI-X vector.
    VectorRefused,
}

impl From<Error> for VirtioError {
    fn from(e: Error) -> VirtioError {
        VirtioError::Kernel(e)
    }
}

impl fmt::Display for VirtioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VirtioError::Kernel(e) => write!(f, "kernel: {}", e.name()),
            VirtioError::Missing(what) => write!(f, "no {what} configuration capability"),
            VirtioError::BadCapability => f.write_str("a capability lies outside its BAR"),
            VirtioError::Legacy => f.write_str("the device does not offer VERSION_1"),
            VirtioError::MissingFeature(bits) => {
                write!(f, "the device lacks required features {bits:#x}")
            }
            VirtioError::FeaturesRefused => f.write_str("the device refused the features"),
            VirtioError::ResetTimeout => f.write_str("the device did not finish its reset"),
            VirtioError::NoQueue(i) => write!(f, "the device has no queue {i}"),
            VirtioError::VectorRefused => f.write_str("the device refused the MSI-X vector"),
        }
    }
}

/// A configuration structure: `length` bytes at `offset` in BAR `bar`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub bar: usize,
    pub offset: u64,
    pub length: u64,
}

/// Where a device's configuration structures are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub common: Window,
    pub notify: Window,
    /// Bytes between the notify addresses of consecutive `queue_notify_off` values.
    pub notify_multiplier: u32,
    pub isr: Window,
    pub device: Option<Window>,
}

impl Layout {
    /// Reads the virtio vendor capabilities of a function. The first capability of
    /// each type is the one used (virtio 1.2, 4.1.4).
    pub fn read<R: Region>(config: &Config<R>) -> Result<Layout, VirtioError> {
        let mut found: [Option<(Window, u32)>; 5] = [None; 5];
        for (id, at) in config.capabilities()? {
            if id != cap::VENDOR {
                continue;
            }
            let r = config.region();
            let kind = r.read8(at + 3)?;
            let bar = r.read8(at + 4)? as usize;
            let window = Window {
                bar,
                offset: r.read32(at + 8)? as u64,
                length: r.read32(at + 12)? as u64,
            };
            let multiplier = if kind == cfg_type::NOTIFY {
                r.read32(at + 16)?
            } else {
                0
            };
            if (cfg_type::COMMON..=cfg_type::DEVICE).contains(&kind)
                && bar < 6
                && found[kind as usize].is_none()
            {
                found[kind as usize] = Some((window, multiplier));
            }
        }
        let need = |kind: u8, what| found[kind as usize].ok_or(VirtioError::Missing(what));
        let (notify, notify_multiplier) = need(cfg_type::NOTIFY, "notify")?;
        let layout = Layout {
            common: need(cfg_type::COMMON, "common")?.0,
            notify,
            notify_multiplier,
            isr: need(cfg_type::ISR, "ISR")?.0,
            device: found[cfg_type::DEVICE as usize].map(|(w, _)| w),
        };
        if layout.common.length < common::SIZE as u64 || layout.isr.length < 1 {
            return Err(VirtioError::BadCapability);
        }
        Ok(layout)
    }

    /// The BARs the layout uses.
    pub fn bars(&self) -> Vec<usize> {
        let mut bars = Vec::new();
        for w in [
            Some(self.common),
            Some(self.notify),
            Some(self.isr),
            self.device,
        ]
        .into_iter()
        .flatten()
        {
            if !bars.contains(&w.bar) {
                bars.push(w.bar);
            }
        }
        bars
    }
}

/// How many times a reset is polled before it counts as stuck.
const RESET_POLLS: u32 = 1_000_000;

/// A device's transport: its BAR windows and where in them its structures are. Each
/// BAR window comes with the offset of the BAR's first byte in it (a window maps
/// whole pages; a BAR need not start on one).
pub struct Transport<R: Region> {
    bars: [Option<(R, u64)>; 6],
    layout: Layout,
}

impl<R: Region> Transport<R> {
    /// A transport over `bars`, which must hold a window for every BAR the layout
    /// uses, each large enough for its structures.
    pub fn new(bars: [Option<(R, u64)>; 6], layout: Layout) -> Result<Transport<R>, VirtioError> {
        let t = Transport { bars, layout };
        for w in [
            Some(layout.common),
            Some(layout.notify),
            Some(layout.isr),
            layout.device,
        ]
        .into_iter()
        .flatten()
        {
            t.window(w)?;
        }
        Ok(t)
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The window over BAR `index`, if the transport has one.
    pub fn bar(&self, index: usize) -> Option<&R> {
        self.bars
            .get(index)
            .and_then(Option::as_ref)
            .map(|(r, _)| r)
    }

    fn window(&self, w: Window) -> Result<Sub<'_, R>, VirtioError> {
        let (region, delta) = self
            .bars
            .get(w.bar)
            .and_then(Option::as_ref)
            .ok_or(VirtioError::BadCapability)?;
        let base = delta
            .checked_add(w.offset)
            .and_then(|b| usize::try_from(b).ok())
            .ok_or(VirtioError::BadCapability)?;
        let length = usize::try_from(w.length).map_err(|_| VirtioError::BadCapability)?;
        Sub::new(region, base, length).map_err(|_| VirtioError::BadCapability)
    }

    fn common(&self) -> Result<Sub<'_, R>, VirtioError> {
        self.window(self.layout.common)
    }

    pub fn status(&self) -> Result<u8, VirtioError> {
        Ok(self.common()?.read8(common::DEVICE_STATUS)?)
    }

    fn set_status(&self, value: u8) -> Result<(), VirtioError> {
        Ok(self.common()?.write8(common::DEVICE_STATUS, value)?)
    }

    fn add_status(&self, bits: u8) -> Result<(), VirtioError> {
        self.set_status(self.status()? | bits)
    }

    /// Resets the device and waits until it says it has (virtio 1.2, 4.1.4.3.2): after
    /// this it uses no buffer it was given before.
    pub fn reset(&self) -> Result<(), VirtioError> {
        self.set_status(0)?;
        for _ in 0..RESET_POLLS {
            if self.status()? == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(VirtioError::ResetTimeout)
    }

    /// The features the device offers.
    pub fn device_features(&self) -> Result<u64, VirtioError> {
        let c = self.common()?;
        c.write32(common::DEVICE_FEATURE_SELECT, 0)?;
        let low = c.read32(common::DEVICE_FEATURE)? as u64;
        c.write32(common::DEVICE_FEATURE_SELECT, 1)?;
        let high = c.read32(common::DEVICE_FEATURE)? as u64;
        Ok(high << 32 | low)
    }

    /// Acknowledges the device and negotiates (virtio 1.2, 3.1.1): takes `wanted` and
    /// `ACCESS_PLATFORM` where offered, refuses a device without `VERSION_1` or
    /// without every bit of `required`, and returns the features taken.
    pub fn negotiate(&self, wanted: u64, required: u64) -> Result<u64, VirtioError> {
        self.add_status(status::ACKNOWLEDGE)?;
        self.add_status(status::DRIVER)?;
        let offered = self.device_features()?;
        if offered & feature::VERSION_1 == 0 {
            self.fail()?;
            return Err(VirtioError::Legacy);
        }
        if offered & required != required {
            self.fail()?;
            return Err(VirtioError::MissingFeature(required & !offered));
        }
        let taken = offered & (wanted | required | feature::VERSION_1 | feature::ACCESS_PLATFORM);
        let c = self.common()?;
        c.write32(common::DRIVER_FEATURE_SELECT, 0)?;
        c.write32(common::DRIVER_FEATURE, taken as u32)?;
        c.write32(common::DRIVER_FEATURE_SELECT, 1)?;
        c.write32(common::DRIVER_FEATURE, (taken >> 32) as u32)?;
        self.add_status(status::FEATURES_OK)?;
        if self.status()? & status::FEATURES_OK == 0 {
            self.fail()?;
            return Err(VirtioError::FeaturesRefused);
        }
        Ok(taken)
    }

    /// How many queues the device has.
    pub fn queues(&self) -> Result<u16, VirtioError> {
        Ok(self.common()?.read16(common::NUM_QUEUES)?)
    }

    /// The largest size queue `index` takes.
    pub fn queue_max(&self, index: u16) -> Result<u16, VirtioError> {
        if index >= self.queues()? {
            return Err(VirtioError::NoQueue(index));
        }
        let c = self.common()?;
        c.write16(common::QUEUE_SELECT, index)?;
        match c.read16(common::QUEUE_SIZE)? {
            0 => Err(VirtioError::NoQueue(index)),
            n => Ok(n),
        }
    }

    /// Sets up and enables queue `index` with `size` entries whose descriptor table,
    /// driver (available) ring and device (used) ring are at those device addresses,
    /// interrupting through MSI-X entry `vector`. Returns its notify offset.
    pub fn setup_queue(
        &self,
        index: u16,
        size: u16,
        rings: [u64; 3],
        vector: u16,
    ) -> Result<u16, VirtioError> {
        let max = self.queue_max(index)?;
        if size == 0 || size > max {
            return Err(VirtioError::NoQueue(index));
        }
        let c = self.common()?;
        c.write16(common::QUEUE_SELECT, index)?;
        c.write16(common::QUEUE_SIZE, size)?;
        c.write64_split(common::QUEUE_DESC, rings[0])?;
        c.write64_split(common::QUEUE_DRIVER, rings[1])?;
        c.write64_split(common::QUEUE_DEVICE, rings[2])?;
        c.write16(common::QUEUE_MSIX_VECTOR, vector)?;
        if vector != NO_VECTOR && c.read16(common::QUEUE_MSIX_VECTOR)? != vector {
            return Err(VirtioError::VectorRefused);
        }
        let notify_off = c.read16(common::QUEUE_NOTIFY_OFF)?;
        c.write16(common::QUEUE_ENABLE, 1)?;
        Ok(notify_off)
    }

    /// The MSI-X entry configuration changes interrupt through, or `NO_VECTOR`.
    pub fn set_config_vector(&self, vector: u16) -> Result<(), VirtioError> {
        let c = self.common()?;
        c.write16(common::MSIX_CONFIG, vector)?;
        if vector != NO_VECTOR && c.read16(common::MSIX_CONFIG)? != vector {
            return Err(VirtioError::VectorRefused);
        }
        Ok(())
    }

    /// Tells the device the driver is ready: queues set up, buffers may flow.
    pub fn driver_ok(&self) -> Result<(), VirtioError> {
        self.add_status(status::DRIVER_OK)
    }

    /// Tells the device the driver has given up on it.
    pub fn fail(&self) -> Result<(), VirtioError> {
        self.add_status(status::FAILED)
    }

    /// Tells the device queue `index` (whose notify offset is `notify_off`) has new
    /// buffers.
    pub fn notify(&self, index: u16, notify_off: u16) -> Result<(), VirtioError> {
        let at = notify_off as usize * self.layout.notify_multiplier as usize;
        Ok(self.window(self.layout.notify)?.write16(at, index)?)
    }

    /// Reads and clears the ISR status (virtio 1.2, 4.1.4.5): bit 0 a queue
    /// interrupt, bit 1 a configuration change. A device whose interrupts arrive by
    /// MSI-X does not set it.
    pub fn isr(&self) -> Result<u8, VirtioError> {
        Ok(self.window(self.layout.isr)?.read8(0)?)
    }

    /// Reads the device-specific configuration with `read`, again until the
    /// configuration generation is the same before and after (virtio 1.2, 2.5.1).
    pub fn device_config<T>(
        &self,
        mut read: impl FnMut(&Sub<'_, R>) -> Result<T, Error>,
    ) -> Result<T, VirtioError> {
        let device = self.layout.device.ok_or(VirtioError::Missing("device"))?;
        let window = self.window(device)?;
        let c = self.common()?;
        loop {
            let before = c.read8(common::CONFIG_GENERATION)?;
            let value = read(&window)?;
            if c.read8(common::CONFIG_GENERATION)? == before {
                return Ok(value);
            }
        }
    }
}
