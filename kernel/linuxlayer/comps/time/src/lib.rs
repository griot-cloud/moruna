// SPDX-License-Identifier: MPL-2.0

//! The system time of Asterinas.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::sync::Arc;
use core::time::Duration;

use component::{ComponentInitError, init_component};
use rtc::Driver;
use spin::Once;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "time: "
    };
}

mod clocksource;
mod rtc;
mod system_time;
mod tsc;

pub use clocksource::{ClockSource, Instant};
pub use system_time::SystemTime;

pub static VDSO_DATA_HIGH_RES_UPDATE_FN: Once<fn(Instant, u64)> = Once::new();

static RTC_DRIVER: Once<Arc<dyn Driver + Send + Sync>> = Once::new();

#[init_component]
fn time_init() -> Result<(), ComponentInitError> {
    let rtc = rtc::init_rtc_driver();
    RTC_DRIVER.call_once(|| rtc);
    tsc::init();
    Ok(())
}

static START_TIME: Once<SystemTime> = Once::new();

/// Returns the start time, which is the system time when calibrating.
pub fn read_start_time() -> SystemTime {
    *START_TIME.get().unwrap()
}

/// Returns the monotonic time from the TSC clocksource.
pub fn read_monotonic_time() -> Duration {
    let instant = tsc::read_instant();
    Duration::new(instant.secs(), instant.nanos())
}

/// Returns the current system time.
pub fn read_system_time() -> SystemTime {
    read_start_time() + read_monotonic_time()
}

/// Returns the default (TSC) clocksource.
pub fn default_clocksource() -> Arc<ClockSource> {
    tsc::CLOCK.get().unwrap().clone()
}

#[cfg(ktest)]
mod test {
    use core::time::Duration;

    use ostd::{
        boot::{BootInfo, init},
        prelude::*,
        timer::monotonic_nanos,
    };

    use super::*;

    // User space: the clock is the core's monotonic clock, and the wall time is the one
    // the start gives plus the monotonic time since.
    #[ktest]
    fn clocks_from_the_core_and_the_start() {
        let at_start = Duration::from_secs(1_700_000_000);
        init(BootInfo {
            wall_clock_at_start: at_start,
            monotonic_at_start: monotonic_nanos(),
            ..Default::default()
        });
        time_init().unwrap();
        let a = read_monotonic_time();
        let b = read_monotonic_time();
        assert!(b >= a);
        let start = read_start_time().since_epoch();
        assert!(start >= at_start && start < at_start + Duration::from_secs(60));
        assert!(read_system_time().since_epoch() >= start);
        let clock = default_clocksource();
        assert_eq!(clock.freq(), 1_000_000_000);
        assert!(clock.read_cycles() > 0);
    }
}
