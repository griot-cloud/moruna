//! The `storvsc` process (`architecture/moruna-os-system.md`, section 1F; work item 1.10b):
//! Azure's disks, served as `moruna.driver.block`.
//!
//! The supervisor starts it on Hyper-V only, as a client of `vmbus` (a service whose server
//! is not on a machine is not on it either). It holds its own DMA grant, a copy of
//! `root.dma`: with no IOMMU, a process that writes a VMBus ring can already name any guest
//! page to the host, so the grant adds no trust (1F).
//!
//! 1. Through `vmbus`'s `scsi` listener it lists the SCSI controllers' offers and opens
//!    each controller's channel.
//! 2. On each it runs storvsc's initialisation, then finds the disks of the controller's
//!    target ([`scan`]), and names each as Azure does ([`naming`]): `boot` (read-only),
//!    `resource`, `lun-<n>`. It says each disk on the console, with its controller, target
//!    and LUN, then that the scan is done. A disk with no name is not served; if no disk is
//!    the OS disk, every disk is served read-only, and it says why ([`plan`]).
//! 3. It serves each disk as `moruna.driver.block` on a listener of its own ([`serve`]),
//!    and registers it with the device manager under its serial, through the link that
//!    carries only that right, so a client finds it as any other disk. A lookup of a disk
//!    not yet registered waits until storvsc has finished: it connects through that link
//!    before the scan, and closes the connection once the last disk is registered.
//!
//! Data moves by the guest page numbers of the client's memory, named in each request, with
//! no copy. Everything it needs of the node is behind [`serve::Node`] and [`scan::Scratch`],
//! so this library is tested on a host against `moruna-storvsc`'s fake storvsp; `main.rs`
//! is the process.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod naming;
pub mod scan;
pub mod serve;

use alloc::{format, string::String, vec::Vec};

use moruna_storvsc::Disk;
use vmbus_protocol::Guid;

pub use serve::{Driver, Node, READ_TO, Served, WRITE_FROM, factory};

/// The pages of each channel's rings: a control page and 32 of data a ring (128 KiB), the
/// outgoing ring first, as Linux's storvsc sizes them.
pub const RING_PAGES: u32 = 66;

/// The outgoing ring's pages among [`RING_PAGES`].
pub const OUTGOING_PAGES: u32 = 33;

/// What the scans come to: the disks served, and the console's lines about all of them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The disks served, in the order they were found.
    pub served: Vec<Served>,
    /// One line each: notes, disks, the read-only fallback's reason, and the scan's end.
    pub lines: Vec<String>,
}

/// One controller's scan: its instance and what was found on it.
pub struct Controller<'a> {
    /// The controller's instance ID, which names its disks.
    pub instance: Guid,
    /// What the scan found.
    pub scan: &'a scan::Scan,
}

fn describe(instance: Guid, disk: &Disk) -> String {
    let address = disk.address();
    format!(
        "storvsc: controller {instance} target {} lun {}: {} blocks of {} bytes",
        address.target,
        address.lun,
        disk.blocks(),
        disk.block_size()
    )
}

/// Names every disk the scans found and says so: a disk Azure gives no name is not served,
/// and when no disk is the OS disk, every disk is served read-only rather than one be
/// guessed at (the PM's fail-safe for 1.10b).
pub fn plan(controllers: &[Controller<'_>]) -> Plan {
    let mut plan = Plan::default();
    let mut found = 0;
    for (index, controller) in controllers.iter().enumerate() {
        for note in &controller.scan.notes {
            plan.lines.push(format!(
                "storvsc: controller {}: {note}",
                controller.instance
            ));
        }
        for disk in &controller.scan.disks {
            found += 1;
            match naming::name(controller.instance, disk.address().lun) {
                Some(name) => plan.served.push(Served {
                    controller: index,
                    disk: *disk,
                    name,
                }),
                None => plan.lines.push(format!(
                    "{}, no Azure name: not served",
                    describe(controller.instance, disk)
                )),
            }
        }
    }
    let boot = plan.served.iter().any(|served| served.name.boot);
    if !boot && !plan.served.is_empty() {
        plan.lines.push(String::from(
            "storvsc: no disk is Azure's OS disk (LUN 0 of the root controller f8b3781a-1e82-4818-a1c3-63d806ec15bb, or a disk of a controller whose instance starts 00000000-0000-): every disk is served read-only",
        ));
        for served in &mut plan.served {
            served.name.read_only = true;
        }
    }
    for served in &plan.served {
        let instance = controllers[served.controller].instance;
        let name = &served.name;
        plan.lines.push(format!(
            "{}, {}{}{}",
            describe(instance, &served.disk),
            name.serial,
            if name.read_only { ", read-only" } else { "" },
            if name.boot { ", the boot disk" } else { "" },
        ));
    }
    plan.lines.push(format!(
        "storvsc: scan done: {found} disks on {} SCSI controllers, {} served",
        controllers.len(),
        plan.served.len()
    ));
    plan
}
