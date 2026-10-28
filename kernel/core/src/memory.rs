//! Memory objects and device memory regions (SDD 20, f.5).
//!
//! A memory object is a set of zeroed pages allocated when it is created. Mapping it
//! into a process maps those same pages, so two processes that map one object share
//! its bytes with no copy: the basis of the data plane. A device memory region is a
//! range of physical MMIO pages OSTD hands out only if no RAM and nothing OSTD itself
//! uses lies in it.

use alloc::vec::Vec;

use moruna_abi::{Error, PAGE_SIZE, limits};
use ostd::{
    io::IoMem,
    mm::{FrameAllocOptions, UFrame, VmIo},
};

const PAGE: usize = PAGE_SIZE as usize;

pub struct MemoryObject {
    frames: Vec<UFrame>,
    size: u64,
}

impl MemoryObject {
    /// Allocates `size` bytes, rounded up to whole pages, all zero.
    pub fn new(size: u64) -> Result<MemoryObject, Error> {
        if size == 0 {
            return Err(Error::InvalidArgs);
        }
        if size > limits::MEMORY_MAX_BYTES {
            return Err(Error::Limit);
        }
        let pages = size.div_ceil(PAGE_SIZE) as usize;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(pages)
            .map_err(|_| Error::NoMemory)?;
        let mut options = FrameAllocOptions::new();
        options.zeroed(true);
        for _ in 0..pages {
            let frame = options.alloc_frame().map_err(|_| Error::NoMemory)?;
            frames.push(frame.into());
        }
        Ok(MemoryObject {
            frames,
            size: pages as u64 * PAGE_SIZE,
        })
    }

    /// A memory object holding a copy of `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Result<MemoryObject, Error> {
        let object = MemoryObject::new(bytes.len().max(1) as u64)?;
        object.write(0, bytes)?;
        Ok(object)
    }

    /// The size in bytes, a whole number of pages.
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn frames(&self) -> &[UFrame] {
        &self.frames
    }

    /// Copies `buf.len()` bytes starting at `offset` out of the object.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::OutOfRange)?;
        if end > self.size {
            return Err(Error::OutOfRange);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset as usize + done;
            let (page, within) = (at / PAGE, at % PAGE);
            let n = (PAGE - within).min(buf.len() - done);
            self.frames[page]
                .read_bytes(within, &mut buf[done..done + n])
                .map_err(|_| Error::Fault)?;
            done += n;
        }
        Ok(())
    }

    /// Copies `buf` into the object at `offset`.
    pub fn write(&self, offset: u64, buf: &[u8]) -> Result<(), Error> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::OutOfRange)?;
        if end > self.size {
            return Err(Error::OutOfRange);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset as usize + done;
            let (page, within) = (at / PAGE, at % PAGE);
            let n = (PAGE - within).min(buf.len() - done);
            self.frames[page]
                .write_bytes(within, &buf[done..done + n])
                .map_err(|_| Error::Fault)?;
            done += n;
        }
        Ok(())
    }
}

/// The largest device memory region one call creates.
pub const DEVICE_MEMORY_MAX: u64 = 1 << 30;

pub struct DeviceMemory {
    pub io: IoMem,
    pub size: u64,
}

impl DeviceMemory {
    /// Claims the physical range `paddr .. paddr + size`, both page aligned.
    pub fn new(paddr: u64, size: u64) -> Result<DeviceMemory, Error> {
        if size == 0 || !paddr.is_multiple_of(PAGE_SIZE) || !size.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        if size > DEVICE_MEMORY_MAX {
            return Err(Error::Limit);
        }
        let end = paddr.checked_add(size).ok_or(Error::InvalidArgs)?;
        let start = usize::try_from(paddr).map_err(|_| Error::InvalidArgs)?;
        let end = usize::try_from(end).map_err(|_| Error::InvalidArgs)?;
        // OSTD refuses a range that holds RAM, overlaps what it uses itself, or is
        // already claimed: that is the kernel's guard on physical memory (KK-I7).
        let io = IoMem::acquire(start..end).map_err(|_| Error::AccessDenied)?;
        Ok(DeviceMemory { io, size })
    }
}
