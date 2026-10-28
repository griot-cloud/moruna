//! The device manager's protocols (SDD 25, d.6 and d.7).
//!
//! `moruna.devices.directory` is how a client finds a driver: by the name its service
//! was published under (`block0`), never by the device. A lookup answers with a fresh
//! connection to the driver, whose handshake is the client's. `moruna.devices.control`
//! is for the processes the boot description links to it, and no other: it restarts a
//! driver, and hands out the resources a driver of a device holds (without its DMA
//! right) and the device's physical windows, for tests of a driver's confinement.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};

use moruna_svc::{Channel, Handle};

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
        minor = 0;
        requests {
            /// Ends the driver of `name` and starts it again; answered with its new
            /// start count once it runs.
            1 => fn restart(name: String) -> u32;
            /// The resources the driver of `name` holds, without its DMA right: the
            /// configuration window, then each memory BAR's window, then its MSI slots.
            2 => fn sandbox(name: String) -> Vec<Handle>;
            /// The device's physical windows.
            3 => fn windows(name: String) -> Windows;
        }
        events {}
        errors {
            /// No service is published under the name.
            1 => NotFound;
            /// The driver could not be restarted, or a resource not duplicated.
            2 => Failed;
        }
    }
}
