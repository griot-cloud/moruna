//! Clocks and sleep, from the kernel's monotonic clock (the clock page): `clock_gettime`,
//! `clock_getres`, `nanosleep`, and the deadlines a futex waits to. A process's or a thread's
//! CPU time has no clock on the core, and is refused as Linux refuses a clock it lacks.

use core::sync::atomic::AtomicU32;

use crate::errno::EINVAL;
use crate::os;
use crate::{Errno, Result};

pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
pub const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
pub const CLOCK_MONOTONIC_RAW: i32 = 4;
pub const CLOCK_REALTIME_COARSE: i32 = 5;
pub const CLOCK_MONOTONIC_COARSE: i32 = 6;
pub const CLOCK_BOOTTIME: i32 = 7;

const NANOS: u64 = 1_000_000_000;

/// A time as POSIX gives one: seconds and nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

impl Timespec {
    fn from_ns(ns: u64) -> Timespec {
        Timespec {
            sec: (ns / NANOS) as i64,
            nsec: (ns % NANOS) as i64,
        }
    }

    /// Nanoseconds, saturating; `EINVAL` for a negative time or nanoseconds past a second.
    pub fn to_ns(self) -> Result<u64> {
        if self.sec < 0 || !(0..NANOS as i64).contains(&self.nsec) {
            return Err(Errno(EINVAL));
        }
        Ok((self.sec as u64)
            .saturating_mul(NANOS)
            .saturating_add(self.nsec as u64))
    }
}

/// `clock_gettime`.
pub fn now(clock: i32) -> Result<Timespec> {
    match clock {
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            Ok(Timespec::from_ns(os::monotonic_ns()))
        }
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => Ok(Timespec::from_ns(os::realtime_ns())),
        _ => Err(Errno(EINVAL)),
    }
}

/// `clock_getres`: a nanosecond, for every clock there is.
pub fn resolution(clock: i32) -> Result<Timespec> {
    now(clock).map(|_| Timespec { sec: 0, nsec: 1 })
}

/// The kernel's deadline `timeout` from now: none is never.
pub fn deadline(timeout: Option<Timespec>) -> Result<u64> {
    match timeout {
        None => Ok(u64::MAX),
        Some(t) => Ok(os::monotonic_ns().saturating_add(t.to_ns()?)),
    }
}

/// `nanosleep`: sleeps for `t`, all of it (no signal ends a sleep early on the core).
pub fn sleep(t: Timespec) -> Result<()> {
    let until = deadline(Some(t))?;
    // A word no one wakes: the wait ends at the deadline.
    let word = AtomicU32::new(0);
    while os::monotonic_ns() < until {
        let _ = os::futex_wait(&word, 0, until);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_read_the_monotonic_clock_and_refuse_cpu_time() {
        let a = now(CLOCK_MONOTONIC).unwrap();
        let b = now(CLOCK_BOOTTIME).unwrap();
        assert!(b.to_ns().unwrap() >= a.to_ns().unwrap());
        for c in [
            CLOCK_REALTIME,
            CLOCK_REALTIME_COARSE,
            CLOCK_MONOTONIC_RAW,
            CLOCK_MONOTONIC_COARSE,
        ] {
            assert!(now(c).unwrap().nsec < NANOS as i64);
            assert_eq!(resolution(c), Ok(Timespec { sec: 0, nsec: 1 }));
        }
        for c in [CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID, 99, -1] {
            assert_eq!(now(c), Err(Errno(EINVAL)));
            assert_eq!(resolution(c), Err(Errno(EINVAL)));
        }
    }

    #[test]
    fn times_convert_and_bad_ones_are_refused() {
        let t = Timespec::from_ns(3 * NANOS + 7);
        assert_eq!(t, Timespec { sec: 3, nsec: 7 });
        assert_eq!(t.to_ns(), Ok(3 * NANOS + 7));
        let huge = Timespec {
            sec: i64::MAX,
            nsec: 0,
        };
        assert_eq!(huge.to_ns(), Ok(u64::MAX));
        for bad in [
            Timespec { sec: -1, nsec: 0 },
            Timespec { sec: 0, nsec: -1 },
            Timespec {
                sec: 0,
                nsec: NANOS as i64,
            },
        ] {
            assert_eq!(bad.to_ns(), Err(Errno(EINVAL)));
            assert_eq!(sleep(bad), Err(Errno(EINVAL)));
            assert_eq!(deadline(Some(bad)), Err(Errno(EINVAL)));
        }
        assert_eq!(deadline(None), Ok(u64::MAX));
        assert_eq!(deadline(Some(huge)), Ok(u64::MAX));
    }

    #[test]
    fn a_sleep_lasts_its_time() {
        let before = os::monotonic_ns();
        sleep(Timespec {
            sec: 0,
            nsec: 3_000_000,
        })
        .unwrap();
        assert!(os::monotonic_ns() - before >= 3_000_000);
        sleep(Timespec::default()).unwrap();
    }
}
