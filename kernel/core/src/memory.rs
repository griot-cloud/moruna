//! Memory objects, device memory regions and pins (SDD 20, f.5; SDD 25, f.3 and f.4).
//!
//! A memory object is a set of zeroed pages allocated when it is created. Mapping it
//! into a process maps those same pages, so two processes that map one object share
//! its bytes with no copy: the basis of the data plane. A device memory region is a
//! range of physical MMIO pages OSTD hands out only if no RAM and nothing OSTD itself
//! uses lies in it. A pin hands a device the pages of a memory object made for DMA.

use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

use moruna_abi::{Error, PAGE_SIZE, limits};
use ostd::{
    io::IoMem,
    mm::{
        FrameAllocOptions, HasDaddr, UFrame, USegment, VmIo,
        dma::{DmaStream, FromAndToDevice},
    },
    sync::SpinLock,
};

use crate::object::{QUARANTINE_MAX, Resource};

const PAGE: usize = PAGE_SIZE as usize;

pub struct MemoryObject {
    frames: Vec<UFrame>,
    size: u64,
    /// Made with `memory_flags::DMA`: only such an object may be pinned for a device.
    dma: bool,
}

impl MemoryObject {
    /// Allocates `size` bytes, rounded up to whole pages, all zero.
    pub fn new(size: u64) -> Result<MemoryObject, Error> {
        MemoryObject::with_dma(size, false)
    }

    /// Allocates `size` bytes as [`MemoryObject::new`] does; `dma` marks it as memory a
    /// holder of a DMA resource may hand to a device.
    pub fn with_dma(size: u64, dma: bool) -> Result<MemoryObject, Error> {
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
            dma,
        })
    }

    /// Whether the object was made for DMA.
    pub fn is_dma(&self) -> bool {
        self.dma
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
    range: Range<u64>,
}

fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Every range the kernel has acquired from OSTD, and the ranges live device memory
/// regions hold now. OSTD hands a range out once and never takes it back (its `IoMem`
/// is never recycled), so the kernel keeps what it acquired and hands out slices of it
/// again once no live region holds them: a driver that ends leaves its windows
/// claimable by the next one (SDD 25, f.3).
struct Claims {
    acquired: Vec<(Range<u64>, IoMem)>,
    live: Vec<Range<u64>>,
}

static CLAIMS: SpinLock<Claims> = SpinLock::new(Claims {
    acquired: Vec::new(),
    live: Vec::new(),
});

impl DeviceMemory {
    /// Claims the physical range `paddr .. paddr + size`, both page aligned. A range a
    /// live region holds, or one that only partly overlaps a range acquired before, is
    /// refused (`AccessDenied`).
    pub fn new(paddr: u64, size: u64) -> Result<DeviceMemory, Error> {
        if size == 0 || !paddr.is_multiple_of(PAGE_SIZE) || !size.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        if size > DEVICE_MEMORY_MAX {
            return Err(Error::Limit);
        }
        let end = paddr.checked_add(size).ok_or(Error::InvalidArgs)?;
        let range = paddr..end;
        let start = usize::try_from(paddr).map_err(|_| Error::InvalidArgs)?;
        let end = usize::try_from(end).map_err(|_| Error::InvalidArgs)?;
        let mut claims = CLAIMS.lock();
        if claims.live.iter().any(|l| overlaps(l, &range)) {
            return Err(Error::AccessDenied);
        }
        claims.live.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let io = match claims.acquired.iter().find(|(a, _)| overlaps(a, &range)) {
            Some((a, io)) if a.start <= range.start && range.end <= a.end => {
                let from = (range.start - a.start) as usize;
                io.slice(from..from + size as usize)
            }
            Some(_) => return Err(Error::AccessDenied),
            None => {
                claims
                    .acquired
                    .try_reserve(1)
                    .map_err(|_| Error::NoMemory)?;
                // OSTD refuses a range that holds RAM or overlaps what it uses itself:
                // that is the kernel's guard on physical memory (KK-I7).
                let io = IoMem::acquire(start..end).map_err(|_| Error::AccessDenied)?;
                claims.acquired.push((range.clone(), io.clone()));
                io
            }
        };
        claims.live.push(range.clone());
        Ok(DeviceMemory { io, size, range })
    }
}

impl Drop for DeviceMemory {
    fn drop(&mut self) {
        let mut claims = CLAIMS.lock();
        if let Some(i) = claims.live.iter().position(|l| *l == self.range) {
            claims.live.swap_remove(i);
        }
    }
}

/// The pages of a memory object as a device sees them: one DMA mapping per page
/// (through the IOMMU when OSTD has one; the physical address otherwise). Dropping it
/// ends the mappings and lets the pages go.
pub struct Pinned {
    _memory: Arc<MemoryObject>,
    _streams: Vec<DmaStream<FromAndToDevice>>,
}

/// A pin: the pages of a memory object handed to the devices a DMA resource covers.
/// When its last handle closes, the pages go into that resource's quarantine rather
/// than back to the allocator, because the device may still be writing them; they
/// leave it when a holder of the resource calls `dma_release`, after it has stopped
/// the device (SDD 25, f.4).
pub struct Pin {
    domain: Arc<Resource>,
    pinned: Option<Pinned>,
}

impl Pin {
    /// Pins `memory` for `domain`'s devices; returns the pin and the device address of
    /// each page, in order.
    pub fn new(domain: Arc<Resource>, memory: Arc<MemoryObject>) -> Result<(Pin, Vec<u64>), Error> {
        if !memory.is_dma() {
            return Err(Error::InvalidArgs);
        }
        if domain.quarantine.lock().len() >= QUARANTINE_MAX {
            return Err(Error::Limit);
        }
        let pages = memory.frames().len();
        let mut streams = Vec::new();
        streams
            .try_reserve_exact(pages)
            .map_err(|_| Error::NoMemory)?;
        let mut addresses = Vec::new();
        addresses
            .try_reserve_exact(pages)
            .map_err(|_| Error::NoMemory)?;
        for frame in memory.frames() {
            // x86_64 keeps DMA coherent with the caches.
            let stream =
                DmaStream::map(USegment::from(frame.clone()), true).map_err(|_| Error::NoMemory)?;
            addresses.push(stream.daddr() as u64);
            streams.push(stream);
        }
        let pin = Pin {
            domain,
            pinned: Some(Pinned {
                _memory: memory,
                _streams: streams,
            }),
        };
        Ok((pin, addresses))
    }

    /// Ends the pin at once, with no quarantine: for a pin no device has been told of.
    pub fn abandon(mut self) {
        self.pinned = None;
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        if let Some(pinned) = self.pinned.take() {
            let mut quarantine = self.domain.quarantine.lock();
            if quarantine.try_reserve(1).is_ok() {
                quarantine.push(pinned);
            } else {
                // No memory to remember the pin: keeping the pages mapped for ever is
                // the only choice that cannot hand a device's write to a new owner.
                ostd::early_println!(
                    "kk: a DMA quarantine could not grow; a pin's pages stay allocated"
                );
                core::mem::forget(pinned);
            }
        }
    }
}

/// Empties a DMA resource's quarantine; returns how many pins it held.
pub fn release_quarantine(domain: &Resource) -> usize {
    let released = core::mem::take(&mut *domain.quarantine.lock());
    let n = released.len();
    // Dropped outside the lock: ending a DMA mapping takes OSTD's own locks.
    drop(released);
    n
}
