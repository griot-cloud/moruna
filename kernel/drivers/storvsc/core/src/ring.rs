//! The channel's two rings as storvsc uses them: one storvsp packet out, one packet in, and
//! the wait between them.
//!
//! Every packet either side sends is a `storvsp_protocol::Packet` header followed by the
//! largest SCSI request the protocol defines, zero padded: "storvsp limits the size of the
//! completion packet to the size of the request packet, so we need to pad the payload to the
//! maximum size to ensure we get a complete response" (`storvsc:606-617`).

use guestmem_core::{MemoryRead, MemoryWrite, ranges::PagedRange};
use storvsp_protocol::{NtStatus, Operation, Packet, SCSI_REQUEST_LEN_MAX};
use vmbus_ring::{
    IncomingOffset, IncomingPacketType, OutgoingOffset, OutgoingPacket, OutgoingPacketType,
    PacketSize, ReadError, Ring, RingMem, WriteError, gparange::GpaRange,
};
use zerocopy::{FromBytes, IntoBytes};

use crate::{Channel, Error, Malformed, Wait};

/// Bytes of the header every storvsp packet starts with.
pub(crate) const HEADER: usize = size_of::<Packet>();
/// Bytes of every packet's payload: the header and the largest SCSI request.
pub(crate) const PACKET: usize = HEADER + SCSI_REQUEST_LEN_MAX;

/// What kind of packet the host sent.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    /// The completion of the transaction with this ID.
    Completion(u64),
    /// A packet of the host's own.
    InBand,
    /// A packet that names memory outside the ring, which no storvsp sends.
    Other,
}

/// One packet from the host: its kind, the payload's length as the ring gives it, and the
/// payload's first bytes, copied out once so that nothing the host changes later is seen.
pub(crate) struct Received {
    pub(crate) kind: Kind,
    len: usize,
    bytes: [u8; PACKET],
}

impl Received {
    /// The packet's operation: its header's first field.
    pub(crate) fn operation(&self) -> Result<Operation, Malformed> {
        if self.len < HEADER {
            return Err(Malformed("a packet shorter than its header"));
        }
        Ok(self.header().operation)
    }

    /// The header and the SCSI request of a completion: exactly the size of the request it
    /// answers (`storvsp:578-590`), and marked as a completion (`storvsc:711-713`).
    pub(crate) fn completion(&self) -> Result<(Packet, &[u8]), Malformed> {
        if self.len != PACKET {
            return Err(Malformed("a completion of the wrong size"));
        }
        let header = self.header();
        if header.operation != Operation::COMPLETE_IO {
            return Err(Malformed("a completion whose operation is not COMPLETE_IO"));
        }
        Ok((header, &self.bytes[HEADER..]))
    }

    fn header(&self) -> Packet {
        let mut header = Packet {
            operation: Operation(0),
            flags: 0,
            status: NtStatus(0),
        };
        header.as_mut_bytes().copy_from_slice(&self.bytes[..HEADER]);
        header
    }
}

/// Where this side stands in each ring, and the room a refused packet is waiting for.
pub(crate) struct Rings {
    read: IncomingOffset,
    write: OutgoingOffset,
    /// Bytes of the outgoing ring a packet needed and did not find; 0 when none is waiting.
    wanted: usize,
}

impl Rings {
    pub(crate) fn new<C: Channel>(channel: &C) -> Result<Rings, vmbus_ring::Error> {
        Ok(Rings {
            read: channel.incoming().incoming()?,
            write: channel.outgoing().outgoing()?,
            wanted: 0,
        })
    }

    /// The most pages one request can name: its packet must fit the ring to the host, and a
    /// packet's descriptor counts its length in 8-byte units in 16 bits.
    pub(crate) fn max_pages<C: Channel>(channel: &C) -> usize {
        // Descriptor, payload and footer; then the GPA direct header and one range's header.
        let fixed = PacketSize::in_band(PACKET) + size_of::<u64>() + size_of::<GpaRange>();
        // `OutgoingRing::write` takes a packet strictly smaller than this.
        let room = channel.outgoing().maximum_packet_size();
        let by_ring = room.saturating_sub(fixed + 1) / size_of::<u64>();
        let footer = size_of::<u64>();
        let by_descriptor = (u16::MAX as usize * 8 - (fixed - footer)) / size_of::<u64>();
        by_ring.min(by_descriptor)
    }

    /// The most requests in flight: as many as the ring to the host holds of the largest
    /// request packet, which names `pages` pages, at least 1 and at most 256 (the PM's rule,
    /// after Linux's storvsc). With no pages the largest request is the in-band one.
    pub(crate) fn limit<C: Channel>(channel: &C, pages: usize) -> usize {
        let largest = match pages {
            0 => PacketSize::in_band(PACKET),
            pages => {
                PacketSize::in_band(PACKET) + size_of::<u64>() + size_of::<GpaRange>() + pages * 8
            }
        };
        (channel.outgoing().mem().len() / largest).clamp(1, 256)
    }

    /// Sends one packet that asks for a completion under `id`: `operation`, then `body`, zero
    /// padded (`storvsc:591-628`). With `data` it is a GPA direct packet naming those pages
    /// (`storvsc:563-588`); without, an in-band one (`storvsc:542-560`).
    pub(crate) fn send<C: Channel>(
        &mut self,
        channel: &mut C,
        id: u64,
        operation: Operation,
        body: &[u8],
        data: Option<PagedRange<'_>>,
    ) -> Result<(), Error> {
        let header = Packet {
            operation,
            flags: 0,
            status: NtStatus::SUCCESS,
        };
        let mut bytes = [0u8; PACKET];
        bytes[..HEADER].copy_from_slice(header.as_bytes());
        bytes[HEADER..HEADER + body.len()].copy_from_slice(body);
        let ranges;
        let typ = match data {
            Some(range) => {
                ranges = [range];
                OutgoingPacketType::GpaDirect(&ranges)
            }
            None => OutgoingPacketType::InBandWithCompletion,
        };
        let packet = OutgoingPacket {
            transaction_id: id,
            size: PACKET,
            typ,
        };
        let signal = {
            let ring = channel.outgoing();
            let mut next = self.write.clone();
            let range = match ring.write(&mut next, &packet) {
                Ok(range) => range,
                Err(WriteError::Full(size)) => {
                    self.wanted = size;
                    return Err(Error::Full);
                }
                Err(WriteError::Corrupt(e)) => return Err(Error::Ring(e)),
            };
            if range.writer(ring).write(&bytes).is_err() {
                return Err(Error::Ring(vmbus_ring::Error::InvalidMessageLength));
            }
            self.write = next;
            ring.commit_write(&mut self.write)
        };
        if signal {
            channel.signal_host().map_err(Error::Channel)?;
        }
        Ok(())
    }

    /// Takes the next packet out of the ring from the host, if there is one.
    pub(crate) fn receive<C: Channel>(
        &mut self,
        channel: &mut C,
    ) -> Result<Option<Received>, Error> {
        let (received, signal) = {
            let ring = channel.incoming();
            let mut next = self.read.clone();
            let packet = match ring.read(&mut next) {
                Ok(packet) => packet,
                Err(ReadError::Empty) => return Ok(None),
                Err(ReadError::Corrupt(e)) => return Err(Error::Ring(e)),
            };
            let kind = match (packet.typ, packet.transaction_id) {
                (IncomingPacketType::Completion, Some(id)) => Kind::Completion(id),
                (IncomingPacketType::InBand, _) => Kind::InBand,
                _ => Kind::Other,
            };
            let len = packet.payload.len();
            let mut bytes = [0u8; PACKET];
            let kept = &mut bytes[..len.min(PACKET)];
            if packet.payload.reader(ring).read(kept).is_err() {
                return Err(Error::Ring(vmbus_ring::Error::InvalidMessageLength));
            }
            self.read = next;
            // True when the host waits for room in this ring and now has it.
            let signal = ring.commit_read(&mut self.read);
            (Received { kind, len, bytes }, signal)
        };
        if signal {
            channel.signal_host().map_err(Error::Channel)?;
        }
        Ok(Some(received))
    }

    /// Sleeps until the host signals or `deadline` passes, unless there is already something
    /// to do: a packet to read, or room for the packet that was refused. The order is the
    /// ring's rule for not missing a signal, as OpenVMM's `vmbus_async` keeps it
    /// (`vm/devices/vmbus/vmbus_async/src/core.rs:109-147`, `:190-227`): ask for the signal
    /// first, then look at the ring again, and only then sleep. A channel's incoming ring
    /// starts with the host's signal masked, so the first wait is the first unmasking.
    pub(crate) fn wait<C: Channel>(
        &mut self,
        channel: &mut C,
        deadline: Option<u64>,
    ) -> Result<Wait, Error> {
        let ready = {
            let incoming = channel.incoming();
            incoming.set_interrupt_mask(false);
            let mut ready = incoming.can_read(&mut self.read).map_err(Error::Ring)?;
            if self.wanted != 0 {
                let outgoing = channel.outgoing();
                outgoing
                    .set_pending_send_size(self.wanted)
                    .map_err(Error::Ring)?;
                ready |= outgoing
                    .can_write(&mut self.write, self.wanted)
                    .map_err(Error::Ring)?;
            }
            ready
        };
        let waited = if ready {
            Ok(Wait::Signalled)
        } else {
            channel.wait(deadline)
        };
        // Back to polling: the host need not signal while this side is reading and writing.
        channel.incoming().set_interrupt_mask(true);
        if self.wanted != 0 {
            self.wanted = 0;
            channel
                .outgoing()
                .set_pending_send_size(0)
                .map_err(Error::Ring)?;
        }
        waited.map_err(Error::Channel)
    }
}

/// The first `T` of a packet's body, which is always long enough for the types read here.
pub(crate) fn field<T: FromBytes>(body: &[u8]) -> Option<T> {
    T::read_from_prefix(body).ok().map(|(value, _)| value)
}
