// SPDX-License-Identifier: MPL-2.0

//! moruna: whether a PIT answers at its ports (Moruna's system design, section 1F: nothing
//! from the old PC is assumed; work item 1.8). A Hyper-V Generation 2 machine has no PIT,
//! and its ports read as all ones. It names nothing of OSTD, so Moruna's host tests compile
//! this same file (`kernel/ostd-adaptation`).
//!
//! A PIT counts down channel 0 at 1.193182 MHz, so of a few latched counts read one after
//! another two differ. Ports with nothing behind them read the same value every time.

/// The most latched counts read before deciding no PIT is there. Each read is three port
/// accesses, about a microsecond each on real hardware and wherever an access exits to a
/// hypervisor, so 1,024 reads span thousands of the PIT's counts (0.84 microsecond each),
/// and hundreds of them even where an access costs a tenth of that. On a machine with no PIT
/// they take a few milliseconds.
pub(crate) const READS: usize = 1024;

/// Whether the counts read from channel 0 show a counter that moves: two of them differ.
/// It stops reading at the first that differs from the one before it.
pub(crate) fn counts(reads: impl IntoIterator<Item = u16>) -> bool {
    let mut reads = reads.into_iter();
    let Some(mut previous) = reads.next() else {
        return false;
    };
    for read in reads {
        if read != previous {
            return true;
        }
        previous = read;
    }
    false
}

/// What the TSC's calibration says when it would need the PIT and there is none.
pub(crate) const NO_PIT_FOR_TSC: &str = "moruna: fatal core: no PIT to calibrate the TSC";
/// What the APIC timer's calibration says when it would need the PIT and there is none.
pub(crate) const NO_PIT_FOR_APIC_TIMER: &str =
    "moruna: fatal core: no PIT to calibrate the APIC timer";
