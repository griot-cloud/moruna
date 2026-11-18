//! std's `Instant` and `SystemTime` (std's `sys::time`).
//!
//! `Instant` is nanoseconds on the kernel's monotonic clock, the clock every deadline is
//! measured on, read from the clock page with no call (`moruna_abi::clock_page`).
//!
//! `SystemTime` is the realtime base the clock page carries (ABI 2.8) plus that monotonic
//! clock: the wall clock the core keeps, from the machine's CMOS clock at boot and from
//! `platform`'s `clock_set_realtime` (system design 1D). While the base is unset the wall
//! clock reads 1970 plus the monotonic clock, as the Linux layer's does; it never panics.

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

/// The wall clock when the realtime base is `base` (zero while unset) and the monotonic clock
/// reads `monotonic`.
fn wall(base: u64, monotonic: u64) -> SystemTime {
    SystemTime(Duration::from_nanos(base.saturating_add(monotonic)))
}

impl SystemTime {
    /// The latest moment a `SystemTime` holds.
    pub const MAX: SystemTime = SystemTime(Duration::MAX);
    /// The earliest: [`UNIX_EPOCH`].
    pub const MIN: SystemTime = SystemTime(Duration::ZERO);

    /// Now, on the wall clock.
    pub fn now() -> SystemTime {
        let (base, monotonic) = os::realtime();
        wall(base, monotonic)
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
    fn the_wall_clock_is_the_realtime_base_plus_the_monotonic_clock() {
        // 2023-11-14T22:13:20Z at the monotonic zero.
        let base = 1_700_000_000_000_000_000;
        let at = wall(base, 5_000);
        assert_eq!(
            at.sub_time(&UNIX_EPOCH),
            Ok(Duration::from_nanos(base + 5_000))
        );
        assert_eq!(
            wall(base, 7_000).sub_time(&at),
            Ok(Duration::from_nanos(2_000)),
            "it moves with the monotonic clock"
        );
        // A base past the clock's end stays there rather than wrapping to 1970.
        assert_eq!(wall(u64::MAX, 1), wall(u64::MAX, 0));
    }

    #[test]
    fn an_unset_wall_clock_reads_as_1970_plus_the_monotonic_clock() {
        assert_eq!(
            wall(0, 3_000_000_000).sub_time(&UNIX_EPOCH),
            Ok(Duration::from_secs(3))
        );
        // The host stands in an unset base: now is 1970 plus its clock, and never panics.
        let since = SystemTime::now().sub_time(&UNIX_EPOCH).unwrap();
        assert!(since > Duration::ZERO, "the monotonic clock has started");
        assert!(
            since < Duration::from_secs(24 * 3600),
            "not a date in this century"
        );
    }
}
