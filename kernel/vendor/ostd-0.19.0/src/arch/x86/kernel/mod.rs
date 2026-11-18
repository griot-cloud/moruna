// SPDX-License-Identifier: MPL-2.0

//! Architecture kernel module.
//
// TODO: The purpose of this module is too ambiguous. We should split it up and move its submodules
// to more suitable locations.

pub(super) mod acpi;
pub(super) mod apic;
// moruna: the hypervisor at boot, and the hypercall page on Hyper-V (work item 1.8); CPU 0's
// SynIC and the two hypercalls of VMBus, which the core's `hyperv_*` calls reach as
// `ostd::arch::kernel::hyperv::synic` (work item 1.9c).
pub mod hyperv;
pub(super) mod kvm_clock;
pub(super) mod tsc;

pub use acpi::{ACPI_INFO, AcpiInfo};
