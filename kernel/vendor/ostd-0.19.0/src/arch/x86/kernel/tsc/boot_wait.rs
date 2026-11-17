// SPDX-License-Identifier: MPL-2.0

//! moruna: the deadline on a boot wait (Moruna's system design, section 1B: no wait
//! without a deadline in boot code). It names nothing of OSTD, so Moruna's host tests
//! compile this same file (`kernel/ostd-adaptation`).
//!
//! The PIT calibration of the TSC and the PIT calibration of the APIC timer each waited
//! for PIT interrupts without end, which on a machine with no PIT is for ever. Each now
//! waits through [`wait`], and says its line and powers the machine off when it gives up.

/// The deadline, as a count of the TSC: above 2^33, about 2 s at any rate a cloud CPU
/// runs. It is counted without the TSC's rate, which the first of these waits is there to
/// measure.
pub(crate) const LIMIT: u64 = 1 << 33;

/// What the TSC's calibration says when its deadline passes.
pub(crate) const PIT_TIMED_OUT: &str = "moruna: fatal core: PIT calibration timed out";
/// What the APIC timer's calibration says when its deadline passes.
pub(crate) const APIC_TIMER_TIMED_OUT: &str =
    "moruna: fatal core: APIC timer calibration timed out";

/// Waits for `done`, calling `pause` before each look at it, until the counter `now` has
/// moved more than [`LIMIT`] from where the wait began. Returns whether `done` came true.
///
/// The counter may wrap during the wait. `pause` must return without an interrupt, or a
/// machine that sends none would never reach the deadline.
pub(crate) fn wait(
    now: impl Fn() -> u64,
    mut pause: impl FnMut(),
    done: impl Fn() -> bool,
) -> bool {
    let began = now();
    loop {
        pause();
        if done() {
            return true;
        }
        if now().wrapping_sub(began) > LIMIT {
            return false;
        }
    }
}
