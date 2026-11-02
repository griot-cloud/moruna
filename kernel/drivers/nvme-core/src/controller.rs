//! The controller's registers in BAR 0 (NVMe Base 2.0, 3.1): its capabilities, and how
//! a driver resets it, enables it with an admin queue pair, rings its doorbells and
//! shuts it down.

use alloc::format;

use moruna_mmio::{Clock, Region};

use crate::NvmeError;

/// Register offsets.
pub mod reg {
    pub const CAP: usize = 0x00;
    pub const VS: usize = 0x08;
    pub const INTMS: usize = 0x0c;
    pub const CC: usize = 0x14;
    pub const CSTS: usize = 0x1c;
    pub const AQA: usize = 0x24;
    pub const ASQ: usize = 0x28;
    pub const ACQ: usize = 0x30;
    /// The first doorbell: admin submission queue tail.
    pub const DOORBELLS: usize = 0x1000;
}

/// Controller Configuration bits.
pub mod cc {
    pub const EN: u32 = 1;
    /// I/O submission and completion queue entry sizes, as powers of two: 64 and 16.
    pub const IOSQES_64: u32 = 6 << 16;
    pub const IOCQES_16: u32 = 4 << 20;
    /// Shutdown Notification: normal shutdown.
    pub const SHN_NORMAL: u32 = 0b01 << 14;
    pub const SHN_MASK: u32 = 0b11 << 14;
}

/// Controller Status bits.
pub mod csts {
    pub const RDY: u32 = 1;
    pub const CFS: u32 = 1 << 1;
    pub const SHST_MASK: u32 = 0b11 << 2;
    pub const SHST_COMPLETE: u32 = 0b10 << 2;
}

/// The capabilities register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    /// The most entries an I/O queue may have (MQES + 1).
    pub max_entries: u32,
    /// Queues must be physically contiguous.
    pub contiguous: bool,
    /// The longest the controller takes to become ready or not ready (TO), in ns.
    pub timeout_ns: u64,
    /// Doorbell stride: doorbells are `4 << stride` bytes apart.
    pub stride: u8,
    /// Supports the NVM command set.
    pub nvm: bool,
    /// The smallest and largest memory page size, as `12 + n` powers of two.
    pub mps_min: u8,
    pub mps_max: u8,
}

impl Cap {
    pub fn parse(raw: u64) -> Cap {
        Cap {
            max_entries: (raw & 0xffff) as u32 + 1,
            contiguous: raw & (1 << 16) != 0,
            timeout_ns: ((raw >> 24) & 0xff) * 500_000_000,
            stride: ((raw >> 32) & 0xf) as u8,
            nvm: (raw >> 37) & 1 != 0,
            mps_min: ((raw >> 48) & 0xf) as u8,
            mps_max: ((raw >> 52) & 0xf) as u8,
        }
    }
}

/// The least time a driver waits for the controller, whatever CAP.TO says: a
/// controller reporting zero is given this rather than no time at all.
pub const TIMEOUT_FLOOR_NS: u64 = 500_000_000;
/// The most time a driver waits for a normal shutdown to complete.
pub const SHUTDOWN_NS: u64 = 20_000_000_000;

/// A controller, over a window on BAR 0.
pub struct Controller<R: Region> {
    bar: R,
    pub cap: Cap,
}

impl<R: Region> Controller<R> {
    /// Reads the capabilities and refuses a controller this driver cannot drive: no
    /// NVM command set, a smallest page above 4 KiB, a BAR too small for the admin
    /// doorbells.
    pub fn new(bar: R) -> Result<Controller<R>, NvmeError> {
        if bar.len() < reg::DOORBELLS + 8 {
            return Err(NvmeError::Unsupported(format!(
                "BAR 0 is {} bytes, too small for the admin doorbells",
                bar.len()
            )));
        }
        let low = bar.read32(reg::CAP)? as u64;
        let high = bar.read32(reg::CAP + 4)? as u64;
        let cap = Cap::parse(high << 32 | low);
        if !cap.nvm {
            return Err(NvmeError::Unsupported(format!(
                "CAP {:#018x} lacks the NVM command set",
                high << 32 | low
            )));
        }
        if cap.mps_min != 0 {
            return Err(NvmeError::Unsupported(format!(
                "the smallest memory page is {} bytes, not 4096",
                4096u64 << cap.mps_min
            )));
        }
        if cap.max_entries < 2 {
            return Err(NvmeError::Malformed(format!(
                "a largest queue of {} entries",
                cap.max_entries
            )));
        }
        let ctrl = Controller { bar, cap };
        ctrl.doorbell(0, true)?;
        Ok(ctrl)
    }

    /// The version register: major, minor, tertiary.
    pub fn version(&self) -> Result<(u16, u8, u8), NvmeError> {
        let vs = self.bar.read32(reg::VS)?;
        Ok(((vs >> 16) as u16, (vs >> 8) as u8, vs as u8))
    }

    pub fn status(&self) -> Result<u32, NvmeError> {
        Ok(self.bar.read32(reg::CSTS)?)
    }

    /// Whether the controller reports a fatal status.
    pub fn fatal(&self) -> Result<bool, NvmeError> {
        Ok(self.status()? & csts::CFS != 0)
    }

    fn timeout(&self) -> u64 {
        self.cap.timeout_ns.max(TIMEOUT_FLOOR_NS)
    }

    fn wait(
        &self,
        clock: &dyn Clock,
        limit: u64,
        what: &'static str,
        mut done: impl FnMut(u32) -> Result<bool, NvmeError>,
    ) -> Result<(), NvmeError> {
        let deadline = clock.now().saturating_add(limit);
        loop {
            if done(self.status()?)? {
                return Ok(());
            }
            if clock.now() >= deadline {
                return Err(NvmeError::Timeout(what));
            }
            clock.pause();
        }
    }

    /// Resets the controller (CC.EN to 0) and waits until it says it is not ready:
    /// then it holds no queue and uses no memory it was given (NVMe Base 2.0, 3.5.2).
    pub fn disable(&self, clock: &dyn Clock) -> Result<(), NvmeError> {
        let cc = self.bar.read32(reg::CC)?;
        if cc & cc::EN != 0 {
            self.bar.write32(reg::CC, cc & !cc::EN)?;
        }
        self.wait(
            clock,
            self.timeout(),
            "become not ready after a reset",
            |s| Ok(s & csts::RDY == 0),
        )
    }

    /// Gives the disabled controller its admin queue pair (`entries` each, at device
    /// addresses `asq` and `acq`), enables it with 4 KiB pages, the NVM command set
    /// and 64- and 16-byte queue entries, and waits until it is ready.
    pub fn enable(
        &self,
        clock: &dyn Clock,
        asq: u64,
        acq: u64,
        entries: u16,
    ) -> Result<(), NvmeError> {
        if entries < 2 || entries as u32 > self.cap.max_entries.min(4096) {
            return Err(NvmeError::Unsupported(format!(
                "an admin queue of {entries} entries"
            )));
        }
        let size = entries as u32 - 1;
        self.bar.write32(reg::AQA, size << 16 | size)?;
        self.bar.write64_split(reg::ASQ, asq)?;
        self.bar.write64_split(reg::ACQ, acq)?;
        // CSS 0 (NVM), MPS 0 (4 KiB), AMS 0 (round robin).
        self.bar
            .write32(reg::CC, cc::IOSQES_64 | cc::IOCQES_16 | cc::EN)?;
        self.wait(clock, self.timeout(), "become ready", |s| {
            if s & csts::CFS != 0 {
                return Err(NvmeError::Fatal);
            }
            Ok(s & csts::RDY != 0)
        })
    }

    /// A normal shutdown (CC.SHN): the controller writes its volatile cache to the
    /// media, then reports the shutdown complete.
    pub fn shutdown(&self, clock: &dyn Clock) -> Result<(), NvmeError> {
        let cc = self.bar.read32(reg::CC)?;
        if cc & cc::EN == 0 {
            return Ok(());
        }
        self.bar
            .write32(reg::CC, (cc & !cc::SHN_MASK) | cc::SHN_NORMAL)?;
        self.wait(clock, SHUTDOWN_NS, "complete its shutdown", |s| {
            Ok(s & csts::SHST_MASK == csts::SHST_COMPLETE)
        })
    }

    /// The offset of queue `qid`'s submission tail (or completion head) doorbell;
    /// `Unsupported` if it lies outside the BAR.
    pub fn doorbell(&self, qid: u16, completion: bool) -> Result<usize, NvmeError> {
        let at = (2 * qid as usize + completion as usize) * (4usize << self.cap.stride);
        let offset = reg::DOORBELLS + at;
        if offset + 4 > self.bar.len() {
            return Err(NvmeError::Unsupported(format!(
                "queue {qid}'s doorbell at {offset:#x} lies outside BAR 0 ({:#x} bytes)",
                self.bar.len()
            )));
        }
        Ok(offset)
    }

    /// Writes `value` to the doorbell at `offset` (from [`Controller::doorbell`]).
    pub fn ring(&self, offset: usize, value: u16) -> Result<(), NvmeError> {
        Ok(self.bar.write32(offset, value as u32)?)
    }

    /// The window on BAR 0.
    pub fn bar(&self) -> &R {
        &self.bar
    }
}
