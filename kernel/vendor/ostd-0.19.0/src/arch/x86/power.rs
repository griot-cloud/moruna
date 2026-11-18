// SPDX-License-Identifier: MPL-2.0

//! Power management.

mod qemu_isa_debug {
    //! The isa-debug-exit device in QEMU.
    //!
    //! Reference: <https://elixir.bootlin.com/qemu/v10.1.2/source/hw/misc/debugexit.c>

    use spin::Once;

    use crate::{
        arch::device::io_port::{PortWrite, WriteOnlyAccess},
        io::IoPort,
        power::ExitCode,
    };

    // For `qemu-system-x86_64`, the exit code will be `(code << 1) | 1`. So it is not possible to
    // let QEMU invoke `exit(0)`. We also need to check if the exit code is returned by the kernel,
    // so we cannot use `0` as `EXIT_SUCCESS` because it may conflict with QEMU's return value `1`,
    // which indicates that QEMU itself fails.
    const EXIT_SUCCESS: u32 = 0x10;
    const EXIT_FAILURE: u32 = 0x20;

    const DEBUG_EXIT_PORT_NUM: u16 = 0xF4;

    static DEBUG_EXIT_PORT: Once<IoPort<u32, WriteOnlyAccess>> = Once::new();

    pub(super) fn try_exit_qemu(code: ExitCode) {
        let value = match code {
            ExitCode::Success => EXIT_SUCCESS,
            ExitCode::Failure => EXIT_FAILURE,
        };

        // If possible, keep this method panic-free because it may be called by the panic handler.
        if let Some(port) = DEBUG_EXIT_PORT.get() {
            port.write(value);
        } else if crate::arch::cpu::cpuid::query_if_running_in_qemu() {
            // moruna: a power-off before `init` has run (a boot wait whose deadline passed,
            // Moruna's system design 1B) tries the device first as every other does, so
            // QEMU still exits with the kernel's failure code where the device exists.
            //
            // SAFETY: Under QEMU this port is the debug-exit device's or no device's, which
            // is what `init` assumes when it takes the port. A write to it ends QEMU or
            // does nothing; it cannot corrupt the kernel's memory.
            unsafe { PortWrite::write_to_port(DEBUG_EXIT_PORT_NUM, value) };
        }
    }

    pub(super) fn init() {
        let debug_exit_port = IoPort::acquire(DEBUG_EXIT_PORT_NUM).unwrap();

        DEBUG_EXIT_PORT.call_once(|| debug_exit_port);
    }
}

// moruna: ACPI S5, the power-off every machine has (Moruna's system design, section 1B).
// The QEMU device above stays first, for the exit codes CI reads; no cloud has it.
mod acpi_s5;
// moruna: the hold before a failed machine enters S5 (section 1B, work item 1.6c).
mod hold;

/// moruna: enters ACPI S5: SLP_TYPa from the DSDT's `\_S5` package, written with SLP_EN to
/// the FADT's PM1a control register. Returns if the firmware describes no such power-off,
/// or if the machine is still running some time after the write.
///
/// A machine that failed has printed its `moruna: fatal` line: it stays halted for
/// `hold::HOLD_SECONDS` before the write, so a cloud's console can still be read (section
/// 1B, work item 1.6c). QEMU's test exit device, tried before this, ends a CI run at once.
///
/// Keep this panic-free: the panic handler powers the machine off through it.
fn try_acpi_s5(code: crate::power::ExitCode) {
    use acpi::{address::AddressSpace, fadt::Fadt};

    use super::kernel::{acpi::get_acpi_tables, tsc::boot_wait};
    use crate::{arch::device::io_port::PortWrite, mm::paddr_to_vaddr};

    // The ACPI tables were read before the IRQ chip came up. Earlier than that, the panic
    // being handled may have come from reading them, and that read must not be entered a
    // second time.
    if super::irq::IRQ_CHIP.get().is_none() {
        return;
    }
    let Some(tables) = get_acpi_tables() else {
        return;
    };
    let Ok(fadt) = tables.find_table::<Fadt>() else {
        return;
    };
    let Ok(control) = fadt.pm1a_control_block() else {
        return;
    };
    if control.address_space != AddressSpace::SystemIo {
        return;
    }
    let Ok(port) = u16::try_from(control.address) else {
        return;
    };
    if port == 0 {
        return;
    }
    let Ok(dsdt) = tables.dsdt() else {
        return;
    };
    // SAFETY: `dsdt` is the DSDT's AML as the FADT locates it and its own header sizes it,
    // which the `acpi` crate has validated. Firmware tables are never written and the
    // linear mapping covers them, as it does for `AcpiMemoryHandler`, so the bytes are
    // valid for reads for as long as the kernel runs.
    let aml = unsafe {
        core::slice::from_raw_parts(
            paddr_to_vaddr(dsdt.address) as *const u8,
            dsdt.length as usize,
        )
    };
    let Some(slp_typa) = acpi_s5::slp_typa(aml) else {
        return;
    };
    hold::power_off(
        matches!(code, crate::power::ExitCode::Failure),
        || {
            let count = hold::count(super::tsc_freq());
            hold::wait(super::read_tsc, core::hint::spin_loop, count);
        },
        // SAFETY: The port is the PM1a control register the firmware's FADT names. Writing
        // SLP_TYPa with SLP_EN to it powers the machine off; it cannot corrupt the kernel's
        // memory.
        || unsafe { PortWrite::write_to_port(port, acpi_s5::pm1a_control(slp_typa)) },
    );
    // A hypervisor may run this CPU a little longer before it acts on the write. Wait for
    // it, within the boot waits' deadline, so that a power-off that worked is not reported
    // as one that failed.
    boot_wait::wait(super::read_tsc, core::hint::spin_loop, || false);
}

pub(super) fn init() {
    use super::cpu::cpuid;

    if !cpuid::query_if_running_in_qemu() {
        return;
    }

    // FIXME: We assume that the kernel is running in QEMU with the following QEMU command line
    // arguments that specify the isa-debug-exit device:
    // `-device isa-debug-exit,iobase=0xf4,iosize=0x04`.
    crate::info!("QEMU hypervisor detected, assuming that the isa-debug-exit device exists");

    qemu_isa_debug::init();
}

/// Attempts to power off the system using an architecture-specific mechanism.
///
/// On x86, this function attempts to power off the system through QEMU's ISA debug-exit device if
/// QEMU was detected, and then through ACPI S5 (moruna: Moruna's system design, section 1B). If
/// neither powers the machine off, it returns, and the caller halts every CPU.
pub fn try_poweroff(code: crate::power::ExitCode) {
    qemu_isa_debug::try_exit_qemu(code);
    // moruna: then ACPI S5, which every machine has, after the hold when it failed.
    try_acpi_s5(code);
}

/// Attempts to restart the system using an architecture-specific mechanism.
///
/// On x86, this function currently does nothing and returns.
pub fn try_restart(_code: crate::power::ExitCode) {
    // TODO: Add an OSTD-level restart mechanism for x86.
}
