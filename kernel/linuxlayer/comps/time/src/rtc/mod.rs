// SPDX-License-Identifier: MPL-2.0

use alloc::sync::Arc;

use crate::SystemTime;

/// Generic interface for RTC drivers.
pub(crate) trait Driver {
    /// Reads RTC.
    fn read_rtc(&self) -> SystemTime;
}

/// User space: the wall time at start is the personality's start's to give
/// (`ostd::boot::BootInfo::wall_clock_at_start`), read once, as the engine reads its own
/// (data-os.md C8). There is no RTC device to read.
pub(super) fn init_rtc_driver() -> Arc<dyn Driver + Send + Sync> {
    Arc::new(RtcStart)
}

struct RtcStart;

impl Driver for RtcStart {
    fn read_rtc(&self) -> SystemTime {
        let at_start = ostd::boot::boot_info().wall_clock_at_start;
        // The wall time now: the wall time at start plus the monotonic time since.
        let since = ostd::timer::monotonic_nanos()
            .saturating_sub(ostd::boot::boot_info().monotonic_at_start);
        SystemTime::new(at_start + core::time::Duration::from_nanos(since))
    }
}
