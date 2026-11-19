//! The channel's two rings as packets: the next one the host sent, and one more for the
//! host. Signals follow the ring's own rules (`vmbus_ring`): the host is signalled when
//! a write or a read changes a ring in a way it asked to hear of, and the host is asked
//! to signal when the ring to it has room again.
//!
//! Packets for the host are batched: each is written to the ring at once, and the ones
//! written since the last [`Wire::flush`] are committed together there, so the host is
//! signalled at most once for all of them, and only by VMBus's rule (the ring was empty
//! before the commit and the host has not masked its interrupt). A wait and an arm flush
//! first, so nothing written is left uncommitted while the driver sleeps.

use alloc::vec::Vec;

use guestmem_core::{MemoryRead, MemoryWrite};
use moruna_vmbus::{Channel, Wait};
use netvsp_protocol::protocol::{self, Version};
use vmbus_ring::{
    IncomingPacketType, OutgoingOffset, OutgoingPacket, OutgoingPacketType, ReadError,
    TransferPageRange, WriteError,
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::NetvscError;

/// Bytes of a packet's payload that are kept: more than any NVSP message this driver
/// reads (`netvsp_protocol`'s largest packet is `PACKET_SIZE_V61`, 40 bytes).
pub(crate) const PAYLOAD: usize = 64;
/// Bytes of the longest NVSP message this driver sends.
pub(crate) const MESSAGE: usize = protocol::PACKET_SIZE_V61;
/// The most ranges a receive names: `netvsp_protocol`'s limit on the packets a host puts
/// in one receive.
const RANGES: usize = protocol::NVSP_MAX_PACKETS_PER_RECEIVE as usize;

/// What kind of packet the host sent.
pub(crate) enum Kind {
    /// The host's answer to a packet of ours.
    Completion,
    /// A message in the packet itself.
    InBand,
    /// A message with ranges of the transfer page set `set`: `count` of them, read into
    /// the caller's list unless the packet had no room for that many or `count` is more
    /// than any host sends, in which case the list is empty.
    Transfer { set: u16, count: u32 },
    /// A message naming guest pages, which only a guest sends.
    Direct,
}

/// A packet the host sent, its payload's first [`PAYLOAD`] bytes copied out.
pub(crate) struct Packet {
    pub kind: Kind,
    /// The transaction to complete, when the host asks for a completion; a completion's
    /// own transaction.
    pub transaction: Option<u64>,
    /// Bytes of the payload on the ring.
    pub len: usize,
}

pub(crate) struct Wire<C: Channel> {
    pub channel: C,
    /// Bytes of every NVSP message sent: the packet size of the version in use.
    message: usize,
    /// Whether the host was asked to signal once the ring to it has room.
    crowded: bool,
    /// The bytes the host was asked to signal for, while `crowded`.
    wanted: usize,
    /// Where the packets written since the last flush end: they are not yet committed,
    /// so the host does not see them.
    pending: Option<OutgoingOffset>,
    /// The NVSP version in use, once negotiated.
    version: Option<u32>,
    /// Whether the host has sent its send indirection table.
    table_seen: bool,
    /// The first table's entry count, until it is said.
    table_news: Option<u32>,
    /// When every wait gives up: a time on the node's monotonic clock, in nanoseconds,
    /// as the channel counts it; `None` for never.
    deadline: Option<u64>,
    /// The trace of the first frames, in a diagnosis build (work item 1.11c): it sees each
    /// commit as it is made.
    #[cfg(feature = "trace-first-frames")]
    pub trace: Option<crate::trace::Trace>,
    /// What the wire has counted: packets in, commits, signals, a full ring (work item 6.3ab).
    pub counts: crate::Counts,
}

/// The NVSP packet size of `version`, in the ring's whole eight-byte words. netvsp
/// sizes its own messages so (`PacketSize`, `NvspMessage::aligned_payload`): the legacy
/// size below 6.1, the larger one from 6.1.
fn packet_size(version: Option<Version>) -> usize {
    let bytes = match version {
        Some(version) if version >= Version::V61 => protocol::PACKET_SIZE_V61,
        _ => protocol::PACKET_SIZE_V1,
    };
    bytes.next_multiple_of(8)
}

impl<C: Channel> Wire<C> {
    pub fn new(channel: C, deadline: Option<u64>) -> Wire<C> {
        // The host signals when it puts a packet on an empty ring only while the ring's
        // interrupt mask is clear, and a channel's incoming ring starts with it set. It
        // stays clear: `next` commits each read before it looks for the next packet, so
        // a packet the host adds is either seen by that look or signalled.
        channel.incoming().set_interrupt_mask(false);
        Wire {
            channel,
            message: packet_size(None),
            crowded: false,
            wanted: 0,
            pending: None,
            version: None,
            table_seen: false,
            table_news: None,
            deadline,
            #[cfg(feature = "trace-first-frames")]
            trace: None,
            counts: crate::Counts::default(),
        }
    }

    /// From now on messages have the packet size of `version`.
    pub fn negotiated(&mut self, version: Version) {
        self.message = packet_size(Some(version));
        self.version = Some(version as u32);
    }

    /// The NVSP version in use, once negotiated.
    pub fn version(&self) -> Option<u32> {
        self.version
    }

    /// If the packet just read, whose payload's first bytes are `payload`, is the host's
    /// send indirection table, notes it (its entry count, the first time) and says so.
    ///
    /// Hyper-V sends the table from NVSP 5: which of the card's channels each flow's
    /// frames go on. Linux takes it (`netvsc_receive_inband`, `netvsc_send_table`), and
    /// FreeBSD's hn takes and ignores it. This card sends on its one channel, so the table
    /// has nothing to steer: it is ignored, wherever it comes, during the start or after.
    pub fn table(&mut self, payload: &[u8]) -> bool {
        let Ok((header, rest)) = protocol::MessageHeader::read_from_prefix(payload) else {
            return false;
        };
        if header.message_type != protocol::MESSAGE5_TYPE_SEND_INDIRECTION_TABLE {
            return false;
        }
        if !self.table_seen {
            self.table_seen = true;
            let entries = protocol::Message5SendIndirectionTable::read_from_prefix(rest)
                .map(|(table, _)| table.table_entry_count)
                .unwrap_or(0);
            self.table_news = Some(entries);
        }
        true
    }

    /// The first table's entry count, once, when it has just come: for the console.
    pub fn take_table(&mut self) -> Option<u32> {
        self.table_news.take()
    }

    /// Asks the host to signal when there is something to do, without waiting: true when
    /// there is already, a packet to read or, after a packet the ring to the host had no
    /// room for, that room. The incoming ring's signal is never masked once the wire is
    /// made, and a full outgoing ring has asked for one (`put`): this only looks again,
    /// after the caller has read everything, so that a packet that came meanwhile is not
    /// left waiting for a signal already given.
    pub fn arm(&mut self) -> Result<bool, NetvscError> {
        self.counts.arms += 1;
        let ready = self.ready()?;
        self.counts.idle += u64::from(!ready);
        Ok(ready)
    }

    /// [`Wire::arm`]'s answer.
    fn ready(&mut self) -> Result<bool, NetvscError> {
        self.flush()?;
        let incoming = self.channel.incoming();
        incoming.set_interrupt_mask(false);
        let mut at = incoming.incoming()?;
        if incoming.can_read(&mut at)? {
            return Ok(true);
        }
        if self.crowded {
            let outgoing = self.channel.outgoing();
            let mut at = outgoing.outgoing()?;
            return Ok(outgoing.can_write(&mut at, self.wanted)?);
        }
        Ok(false)
    }

    /// Waits for the host's signal, while initialisation waits for `step`. A wait that
    /// ends at the deadline ends the step.
    pub fn wait(&mut self, step: &'static str) -> Result<(), NetvscError> {
        self.flush()?;
        match self.channel.wait(self.deadline)? {
            Wait::Signalled => Ok(()),
            Wait::TimedOut => Err(NetvscError::Timeout(step)),
        }
    }

    /// The NVSP message `message_type` with `body`, zero-padded to the packet size.
    pub fn message<T: IntoBytes + Immutable>(
        &self,
        message_type: u32,
        body: &T,
    ) -> ([u8; MESSAGE], usize) {
        const HEADER: usize = size_of::<protocol::MessageHeader>();
        // Every message fits the legacy packet, as netvsp asserts of its own.
        const { assert!(size_of::<T>() <= protocol::PACKET_SIZE_V1 - HEADER) };
        let mut bytes = [0; MESSAGE];
        bytes[..HEADER].copy_from_slice(protocol::MessageHeader { message_type }.as_bytes());
        bytes[HEADER..HEADER + size_of::<T>()].copy_from_slice(body.as_bytes());
        (bytes, self.message)
    }

    /// The next packet the host sent, if there is one: its payload's first bytes in
    /// `payload`, a transfer packet's ranges in `ranges`. The packet is off the ring
    /// when this returns.
    pub fn next(
        &mut self,
        payload: &mut [u8; PAYLOAD],
        ranges: &mut Vec<TransferPageRange>,
    ) -> Result<Option<Packet>, NetvscError> {
        let incoming = self.channel.incoming();
        let mut at = incoming.incoming()?;
        let packet = match incoming.read(&mut at) {
            Ok(packet) => packet,
            Err(ReadError::Empty) => return Ok(None),
            Err(ReadError::Corrupt(e)) => return Err(e.into()),
        };
        let len = packet.payload.len();
        let kept = len.min(PAYLOAD);
        if packet
            .payload
            .reader(incoming)
            .read(&mut payload[..kept])
            .is_err()
        {
            return Err(vmbus_ring::Error::InvalidMessageLength.into());
        }
        let kind = match packet.typ {
            IncomingPacketType::Completion => Kind::Completion,
            IncomingPacketType::InBand => Kind::InBand,
            IncomingPacketType::GpaDirect(..) => Kind::Direct,
            IncomingPacketType::TransferPages(set, count, list) => {
                ranges.clear();
                let room = list.len() / size_of::<TransferPageRange>();
                if count as usize <= room.min(RANGES) {
                    let mut reader = list.reader(incoming);
                    ranges.extend(
                        (0..count).map_while(|_| reader.read_plain::<TransferPageRange>().ok()),
                    );
                }
                Kind::Transfer { set, count }
            }
        };
        // The read is committed before the caller looks for another packet.
        let signal = incoming.commit_read(&mut at);
        self.counts.packets_in += 1;
        if signal {
            self.counts.signals += 1;
            self.channel.signal_host()?;
        }
        Ok(Some(Packet {
            kind,
            transaction: packet.transaction_id,
            len,
        }))
    }

    /// Writes one packet to the ring to the host, for the next [`flush`](Wire::flush);
    /// `Ok(false)` when the ring has no room for it even with what was written before
    /// committed, and the host has then been asked to signal once it has.
    pub fn put(
        &mut self,
        typ: OutgoingPacketType<'_>,
        transaction: u64,
        payload: &[u8],
    ) -> Result<bool, NetvscError> {
        let packet = OutgoingPacket {
            transaction_id: transaction,
            size: payload.len(),
            typ,
        };
        let mut asked = false;
        let (at, range) = loop {
            let outgoing = self.channel.outgoing();
            let batched = self.pending.is_some();
            let mut at = match self.pending.take() {
                Some(at) => at,
                None => outgoing.outgoing()?,
            };
            match outgoing.write(&mut at, &packet) {
                Ok(range) => break (at, range),
                Err(WriteError::Corrupt(e)) => return Err(e.into()),
                Err(WriteError::Full(_)) if batched => {
                    // What was written before goes to the host now, so that it can make
                    // room; the write is tried again from there.
                    self.pending = Some(at);
                    self.flush()?;
                }
                Err(WriteError::Full(_)) if asked => return Ok(false),
                Err(WriteError::Full(bytes)) => {
                    // The host signals once that much is free. It may have made the room
                    // between the write and this request, so the write is tried once more.
                    self.counts.ring_full += 1;
                    outgoing.set_pending_send_size(bytes)?;
                    self.crowded = true;
                    self.wanted = bytes;
                    asked = true;
                }
            }
        };
        let outgoing = self.channel.outgoing();
        if range.writer(outgoing).write(payload).is_err() {
            return Err(vmbus_ring::Error::InvalidMessageLength.into());
        }
        // Written, not yet committed: the host sees it at the next flush.
        self.pending = Some(at);
        if self.crowded {
            outgoing.set_pending_send_size(0)?;
            self.crowded = false;
        }
        Ok(true)
    }

    /// Commits every packet written since the last flush, and signals the host if VMBus's
    /// rule says it needs it: once for all of them.
    pub fn flush(&mut self) -> Result<(), NetvscError> {
        let Some(mut at) = self.pending.take() else {
            return Ok(());
        };
        let signal = self.channel.outgoing().commit_write(&mut at);
        self.counts.commits += 1;
        if signal {
            self.counts.signals += 1;
            self.channel.signal_host()?;
        }
        #[cfg(feature = "trace-first-frames")]
        if let Some(trace) = self.trace.as_mut() {
            trace.committed(signal);
        }
        Ok(())
    }
}
