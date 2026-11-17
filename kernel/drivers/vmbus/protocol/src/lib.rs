//! `moruna.driver.vmbus` 1.0: what the `vmbus` process serves VMBus drivers
//! (`architecture/moruna-os-system.md`, section 1F, "VMBus, split by the microkernel
//! rule"; work item 1.9c).
//!
//! The `vmbus` process holds the connection to the host, and a driver asks it for four
//! things:
//!
//! - **offers:** the channels the host offers of one interface ID, a kind of device;
//! - **open:** a channel opened on rings `vmbus` makes, which gives the driver the
//!   rings' memory, a signal handle that signals that channel's connection ID and nothing
//!   else, and a wake channel;
//! - **share:** more of the driver's memory shared with the host, as a GPADL whose ID the
//!   driver names to its device;
//! - **close:** the channel closed, and its rings the driver's alone again.
//!
//! Packets travel in the rings with no hop through `vmbus`: a driver writes its outgoing
//! ring and signals the host itself. The way in has one hop: the host's interrupt reaches
//! `vmbus`, which reads the event flags and writes an empty message to the wake channel of
//! each channel the host signalled. A driver waits only on handles: its wake channel turns
//! readable among its clients' channels. A write that finds the wake channel full is
//! dropped, since a message is pending already; the driver reads them all, then looks at
//! its rings. `vmbus` closes the wake channel once the channel is closed or rescinded, so
//! its peer-closed signal says the device is gone.
//!
//! A driver reaches `vmbus` through the listener of its kind of device ([`Class`]): `scsi`
//! for disks, `net` for network cards. A connection lists and opens the offers of its
//! class's interface and no other's, so a disk driver cannot open the network card.
//!
//! A channel belongs to the connection that opened it: only it may share memory on it or
//! close it, and `vmbus` closes it when the connection goes.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

pub use guid::Guid;
use moruna_svc::{Decoder, Encoder, Handle, Wire, WireError};

/// An interface or instance ID on the wire: the GUID's 16 bytes in the mixed-endian
/// order VMBus carries them in, as `guid::Guid::from_slice` reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub Guid);

impl Id {
    /// The 16 bytes as they travel.
    pub fn to_bytes(self) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[0..4].copy_from_slice(&self.0.data1.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.0.data2.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.0.data3.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.0.data4);
        bytes
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Wire for Id {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_raw(&self.to_bytes());
        Ok(())
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let bytes: &[u8; 16] = d
            .take_raw(16)?
            .try_into()
            .map_err(|_| WireError::Truncated)?;
        Ok(Id(Guid::from_slice(bytes)))
    }
}

/// Hyper-V's synthetic SCSI controller, which carries a machine's disks (storvsc).
pub const SCSI: Guid = guid::guid!("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f");

/// Hyper-V's synthetic network card (netvsc).
pub const NET: Guid = guid::guid!("f8615163-df3e-46c5-913f-f2d2f965ed0e");

/// A kind of device, and the listener its drivers reach `vmbus` through: a connection on
/// it lists and opens the offers of the class's interface only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// SCSI controllers, which carry the disks (`storvsc`).
    Scsi,
    /// Network cards (`netvsc`).
    Net,
}

impl Class {
    /// Every class, in the order `vmbus` takes their listeners.
    pub const ALL: [Class; 2] = [Class::Scsi, Class::Net];

    /// The interface ID of the class's offers.
    pub fn interface(self) -> Id {
        match self {
            Class::Scsi => Id(SCSI),
            Class::Net => Id(NET),
        }
    }

    /// The start handle `vmbus` listens on for the class's drivers, which node config
    /// links to the driver.
    pub fn slot(self) -> &'static str {
        match self {
            Class::Scsi => "scsi",
            Class::Net => "net",
        }
    }
}

/// One channel the host offers of the interface asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The channel's ID, which `open`, `share` and `close` name.
    pub channel: u32,
    /// Which device of the interface it is.
    pub instance: Id,
    /// Zero for a device's first channel, and its number among the device's others.
    pub subchannel: u16,
}

impl Wire for Offer {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        (self.channel, self.instance, self.subchannel).encode(e)
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let (channel, instance, subchannel) = Wire::decode(d)?;
        Ok(Offer {
            channel,
            instance,
            subchannel,
        })
    }
}

/// What opening a channel gives its driver.
#[derive(Debug)]
pub struct Opened {
    /// The memory of the channel's two rings, as `open` asked: the outgoing ring's pages
    /// first, each ring a page of control words and then its data. Mapped by the driver,
    /// readable and writable.
    pub rings: Handle,
    /// The channel's signal handle (`ObjectType::HvSignal`): it signals the host on the
    /// channel's connection ID, and nothing else.
    pub signal: Handle,
    /// The channel's wake channel: `vmbus` writes it an empty message when the host
    /// signals the channel, and closes it once the channel is closed or rescinded.
    pub wake: Handle,
}

impl Wire for Opened {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        (self.rings, self.signal, self.wake).encode(e)
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let (rings, signal, wake) = Wire::decode(d)?;
        Ok(Opened {
            rings,
            signal,
            wake,
        })
    }
}

/// The fewest pages a ring takes: a page of control words and one of data.
pub const RING_PAGES_MIN: u32 = 2;

/// The most pages one GPADL holds, and so the rings of one channel: a GPADL's page
/// numbers travel in one message whose length is 16 bits (`moruna_vmbus::MAX_PAGES`).
pub const PAGES_MAX: u32 = (u16::MAX / 8 - 1) as u32;

/// Whether `pages` of rings, `outgoing` of them the outgoing ring's, are rings the host
/// takes: each ring at least [`RING_PAGES_MIN`], and all in one GPADL.
pub fn rings_fit(pages: u32, outgoing: u32) -> bool {
    outgoing >= RING_PAGES_MIN
        && pages
            .checked_sub(outgoing)
            .is_some_and(|incoming| incoming >= RING_PAGES_MIN)
        && pages <= PAGES_MAX
}

moruna_svc::protocol! {
    /// The `vmbus` process's service to VMBus drivers.
    pub protocol vmbus {
        name = "moruna.driver.vmbus";
        major = 1;
        minor = 0;
        requests {
            /// The channels the host offers of `interface`, once contact with the host is
            /// made. A channel the host has rescinded is not among them, and an interface
            /// that is not the connection's class has none.
            1 => fn offers(interface: Id) -> Vec<Offer>;
            /// Opens `channel` on new rings of `pages` pages, the first `outgoing` of them
            /// the ring the driver writes. Answered once the host has opened it.
            2 => fn open(channel: u32, pages: u32, outgoing: u32) -> Opened;
            /// Shares `memory` (DMA memory the driver made, which `vmbus` pins) with the
            /// host as a GPADL of `channel`, which must be open: the GPADL's ID, once the
            /// host has it.
            3 => fn share(channel: u32, memory: Handle) -> u32;
            /// Closes `channel`, and tears down the memory shared on it: answered once the
            /// host has let go of the rings.
            4 => fn close(channel: u32) -> ();
        }
        events {}
        errors {
            /// There is no contact with the host.
            1 => NotConnected;
            /// The host offers no such channel of the connection's class, or has
            /// rescinded it.
            2 => NoSuchChannel;
            /// The channel is opened already, or belongs to another connection.
            3 => Busy;
            /// The channel is not open, or is opening or closing.
            4 => NotOpen;
            /// The rings asked for are not rings the host takes (`rings_fit`).
            5 => BadRings;
            /// The memory is not DMA memory `vmbus` can pin, or is too large for a GPADL.
            6 => BadMemory;
            /// The host refused, with a status `vmbus` prints.
            7 => Refused;
            /// The node has no memory or handles left for the channel.
            8 => NoResources;
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloc::string::ToString;

    use moruna_svc::ProtocolError;

    use super::*;

    /// The body `value` encodes to, with no handle in it.
    fn body<T: Wire>(value: T) -> Vec<u8> {
        let mut e = Encoder::new();
        value.encode(&mut e).unwrap();
        e.into_control_bytes().unwrap()
    }

    fn round_trip<T: Wire>(value: T) -> (T, usize) {
        let bytes = body(value);
        let mut d = Decoder::new(&bytes, &mut []);
        let back = T::decode(&mut d).unwrap();
        d.finish().unwrap();
        (back, bytes.len())
    }

    #[test]
    fn the_protocol_is_moruna_driver_vmbus_1_0_with_four_requests() {
        assert_eq!(vmbus::ID.name, "moruna.driver.vmbus");
        assert_eq!((vmbus::ID.major, vmbus::ID.minor), (1, 0));
        assert_eq!(vmbus::REQUESTS, &[1, 2, 3, 4]);
        assert!(vmbus::EVENTS.is_empty());
        for (code, error) in [
            (1, vmbus::Error::NotConnected),
            (2, vmbus::Error::NoSuchChannel),
            (3, vmbus::Error::Busy),
            (4, vmbus::Error::NotOpen),
            (5, vmbus::Error::BadRings),
            (6, vmbus::Error::BadMemory),
            (7, vmbus::Error::Refused),
            (8, vmbus::Error::NoResources),
        ] {
            assert_eq!(error.code(), code);
            assert_eq!(vmbus::Error::from_code(code), Some(error));
        }
        assert_eq!(vmbus::Error::from_code(9), None);
    }

    #[test]
    fn an_id_travels_as_vmbus_carries_a_guid() {
        let id = Id(SCSI);
        assert_eq!(
            id.to_bytes(),
            [
                0xd9, 0x63, 0x61, 0xba, 0xa1, 0x04, 0x29, 0x4d, 0xb6, 0x05, 0x72, 0xe2, 0xff, 0xb1,
                0xdc, 0x7f
            ]
        );
        assert_eq!(Guid::from_slice(&id.to_bytes()), SCSI);
        assert_eq!(round_trip(id), (id, 16));
        assert_eq!(id.to_string(), "ba6163d9-04a1-4d29-b605-72e2ffb1dc7f");
    }

    #[test]
    fn an_offer_and_a_list_of_them_round_trip() {
        let offer = Offer {
            channel: 14,
            instance: Id(SCSI),
            subchannel: 3,
        };
        assert_eq!(round_trip(offer), (offer, 4 + 16 + 2));
        let offers = vec![
            offer,
            Offer {
                channel: 2047,
                ..offer
            },
        ];
        assert_eq!(round_trip(offers.clone()).0, offers);
        // A short one is refused, not read past.
        let bytes = body(offer);
        let mut d = Decoder::new(&bytes[..bytes.len() - 1], &mut []);
        assert!(Offer::decode(&mut d).is_err());
        let mut d = Decoder::new(&bytes[..15], &mut []);
        assert!(Id::decode(&mut d).is_err());
    }

    #[test]
    fn opened_carries_three_handles_in_order() {
        let opened = Opened {
            rings: Handle::inert(),
            signal: Handle::inert(),
            wake: Handle::inert(),
        };
        let mut e = Encoder::new();
        opened.encode(&mut e).unwrap();
        assert_eq!(e.handle_count(), 3);
        assert_eq!(e.body_len(), 3, "one index a handle");
        // The handles are taken out of a message in the order they were put in.
        let mut handles = [
            Some(Handle::inert()),
            Some(Handle::inert()),
            Some(Handle::inert()),
        ];
        let mut d = Decoder::new(&[0, 1, 2], &mut handles);
        let back = Opened::decode(&mut d).unwrap();
        d.finish().unwrap();
        assert!(back.rings.info().is_ok());
        let mut handles = [Some(Handle::inert()), Some(Handle::inert())];
        let mut d = Decoder::new(&[0, 1, 2], &mut handles);
        assert!(Opened::decode(&mut d).is_err(), "a handle short");
    }

    #[test]
    fn each_class_has_its_interface_and_its_listener() {
        assert_eq!(Class::ALL, [Class::Scsi, Class::Net]);
        assert_eq!(Class::Scsi.interface(), Id(SCSI));
        assert_eq!(Class::Net.interface(), Id(NET));
        assert_eq!((Class::Scsi.slot(), Class::Net.slot()), ("scsi", "net"));
        assert_eq!(NET.to_string(), "f8615163-df3e-46c5-913f-f2d2f965ed0e");
    }

    #[test]
    fn rings_fit_when_each_has_a_control_page_and_data_in_one_gpadl() {
        assert_eq!(PAGES_MAX, 8190);
        assert!(rings_fit(4, 2));
        assert!(rings_fit(PAGES_MAX, 2));
        assert!(rings_fit(PAGES_MAX, PAGES_MAX - 2));
        assert!(!rings_fit(3, 2), "an incoming ring of one page");
        assert!(!rings_fit(4, 1), "an outgoing ring of one page");
        assert!(!rings_fit(4, 3));
        assert!(!rings_fit(2, 4), "more outgoing than all");
        assert!(!rings_fit(PAGES_MAX + 1, 2));
        assert!(!rings_fit(0, 0));
    }
}
