// SPDX-License-Identifier: MPL-2.0

//! CPU context & state control and CPU local memory.

pub mod context;
pub mod cpuid;
pub mod extension;
// moruna: the hypervisor and the clock rates it gives, decided where the host tests reach
// (work item 1.8).
pub(in crate::arch) mod hypervisor;
pub mod local;
