//! Split virtqueues (virtio 1.2, 2.7): a descriptor table, a driver ring the driver
//! offers buffers on, and a device ring the device returns them on, each in its own
//! page of one DMA memory object.
//!
//! A buffer is a chain of descriptors, each a device address and a length the device
//! reads or writes. The driver keeps the free descriptors and the chains in flight in
//! its own memory; the device sees only the rings, and what it writes there is checked
//! before it is believed: a returned id that names no chain in flight is refused.

use alloc::{vec, vec::Vec};
use core::sync::atomic::{Ordering, fence};

use moruna_abi::Error;
use moruna_mmio::Region;

/// Where each part of the queue is in its memory, and how large the memory is.
pub const DESC: usize = 0;
pub const DRIVER: usize = 4096;
pub const DEVICE: usize = 8192;
pub const BYTES: usize = 12288;
/// The largest queue: its descriptor table fills one page.
pub const MAX_SIZE: u16 = 256;

/// Descriptor flags.
pub mod flags {
    pub const NEXT: u16 = 1;
    pub const WRITE: u16 = 2;
}

/// One part of a chain: `len` bytes at device address `addr`, which the device
/// writes if `device_writes` and reads otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffer {
    pub addr: u64,
    pub len: u32,
    pub device_writes: bool,
}

/// A chain the device has finished with: its head, and the bytes it wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    pub head: u16,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// Fewer free descriptors than the chain needs: wait for the device to return some.
    Full,
    /// An empty chain, a chain longer than the queue, or a zero-length part.
    Invalid,
    /// The device returned an id that names no chain in flight.
    Malformed,
    Kernel(Error),
}

impl From<Error> for QueueError {
    fn from(e: Error) -> QueueError {
        QueueError::Kernel(e)
    }
}

pub struct Queue<M: Region> {
    memory: M,
    size: u16,
    /// Free descriptors, as a stack.
    free: Vec<u16>,
    /// For each descriptor, the next in its chain (the driver's own copy).
    next: Vec<u16>,
    /// For each head in flight, its chain's length; zero for none.
    chains: Vec<u16>,
    driver_idx: u16,
    device_seen: u16,
}

impl<M: Region> Queue<M> {
    /// A queue of `size` entries (a power of two up to [`MAX_SIZE`]) in `memory`
    /// (at least [`BYTES`]), which it clears.
    pub fn new(memory: M, size: u16) -> Result<Queue<M>, QueueError> {
        if size == 0 || size > MAX_SIZE || !size.is_power_of_two() || memory.len() < BYTES {
            return Err(QueueError::Invalid);
        }
        for at in (0..BYTES).step_by(8) {
            memory.write64(at, 0)?;
        }
        Ok(Queue {
            memory,
            size,
            free: (0..size).rev().collect(),
            next: vec![0; size as usize],
            chains: vec![0; size as usize],
            driver_idx: 0,
            device_seen: 0,
        })
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    /// Free descriptors.
    pub fn free(&self) -> usize {
        self.free.len()
    }

    /// Chains in flight.
    pub fn in_flight(&self) -> usize {
        self.chains.iter().filter(|n| **n != 0).count()
    }

    /// Offers a chain to the device; returns its head. The device learns of it at the
    /// next notify.
    pub fn add(&mut self, chain: &[Buffer]) -> Result<u16, QueueError> {
        if chain.is_empty() || chain.len() > self.size as usize || chain.iter().any(|b| b.len == 0)
        {
            return Err(QueueError::Invalid);
        }
        if chain.len() > self.free.len() {
            return Err(QueueError::Full);
        }
        let ids: Vec<u16> = (0..chain.len()).filter_map(|_| self.free.pop()).collect();
        for (i, (id, buffer)) in ids.iter().zip(chain).enumerate() {
            let at = DESC + *id as usize * 16;
            let mut f = 0;
            let mut next = 0;
            if let Some(n) = ids.get(i + 1) {
                f |= flags::NEXT;
                next = *n;
            }
            if buffer.device_writes {
                f |= flags::WRITE;
            }
            self.memory.write64(at, buffer.addr)?;
            self.memory.write32(at + 8, buffer.len)?;
            self.memory.write16(at + 12, f)?;
            self.memory.write16(at + 14, next)?;
            self.next[*id as usize] = next;
        }
        let head = ids[0];
        self.chains[head as usize] = chain.len() as u16;
        let slot = (self.driver_idx % self.size) as usize;
        self.memory.write16(DRIVER + 4 + slot * 2, head)?;
        // The ring entry is visible before the index that publishes it.
        fence(Ordering::SeqCst);
        self.driver_idx = self.driver_idx.wrapping_add(1);
        self.memory.write16(DRIVER + 2, self.driver_idx)?;
        fence(Ordering::SeqCst);
        Ok(head)
    }

    /// The next chain the device has finished with, if any; its descriptors are free
    /// again.
    pub fn pop_used(&mut self) -> Result<Option<Used>, QueueError> {
        fence(Ordering::SeqCst);
        let idx = self.memory.read16(DEVICE + 2)?;
        if idx == self.device_seen {
            return Ok(None);
        }
        let slot = (self.device_seen % self.size) as usize;
        let at = DEVICE + 4 + slot * 8;
        let id = self.memory.read32(at)?;
        let len = self.memory.read32(at + 4)?;
        let head = u16::try_from(id).map_err(|_| QueueError::Malformed)?;
        let count = *self
            .chains
            .get(head as usize)
            .ok_or(QueueError::Malformed)?;
        if count == 0 {
            return Err(QueueError::Malformed);
        }
        self.device_seen = self.device_seen.wrapping_add(1);
        self.chains[head as usize] = 0;
        let mut id = head;
        for _ in 0..count {
            self.free.push(id);
            id = self.next[id as usize];
        }
        Ok(Some(Used { head, len }))
    }
}
