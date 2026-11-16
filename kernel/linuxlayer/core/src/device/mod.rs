// SPDX-License-Identifier: MPL-2.0

mod mem;
pub(crate) mod misc;
mod model;
pub mod registry;
// Moruna: no terminals (data-os.md section 3a): the tty layer went with job control.

use device_id::DeviceId;
pub(crate) use mem::{getrandom, geturandom};
pub(crate) use registry::lookup;

use crate::{
    fs::{devtmpfs::DevtmpfsNodeMeta, file::PerOpenFileOps},
    prelude::*,
};

/// The abstraction of a device.
pub trait Device: Send + Sync + 'static {
    /// Returns the device type.
    fn type_(&self) -> DeviceType;

    /// Returns the device ID.
    fn id(&self) -> DeviceId;

    /// Returns the metadata that specifies a device inode to be created in devtmpfs, if any.
    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta>;

    /// Opens the device, returning a file-like object that the userspace can interact with by
    /// doing I/O.
    fn open(&self) -> Result<Box<dyn PerOpenFileOps>>;
}

impl Debug for dyn Device {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("type", &self.type_())
            .field("id", &self.id())
            .field("devtmpfs_meta", &self.devtmpfs_meta())
            .finish_non_exhaustive()
    }
}

/// Device type
#[derive(Debug)]
pub enum DeviceType {
    Char,
    Block,
}

pub(crate) fn init_in_first_kthread() {
    model::init_in_first_kthread();
    registry::init_in_first_kthread();
    mem::init_in_first_kthread();
    misc::init_in_first_kthread();
}

/// Initializes device state after mounting rootfs.
pub(crate) fn init_in_first_process() -> Result<()> {
    registry::init_in_first_process()?;

    Ok(())
}
