//! Host tests of the arithmetic Moruna's patches add to the vendored OSTD (`NOTICE.md`).
//!
//! OSTD builds only for a kernel target, so nothing in it runs on the host. The parts of
//! Moruna's patches that are arithmetic are therefore written in files that name nothing
//! of OSTD, and this crate compiles those same files for its tests:
//!
//! - `src/arch/x86/power/acpi_s5.rs`: SLP_TYPa from a DSDT's `\_S5` package, and the value
//!   the PM1a control register takes (the system design, section 1B, power-off);
//! - `src/arch/x86/kernel/tsc/boot_wait.rs`: the deadline of the two calibration waits
//!   (section 1B, no wait without a deadline in boot code);
//! - `src/arch/x86/cpu/hypervisor.rs`: which hypervisor the machine runs on, where the
//!   TSC's and the APIC timer's rates come from, the guest OS ID and the hypercall MSR's
//!   value (section 1F, "The core on Hyper-V"; work item 1.8), over recorded CPUID and MSR
//!   answers;
//! - `src/arch/x86/timer/pit/presence.rs`: whether a PIT answers at its ports (section 1F,
//!   nothing from the old PC is assumed).
//!
//! The crate is its tests: built for anything else it is empty.

#![cfg_attr(not(test), no_std)]

#[cfg(test)]
#[path = "../../vendor/ostd-0.19.0/src/arch/x86/power/acpi_s5.rs"]
mod acpi_s5;
#[cfg(test)]
#[path = "../../vendor/ostd-0.19.0/src/arch/x86/kernel/tsc/boot_wait.rs"]
mod boot_wait;
#[cfg(test)]
#[path = "../../vendor/ostd-0.19.0/src/arch/x86/cpu/hypervisor.rs"]
mod hypervisor;
#[cfg(test)]
mod hypervisor_tests;
#[cfg(test)]
#[path = "../../vendor/ostd-0.19.0/src/arch/x86/timer/pit/presence.rs"]
mod pit_presence;

#[cfg(test)]
mod s5_tests {
    use super::acpi_s5::{pm1a_control, slp_typa};

    /// The length of an ACPI table's header, which the AML follows.
    const HEADER: usize = 36;

    /// A DSDT: the header every ACPI table has, then `aml`.
    fn dsdt(aml: &[u8]) -> Vec<u8> {
        let mut table = Vec::new();
        table.extend_from_slice(b"DSDT");
        table.extend_from_slice(&((HEADER + aml.len()) as u32).to_le_bytes());
        table.push(1); // revision
        table.push(0); // checksum, which the `acpi` crate checks before the scan
        table.extend_from_slice(b"BOCHS ");
        table.extend_from_slice(b"BXPC    ");
        table.extend_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(b"BXPC");
        table.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(table.len(), HEADER);
        table.extend_from_slice(aml);
        table
    }

    /// `Name (name, Package (elements) { body })`, with a one-byte PkgLength.
    fn name_package(name: &[u8], elements: u8, body: &[u8]) -> Vec<u8> {
        let length = 1 + 1 + body.len();
        assert!(length < 64);
        let mut aml = vec![0x08];
        aml.extend_from_slice(name);
        aml.extend_from_slice(&[0x12, length as u8, elements]);
        aml.extend_from_slice(body);
        aml
    }

    /// The sleep packages as QEMU's machines declare them: `Scope (\)` holding `_S3`
    /// `{1, 1, 0, 0}`, `_S4` `{2, 2, 0, 0}` and `_S5` `{0, 0, 0, 0}`, after a device that
    /// has nothing to do with them.
    fn qemu_aml() -> Vec<u8> {
        let mut sleep = Vec::new();
        sleep.extend(name_package(b"_S3_", 4, &[0x01, 0x01, 0x00, 0x00]));
        sleep.extend(name_package(
            b"_S4_",
            4,
            &[0x0a, 0x02, 0x0a, 0x02, 0x00, 0x00],
        ));
        sleep.extend(name_package(b"_S5_", 4, &[0x00, 0x00, 0x00, 0x00]));
        // Device (PCI0) { Name (_HID, EisaId ("PNP0A08")) }
        let mut aml = vec![
            0x5b, 0x82, 0x0f, b'P', b'C', b'I', b'0', 0x08, b'_', b'H', b'I', b'D', 0x0c, 0x41,
            0xd0, 0x0a, 0x08,
        ];
        // Scope (\) { the three packages }
        aml.push(0x10);
        aml.push((1 + 2 + sleep.len()) as u8);
        aml.extend_from_slice(&[b'\\', 0x00]);
        aml.extend(sleep);
        aml
    }

    #[test]
    fn qemus_table_gives_slp_typa_zero() {
        let table = dsdt(&qemu_aml());
        assert_eq!(slp_typa(&table[HEADER..]), Some(0));
        // SLP_EN alone: what QEMU's PM1a control register takes as a power-off.
        assert_eq!(pm1a_control(0), 0x2000);
    }

    #[test]
    fn a_table_that_declares_it_by_its_root_path_with_two_elements_is_read() {
        // Name (\_S5, Package (0x02) { Zero, Zero })
        let table = dsdt(&name_package(b"\\_S5_", 2, &[0x00, 0x00]));
        assert_eq!(slp_typa(&table[HEADER..]), Some(0));
    }

    #[test]
    fn a_table_of_real_hardware_gives_its_slp_typa() {
        // Name (_S5, Package (0x04) { 0x07, Zero, Zero, Zero }), as chipsets with a byte
        // prefix declare it.
        let table = dsdt(&name_package(b"_S5_", 4, &[0x0a, 0x07, 0x00, 0x00, 0x00]));
        assert_eq!(slp_typa(&table[HEADER..]), Some(7));
        assert_eq!(pm1a_control(7), 0x3c00);
        let table = dsdt(&name_package(
            b"_S5_",
            4,
            &[0x0a, 0x05, 0x0a, 0x05, 0x00, 0x00],
        ));
        assert_eq!(slp_typa(&table[HEADER..]), Some(5));
        assert_eq!(pm1a_control(5), 0x3400);
    }

    #[test]
    fn the_package_morunas_own_launcher_writes_is_read() {
        // `Name (_S5, Package (0x02) { 0x05, Zero })`, as `moruna-vmm`'s DSDT declares it
        // for its guests (crates/moruna-vmm/src/acpi.rs, through the `acpi_tables` crate).
        let table = dsdt(&name_package(b"_S5_", 2, &[0x0a, 0x05, 0x00]));
        assert_eq!(slp_typa(&table[HEADER..]), Some(5));
    }

    #[test]
    fn every_integer_encoding_of_the_first_element_is_read() {
        for (body, value) in [
            (&[0x00][..], 0),
            (&[0x01][..], 1),
            (&[0x0a, 0x06][..], 6),
            (&[0x0b, 0x03, 0x00][..], 3),
            (&[0x0c, 0x02, 0x00, 0x00, 0x00][..], 2),
            (&[0x0e, 0x04, 0, 0, 0, 0, 0, 0, 0][..], 4),
        ] {
            assert_eq!(
                slp_typa(&name_package(b"_S5_", 1, body)),
                Some(value),
                "{body:x?}"
            );
        }
    }

    #[test]
    fn the_value_written_is_slp_typa_in_bits_10_to_12_with_slp_en() {
        for value in 0..=7u8 {
            let written = pm1a_control(value);
            assert_eq!(written & (1 << 13), 1 << 13, "SLP_EN");
            assert_eq!((written >> 10) & 7, u16::from(value), "SLP_TYP");
            assert_eq!(written & !0x3c00, 0, "no other bit");
        }
    }

    #[test]
    fn a_package_length_of_any_width_is_read() {
        // Two bytes: 75 = 0x4b, its low four bits in the first byte and the rest after.
        let mut aml = vec![
            0x08, b'_', b'S', b'5', b'_', 0x12, 0x4b, 0x04, 0x04, 0x0a, 0x05,
        ];
        aml.extend_from_slice(&[0x00; 70]);
        assert_eq!(slp_typa(&aml), Some(5));
        // A byte short of what its length says.
        assert_eq!(slp_typa(&aml[..aml.len() - 1]), None);
        // Three bytes and four bytes, each a length of 8 and 9 written long.
        let aml = [
            0x08, b'_', b'S', b'5', b'_', 0x12, 0x88, 0x00, 0x00, 0x02, 0x0a, 0x03, 0x00, 0x00,
        ];
        assert_eq!(slp_typa(&aml), Some(3));
        let aml = [
            0x08, b'_', b'S', b'5', b'_', 0x12, 0xc9, 0x00, 0x00, 0x00, 0x02, 0x0a, 0x04, 0x00,
            0x00,
        ];
        assert_eq!(slp_typa(&aml), Some(4));
    }

    #[test]
    fn a_table_with_no_s5_gives_none() {
        assert_eq!(slp_typa(&[]), None);
        assert_eq!(slp_typa(b"_S5"), None);
        let table = dsdt(&name_package(b"_S3_", 4, &[0x01, 0x01, 0x00, 0x00]));
        assert_eq!(slp_typa(&table[HEADER..]), None);
    }

    #[test]
    fn an_s5_that_is_not_a_declared_package_is_passed_over() {
        let real = name_package(b"_S5_", 4, &[0x0a, 0x05, 0x00, 0x00, 0x00]);
        // A reference to the name inside a method, then the declaration.
        let mut aml = vec![0x14, 0x0a, b'_', b'P', b'T', b'S', 0x01, 0x70];
        aml.extend_from_slice(b"_S5_");
        aml.push(0x60);
        aml.extend_from_slice(&real);
        assert_eq!(slp_typa(&aml), Some(5));
        // A name declared as an integer, not a package, then the declaration.
        let mut aml = vec![0x08, b'_', b'S', b'5', b'_', 0x0a, 0x05];
        aml.extend_from_slice(&real);
        assert_eq!(slp_typa(&aml), Some(5));
        // Either alone gives nothing.
        assert_eq!(
            slp_typa(&[0x70, b'_', b'S', b'5', b'_', 0x12, 0x03, 0x01, 0x00]),
            None
        );
        assert_eq!(slp_typa(&[0x08, b'_', b'S', b'5', b'_', 0x0a, 0x05]), None);
        // The name at the very start of the bytes has no `NameOp` before it.
        assert_eq!(
            slp_typa(&[b'_', b'S', b'5', b'_', 0x12, 0x03, 0x01, 0x00]),
            None
        );
    }

    #[test]
    fn a_first_element_that_is_no_slp_typ_gives_none() {
        // More than three bits.
        assert_eq!(slp_typa(&name_package(b"_S5_", 1, &[0x0a, 0x08])), None);
        assert_eq!(
            slp_typa(&name_package(b"_S5_", 1, &[0x0b, 0x00, 0x01])),
            None
        );
        // OnesOp, a string, a nested package.
        assert_eq!(slp_typa(&name_package(b"_S5_", 1, &[0xff])), None);
        assert_eq!(
            slp_typa(&name_package(b"_S5_", 1, &[0x0d, b'5', 0x00])),
            None
        );
        assert_eq!(
            slp_typa(&name_package(b"_S5_", 1, &[0x12, 0x02, 0x00])),
            None
        );
        // No element at all.
        assert_eq!(slp_typa(&name_package(b"_S5_", 0, &[])), None);
        assert_eq!(slp_typa(&name_package(b"_S5_", 1, &[])), None);
        // A prefix with too few bytes after it inside the package.
        assert_eq!(slp_typa(&name_package(b"_S5_", 1, &[0x0a])), None);
        assert_eq!(
            slp_typa(&name_package(b"_S5_", 1, &[0x0c, 0x01, 0x00])),
            None
        );
    }

    #[test]
    fn a_package_length_that_does_not_hold_the_package_gives_none() {
        // Shorter than its own bytes and the element count.
        assert_eq!(
            slp_typa(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x00, 0x01, 0x00]),
            None
        );
        assert_eq!(
            slp_typa(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x01, 0x01, 0x00]),
            None
        );
        // Long enough for the count and not for the element.
        assert_eq!(
            slp_typa(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x02, 0x01, 0x00]),
            None
        );
        // Its following bytes are missing.
        assert_eq!(slp_typa(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x80]), None);
    }

    #[test]
    fn a_table_cut_short_anywhere_gives_none_and_never_panics() {
        let aml = qemu_aml();
        for cut in 0..aml.len() {
            assert_eq!(slp_typa(&aml[..cut]), None, "cut at {cut}");
        }
        assert_eq!(slp_typa(&aml), Some(0));
        // Every byte value in every place after the name, for the same reason.
        let base = name_package(b"_S5_", 4, &[0x0a, 0x05, 0x00, 0x00, 0x00]);
        for at in 5..base.len() {
            for byte in 0..=255u8 {
                let mut aml = base.clone();
                aml[at] = byte;
                if let Some(value) = slp_typa(&aml) {
                    assert!(value <= 7);
                }
            }
        }
    }
}

#[cfg(test)]
mod boot_wait_tests {
    use std::cell::Cell;

    use moruna_abi::boot_line::{CORE, Fatal};

    use super::boot_wait::{APIC_TIMER_TIMED_OUT, CPU_START_TIMED_OUT, LIMIT, PIT_TIMED_OUT, wait};

    /// A counter that reads `start` first and `step` more at each reading after.
    fn counter(start: u64, step: u64) -> impl Fn() -> u64 {
        let next = Cell::new(start);
        move || {
            let now = next.get();
            next.set(now.wrapping_add(step));
            now
        }
    }

    #[test]
    fn the_deadline_is_a_count_above_two_to_the_33() {
        assert_eq!(LIMIT, 1 << 33);
        // About 2 s at the rates a cloud CPU runs: 4.3 s at 2 GHz, 2.1 s at 4 GHz.
        assert_eq!(LIMIT / 2_000_000_000, 4);
        assert_eq!(LIMIT / 4_000_000_000, 2);
    }

    #[test]
    fn a_wait_that_is_done_at_once_pauses_once_and_is_in_time() {
        let pauses = Cell::new(0);
        assert!(wait(
            counter(0, LIMIT * 2),
            || pauses.set(pauses.get() + 1),
            || true
        ));
        assert_eq!(pauses.get(), 1);
    }

    #[test]
    fn a_wait_ends_in_time_when_what_it_waits_for_comes() {
        // The PIT's hundred interrupts, one each pause, at a count far below the deadline.
        let interrupts = Cell::new(0u64);
        assert!(wait(
            counter(1_000, 3_000_000),
            || interrupts.set(interrupts.get() + 1),
            || interrupts.get() >= 100
        ));
        assert_eq!(interrupts.get(), 100);
    }

    #[test]
    fn a_wait_for_what_never_comes_ends_once_the_count_is_above_the_limit() {
        // No PIT: nothing ever finishes the calibration. Each look reads the counter once,
        // a quarter of the limit on: four quarters are not above it, the fifth is.
        let pauses = Cell::new(0);
        assert!(!wait(
            counter(77, LIMIT / 4),
            || pauses.set(pauses.get() + 1),
            || false
        ));
        assert_eq!(pauses.get(), 5);
    }

    #[test]
    fn a_count_of_exactly_the_limit_is_not_yet_late() {
        let looks = Cell::new(0);
        // The first look, at exactly the limit, still goes on to a second.
        assert!(!wait(
            counter(0, LIMIT),
            || looks.set(looks.get() + 1),
            || false
        ));
        assert_eq!(looks.get(), 2);
        let looks = Cell::new(0);
        assert!(!wait(
            counter(0, LIMIT + 1),
            || looks.set(looks.get() + 1),
            || false
        ));
        assert_eq!(looks.get(), 1);
    }

    #[test]
    fn what_came_at_the_last_look_counts_though_the_deadline_has_passed() {
        let looks = Cell::new(0);
        assert!(wait(
            counter(0, LIMIT * 3),
            || looks.set(looks.get() + 1),
            || looks.get() == 1
        ));
    }

    #[test]
    fn a_counter_that_wraps_during_the_wait_is_measured_through_the_wrap() {
        let pauses = Cell::new(0);
        assert!(!wait(
            counter(u64::MAX - LIMIT / 2, LIMIT / 4),
            || pauses.set(pauses.get() + 1),
            || false
        ));
        assert_eq!(pauses.get(), 5);
        // And a wait that ends in time across the wrap is in time.
        let looks = Cell::new(0);
        assert!(wait(
            counter(u64::MAX - 10, 100),
            || looks.set(looks.get() + 1),
            || looks.get() == 3
        ));
    }

    #[test]
    fn a_machine_with_no_pit_gives_up_after_two_seconds_at_three_gigahertz() {
        // The patched calibration's wait as the machine runs it: the TSC at 3 GHz, a pause
        // of a millisecond, and a calibration that no interrupt ever finishes.
        const HZ: u64 = 3_000_000_000;
        let pauses = Cell::new(0u64);
        assert!(!wait(
            counter(123_456_789, HZ / 1_000),
            || pauses.set(pauses.get() + 1),
            || false
        ));
        let waited_ms = pauses.get();
        assert!((2_000..3_000).contains(&waited_ms), "{waited_ms} ms");
    }

    #[test]
    fn each_wait_says_the_cores_fatal_line() {
        assert_eq!(
            PIT_TIMED_OUT,
            format!(
                "{}",
                Fatal {
                    stage: CORE,
                    reason: "PIT calibration timed out"
                }
            )
        );
        assert_eq!(
            APIC_TIMER_TIMED_OUT,
            format!(
                "{}",
                Fatal {
                    stage: CORE,
                    reason: "APIC timer calibration timed out"
                }
            )
        );
        assert_eq!(
            CPU_START_TIMED_OUT,
            format!(
                "{}",
                Fatal {
                    stage: CORE,
                    reason: "CPU start timed out"
                }
            )
        );
    }

    #[test]
    fn the_wait_for_the_other_cpus_ends_when_the_last_has_started_or_at_the_deadline() {
        // The boot processor's wait as `boot/smp.rs` runs it: a pause, then a look at how
        // many CPUs have started. Three of four start, a look apart, and the fourth after
        // seven looks.
        let started = Cell::new(1usize);
        let looks = Cell::new(0);
        assert!(wait(
            counter(5_000, 40_000_000),
            || {
                looks.set(looks.get() + 1);
                if looks.get() <= 2 || looks.get() == 7 {
                    started.set(started.get() + 1);
                }
            },
            || started.get() == 4
        ));
        assert_eq!(looks.get(), 7);
        // A CPU that never starts: the wait gives up once the count is above the limit.
        let started = Cell::new(1usize);
        assert!(!wait(
            counter(5_000, LIMIT / 2),
            || started.set(started.get().max(3)),
            || started.get() == 4
        ));
    }
}
