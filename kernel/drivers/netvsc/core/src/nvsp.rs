//! NVSP initialisation, in netvsc's order: the version, the NDIS configuration and
//! version, then the receive buffer and the send buffer as GPADLs.
//!
//! Each step is one message that asks for a completion, and the host's completion is its
//! answer. Before the buffers exist the host has nothing else to say (netvsp's
//! `initialize` only answers), so anything but the awaited completion is refused.

use alloc::{format, vec, vec::Vec};
use core::fmt;

use moruna_vmbus::Channel;
use netvsp_protocol::protocol::{self, Status, Version};
use vmbus_ring::OutgoingPacketType;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::{
    Buffer, ETHERNET_HEADER, NetvscError, Shared, rndis,
    wire::{Kind, PAYLOAD, Wire},
};

/// Every NVSP version `netvsp_protocol` defines from 5 up, the newest first: what is
/// offered, in the order it is offered. Anything older is never offered, so a host
/// that accepts none of these is refused.
pub(crate) const OFFERS: [Version; 3] = [Version::V61, Version::V6, Version::V5];

/// The transactions of initialisation's requests. A sent frame's transaction is its
/// section's index plus one ([`transaction`]), which is below this.
const REQUEST: u64 = 1 << 32;

/// The transaction that names send section `section` to the host: its index plus one, so
/// that it is never 0. Hyper-V neither acknowledges nor acts on a packet that asks for a
/// completion under transaction ID 0, which is its own, for messages the guest did not
/// ask for: Linux never hands one out for that reason (`drivers/hv/channel.c`,
/// `vmbus_next_request_id`). netvsc sent its first RNDIS request, in section 0, under
/// ID 0, and Azure's host never answered it (console 2026-11-18T0213, image cf0271f5).
pub(crate) fn transaction(section: u32) -> u64 {
    u64::from(section) + 1
}

/// The largest frame of standard Ethernet, a 1,500-byte payload and its header, which
/// NVSP's NDIS configuration calls the MTU. It is netvsp's `DEFAULT_MTU` and its
/// smallest: what the host uses when the guest states none.
const FRAME: u32 = 1500 + ETHERNET_HEADER as u32;

/// The IDs this driver gives its receive buffer and its send buffer: Linux's hv_netvsc's,
/// `NETVSC_RECEIVE_BUFFER_ID` 0xcafe and `NETVSC_SEND_BUFFER_ID` 0
/// (`drivers/net/hyperv/hyperv_net.h`). The host names the receive buffer by its ID in
/// every receive.
///
/// The receive buffer's must not be 0: Azure's host refused one with ID 0 with `FAILURE`
/// (the first Azure boot of netvsc, console 2026-11-18T0146, image 4ba46ef5), where
/// netvsp, whose test guest this driver first copied, takes any ID. Every guest that works
/// on Hyper-V gives it an ID that is not 0: Linux and DPDK 0xcafe (`HN_NVS_RXBUF_SIG` in
/// FreeBSD too), and Microsoft's own UEFI netvsc 0x1981 (`MsvmPkg/NetvscDxe/NetvscDxe.c`,
/// `RECEIVE_BUFFER_ID`). Linux's send buffer ID of 0 works on Azure.
pub(crate) const RECEIVE_BUFFER: u16 = 0xcafe;
const SEND_BUFFER: u16 = 0;

/// The receive buffer, and the sections the host cut it into.
pub(crate) struct Receive<M> {
    pub memory: M,
    /// Where the sections start, and where they end.
    pub start: usize,
    pub end: usize,
    /// Bytes of one section.
    pub section: usize,
    /// How many sections there are.
    pub count: usize,
}

/// The send buffer, its sections, and which of them the host holds.
pub(crate) struct Sections<M> {
    pub memory: M,
    /// Bytes of one section.
    pub size: usize,
    free: Vec<u32>,
    busy: Vec<bool>,
}

impl<M> Sections<M> {
    fn new(memory: M, size: usize, count: u32) -> Sections<M> {
        Sections {
            memory,
            size,
            // The lowest section is taken first.
            free: (0..count).rev().collect(),
            busy: vec![false; count as usize],
        }
    }

    /// A free section, now held.
    pub fn take(&mut self) -> Option<u32> {
        let section = self.free.pop()?;
        self.busy[section as usize] = true;
        Some(section)
    }

    /// Frees the section a transaction names ([`transaction`]); `false` when it names
    /// none that is held.
    pub fn give(&mut self, transaction: u64) -> bool {
        let Some(section) = transaction
            .checked_sub(1)
            .and_then(|section| u32::try_from(section).ok())
        else {
            return false;
        };
        match self.busy.get_mut(section as usize) {
            Some(busy) if *busy => {
                *busy = false;
                self.free.push(section);
                true
            }
            _ => false,
        }
    }
}

/// The body of the NVSP message `expect` in `payload`: `what` names it in the refusal
/// of anything else.
pub(crate) fn body<T: FromBytes>(
    payload: &[u8],
    expect: u32,
    what: &'static str,
) -> Result<T, NetvscError> {
    let Ok((header, rest)) = protocol::MessageHeader::read_from_prefix(payload) else {
        return Err(NetvscError::Malformed(format!(
            "{} bytes for {what}, too few for an NVSP message",
            payload.len()
        )));
    };
    if header.message_type != expect {
        return Err(NetvscError::Malformed(format!(
            "NVSP message {} for {what}, where message {expect} is due",
            header.message_type
        )));
    }
    match T::read_from_prefix(rest) {
        Ok((body, _)) => Ok(body),
        Err(_) => Err(NetvscError::Malformed(format!(
            "NVSP message {expect} for {what} in {} bytes, too few",
            payload.len()
        ))),
    }
}

/// Sends request `number` of initialisation and waits for the host's completion of it,
/// whose payload it leaves in `payload`, returning the bytes of it that were kept.
fn request<C: Channel, T: IntoBytes + Immutable>(
    wire: &mut Wire<C>,
    number: u64,
    message_type: u32,
    body: &T,
    what: &'static str,
    payload: &mut [u8; PAYLOAD],
) -> Result<usize, NetvscError> {
    let transaction = REQUEST + number;
    let (message, len) = wire.message(message_type, body);
    while !wire.put(
        OutgoingPacketType::InBandWithCompletion,
        transaction,
        &message[..len],
    )? {
        wire.wait(what)?;
    }
    let mut ranges = Vec::new();
    loop {
        match wire.next(payload, &mut ranges)? {
            None => wire.wait(what)?,
            Some(packet)
                if matches!(packet.kind, Kind::Completion)
                    && packet.transaction == Some(transaction) =>
            {
                return Ok(packet.len.min(PAYLOAD));
            }
            // The send indirection table, which may come at any time: ignored, and
            // acknowledged if the host asks (`Wire::table`).
            Some(packet)
                if matches!(packet.kind, Kind::InBand)
                    && wire.table(&payload[..packet.len.min(PAYLOAD)]) =>
            {
                if let Some(table) = packet.transaction {
                    while !wire.put(OutgoingPacketType::Completion, table, &[])? {
                        wire.wait(what)?;
                    }
                }
            }
            Some(_) => {
                return Err(NetvscError::Malformed(format!(
                    "another packet where its answer to {what} is due"
                )));
            }
        }
    }
}

/// Offers the newest NVSP version, then each older one down to 5, until the host
/// accepts one. The host refuses a version by a status that is not success (netvsp:
/// `Status::NONE`, and it then waits for another offer), and accepts nothing more from
/// this driver below 5.
///
/// Source: `netvsp_protocol`'s `MessageInit` and `MessageInitComplete`; netvsp
/// `initialize`, the `PacketData::Init` arm, which reads the offer from
/// `protocol_version`. Both of the message's fields carry the version offered: the
/// protocol crate notes they were once a minimum and a maximum and now state one
/// version.
pub(crate) fn negotiate<C: Channel>(
    wire: &mut Wire<C>,
    say: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<Version, NetvscError> {
    let mut payload = [0; PAYLOAD];
    for (number, version) in OFFERS.into_iter().enumerate() {
        say(format_args!("NVSP {} offered", crate::Nvsp(version as u32)));
        let init = protocol::MessageInit {
            protocol_version: version as u32,
            protocol_version2: version as u32,
        };
        let what = "the NVSP version";
        let len = request(
            wire,
            number as u64,
            protocol::MESSAGE_TYPE_INIT,
            &init,
            what,
            &mut payload,
        )?;
        let complete: protocol::MessageInitComplete =
            body(&payload[..len], protocol::MESSAGE_TYPE_INIT_COMPLETE, what)?;
        if complete.status == Status::SUCCESS {
            wire.negotiated(version);
            say(format_args!(
                "NVSP {} accepted",
                crate::Nvsp(version as u32)
            ));
            return Ok(version);
        }
        say(format_args!(
            "NVSP {} refused with status {:#x}",
            crate::Nvsp(version as u32),
            complete.status.0
        ));
    }
    Err(NetvscError::Version)
}

/// States the NDIS configuration, then the NDIS version.
///
/// The work item's list has the NDIS version alone. From NVSP 2 the host also needs the
/// NDIS configuration, and needs it before the receive buffer: netvsp refuses a receive
/// buffer without it (`PacketOrderError::SendReceiveBufferMissingMTU`) and sizes the
/// receive sections from its MTU. It is sent first, as netvsp's test guest sends it
/// (`initialize_with_version`).
///
/// - The MTU is standard Ethernet's largest frame, the host's own default.
/// - No capability is offered: not SR-IOV (the node uses the synthetic card only, 1F),
///   not VLAN (802.1Q), not receive-segment coalescing, nor any other.
///
/// The host completes each with a completion that may be empty: netvsp notes that
/// Microsoft's UEFI client expects one.
///
/// Source: `netvsp_protocol`'s `Message2SendNdisConfig` and `Message1SendNdisVersion`;
/// netvsp `initialize`, the `SendNdisConfig` and `SendNdisVersion` arms.
pub(crate) fn configure<C: Channel>(
    wire: &mut Wire<C>,
    say: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<(), NetvscError> {
    let mut payload = [0; PAYLOAD];
    say(format_args!(
        "NDIS configuration sent: frames of {FRAME} bytes"
    ));
    let config = protocol::Message2SendNdisConfig {
        mtu: FRAME,
        reserved: 0,
        capabilities: protocol::NdisConfigCapabilities::new(),
    };
    request(
        wire,
        10,
        protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG,
        &config,
        "the NDIS configuration",
        &mut payload,
    )?;
    say(format_args!(
        "NDIS configuration completed; NDIS version {}.{} sent",
        crate::NDIS_VERSION.0,
        crate::NDIS_VERSION.1
    ));
    let version = protocol::Message1SendNdisVersion {
        ndis_major_version: crate::NDIS_VERSION.0,
        ndis_minor_version: crate::NDIS_VERSION.1,
    };
    request(
        wire,
        11,
        protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION,
        &version,
        "the NDIS version",
        &mut payload,
    )?;
    say(format_args!("NDIS version completed"));
    Ok(())
}

/// Shares the caller's memory with the host as the receive buffer, a GPADL through the
/// channel, and takes the sections the host cut it into: one list of sections, each
/// long enough for the shortest frame's RNDIS message, all inside the buffer.
///
/// Source: `netvsp_protocol`'s `Message1SendReceiveBuffer` and
/// `Message1SendReceiveBufferComplete`, whose comment says no host has sent more than
/// one list; netvsp `initialize`, the `SendReceiveBuffer` arm.
pub(crate) fn receive_buffer<C: Channel, S: Shared>(
    wire: &mut Wire<C>,
    buffer: Buffer<S>,
    say: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<Receive<S>, NetvscError> {
    say(format_args!(
        "receive buffer: sharing {} bytes",
        buffer.memory.len()
    ));
    let gpadl_handle = wire.channel.share(buffer.id)?;
    say(format_args!(
        "receive buffer: GPADL {:#x} made; sent with ID {RECEIVE_BUFFER:#x}",
        gpadl_handle.0
    ));
    let memory = buffer.memory;
    let what = "the receive buffer";
    let mut payload = [0; PAYLOAD];
    let message = protocol::Message1SendReceiveBuffer {
        gpadl_handle,
        id: RECEIVE_BUFFER,
        reserved: 0,
    };
    let len = request(
        wire,
        20,
        protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER,
        &message,
        what,
        &mut payload,
    )?;
    let complete: protocol::Message1SendReceiveBufferComplete = body(
        &payload[..len],
        protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER_COMPLETE,
        what,
    )?;
    if complete.status != Status::SUCCESS {
        return Err(NetvscError::Refused {
            step: what,
            status: complete.status.0,
            nvsp: wire.version(),
        });
    }
    if complete.num_sections != 1 {
        return Err(NetvscError::Malformed(format!(
            "{} lists of receive sections, where a host sends one",
            complete.num_sections
        )));
    }
    let list = complete.sections[0];
    let start = list.offset as usize;
    let section = list.sub_allocation_size as usize;
    let count = list.num_sub_allocations as usize;
    let end = section
        .checked_mul(count)
        .and_then(|sections| sections.checked_add(start))
        .filter(|end| *end <= memory.len());
    match end {
        Some(end) if count > 0 && section >= rndis::DATA + ETHERNET_HEADER => {
            say(format_args!(
                "receive buffer accepted: {count} sections of {section} bytes from byte {start}"
            ));
            Ok(Receive {
                memory,
                start,
                end,
                section,
                count,
            })
        }
        _ => Err(NetvscError::Malformed(format!(
            "{count} receive sections of {section} bytes from byte {start}, in a buffer of {} bytes",
            memory.len()
        ))),
    }
}

/// Shares the caller's memory with the host as the send buffer, a GPADL through the
/// channel, and takes the length of a section, which the host chooses: at least one
/// section, each long enough for the control messages this driver sends.
///
/// Source: `netvsp_protocol`'s `Message1SendSendBuffer` and
/// `Message1SendSendBufferComplete`, whose comment gives the buffer's size to the
/// guest and the section's to the host; netvsp `initialize`, the `SendSendBuffer` arm.
pub(crate) fn send_buffer<C: Channel, S: Shared>(
    wire: &mut Wire<C>,
    buffer: Buffer<S>,
    say: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<Sections<S>, NetvscError> {
    say(format_args!(
        "send buffer: sharing {} bytes",
        buffer.memory.len()
    ));
    let gpadl_handle = wire.channel.share(buffer.id)?;
    say(format_args!(
        "send buffer: GPADL {:#x} made; sent with ID {SEND_BUFFER:#x}",
        gpadl_handle.0
    ));
    let memory = buffer.memory;
    let what = "the send buffer";
    let mut payload = [0; PAYLOAD];
    let message = protocol::Message1SendSendBuffer {
        gpadl_handle,
        id: SEND_BUFFER,
        reserved: 0,
    };
    let len = request(
        wire,
        21,
        protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER,
        &message,
        what,
        &mut payload,
    )?;
    let complete: protocol::Message1SendSendBufferComplete = body(
        &payload[..len],
        protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER_COMPLETE,
        what,
    )?;
    if complete.status != Status::SUCCESS {
        return Err(NetvscError::Refused {
            step: what,
            status: complete.status.0,
            nvsp: wire.version(),
        });
    }
    let size = complete.section_size as usize;
    let count = if size < rndis::CONTROL {
        0
    } else {
        u32::try_from(memory.len() / size).unwrap_or(0)
    };
    if count == 0 {
        return Err(NetvscError::Malformed(format!(
            "send sections of {size} bytes, in a buffer of {} bytes",
            memory.len()
        )));
    }
    say(format_args!(
        "send buffer accepted: {count} sections of {size} bytes"
    ));
    Ok(Sections::new(memory, size, count))
}
