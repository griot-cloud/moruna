//! What Azure calls a disk, from the SCSI controller it is on and its LUN, as Microsoft's own
//! udev rules name them: WALinuxAgent's `config/66-azure-storage.rules` and azure-vm-utils'
//! `udev/80-azure-disk.rules` (system design 1F, read from their source for item 1.10b).
//!
//! - The root controller, `{f8b3781a-1e82-4818-a1c3-63d806ec15bb}` (scsi0), holds the OS disk
//!   at LUN 0 and the resource disk at LUN 1; LUN 2 is the DVD drive, which is no disk.
//! - The data controllers, `{f8b3781b-…}`, `{f8b3781c-…}` and `{f8b3781d-…}` (scsi1 to
//!   scsi3), hold the data disks: LUN n is the disk attached at LUN n, `lun-<n>` (3C).
//! - A controller whose instance starts `00000000-0000-` holds the OS disk, and one that
//!   starts `00000000-0001-` a resource disk (Generation 1, and sizes such as LSv3).
//!
//! Any other disk has no name, and is not served.

use alloc::{format, string::String};

use vmbus_protocol::Guid;

/// The root controller (scsi0): the OS disk at LUN 0, the resource disk at LUN 1.
pub const ROOT: Guid = controller(0xf8b3_781a);

/// The data controllers (scsi1 to scsi3).
pub const DATA: [Guid; 3] = [
    controller(0xf8b3_781b),
    controller(0xf8b3_781c),
    controller(0xf8b3_781d),
];

/// The serial the OS disk is registered under.
pub const BOOT: &str = "boot";

/// The serial a resource disk is registered under.
pub const RESOURCE: &str = "resource";

/// Azure's SCSI controllers' instance IDs differ in their first word only.
const fn controller(first: u32) -> Guid {
    Guid {
        data1: first,
        data2: 0x1e82,
        data3: 0x4818,
        data4: [0xa1, 0xc3, 0x63, 0xd8, 0x06, 0xec, 0x15, 0xbb],
    }
}

/// How a disk is registered: the serial it is found by, and how it is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    /// `boot`, `resource` or `lun-<n>`.
    pub serial: String,
    /// Served with every write refused: the OS disk, which the node never writes (1A).
    pub read_only: bool,
    /// The disk the node booted from, never a job's volume (3C).
    pub boot: bool,
}

impl Name {
    fn boot() -> Name {
        Name {
            serial: String::from(BOOT),
            read_only: true,
            boot: true,
        }
    }

    fn resource() -> Name {
        Name {
            serial: String::from(RESOURCE),
            read_only: false,
            boot: false,
        }
    }
}

/// The name of the disk at `lun` of the controller `instance`, if Azure gives it one.
pub fn name(instance: Guid, lun: u8) -> Option<Name> {
    if instance == ROOT {
        return match lun {
            0 => Some(Name::boot()),
            1 => Some(Name::resource()),
            _ => None,
        };
    }
    if DATA.contains(&instance) {
        return Some(Name {
            serial: format!("lun-{lun}"),
            read_only: false,
            boot: false,
        });
    }
    match (instance.data1, instance.data2) {
        (0, 0) => Some(Name::boot()),
        (0, 1) => Some(Name::resource()),
        _ => None,
    }
}
