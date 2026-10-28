//! PCI configuration space (SDD 25, f.6): what the device manager enumerates and a
//! driver reads, over a [`Region`] that maps one function's 4 KiB of ECAM.
//!
//! The kernel knows no PCI: the device manager receives the ECAM window as a device
//! memory resource, maps it, and walks it with this code; each driver receives a
//! window over its own function only and reads its capabilities and programs its
//! MSI-X table with this code.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use moruna_abi::Error;
use moruna_mmio::Region;

/// The configuration space of one function under ECAM.
pub const CONFIG_SIZE: usize = 4096;
/// Functions per device, devices per bus, and the ECAM bytes of one bus.
pub const FUNCTIONS: u8 = 8;
pub const DEVICES: u8 = 32;
pub const BUS_BYTES: u64 = 1 << 20;

/// Offsets in the type 0 header.
pub mod reg {
    pub const VENDOR: usize = 0x00;
    pub const DEVICE: usize = 0x02;
    pub const COMMAND: usize = 0x04;
    pub const STATUS: usize = 0x06;
    pub const REVISION: usize = 0x08;
    pub const CLASS: usize = 0x08;
    pub const HEADER_TYPE: usize = 0x0e;
    pub const BAR0: usize = 0x10;
    pub const CAPABILITIES: usize = 0x34;
}

/// Bits of the command register.
pub mod command {
    pub const IO: u16 = 1 << 0;
    pub const MEMORY: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
    pub const INTX_DISABLE: u16 = 1 << 10;
}

/// The status register's bit that says the capability list exists.
pub const STATUS_CAPABILITIES: u16 = 1 << 4;
/// The capability ids this crate reads.
pub mod cap {
    pub const MSI: u8 = 0x05;
    pub const VENDOR: u8 = 0x09;
    pub const MSIX: u8 = 0x11;
}

/// A function's address: bus, device and function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Address {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Address {
    /// The function's offset in an ECAM window whose first bus is bus 0.
    pub const fn ecam_offset(&self) -> u64 {
        ((self.bus as u64) << 20) | ((self.device as u64) << 15) | ((self.function as u64) << 12)
    }

    /// The PCI requester id a device's DMA carries: `bus << 8 | device << 3 | function`.
    pub const fn requester_id(&self) -> u16 {
        ((self.bus as u16) << 8) | ((self.device as u16) << 3) | self.function as u16
    }

    /// The function at `offset` in an ECAM window, if the offset is a function's start.
    pub fn from_ecam_offset(offset: u64) -> Option<Address> {
        if !offset.is_multiple_of(CONFIG_SIZE as u64) || offset >> 28 != 0 {
            return None;
        }
        Some(Address {
            bus: (offset >> 20) as u8,
            device: ((offset >> 15) & 0x1f) as u8,
            function: ((offset >> 12) & 0x7) as u8,
        })
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

/// A base address register's decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// Memory space: a physical address range.
    Memory {
        base: u64,
        size: u64,
        prefetchable: bool,
        wide: bool,
    },
    /// I/O space: ports. A process on Moruna has no port I/O, so no driver gets one.
    Io { base: u32, size: u32 },
}

impl Bar {
    /// The memory range rounded out to whole pages, for a device memory resource.
    pub fn pages(&self) -> Option<(u64, u64)> {
        match *self {
            Bar::Memory { base, size, .. } if size != 0 => {
                let start = base & !0xfff;
                let end = base.checked_add(size)?.checked_add(0xfff)? & !0xfff;
                Some((start, end - start))
            }
            _ => None,
        }
    }
}

/// One function's configuration space.
pub struct Config<R: Region> {
    region: R,
}

impl<R: Region> Config<R> {
    /// A function's configuration space; `OutOfRange` for a window smaller than one.
    pub fn new(region: R) -> Result<Config<R>, Error> {
        if region.len() < CONFIG_SIZE {
            return Err(Error::OutOfRange);
        }
        Ok(Config { region })
    }

    /// The window underneath.
    pub fn region(&self) -> &R {
        &self.region
    }

    pub fn vendor(&self) -> Result<u16, Error> {
        self.region.read16(reg::VENDOR)
    }

    pub fn device(&self) -> Result<u16, Error> {
        self.region.read16(reg::DEVICE)
    }

    /// Whether a function answers here: an absent one reads all ones.
    pub fn present(&self) -> Result<bool, Error> {
        Ok(self.vendor()? != 0xffff)
    }

    /// The class, subclass and programming interface, in that order.
    pub fn class(&self) -> Result<(u8, u8, u8), Error> {
        let word = self.region.read32(reg::CLASS)?;
        Ok(((word >> 24) as u8, (word >> 16) as u8, (word >> 8) as u8))
    }

    /// The header type, without the multi-function bit.
    pub fn header_type(&self) -> Result<u8, Error> {
        Ok(self.region.read8(reg::HEADER_TYPE)? & 0x7f)
    }

    /// Whether function 0 of a device says it has others.
    pub fn multifunction(&self) -> Result<bool, Error> {
        Ok(self.region.read8(reg::HEADER_TYPE)? & 0x80 != 0)
    }

    pub fn command(&self) -> Result<u16, Error> {
        self.region.read16(reg::COMMAND)
    }

    pub fn set_command(&self, value: u16) -> Result<(), Error> {
        self.region.write16(reg::COMMAND, value)
    }

    /// Sets `bits` in the command register, keeping the others.
    pub fn enable(&self, bits: u16) -> Result<(), Error> {
        self.set_command(self.command()? | bits)
    }

    /// Clears `bits` in the command register, keeping the others.
    pub fn disable(&self, bits: u16) -> Result<(), Error> {
        self.set_command(self.command()? & !bits)
    }

    /// The capabilities, `(id, offset)`, in list order. A list that loops, points into
    /// the header or runs past 48 entries is `InvalidArgs`.
    pub fn capabilities(&self) -> Result<Vec<(u8, usize)>, Error> {
        let mut caps = Vec::new();
        if self.region.read16(reg::STATUS)? & STATUS_CAPABILITIES == 0 {
            return Ok(caps);
        }
        let mut next = (self.region.read8(reg::CAPABILITIES)? & 0xfc) as usize;
        while next != 0 {
            if next < 0x40 || caps.len() == 48 || caps.iter().any(|(_, at)| *at == next) {
                return Err(Error::InvalidArgs);
            }
            let id = self.region.read8(next)?;
            caps.push((id, next));
            next = (self.region.read8(next + 1)? & 0xfc) as usize;
        }
        Ok(caps)
    }

    /// The offset of the first capability with `id`.
    pub fn find(&self, id: u8) -> Result<Option<usize>, Error> {
        Ok(self
            .capabilities()?
            .into_iter()
            .find(|(i, _)| *i == id)
            .map(|(_, at)| at))
    }

    /// A BAR's decoding from its register, without sizing it: the size is zero.
    pub fn bar_base(&self, index: usize) -> Result<Bar, Error> {
        Ok(self.bars_raw(index)?.0)
    }

    /// BAR `index` decoded, and how many registers it spans (2 for a 64-bit BAR).
    fn bars_raw(&self, index: usize) -> Result<(Bar, usize), Error> {
        if index >= 6 {
            return Err(Error::InvalidArgs);
        }
        let at = reg::BAR0 + index * 4;
        let low = self.region.read32(at)?;
        if low & 1 == 1 {
            return Ok((
                Bar::Io {
                    base: low & !0x3,
                    size: 0,
                },
                1,
            ));
        }
        let wide = (low >> 1) & 0x3 == 0x2;
        let prefetchable = low & 0x8 != 0;
        let mut base = (low & !0xf) as u64;
        if wide {
            if index == 5 {
                return Err(Error::InvalidArgs);
            }
            base |= (self.region.read32(at + 4)? as u64) << 32;
        }
        Ok((
            Bar::Memory {
                base,
                size: 0,
                prefetchable,
                wide,
            },
            if wide { 2 } else { 1 },
        ))
    }

    /// Every BAR with its size, found by the standard probe: decoding is turned off,
    /// each register is written all ones and read back, then restored, and decoding is
    /// turned back on as it was. A BAR whose probe reads zero is absent (`None`); the
    /// upper half of a 64-bit BAR is `None` too.
    pub fn size_bars(&self) -> Result<[Option<Bar>; 6], Error> {
        let mut bars = [None; 6];
        let saved = self.command()?;
        self.set_command(saved & !(command::IO | command::MEMORY))?;
        let result = self.probe_bars(&mut bars);
        self.set_command(saved)?;
        result.map(|()| bars)
    }

    fn probe_bars(&self, bars: &mut [Option<Bar>; 6]) -> Result<(), Error> {
        let mut index = 0;
        while index < 6 {
            let (bar, span) = self.bars_raw(index)?;
            let at = reg::BAR0 + index * 4;
            let low = self.region.read32(at)?;
            self.region.write32(at, 0xffff_ffff)?;
            let mask_low = self.region.read32(at)?;
            self.region.write32(at, low)?;
            bars[index] = match bar {
                Bar::Io { base, .. } => {
                    let mut mask = mask_low & !0x3;
                    // A device that decodes 16-bit ports hardwires the upper half to 0.
                    if mask != 0 && mask & 0xffff_0000 == 0 {
                        mask |= 0xffff_0000;
                    }
                    (mask != 0).then_some(Bar::Io {
                        base,
                        size: (!mask).wrapping_add(1),
                    })
                }
                Bar::Memory {
                    base,
                    prefetchable,
                    wide,
                    ..
                } => {
                    let mut mask = (mask_low & !0xf) as u64;
                    if wide {
                        let high = self.region.read32(at + 4)?;
                        self.region.write32(at + 4, 0xffff_ffff)?;
                        mask |= (self.region.read32(at + 4)? as u64) << 32;
                        self.region.write32(at + 4, high)?;
                    } else {
                        mask |= 0xffff_ffff_0000_0000;
                    }
                    (mask & !0xffff_ffff_0000_0000 != 0 || (wide && mask != 0)).then_some(
                        Bar::Memory {
                            base,
                            size: (!mask).wrapping_add(1),
                            prefetchable,
                            wide,
                        },
                    )
                }
            };
            index += span;
        }
        Ok(())
    }

    /// The function's MSI-X capability, if it has one.
    pub fn msix(&self) -> Result<Option<Msix>, Error> {
        let Some(at) = self.find(cap::MSIX)? else {
            return Ok(None);
        };
        let control = self.region.read16(at + 2)?;
        let table = self.region.read32(at + 4)?;
        let pba = self.region.read32(at + 8)?;
        Ok(Some(Msix {
            offset: at,
            entries: (control & 0x7ff) + 1,
            table_bar: (table & 0x7) as usize,
            table_offset: (table & !0x7) as u64,
            pba_bar: (pba & 0x7) as usize,
            pba_offset: (pba & !0x7) as u64,
        }))
    }
}

/// An MSI-X capability: where its table is and how many entries it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msix {
    /// The capability's offset in configuration space.
    pub offset: usize,
    pub entries: u16,
    pub table_bar: usize,
    pub table_offset: u64,
    pub pba_bar: usize,
    pub pba_offset: u64,
}

/// The MSI-X control register's enable and function mask bits.
pub const MSIX_ENABLE: u16 = 1 << 15;
pub const MSIX_FUNCTION_MASK: u16 = 1 << 14;
/// The size of one MSI-X table entry, and its vector control's mask bit.
pub const MSIX_ENTRY: usize = 16;
pub const MSIX_MASKED: u32 = 1;

impl Msix {
    /// Writes table entry `entry` (in `table`, a window over the table's BAR) with the
    /// message `address` and `data`, unmasked.
    pub fn program<T: Region + ?Sized>(
        &self,
        table: &T,
        entry: u16,
        address: u64,
        data: u32,
    ) -> Result<(), Error> {
        if entry >= self.entries {
            return Err(Error::InvalidArgs);
        }
        let at = usize::try_from(self.table_offset).map_err(|_| Error::OutOfRange)?
            + entry as usize * MSIX_ENTRY;
        table.write32(at + 12, MSIX_MASKED)?;
        table.write32(at, address as u32)?;
        table.write32(at + 4, (address >> 32) as u32)?;
        table.write32(at + 8, data)?;
        table.write32(at + 12, 0)
    }

    /// Turns MSI-X on for the function, with the function mask clear, so programmed and
    /// unmasked entries deliver.
    pub fn enable<R: Region>(&self, config: &Config<R>) -> Result<(), Error> {
        let at = self.offset + 2;
        let control = config.region.read16(at)?;
        config
            .region
            .write16(at, (control | MSIX_ENABLE) & !MSIX_FUNCTION_MASK)
    }
}

#[cfg(target_os = "none")]
pub mod function;

#[cfg(test)]
mod tests;
