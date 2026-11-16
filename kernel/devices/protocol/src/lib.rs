//! The device manager's protocols (SDD 25, d.6 and d.7).
//!
//! `moruna.devices.directory` is how a client finds a driver: by the name its service
//! was published under (`block0`), never by the device. A lookup answers with a fresh
//! connection to the driver, whose handshake is the client's. `moruna.devices.control`
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

use moruna_svc::{CallError, Channel, Handle};

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

/// One published service: its name, the device's `vendor << 16 | device`, the driver
/// program, and how many times the driver has been started.
pub type Entry = (String, u32, String, u32);

/// A device's windows: its configuration space's physical address, and each memory
/// BAR's index, physical address and length.
pub type Windows = (u64, Vec<(u32, u64, u64)>);

moruna_svc::protocol! {
    /// Finds a driver's service by name.
    pub protocol directory {
        name = "moruna.devices.directory";
        major = 1;
        minor = 0;
        requests {
            /// A new connection to the service published as `name`; answered once its
            /// driver is running.
            1 => fn lookup(name: String) -> Channel;
            /// Every published service.
            2 => fn list() -> Vec<Entry>;
        }
        events {}
        errors {
            /// No service is published under the name.
            1 => NotFound;
            /// The service's driver could not be started.
            2 => Unavailable;
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
}
