//! std's `Instant` and `SystemTime` (std's `sys::time`).
//!
//! `Instant` is nanoseconds on the kernel's monotonic clock, the clock every deadline is
//! measured on, read from the clock page with no call (`moruna_abi::clock_page`).
//!
//! `SystemTime` has no source on Moruna: the node's wall clock is `platform`'s (system design
//! 1D), and a native task is given no realtime base. Its arithmetic works; `SystemTime::now`
//! is unsupported, and panics, as on every platform of std without a wall clock.

use core::time::Duration;

use super::os;

/// A moment on the monotonic clock, in nanoseconds since the clock started.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(u64);

impl Instant {
    /// Now.
    pub fn now() -> Instant {
        Instant(os::clock())
    }

    /// The moment `nanos` nanoseconds after the clock started.
    pub const fn from_nanos(nanos: u64) -> Instant {
        Instant(nanos)
    }

    /// Nanoseconds since the clock started: a deadline for the kernel's calls.
    pub const fn as_nanos(&self) -> u64 {
        self.0
    }

    /// The time from `other` to `self`; `None` if `other` is later.
    pub fn checked_sub_instant(&self, other: &Instant) -> Option<Duration> {
        self.0.checked_sub(other.0).map(Duration::from_nanos)
    }

    /// `duration` later; `None` past the clock's end.
    pub fn checked_add_duration(&self, duration: &Duration) -> Option<Instant> {
        let nanos = u64::try_from(duration.as_nanos()).ok()?;
        self.0.checked_add(nanos).map(Instant)
    }

    /// `duration` earlier; `None` before the clock started.
    pub fn checked_sub_duration(&self, duration: &Duration) -> Option<Instant> {
        let nanos = u64::try_from(duration.as_nanos()).ok()?;
        self.0.checked_sub(nanos).map(Instant)
    }
}

/// A moment on the wall clock: the time since 1970-01-01T00:00:00Z.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime(Duration);

/// 1970-01-01T00:00:00Z.
pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::ZERO);

impl SystemTime {
    /// The latest moment a `SystemTime` holds.
    pub const MAX: SystemTime = SystemTime(Duration::MAX);
    /// The earliest: [`UNIX_EPOCH`].
    pub const MIN: SystemTime = SystemTime(Duration::ZERO);

    /// Unsupported: a native task has no realtime base to read.
    pub fn now() -> SystemTime {
        panic!("SystemTime::now is not supported on Moruna: a native task has no realtime base")
    }

    /// The time from `other` to `self`, or, when `other` is later, the time back to it.
    pub fn sub_time(&self, other: &SystemTime) -> Result<Duration, Duration> {
        self.0.checked_sub(other.0).ok_or_else(|| other.0 - self.0)
    }

    /// `duration` later; `None` past [`SystemTime::MAX`].
    pub fn checked_add_duration(&self, duration: &Duration) -> Option<SystemTime> {
        self.0.checked_add(*duration).map(SystemTime)
    }

    /// `duration` earlier; `None` before [`UNIX_EPOCH`].
    pub fn checked_sub_duration(&self, duration: &Duration) -> Option<SystemTime> {
        self.0.checked_sub(*duration).map(SystemTime)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn the_clock_does_not_go_back() {
        let a = Instant::now();
        let b = Instant::now();
        assert!(b >= a);
        assert!(b.checked_sub_instant(&a).is_some());
        assert_ne!(
            a.as_nanos(),
            0,
            "the clock has started by the time a program runs"
        );
    }

    #[test]
    fn instants_add_and_subtract_in_nanoseconds() {
        let t = Instant::from_nanos(1_000);
        assert_eq!(t.as_nanos(), 1_000);
        let later = t.checked_add_duration(&Duration::from_micros(2)).unwrap();
        assert_eq!(later, Instant::from_nanos(3_000));
        assert_eq!(
            later.checked_sub_instant(&t),
            Some(Duration::from_nanos(2_000))
        );
        assert_eq!(t.checked_sub_instant(&later), None, "earlier minus later");
        assert_eq!(
            later.checked_sub_duration(&Duration::from_nanos(3_000)),
            Some(Instant::from_nanos(0))
        );
        assert_eq!(t.checked_sub_duration(&Duration::from_nanos(1_001)), None);
    }

    #[test]
    fn instants_stop_at_the_clock_s_ends() {
        let end = Instant::from_nanos(u64::MAX);
        assert_eq!(end.checked_add_duration(&Duration::from_nanos(1)), None);
        assert_eq!(
            Instant::from_nanos(0).checked_add_duration(&Duration::MAX),
            None
        );
        assert_eq!(end.checked_sub_duration(&Duration::MAX), None);
        assert_eq!(
            end.checked_sub_instant(&Instant::from_nanos(0)),
            Some(Duration::from_nanos(u64::MAX))
        );
    }

    #[test]
    fn system_times_add_subtract_and_compare() {
        let t = UNIX_EPOCH
            .checked_add_duration(&Duration::from_secs(10))
            .unwrap();
        assert_eq!(t.sub_time(&UNIX_EPOCH), Ok(Duration::from_secs(10)));
        assert_eq!(UNIX_EPOCH.sub_time(&t), Err(Duration::from_secs(10)));
        assert_eq!(t.checked_sub_duration(&Duration::from_secs(11)), None);
        assert_eq!(
            t.checked_sub_duration(&Duration::from_secs(10)),
            Some(UNIX_EPOCH)
        );
        assert_eq!(SystemTime::MIN, UNIX_EPOCH);
        assert_eq!(
            SystemTime::MAX.checked_add_duration(&Duration::from_nanos(1)),
            None
        );
        assert!(SystemTime::MAX > t);
    }

    #[test]
    #[should_panic(expected = "no realtime base")]
    fn the_wall_clock_is_unsupported() {
        let _ = SystemTime::now();
    }
}
