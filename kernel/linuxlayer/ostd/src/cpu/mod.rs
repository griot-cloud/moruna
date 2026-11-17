// SPDX-License-Identifier: MPL-2.0

//! CPU-related definitions.

mod id;
pub mod local;

pub use id::{AtomicCpuSet, CpuId, CpuIdFromIntError, CpuSet, PinCurrentCpu, all_cpus, num_cpus};

/// The CPU privilege level: user mode or kernel mode.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PrivilegeLevel {
    /// User mode.
    User = 0,
    /// Kernel mode.
    Kernel = 1,
}

pub use crate::arch::cpu::context::UserContext;
