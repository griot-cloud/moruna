// SPDX-License-Identifier: MPL-2.0

//! VFS filesystem-type registration for ext2.
//!
//! `Ext2Type` implements the `FsType` trait so the VFS layer can
//! discover and mount ext2 volumes by name (`"ext2"`).

use aster_systree::SysNode;

use super::prelude::*;
use crate::fs::vfs::registry::{FsProperties, FsType};

/// VFS-visible Ext2 filesystem type.
pub(in crate::fs) struct Ext2Type;

/// The VFS filesystem type descriptor for Ext2.
pub(in crate::fs) static EXT2_TYPE: Ext2Type = Ext2Type;

impl FsType for Ext2Type {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn properties(&self) -> FsProperties {
        FsProperties::NEED_DISK
    }

    fn sysnode(&self) -> Option<Arc<dyn SysNode>> {
        None
    }
}
