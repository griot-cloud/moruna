//! A [`Channel`] in memory, for the host tests of drivers.
//!
//! [`TestChannel`] is an open channel with no host behind it. Its rings are ordinary
//! memory, and a test's fake device, a [`Device`], stands where the host's device would:
//! it is called when the driver signals the host, when the driver is about to wait, and
//! when the driver shares memory. Through [`Host`] it reads what the driver sent, writes
//! its answers, signals the driver, reaches the memory the driver shared, and can take
//! the channel away as a rescind would.
//!
//! There is one thread and no clock. A wait with a deadline that finds no signal has
//! timed out; a wait with no deadline that finds none would never end, and panics to
//! say so.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering;

use guestmem_core::MemoryRead;
use guestmem_core::MemoryWrite;
use vmbus_core::protocol::GpadlId;
use vmbus_ring::FlatRingMem;
use vmbus_ring::IncomingPacket;
use vmbus_ring::IncomingPacketType;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingPacket;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::OutgoingRing;
use vmbus_ring::ReadError;
use vmbus_ring::WriteError;

use crate::channel::Channel;
use crate::channel::ChannelError;
use crate::channel::Wait;
use crate::gpadl::MAX_PAGES;
use crate::gpadl::PAGE_SIZE;
use crate::hv::MemoryId;

/// Memory a test's driver shares with its fake device: whole pages, zero at first. A
/// clone is the same memory.
#[derive(Clone)]
pub struct Memory(Arc<[AtomicU8]>);

impl core::fmt::Debug for Memory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Memory({} bytes)", self.0.len())
    }
}

impl Memory {
    /// `pages` pages of zeroes.
    pub fn new(pages: usize) -> Memory {
        Memory((0..pages * PAGE_SIZE).map(|_| AtomicU8::new(0)).collect())
    }

    /// Its length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it has no pages.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Copies out the bytes from `offset`. Panics past the end, as a slice does.
    pub fn read(&self, offset: usize, data: &mut [u8]) {
        let cells = &self.0[offset..offset + data.len()];
        for (byte, cell) in data.iter_mut().zip(cells) {
            *byte = cell.load(Ordering::Relaxed);
        }
    }

    /// Copies `data` in at `offset`. Panics past the end, as a slice does.
    pub fn write(&self, offset: usize, data: &[u8]) {
        for (byte, cell) in data.iter().zip(&self.0[offset..offset + data.len()]) {
            cell.store(*byte, Ordering::Relaxed);
        }
    }
}

/// One packet the driver sent, read whole.
#[derive(Debug)]
pub struct Received {
    /// What the ring says of it: its kind and its transaction ID.
    pub packet: IncomingPacket,
    /// Its payload.
    pub payload: Vec<u8>,
    /// For a packet that names memory outside the ring (GPA-direct, transfer pages): the
    /// bytes that describe those ranges. Empty otherwise.
    pub ranges: Vec<u8>,
}

/// The fake device on the other end of a [`TestChannel`].
pub trait Device {
    /// The driver signalled the host. The device reads what the driver sent and answers.
    fn signalled(&mut self, host: &mut Host);

    /// The driver is about to wait and nothing has signalled it. A device with something
    /// to say unasked, a packet from the network, says it now. Nothing by default.
    fn idle(&mut self, _host: &mut Host) {}

    /// The driver shared `memory` as `gpadl`. False refuses it. Accepted by default.
    fn shared(&mut self, _gpadl: GpadlId, _memory: &Memory) -> bool {
        true
    }
}

/// The host's end of a [`TestChannel`], as its [`Device`] uses it.
pub struct Host {
    /// What the driver writes.
    incoming: IncomingRing<FlatRingMem>,
    /// What the driver reads.
    outgoing: OutgoingRing<FlatRingMem>,
    signalled: bool,
    closed: bool,
    gpadls: BTreeMap<GpadlId, Memory>,
}

impl Host {
    /// The ring the driver writes and the host reads.
    pub fn incoming(&self) -> &IncomingRing<FlatRingMem> {
        &self.incoming
    }

    /// The ring the host writes and the driver reads.
    pub fn outgoing(&self) -> &OutgoingRing<FlatRingMem> {
        &self.outgoing
    }

    /// Reads the next packet the driver sent, if there is one, and frees its room in the
    /// ring. Signals the driver if it asked to be told of that room.
    pub fn read(&mut self) -> Result<Option<Received>, vmbus_ring::Error> {
        let mut offset = self.incoming.incoming()?;
        let packet = match self.incoming.read(&mut offset) {
            Ok(packet) => packet,
            Err(ReadError::Empty) => return Ok(None),
            Err(ReadError::Corrupt(error)) => return Err(error),
        };
        let all = |range: &vmbus_ring::RingRange| {
            range
                .reader(&self.incoming)
                .read_all()
                .map_err(|_| vmbus_ring::Error::InvalidMessageLength)
        };
        let payload = all(&packet.payload)?;
        let ranges = match &packet.typ {
            IncomingPacketType::GpaDirect(_, range)
            | IncomingPacketType::TransferPages(_, _, range) => all(range)?,
            IncomingPacketType::InBand | IncomingPacketType::Completion => Vec::new(),
        };
        if self.incoming.commit_read(&mut offset) {
            self.signalled = true;
        }
        Ok(Some(Received {
            packet,
            payload,
            ranges,
        }))
    }

    /// Writes one packet for the driver. Signals the driver if its ring says it wants to
    /// be: it was empty, and the driver has not masked the signal.
    pub fn write(
        &mut self,
        typ: OutgoingPacketType<'_>,
        transaction_id: u64,
        payload: &[u8],
    ) -> Result<(), WriteError> {
        let mut offset = self.outgoing.outgoing()?;
        let packet = OutgoingPacket {
            transaction_id,
            size: payload.len(),
            typ,
        };
        let range = self.outgoing.write(&mut offset, &packet)?;
        range
            .writer(&self.outgoing)
            .write(payload)
            .map_err(|_| vmbus_ring::Error::InvalidMessageLength)?;
        if self.outgoing.commit_write(&mut offset) {
            self.signalled = true;
        }
        Ok(())
    }

    /// Signals the driver: its next wait returns at once.
    pub fn signal_driver(&mut self) {
        self.signalled = true;
    }

    /// The memory the driver shared as `gpadl`.
    pub fn memory(&self, gpadl: GpadlId) -> Option<&Memory> {
        self.gpadls.get(&gpadl)
    }

    /// Takes the channel away, as the host does when it rescinds the offer: everything
    /// the driver asks of the channel from now on fails with [`ChannelError::Closed`].
    pub fn rescind(&mut self) {
        self.closed = true;
    }
}

/// An open channel in memory, with a [`Device`] on its other end.
///
/// As with a channel on a node, the driver's incoming ring starts with its signal
/// masked: a driver that waits for signals unmasks it first
/// (`IncomingRing::set_interrupt_mask`).
pub struct TestChannel<D> {
    incoming: IncomingRing<FlatRingMem>,
    outgoing: OutgoingRing<FlatRingMem>,
    device: D,
    host: Host,
    memory: BTreeMap<MemoryId, Memory>,
    next_gpadl: u32,
    host_signals: usize,
}

impl<D: Device> TestChannel<D> {
    /// A channel whose outgoing and incoming rings hold this many bytes of packets,
    /// each a multiple of 4,096.
    pub fn new(
        outgoing_bytes: usize,
        incoming_bytes: usize,
        device: D,
    ) -> Result<TestChannel<D>, vmbus_ring::Error> {
        let to_host = FlatRingMem::new(outgoing_bytes);
        let to_driver = FlatRingMem::new(incoming_bytes);
        let host = Host {
            incoming: IncomingRing::new(to_host.clone())?,
            outgoing: OutgoingRing::new(to_driver.clone())?,
            signalled: false,
            closed: false,
            gpadls: BTreeMap::new(),
        };
        // A host that is idle wants to be told of the first packet.
        host.incoming.set_interrupt_mask(false);
        Ok(TestChannel {
            incoming: IncomingRing::new(to_driver)?,
            outgoing: OutgoingRing::new(to_host)?,
            device,
            host,
            memory: BTreeMap::new(),
            next_gpadl: 1,
            host_signals: 0,
        })
    }

    /// Makes `memory` known to the channel as `id`, so that the driver can share it.
    pub fn add_memory(&mut self, id: MemoryId, memory: Memory) {
        self.memory.insert(id, memory);
    }

    /// The fake device.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// The fake device, to change what it does next.
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// The host's end, for a test to act as the device between the driver's calls.
    pub fn host(&mut self) -> &mut Host {
        &mut self.host
    }

    /// How many times the driver has signalled the host.
    pub fn host_signals(&self) -> usize {
        self.host_signals
    }
}

impl<D: Device> Channel for TestChannel<D> {
    type Memory = FlatRingMem;

    fn incoming(&self) -> &IncomingRing<FlatRingMem> {
        &self.incoming
    }

    fn outgoing(&self) -> &OutgoingRing<FlatRingMem> {
        &self.outgoing
    }

    fn signal_host(&mut self) -> Result<(), ChannelError> {
        if self.host.closed {
            return Err(ChannelError::Closed);
        }
        self.host_signals += 1;
        self.device.signalled(&mut self.host);
        Ok(())
    }

    fn wait(&mut self, deadline: Option<u64>) -> Result<Wait, ChannelError> {
        if !self.host.closed && !self.host.signalled {
            self.device.idle(&mut self.host);
        }
        if self.host.closed {
            return Err(ChannelError::Closed);
        }
        if core::mem::take(&mut self.host.signalled) {
            return Ok(Wait::Signalled);
        }
        match deadline {
            Some(_) => Ok(Wait::TimedOut),
            None => panic!(
                "TestChannel: the driver waits with no deadline, and the device has nothing for it"
            ),
        }
    }

    fn share(&mut self, memory: MemoryId) -> Result<GpadlId, ChannelError> {
        if self.host.closed {
            return Err(ChannelError::Closed);
        }
        let shared = self.memory.get(&memory).ok_or(ChannelError::NotShared)?;
        if shared.is_empty() || shared.len() > MAX_PAGES * PAGE_SIZE {
            return Err(ChannelError::NotShared);
        }
        let gpadl = GpadlId(self.next_gpadl);
        if !self.device.shared(gpadl, shared) {
            return Err(ChannelError::NotShared);
        }
        self.next_gpadl += 1;
        self.host.gpadls.insert(gpadl, shared.clone());
        Ok(gpadl)
    }
}
