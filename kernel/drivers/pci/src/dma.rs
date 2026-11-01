//! Memory a driver's device reads or writes (SDD 25, f.4): a DMA memory object the
//! driver owns, mapped in it and pinned through its function's DMA resource, with each
//! page's device address. Rings, headers, command lists and bounce buffers are these.

use alloc::{format, string::String, vec, vec::Vec};

use moruna_abi::Error;
use moruna_mmio::Region;
use moruna_svc::{Handle, Mapping, Memory, Pin, map};

/// Memory a device reads or writes: a DMA memory object, mapped in the driver, pinned
/// for the device, with each page's device address.
pub struct DmaBuffer {
    pub memory: Memory,
    _pin: Pin,
    pub map: Mapping<'static>,
    pages: Vec<u64>,
}

impl DmaBuffer {
    /// `bytes` (whole pages) of zeroed DMA memory pinned through `dma`.
    pub fn new(dma: &Handle, bytes: u64) -> Result<DmaBuffer, String> {
        let memory = Memory::create_dma(bytes).map_err(|e| format!("DMA memory: {}", e.name()))?;
        DmaBuffer::pin(dma, memory, bytes)
    }

    /// `bytes` of zeroed DMA memory whose pages a device sees as one run of addresses
    /// (`memory_flags::CONTIGUOUS`, ABI 2.1): for a ring the device reads as one block.
    /// The run is checked, not assumed.
    pub fn contiguous(dma: &Handle, bytes: u64) -> Result<DmaBuffer, String> {
        let memory = Memory::create_dma_contiguous(bytes)
            .map_err(|e| format!("contiguous DMA memory of {bytes} bytes: {}", e.name()))?;
        let buffer = DmaBuffer::pin(dma, memory, bytes)?;
        let first = buffer.pages.first().copied().unwrap_or(0);
        if let Some(i) =
            (0..buffer.pages.len()).find(|i| buffer.pages[*i] != first + *i as u64 * 4096)
        {
            return Err(format!(
                "a contiguous object's page {i} is at {:#x}, not {:#x}",
                buffer.pages[i],
                first + i as u64 * 4096
            ));
        }
        Ok(buffer)
    }

    fn pin(dma: &Handle, memory: Memory, bytes: u64) -> Result<DmaBuffer, String> {
        let mut pages = vec![0u64; bytes.div_ceil(4096) as usize];
        let (pin, n) = Pin::create(dma, &memory.0, true, &mut pages)
            .map_err(|e| format!("pinning DMA memory: {}", e.name()))?;
        pages.truncate(n);
        let map = memory
            .map_detached(0, n as u64 * 4096, map::WRITE)
            .map_err(|e| format!("mapping DMA memory: {}", e.name()))?;
        Ok(DmaBuffer {
            memory,
            _pin: pin,
            map,
            pages,
        })
    }

    /// The device address of byte `offset`; a part handed to the device starting there
    /// must not cross the page's end.
    pub fn addr(&self, offset: usize) -> u64 {
        self.pages[offset / 4096] + (offset % 4096) as u64
    }

    pub fn len(&self) -> usize {
        self.pages.len() * 4096
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Each page's device address, in order.
    pub fn pages(&self) -> &[u64] {
        &self.pages
    }
}

/// Driver memory as a [`Region`]: its rings and buffers, read and written at their
/// own width; a run of bytes is copied whole, as plain memory may be.
impl Region for DmaBuffer {
    fn len(&self) -> usize {
        self.pages.len() * 4096
    }
    fn read8(&self, o: usize) -> Result<u8, Error> {
        self.map.read8(o)
    }
    fn read16(&self, o: usize) -> Result<u16, Error> {
        self.map.read16(o)
    }
    fn read32(&self, o: usize) -> Result<u32, Error> {
        self.map.read32(o)
    }
    fn read64(&self, o: usize) -> Result<u64, Error> {
        self.map.read64(o)
    }
    fn write8(&self, o: usize, v: u8) -> Result<(), Error> {
        self.map.write8(o, v)
    }
    fn write16(&self, o: usize, v: u16) -> Result<(), Error> {
        self.map.write16(o, v)
    }
    fn write32(&self, o: usize, v: u32) -> Result<(), Error> {
        self.map.write32(o, v)
    }
    fn write64(&self, o: usize, v: u64) -> Result<(), Error> {
        self.map.write64(o, v)
    }
    fn write_bytes(&self, o: usize, bytes: &[u8]) -> Result<(), Error> {
        self.map.write(o, bytes)
    }
    fn read_bytes(&self, o: usize, out: &mut [u8]) -> Result<(), Error> {
        self.map.read(o, out)
    }
}
