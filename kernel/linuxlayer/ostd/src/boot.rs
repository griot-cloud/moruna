// SPDX-License-Identifier: MPL-2.0

//! What the personality starts with, in place of a bootloader's information.

use alloc::{string::String, vec::Vec};

use spin::Once;

use crate::log::LevelFilter;

/// What the personality's start gives it.
#[derive(Debug, Default)]
pub struct BootInfo {
    /// Who started the personality.
    pub bootloader_name: String,
    /// The command line, as a kernel's.
    pub kernel_cmdline: String,
    /// An initial ramfs, if the start gives one.
    pub initramfs: Option<&'static [u8]>,
    /// The memory regions: the personality's own budget, as one usable region.
    pub memory_regions: Vec<memory_region::MemoryRegion>,
    /// The wall-clock time at start, since the Unix epoch.
    pub wall_clock_at_start: core::time::Duration,
    /// The monotonic clock at the same moment, in nanoseconds.
    pub monotonic_at_start: u64,
}

static INFO: Once<BootInfo> = Once::new();

/// Records what the personality starts with; once.
pub fn init(info: BootInfo) {
    INFO.call_once(|| info);
}

/// What the personality started with.
///
/// # Panics
///
/// Before [`init`].
pub fn boot_info() -> &'static BootInfo {
    INFO.get()
        .expect("the personality's start has not set its boot information")
}

/// What the personality started with, if its start has said.
pub fn try_boot_info() -> Option<&'static BootInfo> {
    INFO.get()
}

/// What the early command line says.
#[derive(Clone, Copy, Debug)]
pub struct EarlyCmdline {
    /// The log level.
    pub log_level: LevelFilter,
    /// Whether there is an early console.
    pub has_early_console: bool,
}

pub mod memory_region {
    //! Memory regions.

    /// What a region holds.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum MemoryRegionType {
        /// Memory the personality may use.
        Usable,
        /// Anything else.
        Reserved,
    }

    /// A memory region.
    #[derive(Clone, Copy, Debug)]
    pub struct MemoryRegion {
        base: usize,
        len: usize,
        typ: MemoryRegionType,
    }

    impl MemoryRegion {
        /// Makes a region.
        pub const fn new(base: usize, len: usize, typ: MemoryRegionType) -> Self {
            Self { base, len, typ }
        }

        /// Its first address.
        pub fn base(&self) -> usize {
            self.base
        }

        /// Its length.
        pub fn len(&self) -> usize {
            self.len
        }

        /// Whether it is empty.
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }

        /// What it holds.
        pub fn typ(&self) -> MemoryRegionType {
            self.typ
        }
    }
}
