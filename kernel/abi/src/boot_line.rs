//! The node's console lines (`architecture/moruna-os-system.md`, section 1B).
//!
//! A node says how far it got with one line a stage, and a node that fails says why with
//! one line, `moruna: fatal <stage>: <reason>`, before it powers off. The core prints its
//! lines and each service prints its own, so the text is written once, here.

use core::fmt::{self, Display, Write};

/// The stage of the core: its line is [`Core`], and its failures are `fatal core`.
pub const CORE: &str = "core";
/// The stage of the node's services, which the supervisor starts.
pub const SERVICES: &str = "services";

/// The core's line, printed once every CPU is online:
/// `moruna: core <version> cpus=<n> memory=<bytes>`.
pub struct Core<'a> {
    /// The `moruna-microkernel` crate's version.
    pub version: &'a str,
    /// The CPUs online.
    pub cpus: usize,
    /// The usable RAM in the boot memory map, in bytes.
    pub memory: u64,
}

impl Display for Core<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "moruna: core {} cpus={} memory={}",
            self.version, self.cpus, self.memory
        )
    }
}

/// The hypervisor the core found (`architecture/moruna-os-system.md`, section 1F; work item
/// 1.8): `moruna: hypervisor <name>`, [`HYPERV`] or [`KVM`]. The core says nothing on a
/// machine with no hypervisor.
pub struct Hypervisor<'a>(pub &'a str);

/// Microsoft's hypervisor, found by its interface signature `Hv#1`.
pub const HYPERV: &str = "hyperv";
/// KVM, found by its signature `KVMKVMKVM`.
pub const KVM: &str = "kvm";

impl Display for Hypervisor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "moruna: hypervisor {}", self.0)
    }
}

/// What `devices` says when `root.ecam` covers nothing, as on a machine whose firmware
/// describes no PCI configuration space (section 1F, "No PCI bus"). It then serves with no
/// driver.
pub const NO_PCI_BUS: &str = "devices: no PCI bus on this machine";

/// A failure's line: `moruna: fatal <stage>: <reason>`.
///
/// It is always one line: a line break in the reason is written as a space.
pub struct Fatal<S, R> {
    pub stage: S,
    pub reason: R,
}

impl<S: Display, R: Display> Display for Fatal<S, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "moruna: fatal {}: ", self.stage)?;
        write!(OneLine(f), "{}", self.reason)
    }
}

/// The reason of a panic, for [`Fatal`]: `panic at <file>:<line>: <message>`, or
/// `panic: <message>` when the panic carries no location.
pub struct Panic<'a, M> {
    /// The file and line the panic names.
    pub location: Option<(&'a str, u32)>,
    pub message: M,
}

impl<M: Display> Display for Panic<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.location {
            Some((file, line)) => write!(f, "panic at {file}:{line}: {}", self.message),
            None => write!(f, "panic: {}", self.message),
        }
    }
}

/// Writes text with every line break as a space.
struct OneLine<'a, 'b>(&'a mut fmt::Formatter<'b>);

impl Write for OneLine<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut pieces = text.split(['\n', '\r']);
        if let Some(first) = pieces.next() {
            self.0.write_str(first)?;
        }
        for piece in pieces {
            self.0.write_char(' ')?;
            self.0.write_str(piece)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    #[test]
    fn the_cores_line_is_exactly_the_system_designs() {
        let line = Core {
            version: "0.1.0",
            cpus: 4,
            memory: 8_589_934_592,
        };
        assert_eq!(
            format!("{line}"),
            "moruna: core 0.1.0 cpus=4 memory=8589934592"
        );
        let line = Core {
            version: "0.7.0",
            cpus: 1,
            memory: 0,
        };
        assert_eq!(format!("{line}"), "moruna: core 0.7.0 cpus=1 memory=0");
    }

    #[test]
    fn the_hypervisor_line_names_the_hypervisor_found() {
        assert_eq!(
            format!("{}", Hypervisor(HYPERV)),
            "moruna: hypervisor hyperv"
        );
        assert_eq!(format!("{}", Hypervisor(KVM)), "moruna: hypervisor kvm");
    }

    #[test]
    fn devices_says_so_when_there_is_no_pci_bus() {
        assert_eq!(NO_PCI_BUS, "devices: no PCI bus on this machine");
        assert!(!NO_PCI_BUS.contains('\n'));
    }

    #[test]
    fn a_fatal_line_names_its_stage_and_its_reason() {
        let line = Fatal {
            stage: CORE,
            reason: "the bootloader passed no initramfs",
        };
        assert_eq!(
            format!("{line}"),
            "moruna: fatal core: the bootloader passed no initramfs"
        );
        let line = Fatal {
            stage: format_args!("service {}", "devices"),
            reason: "ended",
        };
        assert_eq!(format!("{line}"), "moruna: fatal service devices: ended");
        let line = Fatal {
            stage: SERVICES,
            reason: format_args!("{} did not start within 5 s", "api"),
        };
        assert_eq!(
            format!("{line}"),
            "moruna: fatal services: api did not start within 5 s"
        );
    }

    #[test]
    fn a_fatal_line_is_one_line_whatever_its_reason_holds() {
        let line = Fatal {
            stage: CORE,
            reason: "assertion failed\n  left: 1\r\n right: 2\n",
        };
        assert_eq!(
            format!("{line}"),
            "moruna: fatal core: assertion failed   left: 1   right: 2 "
        );
        // A reason written in pieces, each with its own breaks.
        let line = Fatal {
            stage: CORE,
            reason: format_args!("{}{}{}", "a\n", "b", "\nc"),
        };
        assert_eq!(format!("{line}"), "moruna: fatal core: a b c");
        let line = Fatal {
            stage: SERVICES,
            reason: "",
        };
        assert_eq!(format!("{line}"), "moruna: fatal services: ");
    }

    #[test]
    fn a_panic_is_a_fatal_error_of_the_core_on_one_line() {
        let line = Fatal {
            stage: CORE,
            reason: Panic {
                location: Some(("kernel/core/src/vm.rs", 269)),
                message: "index out of bounds:\nthe len is 3",
            },
        };
        assert_eq!(
            format!("{line}"),
            "moruna: fatal core: panic at kernel/core/src/vm.rs:269: index out of bounds: the len is 3"
        );
        let line = Fatal {
            stage: CORE,
            reason: Panic {
                location: None,
                message: format_args!("no {}", "location"),
            },
        };
        assert_eq!(format!("{line}"), "moruna: fatal core: panic: no location");
    }
}
