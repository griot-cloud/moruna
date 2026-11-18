//! The device manager's protocols (SDD 25, d.6 and d.7).
//!
//! `moruna.devices.directory` is how a client finds a driver: by the name its service
//! was published under (`block0`), never by the device. A lookup answers with a fresh
//! connection to the driver, whose handshake is the client's. A driver the node's
//! supervisor starts rather than the device manager, such as `storvsc` on Azure, registers
//! each block device it serves ([`Block`]) through the listener the boot description links
//! to it, whose connections may register and do nothing else ([`REGISTER_SLOTS`]); the
//! device is then found as any other, by its serial (`lun-0`, `boot`), so there is one way
//! to find a disk whatever drives it (`architecture/moruna-os-system.md`, section 3C).
//! `moruna.devices.control`
//! is for the processes the boot description links to it, and no other: it restarts a
//! driver, and hands out the resources a driver of a device holds (without its DMA
//! right) and the device's physical windows, for tests of a driver's confinement. Through
//! the one listener the boot description links to the engine's supervisor
//! ([`HAND_OVER_SLOT`]), and only there, it hands a data disk (a table line with no driver)
//! to the supervisor, once, so the engine drives it itself (`architecture/data-os.md` §3).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};

use alloc::format;

use moruna_svc::{CallError, Channel, Decoder, Encoder, Handle, Wire, WireError};

/// The device manager's listener slot whose connections alone may call `hand_over`: the
/// boot description links it to the engine's supervisor and to nothing else.
pub const HAND_OVER_SLOT: &str = "control.engine";

/// The names `hand_over` gives the handles it hands over, in order: the configuration
/// window, the MSI slots, the DMA resource, then each memory BAR by index.
pub fn hand_over_names(bars: &[usize]) -> String {
    let mut names = String::from("config msi dma");
    for b in bars {
        names.push_str(&format!(" bar{b}"));
    }
    names
}

/// The prefix of the device manager's listener slots whose connections may `register` a
/// block device and do nothing else: the boot description links one to each driver the
/// node's supervisor starts (`register-storvsc`), and to nothing else. Connections through
/// a `directory*` slot may look up and list, and not register.
pub const REGISTER_SLOTS: &str = "register";

/// The longest serial a block device is registered under.
pub const SERIAL_MAX: usize = 64;

/// One published service: its name, the device's `vendor << 16 | device`, the driver
/// program, and how many times the driver has been started.
pub type Entry = (String, u32, String, u32);

/// A block device a driver registers: the serial it is found by, its size, and how it may
/// be used. Its driver serves `moruna.driver.block` for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The name a lookup finds it by: the serial the cloud gives the disk (`lun-<n>` on
    /// Azure), or `boot` for the disk the node booted from.
    pub serial: String,
    /// Bytes in a sector: 512 or a larger power of two.
    pub sector_size: u32,
    /// The device's capacity, in sectors.
    pub sectors: u64,
    /// Its driver refuses every write.
    pub read_only: bool,
    /// The disk the node booted from, which is never a job's volume.
    pub boot: bool,
}

impl Block {
    /// Whether the device is one the directory takes: a serial of 1 to [`SERIAL_MAX`]
    /// visible ASCII characters, sectors of 512 bytes or a larger power of two, and at
    /// least one of them.
    pub fn valid(&self) -> bool {
        (1..=SERIAL_MAX).contains(&self.serial.len())
            && self.serial.bytes().all(|b| b.is_ascii_graphic())
            && self.sector_size >= 512
            && self.sector_size.is_power_of_two()
            && self.sectors > 0
    }
}

impl Wire for Block {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        (self.serial, self.sector_size, self.sectors).encode(e)?;
        (self.read_only, self.boot).encode(e)
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let (serial, sector_size, sectors) = Wire::decode(d)?;
        let (read_only, boot) = Wire::decode(d)?;
        Ok(Block {
            serial,
            sector_size,
            sectors,
            read_only,
            boot,
        })
    }
}

/// A device's windows: its configuration space's physical address, and each memory
/// BAR's index, physical address and length.
pub type Windows = (u64, Vec<(u32, u64, u64)>);

moruna_svc::protocol! {
    /// Finds a driver's service by name.
    pub protocol directory {
        name = "moruna.devices.directory";
        major = 1;
        minor = 1;
        requests {
            /// A new connection to the service published as `name`, or to the block
            /// device registered under that serial; answered once its driver is running.
            1 => fn lookup(name: String) -> Channel;
            /// Every service the device manager's drivers publish.
            2 => fn list() -> Vec<Entry>;
            /// Minor 1: registers `block`, which the caller serves on `listener`: a lookup
            /// of its serial connects through `listener`. Only through a
            /// [`REGISTER_SLOTS`] listener, which may make no other request.
            3 => fn register(block: Block, listener: Channel) -> ();
        }
        events {}
        errors {
            /// No service is published under the name.
            1 => NotFound;
            /// The service's driver could not be started, or has gone.
            2 => Unavailable;
            /// Minor 1: a request this connection's listener does not allow.
            3 => Refused;
            /// Minor 1: the serial is a name published already.
            4 => Exists;
            /// Minor 1: a block device that is not [`Block::valid`].
            5 => Invalid;
        }
    }
}

moruna_svc::protocol! {
    /// Administers drivers. Reached only by the processes linked to it.
    pub protocol control {
        name = "moruna.devices.control";
        major = 1;
        minor = 1;
        requests {
            /// Ends the driver of `name` and starts it again; answered with its new
            /// start count once it runs.
            1 => fn restart(name: String) -> u32;
            /// The resources the driver of `name` holds, without its DMA right: the
            /// configuration window, then each memory BAR's window, then its MSI slots.
            2 => fn sandbox(name: String) -> Vec<Handle>;
            /// The device's physical windows.
            3 => fn windows(name: String) -> Windows;
            /// Minor 1: every resource of the device `name`, whose table line starts no
            /// driver, DMA included, and their names ([`hand_over_names`]). Once per device,
            /// and only through [`HAND_OVER_SLOT`].
            4 => fn hand_over(name: String) -> (String, Vec<Handle>);
        }
        events {}
        errors {
            /// No service is published under the name.
            1 => NotFound;
            /// The driver could not be restarted, or a resource not duplicated.
            2 => Failed;
            /// Minor 1: a hand-over this connection may not have: not through the
            /// supervisor's listener, a device with a driver, or one already handed over.
            3 => Refused;
        }
    }
}

/// A directory client through `listener`, an endpoint the boot description linked to
/// one of the device manager's `directory*` slots: a connection is opened through it
/// (SDD 23 f.4), and the handshake made on that, until `deadline`.
pub fn open_directory(
    listener: Channel,
    deadline: u64,
) -> Result<directory::Client, CallError<directory::Error>> {
    let channel = moruna_svc::connect(&listener).map_err(CallError::typed)?;
    directory::Client::connect(channel, deadline)
}

/// A control client through `listener`, linked to one of the `control*` slots.
pub fn open_control(
    listener: Channel,
    deadline: u64,
) -> Result<control::Client, CallError<control::Error>> {
    let channel = moruna_svc::connect(&listener).map_err(CallError::typed)?;
    control::Client::connect(channel, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_over_names_its_handles_in_order() {
        assert_eq!(hand_over_names(&[]), "config msi dma");
        assert_eq!(hand_over_names(&[4, 1]), "config msi dma bar4 bar1");
        assert_eq!(control::REQUESTS, &[1, 2, 3, 4]);
    }

    fn disk(serial: &str) -> Block {
        Block {
            serial: String::from(serial),
            sector_size: 512,
            sectors: 2048,
            read_only: false,
            boot: false,
        }
    }

    #[test]
    fn a_block_device_travels_whole() {
        assert_eq!(directory::REQUESTS, &[1, 2, 3]);
        assert_eq!((directory::ID.major, directory::ID.minor), (1, 1));
        let boot = Block {
            read_only: true,
            boot: true,
            sector_size: 4096,
            ..disk("boot")
        };
        let mut e = Encoder::new();
        boot.clone().encode(&mut e).unwrap();
        let bytes = e.into_control_bytes().unwrap();
        let mut d = Decoder::new(&bytes, &mut []);
        assert_eq!(Block::decode(&mut d).unwrap(), boot);
        d.finish().unwrap();
        let mut d = Decoder::new(&bytes[..bytes.len() - 1], &mut []);
        assert!(Block::decode(&mut d).is_err(), "a byte short");
    }

    #[test]
    fn a_block_device_has_a_serial_a_sector_size_and_sectors() {
        assert!(disk("lun-0").valid());
        assert!(disk(&"x".repeat(SERIAL_MAX)).valid());
        assert!(!disk("").valid());
        assert!(!disk(&"x".repeat(SERIAL_MAX + 1)).valid());
        assert!(!disk("lun 0").valid(), "a space");
        assert!(!disk("lün-0").valid(), "not ASCII");
        assert!(
            !Block {
                sector_size: 256,
                ..disk("a")
            }
            .valid()
        );
        assert!(
            !Block {
                sector_size: 1000,
                ..disk("a")
            }
            .valid()
        );
        assert!(
            !Block {
                sectors: 0,
                ..disk("a")
            }
            .valid()
        );
    }
}
