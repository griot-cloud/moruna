//! The card: RNDIS initialisation over the buffers NVSP set up, then frames out through
//! send-buffer sections and in from receive-buffer sections.

use alloc::{collections::VecDeque, format, string::String, vec::Vec};
use core::ops::Range;

use moruna_vmbus::Channel;
use netvsp_protocol::{
    protocol::{self, Status},
    rndisprot::{self, Oid},
};
use vmbus_ring::{OutgoingPacketType, TransferPageRange};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::{
    Buffer, ETHERNET_HEADER, NetvscError, PAGE, Shared,
    nvsp::{self, Receive, Sections},
    rndis,
    wire::{Kind, PAYLOAD, Packet, Wire},
};

/// The packet filter: frames to the card's own address, multicast frames and
/// broadcast frames (work item 1.11a, RNDIS initialisation).
const FILTER: u32 = rndisprot::NDIS_PACKET_TYPE_DIRECTED
    | rndisprot::NDIS_PACKET_TYPE_MULTICAST
    | rndisprot::NDIS_PACKET_TYPE_BROADCAST;

/// Completions that may be owed beyond one for each receive section: acknowledgements
/// of what the host says about a virtual function.
const ACKNOWLEDGEMENTS: usize = 8;

/// What the host has sent, one thing at a time.
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A frame the card received: a whole Ethernet frame without its check sequence.
    /// Its section goes back to the host at the next [`Netvsc::poll`].
    Frame(&'a [u8]),
    /// The link changed: `true` when it came up, `false` when it went down.
    Link(bool),
}

/// What one step through the host's packets found.
enum Seen {
    /// A frame, now in `frame`.
    Frame,
    /// The link changed to this.
    Link(bool),
    /// The completion of an RNDIS request, now in `frame`: this many bytes.
    Control(usize),
}

/// The receive whose ranges are being worked through.
struct Receiving {
    transaction: u64,
    /// The next of its ranges.
    next: usize,
    /// What its completion will say: failure once any range was refused.
    status: Status,
}

/// A completion the host is owed, written as soon as the ring to it has room.
#[derive(Clone, Copy)]
enum Owed {
    /// Every range of this receive has been handed over: its sections go back.
    Received(u64, Status),
    /// A message the host wanted acknowledged, and nothing more.
    Acknowledged(u64),
}

/// Azure's synthetic network card over a VMBus channel `C`, with its two buffers in
/// memory `S` of the caller's.
pub struct Netvsc<C: Channel, S: Shared> {
    wire: Wire<C>,
    receive: Receive<S>,
    send: Sections<S>,
    mac: [u8; 6],
    mtu: u32,
    /// The longest frame taken from the host or given to it: the MTU and a header once
    /// the MTU is known, and until then what a receive section holds.
    longest: usize,
    link: bool,
    /// The last RNDIS request's ID.
    requests: u32,
    payload: [u8; PAYLOAD],
    ranges: Vec<TransferPageRange>,
    receiving: Option<Receiving>,
    owed: VecDeque<Owed>,
    /// The frame, or the RNDIS completion, most recently read.
    frame: Vec<u8>,
}

fn malformed<T>(what: impl Into<String>) -> Result<T, NetvscError> {
    Err(NetvscError::Malformed(what.into()))
}

impl<C: Channel, S: Shared> Netvsc<C, S> {
    /// Initialises the card on an open channel, in netvsc's order: NVSP (the version,
    /// the NDIS configuration and version, the receive and send buffers), then RNDIS
    /// (initialise, the address, the MTU, the packet filter).
    ///
    /// `receive` and `send` are the card's two buffers, whole pages of the caller's
    /// memory that its channel can share: the host cuts the first into sections and
    /// puts each frame it delivers in one, and says how long the sections of the second
    /// are, of which each frame sent occupies one until the host has taken it.
    ///
    /// It waits on the channel for each of the host's answers, so it returns only when
    /// the card is ready or cannot be. `deadline` is when it gives up, a time on the
    /// channel's clock (`Channel::wait`); with none it waits for as long as the channel
    /// is open.
    pub fn start(
        channel: C,
        receive: Buffer<S>,
        send: Buffer<S>,
        deadline: Option<u64>,
    ) -> Result<Netvsc<C, S>, NetvscError> {
        for (bytes, what) in [
            (
                receive.memory.len(),
                "the receive buffer is not whole pages",
            ),
            (send.memory.len(), "the send buffer is not whole pages"),
        ] {
            if bytes == 0 || !bytes.is_multiple_of(PAGE) {
                return Err(NetvscError::Buffer(what));
            }
        }
        let mut wire = Wire::new(channel, deadline);
        nvsp::negotiate(&mut wire)?;
        nvsp::configure(&mut wire)?;
        let receive = nvsp::receive_buffer(&mut wire, receive)?;
        let send = nvsp::send_buffer(&mut wire, send)?;
        let mut card = Netvsc {
            wire,
            longest: receive.section,
            receive,
            send,
            mac: [0; 6],
            mtu: 0,
            // The host takes the guest to see the link up from the start and indicates
            // each change after that (netvsp: `guest_link_up: true` in
            // `PrimaryChannelState::new`, then `indicate_status`).
            link: true,
            requests: 0,
            payload: [0; PAYLOAD],
            ranges: Vec::new(),
            receiving: None,
            owed: VecDeque::new(),
            frame: Vec::new(),
        };
        card.initialize()?;
        card.mac = card.address()?;
        card.mtu = card.frame_size()?;
        card.longest = card.mtu as usize + ETHERNET_HEADER;
        // The largest frame and its RNDIS fields must fit one send section, or some
        // frames could never be sent: the card would not be one. Checked before the
        // filter is set, so a card that fails here never starts taking frames (the PM's
        // decision on item 1.11a's question 1).
        let message = rndis::DATA + card.longest;
        if message > card.send.size {
            return Err(NetvscError::Unfit {
                message,
                section: card.send.size,
            });
        }
        card.filter()?;
        card.wire.flush()?;
        Ok(card)
    }

    /// The card's address.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// The largest payload a frame carries; the largest frame is this and an Ethernet
    /// header.
    pub fn mtu(&self) -> u32 {
        self.mtu
    }

    /// Whether the link is up, as the host last indicated.
    pub fn link_up(&self) -> bool {
        self.link
    }

    /// Asks the host to signal when there is something to do, without waiting: true when
    /// there is already, a packet to [`poll`](Netvsc::poll), or room for a frame
    /// [`send`](Netvsc::send) found none for. A driver that waits on handles (its clients'
    /// and, on a node, its channel's wake channel) polls until `None`, sends what waits,
    /// then arms, and waits only when this answers false: the host's next packet, or the
    /// room, signals the channel. It [flushes](Netvsc::flush) first, so nothing written is
    /// left uncommitted while the driver sleeps. As `moruna_storvsc::Storvsc::arm`.
    pub fn arm(&mut self) -> Result<bool, NetvscError> {
        self.wire.arm()
    }

    /// Sends the host everything written since the last flush (frames, completions): one
    /// commit of the ring to the host, and a signal only if VMBus's rule asks for one (the
    /// ring was empty before it and the host has not masked its interrupt).
    pub fn flush(&mut self) -> Result<(), NetvscError> {
        self.wire.flush()
    }

    /// The channel, for the caller to wait on between polls.
    pub fn channel(&mut self) -> &mut C {
        &mut self.wire.channel
    }

    /// Sends one frame: an RNDIS packet message in a free section of the send buffer,
    /// named to the host by an NVSP message that asks for a completion. The host's
    /// completion, read by [`Netvsc::poll`], frees the section.
    ///
    /// `Ok(false)` when no section is free or the ring to the host has no room: the
    /// frame was not sent, and can be once `poll` has read what the host sent next.
    ///
    /// Frames sent back to back go to the host together, at the next
    /// [`flush`](Netvsc::flush) (or [`arm`](Netvsc::arm), or a `poll` that answers `None`):
    /// one commit of the ring, and at most one signal, by VMBus's rule.
    ///
    /// Source: `netvsp_protocol`'s `Message1SendRndisPacket`, which names a section of
    /// the send buffer by its index; netvsp `parse_packet` and
    /// `handle_rndis_packet_message`, which read the section's
    /// `send_buffer_section_size` bytes as RNDIS packet messages and need a transaction
    /// to complete; Microsoft, REMOTE_NDIS_PACKET_MSG.
    pub fn send(&mut self, frame: &[u8]) -> Result<bool, NetvscError> {
        if frame.len() < ETHERNET_HEADER || frame.len() > self.longest {
            return Err(NetvscError::Length(frame.len()));
        }
        let message = rndis::DATA + frame.len();
        if message > self.send.size {
            // A frame that fits no section is refused, not sent by guest page numbers
            // (the PM's decision on item 1.11a's question 1). `start` has checked that
            // the largest frame fits, so on a started card this never happens. A
            // Hyper-V host's sections are 6,144 bytes (netvsp: `section_size: 6144`).
            return Err(NetvscError::Unfit {
                message,
                section: self.send.size,
            });
        }
        // What the host is owed goes first: it is waiting on it, and it is older.
        self.settle()?;
        let Some(section) = self.send.take() else {
            return Ok(false);
        };
        let at = section as usize * self.send.size;
        self.send.memory.write(at, &rndis::data(frame.len()));
        self.send.memory.write(at + rndis::DATA, frame);
        let sent = self.name(section, protocol::DATA_CHANNEL_TYPE, message)?;
        if !sent {
            self.send.give(section.into());
        }
        Ok(sent)
    }

    /// The next thing the host has sent: a frame, or a change of the link. `None` when
    /// it has sent nothing more; the caller then waits for the host's signal on its
    /// channel. Call it until it answers `None`: a receive's sections go back to the
    /// host, and a sent frame's section is freed, as it works through the host's
    /// packets.
    ///
    /// An error is one thing the host sent that was refused and dropped (or, for
    /// [`NetvscError::NotSent`], a frame the host could not send). The card goes on
    /// working, and the next call carries on, unless the error is of the ring or the
    /// channel.
    ///
    /// Source for receiving: netvsp `process_endpoint_rx` and `try_send_rndis_message`
    /// (one transfer-page packet names a section for each frame, and asks for a
    /// completion); netvsp `write_header` in `buffers.rs` (the RNDIS packet message at
    /// the start of each section); Microsoft, REMOTE_NDIS_PACKET_MSG and
    /// REMOTE_NDIS_INDICATE_STATUS_MSG.
    pub fn poll(&mut self) -> Result<Option<Event<'_>>, NetvscError> {
        match self.pump()? {
            None => {
                // The completions written while reading go to the host together.
                self.wire.flush()?;
                Ok(None)
            }
            Some(Seen::Frame) => Ok(Some(Event::Frame(&self.frame))),
            Some(Seen::Link(up)) => Ok(Some(Event::Link(up))),
            Some(Seen::Control(_)) => malformed(unasked()),
        }
    }

    /// Names the RNDIS message of `len` bytes in `section` to the host; `Ok(false)` when
    /// the ring has no room. The transaction is the section, so the host's completion
    /// says which section to free.
    fn name(&mut self, section: u32, channel_type: u32, len: usize) -> Result<bool, NetvscError> {
        let packet = protocol::Message1SendRndisPacket {
            channel_type,
            send_buffer_section_index: section,
            send_buffer_section_size: len as u32,
        };
        let (bytes, size) = self
            .wire
            .message(protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET, &packet);
        self.wire.put(
            OutgoingPacketType::InBandWithCompletion,
            section.into(),
            &bytes[..size],
        )
    }

    /// Writes the completions the host is owed, the oldest first, while the ring to it
    /// has room.
    fn settle(&mut self) -> Result<(), NetvscError> {
        while let Some(owed) = self.owed.front().copied() {
            let sent = match owed {
                Owed::Received(transaction, status) => {
                    // Source: netvsp's test guest (`read_rndis_control_message`) and
                    // netvsp `release_recv_buffers`: a completion with the receive's
                    // transaction gives its sections back.
                    let complete = protocol::Message1SendRndisPacketComplete { status };
                    let (bytes, size) = self.wire.message(
                        protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                        &complete,
                    );
                    self.wire
                        .put(OutgoingPacketType::Completion, transaction, &bytes[..size])?
                }
                Owed::Acknowledged(transaction) => {
                    self.wire
                        .put(OutgoingPacketType::Completion, transaction, &[])?
                }
            };
            if !sent {
                break;
            }
            self.owed.pop_front();
        }
        Ok(())
    }

    /// Records a completion the host is owed. A host that has more owed to it than it
    /// has receive sections to wait on is not reading what it is sent, and is refused.
    fn owe(&mut self, owed: Owed) -> Result<(), NetvscError> {
        if self.owed.len() >= self.receive.count + ACKNOWLEDGEMENTS {
            return malformed(format!(
                "one more packet to complete with {} completions unread",
                self.owed.len()
            ));
        }
        self.owed.push_back(owed);
        Ok(())
    }

    /// Works through what the host has sent until there is something for the caller:
    /// the next range of the receive in hand, then the next packet on the ring.
    fn pump(&mut self) -> Result<Option<Seen>, NetvscError> {
        loop {
            if let Some(receiving) = self.receiving.as_mut()
                && let Some(range) = self.ranges.get(receiving.next).copied()
            {
                receiving.next += 1;
                match self.message(range) {
                    Ok(Some(seen)) => return Ok(Some(seen)),
                    Ok(None) => continue,
                    Err(e) => {
                        if let Some(receiving) = self.receiving.as_mut() {
                            receiving.status = Status::FAILURE;
                        }
                        return Err(e);
                    }
                }
            }
            self.finish()?;
            let Some(packet) = self.wire.next(&mut self.payload, &mut self.ranges)? else {
                return Ok(None);
            };
            self.packet(packet)?;
        }
    }

    /// Gives the sections of the receive in hand back to the host, once every one of
    /// its ranges has been handed over, and writes what the host is owed.
    fn finish(&mut self) -> Result<(), NetvscError> {
        let ranges = self.ranges.len();
        if let Some(done) = self.receiving.take_if(|receiving| receiving.next >= ranges) {
            self.owe(Owed::Received(done.transaction, done.status))?;
        }
        self.settle()
    }

    /// One packet from the ring.
    fn packet(&mut self, packet: Packet) -> Result<(), NetvscError> {
        let len = packet.len.min(PAYLOAD);
        match packet.kind {
            Kind::Completion => self.sent(packet.transaction, len),
            Kind::InBand => {
                // The one message the host sends unasked in a packet of its own is what
                // it says of a virtual function (netvsp `guest_vf_is_available`). It is
                // acknowledged, when the host asks for that, and ignored: the node uses
                // the synthetic card only, and never asks for the data path to move
                // (1F; work item, 6). The acknowledgement is an empty completion of the
                // message's transaction, as netvsp's test guest sends it.
                let what = "a message of its own";
                let _: protocol::Message4SendVfAssociation = nvsp::body(
                    &self.payload[..len],
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                    what,
                )?;
                match packet.transaction {
                    Some(transaction) => self.owe(Owed::Acknowledged(transaction)),
                    None => Ok(()),
                }
            }
            Kind::Transfer { set, count } => self.transfer(packet.transaction, len, set, count),
            Kind::Direct => malformed("a packet naming guest pages, which only a guest sends"),
        }
    }

    /// The host's completion of a sent frame: its section is free again.
    ///
    /// Source: netvsp `complete_tx_packet` and `try_send_tx_packet`, which complete the
    /// guest's transaction with `Message1SendRndisPacketComplete`.
    fn sent(&mut self, transaction: Option<u64>, len: usize) -> Result<(), NetvscError> {
        let transaction = transaction.unwrap_or(u64::MAX);
        if !self.send.give(transaction) {
            return malformed(format!(
                "a completion of transaction {transaction:#x}, which is not in flight"
            ));
        }
        let complete: protocol::Message1SendRndisPacketComplete = nvsp::body(
            &self.payload[..len],
            protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
            "a frame sent",
        )?;
        if complete.status != Status::SUCCESS {
            return Err(NetvscError::NotSent(complete.status.0));
        }
        Ok(())
    }

    /// A receive: an NVSP message that RNDIS messages wait in the receive buffer, at
    /// the ranges the packet names. Its ranges are then worked through one at a time.
    ///
    /// Whatever is wrong with a receive that asks for a completion, it is completed, as
    /// a failure: the host gets its sections back.
    fn transfer(
        &mut self,
        transaction: Option<u64>,
        len: usize,
        set: u16,
        count: u32,
    ) -> Result<(), NetvscError> {
        let Some(transaction) = transaction else {
            return malformed("a receive that asks for no completion");
        };
        let what = "a receive";
        let message: Result<protocol::Message1SendRndisPacket, _> = nvsp::body(
            &self.payload[..len],
            protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
            what,
        );
        let refusal = match message {
            Err(e) => Some(e),
            Ok(_) if set != nvsp::RECEIVE_BUFFER => Some(NetvscError::Malformed(format!(
                "a receive from transfer page set {set}, which is not the receive buffer"
            ))),
            Ok(_) if count == 0 || count as usize != self.ranges.len() => Some(
                NetvscError::Malformed(format!("a receive that names {count} ranges")),
            ),
            Ok(_) => None,
        };
        if let Some(refusal) = refusal {
            self.owe(Owed::Received(transaction, Status::FAILURE))?;
            return Err(refusal);
        }
        self.receiving = Some(Receiving {
            transaction,
            next: 0,
            status: Status::SUCCESS,
        });
        Ok(())
    }

    /// The RNDIS message in one range of a receive. The range lies in the receive
    /// buffer's sections, and the message, whose length the range's first bytes state,
    /// lies in the range.
    fn message(&mut self, range: TransferPageRange) -> Result<Option<Seen>, NetvscError> {
        let at = range.byte_offset as usize;
        let len = range.byte_count as usize;
        let end = u64::from(range.byte_offset) + u64::from(range.byte_count);
        if at < self.receive.start || end > self.receive.end as u64 || len < rndis::HEADER {
            return malformed(format!(
                "a receive of {len} bytes at byte {at}, with sections from byte {} to {}",
                self.receive.start, self.receive.end
            ));
        }
        let header: rndisprot::MessageHeader = rndis::read(&self.receive.memory, at);
        let total = header.message_length as usize;
        if total < rndis::HEADER || total > len {
            return malformed(format!(
                "an RNDIS message of {total} bytes in a receive of {len} bytes"
            ));
        }
        match header.message_type {
            rndisprot::MESSAGE_TYPE_PACKET_MSG => self.frame_at(at, total),
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG => self.indication_at(at, total),
            rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT
            | rndisprot::MESSAGE_TYPE_QUERY_CMPLT
            | rndisprot::MESSAGE_TYPE_SET_CMPLT => self.completion_at(at, total),
            other => malformed(format!(
                "RNDIS message {other:#x}, which this driver has no use for"
            )),
        }
    }

    /// A frame: REMOTE_NDIS_PACKET_MSG, whose data lies in the message after its fields.
    /// The data's offset counts from the DataOffset field, the first after the header
    /// (Microsoft, REMOTE_NDIS_PACKET_MSG). Whatever per-packet information the host put
    /// before the data is not read: no offload is in use.
    fn frame_at(&mut self, at: usize, total: usize) -> Result<Option<Seen>, NetvscError> {
        if total < rndis::DATA {
            return malformed(format!("an RNDIS packet message of {total} bytes"));
        }
        let packet: rndisprot::Packet = rndis::read(&self.receive.memory, at + rndis::HEADER);
        let offset = u64::from(packet.data_offset);
        let length = u64::from(packet.data_length);
        let start = rndis::HEADER as u64 + offset;
        if offset < size_of::<rndisprot::Packet>() as u64 || start + length > total as u64 {
            return malformed(format!(
                "a frame of {length} bytes at byte {offset} of a message of {total} bytes"
            ));
        }
        if length < ETHERNET_HEADER as u64 || length > self.longest as u64 {
            return malformed(format!("a frame of {length} bytes"));
        }
        self.frame.resize(length as usize, 0);
        self.receive
            .memory
            .read(at + start as usize, &mut self.frame);
        Ok(Some(Seen::Frame))
    }

    /// An indication: REMOTE_NDIS_INDICATE_STATUS_MSG. The status RNDIS_STATUS_MEDIA_CONNECT
    /// says the link went from down to up, RNDIS_STATUS_MEDIA_DISCONNECT that it went
    /// from up to down, and an indication is never answered (Microsoft,
    /// REMOTE_NDIS_INDICATE_STATUS_MSG). netvsp sends them from `indicate_status`. Any
    /// other indication is of something this driver does not use, and is passed over.
    fn indication_at(&mut self, at: usize, total: usize) -> Result<Option<Seen>, NetvscError> {
        if total < rndis::HEADER + size_of::<rndisprot::IndicateStatus>() {
            return malformed(format!("an RNDIS indication of {total} bytes"));
        }
        let indication: rndisprot::IndicateStatus =
            rndis::read(&self.receive.memory, at + rndis::HEADER);
        let up = match indication.status {
            rndisprot::STATUS_MEDIA_CONNECT => true,
            rndisprot::STATUS_MEDIA_DISCONNECT => false,
            _ => return Ok(None),
        };
        if self.link == up {
            return Ok(None);
        }
        self.link = up;
        Ok(Some(Seen::Link(up)))
    }

    /// The completion of an RNDIS request, copied out whole: no longer than a section,
    /// and long enough for the request's ID and its status.
    fn completion_at(&mut self, at: usize, total: usize) -> Result<Option<Seen>, NetvscError> {
        let least = rndis::HEADER + size_of::<rndisprot::SetComplete>();
        if total < least || total > self.receive.section {
            return malformed(format!("an RNDIS completion of {total} bytes"));
        }
        self.frame.resize(total, 0);
        self.receive.memory.read(at, &mut self.frame);
        Ok(Some(Seen::Control(total)))
    }

    /// Reads what the host has sent, during initialisation: up to an RNDIS completion,
    /// whose length it answers, or until the host has sent nothing more. A frame that
    /// arrives before the card has started has no one to take it and is dropped; the
    /// link is as the host last indicated.
    fn drain(&mut self) -> Result<Option<usize>, NetvscError> {
        loop {
            match self.pump()? {
                None => return Ok(None),
                Some(Seen::Control(len)) => return Ok(Some(len)),
                Some(Seen::Frame) | Some(Seen::Link(_)) => {}
            }
        }
    }

    /// Sends one RNDIS control message and waits for its completion, which it leaves in
    /// `frame`, returning its length.
    ///
    /// The message goes in a send-buffer section, named on the control channel, and the
    /// host answers twice: it completes the packet (which frees the section), and it
    /// sends the RNDIS completion as a receive of its own. The RNDIS completion carries
    /// the request's ID.
    ///
    /// Source: netvsp `handle_rndis_message` and `handle_rndis_control_message`, and
    /// `send_rndis_control_message` for the answer; Microsoft's pages for each message,
    /// where the RequestId field is what matches an answer to its request.
    fn control<T: IntoBytes + Immutable>(
        &mut self,
        message_type: u32,
        body: &T,
        information: &[u8],
        completion: u32,
        request_id: u32,
        step: &'static str,
    ) -> Result<usize, NetvscError> {
        let (message, len) = rndis::control(message_type, body, information);
        // Each wait below comes after everything the host has sent was read, and after
        // what was waited for was looked for once more: a completion read on the way
        // may be the very thing, and the host signals only what it sends after that.
        let section = loop {
            if let Some(section) = self.send.take() {
                break section;
            }
            // The host holds every section: one comes back with a completion.
            if self.drain()?.is_some() {
                return malformed(unasked());
            }
            if let Some(section) = self.send.take() {
                break section;
            }
            self.wire.wait(step)?;
        };
        self.send
            .memory
            .write(section as usize * self.send.size, &message[..len]);
        while !self.name(section, protocol::CONTROL_CHANNEL_TYPE, len)? {
            // The ring to the host is full, and the host has been asked to signal when
            // it has room. It may itself be waiting for this side to read.
            if self.drain()?.is_some() {
                return malformed(unasked());
            }
            self.wire.wait(step)?;
        }
        let len = loop {
            if let Some(len) = self.drain()? {
                break len;
            }
            self.wire.wait(step)?;
        };
        // The completion is copied out, so its section goes back now.
        self.finish()?;
        // Every completion starts, after the header, with the request's ID and a status.
        let message = &self.frame[..len];
        let kind = rndisprot::MessageHeader::read_from_prefix(message)
            .ok()
            .map(|(header, _)| header.message_type);
        let answer = rndis::fields::<rndisprot::SetComplete>(message);
        if kind != Some(completion) || answer.map(|a| a.request_id) != Some(request_id) {
            return malformed(format!(
                "another RNDIS completion where its answer to {step} is due"
            ));
        }
        match answer.map(|a| a.status) {
            Some(rndisprot::STATUS_SUCCESS) => Ok(len),
            status => Err(NetvscError::Refused {
                step,
                status: status.unwrap_or_default(),
            }),
        }
    }

    fn request_id(&mut self) -> u32 {
        self.requests += 1;
        self.requests
    }

    /// RNDIS initialisation: REMOTE_NDIS_INITIALIZE_MSG, the first RNDIS message a
    /// device is sent. It states RNDIS 1.0, the version Microsoft's page gives, and
    /// the largest single transfer this side expects to receive, which here is one
    /// receive section. In RNDIS's words this driver is the host and the card is the
    /// device. The answer, REMOTE_NDIS_INITIALIZE_CMPLT, must report success and
    /// Ethernet (RNDIS_MEDIUM_802_3).
    ///
    /// Source: Microsoft, REMOTE_NDIS_INITIALIZE_MSG and REMOTE_NDIS_INITIALIZE_CMPLT;
    /// netvsp `handle_rndis_control_message`, the `MESSAGE_TYPE_INITIALIZE_MSG` arm,
    /// which accepts it once.
    fn initialize(&mut self) -> Result<(), NetvscError> {
        let step = "RNDIS initialisation";
        let request_id = self.request_id();
        let request = rndisprot::InitializeRequest {
            request_id,
            major_version: rndisprot::MAJOR_VERSION,
            minor_version: rndisprot::MINOR_VERSION,
            max_transfer_size: self.receive.section as u32,
        };
        let len = self.control(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            &request,
            &[],
            rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT,
            request_id,
            step,
        )?;
        let Some(complete) = rndis::fields::<rndisprot::InitializeComplete>(&self.frame[..len])
        else {
            return malformed(format!("{len} bytes in answer to {step}"));
        };
        if complete.medium != rndisprot::MEDIUM_802_3 {
            return malformed(format!(
                "medium {} in answer to {step}, which is not Ethernet",
                complete.medium
            ));
        }
        Ok(())
    }

    /// Queries one value: REMOTE_NDIS_QUERY_MSG with no input, so its input length and
    /// offset are zero, answered by REMOTE_NDIS_QUERY_CMPLT, whose value lies at an
    /// offset counted from the RequestId field, the first after the header. Answers
    /// where the value is in `frame`.
    ///
    /// Source: Microsoft, REMOTE_NDIS_QUERY_MSG and REMOTE_NDIS_QUERY_CMPLT; netvsp
    /// `handle_rndis_control_message`, the `MESSAGE_TYPE_QUERY_MSG` arm, and
    /// `handle_oid_query`.
    fn query(&mut self, oid: Oid, step: &'static str) -> Result<Range<usize>, NetvscError> {
        let request_id = self.request_id();
        let request = rndisprot::QueryRequest {
            request_id,
            oid,
            information_buffer_length: 0,
            information_buffer_offset: 0,
            device_vc_handle: 0,
        };
        let len = self.control(
            rndisprot::MESSAGE_TYPE_QUERY_MSG,
            &request,
            &[],
            rndisprot::MESSAGE_TYPE_QUERY_CMPLT,
            request_id,
            step,
        )?;
        let Some(complete) = rndis::fields::<rndisprot::QueryComplete>(&self.frame[..len]) else {
            return malformed(format!("{len} bytes in answer to {step}"));
        };
        let fields = (rndis::HEADER + size_of::<rndisprot::QueryComplete>()) as u64;
        let start = rndis::HEADER as u64 + u64::from(complete.information_buffer_offset);
        let end = start + u64::from(complete.information_buffer_length);
        if start < fields || end > len as u64 {
            return malformed(format!(
                "a value of {} bytes at byte {} in an answer of {len} bytes to {step}",
                complete.information_buffer_length, complete.information_buffer_offset
            ));
        }
        Ok(start as usize..end as usize)
    }

    /// The card's address: OID_802_3_CURRENT_ADDRESS, the address the card is using now
    /// (Microsoft, OID_802_3_CURRENT_ADDRESS), six bytes. netvsp answers it from the
    /// same address as the permanent one (`handle_oid_query`).
    fn address(&mut self) -> Result<[u8; 6], NetvscError> {
        let step = "the query of its address";
        let at = self.query(Oid::OID_802_3_CURRENT_ADDRESS, step)?;
        match <[u8; 6]>::try_from(&self.frame[at.clone()]) {
            Ok(mac) => Ok(mac),
            Err(_) => malformed(format!("an address of {} bytes", at.len())),
        }
    }

    /// The MTU: OID_GEN_MAXIMUM_FRAME_SIZE, the largest packet the card carries, in
    /// bytes, not counting the header (Microsoft, OID_GEN_MAXIMUM_FRAME_SIZE), four
    /// bytes. netvsp answers the NDIS configuration's MTU less the Ethernet header
    /// (`handle_oid_query`). The longest frame it allows must fit a receive section
    /// behind its RNDIS fields, or the host could not deliver one.
    fn frame_size(&mut self) -> Result<u32, NetvscError> {
        let step = "the query of its MTU";
        let at = self.query(Oid::OID_GEN_MAXIMUM_FRAME_SIZE, step)?;
        let Ok(bytes) = <[u8; 4]>::try_from(&self.frame[at.clone()]) else {
            return malformed(format!("an MTU of {} bytes", at.len()));
        };
        let mtu = u32::from_le_bytes(bytes);
        let message = rndis::DATA as u64 + ETHERNET_HEADER as u64 + u64::from(mtu);
        if mtu == 0 || message > self.receive.section as u64 {
            return malformed(format!(
                "an MTU of {mtu}, with receive sections of {} bytes",
                self.receive.section
            ));
        }
        Ok(mtu)
    }

    /// Sets the packet filter, OID_GEN_CURRENT_PACKET_FILTER, to directed, multicast and
    /// broadcast frames: REMOTE_NDIS_SET_MSG with the four-byte filter right after its
    /// fields, at an offset counted from the RequestId field, answered by
    /// REMOTE_NDIS_SET_CMPLT. Until it is set the host delivers no frame (netvsp
    /// `process_endpoint_rx` drops what arrives while the filter is none).
    ///
    /// Source: Microsoft, REMOTE_NDIS_SET_MSG, REMOTE_NDIS_SET_CMPLT and
    /// OID_GEN_CURRENT_PACKET_FILTER; netvsp `handle_oid_set` and
    /// `oid_set_packet_filter`, which read the filter right after the request's fields.
    fn filter(&mut self) -> Result<(), NetvscError> {
        let step = "the packet filter";
        let request_id = self.request_id();
        let request = rndisprot::SetRequest {
            request_id,
            oid: Oid::OID_GEN_CURRENT_PACKET_FILTER,
            information_buffer_length: size_of::<u32>() as u32,
            information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
            device_vc_handle: 0,
        };
        self.control(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            &request,
            &FILTER.to_le_bytes(),
            rndisprot::MESSAGE_TYPE_SET_CMPLT,
            request_id,
            step,
        )?;
        Ok(())
    }
}

/// An RNDIS completion when nothing was asked.
fn unasked() -> &'static str {
    "an RNDIS completion of a request nobody made"
}
