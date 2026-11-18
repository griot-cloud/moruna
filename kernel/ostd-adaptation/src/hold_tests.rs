//! The hold before a failed machine powers off (`src/arch/x86/power/hold.rs`; the system
//! design, section 1B, failure; work item 1.6c).

use std::cell::{Cell, RefCell};

use moruna_abi::boot_line::Fatal;

use super::boot_wait::LIMIT;
use super::hold::{FASTEST_TSC_HZ, HOLD_SECONDS, UNKNOWN_RATE_COUNT, count, power_off, wait};

/// The TSC rate kvmclock gave the core on Servercore (run 7 of item 1.6).
const SERVERCORE_TSC_HZ: u64 = 2_394_374_000;

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
fn the_hold_is_60_s() {
    assert_eq!(HOLD_SECONDS, 60);
}

#[test]
fn at_a_known_rate_the_hold_counts_60_s_of_it() {
    assert_eq!(count(SERVERCORE_TSC_HZ), 60 * SERVERCORE_TSC_HZ);
    assert_eq!(count(1_000_000_000), 60_000_000_000);
    // A rate no CPU has does not wrap the count to a short hold.
    assert_eq!(count(u64::MAX), u64::MAX);
}

#[test]
fn at_an_unknown_rate_the_hold_is_at_least_60_s_at_any_rate_a_cloud_cpu_runs() {
    assert_eq!(count(0), UNKNOWN_RATE_COUNT);
    // The boot waits' deadline counts cloud CPUs as 2 GHz to 4 GHz (2^33 is 4.3 s at 2 GHz
    // and 2.1 s at 4 GHz); the hold takes the fastest, so it is never shorter than 60 s.
    assert_eq!(FASTEST_TSC_HZ, 4_000_000_000);
    assert_eq!(LIMIT / FASTEST_TSC_HZ, 2);
    assert_eq!(UNKNOWN_RATE_COUNT / FASTEST_TSC_HZ, 60);
    assert_eq!(UNKNOWN_RATE_COUNT / 2_000_000_000, 120);
}

#[test]
fn the_wait_ends_once_the_count_has_passed_and_not_before() {
    let pauses = Cell::new(0u64);
    // 10 a reading from where the wait began: the readings 10 to 90 on each pause, and 100
    // ends it.
    wait(counter(1000, 10), || pauses.set(pauses.get() + 1), 100);
    assert_eq!(pauses.get(), 9);
}

#[test]
fn the_wait_goes_on_across_the_counter_wrapping() {
    let pauses = Cell::new(0u64);
    wait(
        counter(u64::MAX - 25, 10),
        || pauses.set(pauses.get() + 1),
        100,
    );
    assert_eq!(pauses.get(), 9);
}

#[test]
fn a_wait_of_nothing_does_not_pause() {
    let pauses = Cell::new(0u64);
    wait(counter(7, 1), || pauses.set(pauses.get() + 1), 0);
    assert_eq!(pauses.get(), 0);
}

#[test]
fn a_failed_machine_holds_then_powers_off() {
    let steps = RefCell::new(Vec::new());
    power_off(
        true,
        || steps.borrow_mut().push("hold"),
        || steps.borrow_mut().push("S5"),
    );
    assert_eq!(*steps.borrow(), ["hold", "S5"]);
}

#[test]
fn a_machine_that_succeeded_powers_off_at_once() {
    let steps = RefCell::new(Vec::new());
    power_off(
        false,
        || steps.borrow_mut().push("hold"),
        || steps.borrow_mut().push("S5"),
    );
    assert_eq!(*steps.borrow(), ["S5"]);
}

/// The sequence 1B asks for, as the core runs it: the fatal line on the console, 60 s of
/// the machine's clock with nothing but pauses, then the write that powers it off.
#[test]
fn the_fatal_line_then_60_s_then_the_power_off() {
    let console = RefCell::new(Vec::<String>::new());
    let tsc = Cell::new(0u64);
    let entered_at = Cell::new(None);
    // A TSC at Servercore's rate that moves 1 ms each time the holding CPU pauses.
    let step = SERVERCORE_TSC_HZ / 1000;

    console.borrow_mut().push(
        Fatal {
            stage: "network",
            reason: "no DHCP lease within 30 s",
        }
        .to_string(),
    );
    let fatal_at = tsc.get();
    power_off(
        true,
        || {
            wait(
                || tsc.get(),
                || tsc.set(tsc.get() + step),
                count(SERVERCORE_TSC_HZ),
            )
        },
        || entered_at.set(Some(tsc.get())),
    );

    assert_eq!(
        console.borrow().as_slice(),
        ["moruna: fatal network: no DHCP lease within 30 s"]
    );
    let held = entered_at.get().expect("the machine powered off") - fatal_at;
    assert!(held >= 60 * SERVERCORE_TSC_HZ, "held {held} counts");
    assert!(held < 60 * SERVERCORE_TSC_HZ + step, "held {held} counts");
}
