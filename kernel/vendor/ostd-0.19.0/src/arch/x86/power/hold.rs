// SPDX-License-Identifier: MPL-2.0

//! moruna: the hold before a failed machine powers off (Moruna's system design, section 1B;
//! work item 1.6c). It names nothing of OSTD, so Moruna's host tests compile this same file
//! (`kernel/ostd-adaptation`).
//!
//! A machine that powers off because it failed has printed its `moruna: fatal` line. It stays
//! halted for [`HOLD_SECONDS`] first, then enters S5 as before. A machine that powers off
//! because it succeeded does not wait.

/// How long a failed machine stays halted after its fatal line before it powers off.
///
/// OpenStack serves a guest's console log only while the guest runs, and shows SHUTOFF 15 s
/// after it stops. A node that powered off a moment after its fatal line left no console that
/// could be read, so its cause could not be seen (work item 1.6b, on Servercore).
pub(crate) const HOLD_SECONDS: u64 = 60;

/// The fastest TSC a cloud CPU runs, as the boot waits' deadline counts them
/// (`kernel/tsc/boot_wait.rs`: 2 GHz to 4 GHz).
pub(crate) const FASTEST_TSC_HZ: u64 = 4_000_000_000;

/// The hold as a count of the TSC when its rate is not known yet: at least [`HOLD_SECONDS`]
/// at any rate a cloud CPU runs (twice that at 2 GHz). Only a fatal line from the TSC's own
/// calibration comes before the rate is known.
pub(crate) const UNKNOWN_RATE_COUNT: u64 = HOLD_SECONDS * FASTEST_TSC_HZ;

/// The hold as a count of the TSC, whose rate is `tsc_hz` (0 when not known yet).
pub(crate) fn count(tsc_hz: u64) -> u64 {
    if tsc_hz == 0 {
        UNKNOWN_RATE_COUNT
    } else {
        tsc_hz.saturating_mul(HOLD_SECONDS)
    }
}

/// Waits, calling `pause` between looks, until the counter `now` has moved at least `count`
/// from where the wait began. The counter may wrap during the wait. `pause` must return
/// without an interrupt: the CPU holding may have its interrupts off (a panic's).
pub(crate) fn wait(now: impl Fn() -> u64, mut pause: impl FnMut(), count: u64) {
    let began = now();
    while now().wrapping_sub(began) < count {
        pause();
    }
}

/// The end of a power-off by S5: for a machine that `failed`, `hold` first, then `enter`.
pub(crate) fn power_off(failed: bool, hold: impl FnOnce(), enter: impl FnOnce()) {
    if failed {
        hold();
    }
    enter();
}
