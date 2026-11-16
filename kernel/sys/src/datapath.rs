//! The data path's mechanisms (ABI 2.6; `architecture/data-os.md` §2), for the engine.
//!
//! These signatures are frozen for the parallel streams (`architecture/data-os-streams.md`):
//! stream A implements the kernel's side, and streams C and P call them. Until stream A
//! lands, each answers `Error::BadSyscall`, and a caller falls back to today's path.

use moruna_abi::{clock_page as page, cpu, nr};

use crate::{Process, Result, Thread, check, raw};

/// Runs `thread` on CPU `cpu` only, or anywhere with `None`. It places a thread on a core
/// and nothing else: which work the thread runs stays the engine's decision.
pub fn thread_set_cpu(thread: &Thread, cpu: Option<u32>) -> Result<()> {
    let target = cpu.map_or(cpu::ANY, u64::from);
    check(raw::call2(nr::THREAD_SET_CPU, u64::from(thread.0.raw()), target).0).map(|_| ())
}

/// Maps the read-only clock page into this process (once) and returns its address.
pub fn clock_page() -> Result<u64> {
    check(raw::call0(nr::CLOCK_PAGE).0)
}

/// The fields of a clock page read under its sequence (see `moruna_abi::clock_page`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ClockReading {
    /// Monotonic nanoseconds at `base_tsc`.
    pub base_ns: u64,
    /// The time-stamp counter the page was written at.
    pub base_tsc: u64,
    /// Counter ticks to nanoseconds: multiply, then shift right.
    pub multiplier: u64,
    /// See `multiplier`.
    pub shift: u32,
}

impl ClockReading {
    /// The page's fields from its bytes, or `None` while the kernel is writing it (an odd
    /// sequence) or if the sequence moved between `before` and `after`.
    pub fn from_bytes(before: u64, bytes: &[u8; page::LEN], after: u64) -> Option<ClockReading> {
        if before != after || before & 1 == 1 {
            return None;
        }
        let word = |at: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(b)
        };
        let mut s = [0u8; 4];
        s.copy_from_slice(&bytes[page::SHIFT..page::SHIFT + 4]);
        Some(ClockReading {
            base_ns: word(page::BASE_NS),
            base_tsc: word(page::BASE_TSC),
            multiplier: word(page::MULTIPLIER),
            shift: u32::from_le_bytes(s),
        })
    }

    /// Nanoseconds at counter value `tsc`.
    pub fn nanos_at(&self, tsc: u64) -> u64 {
        let ticks = u128::from(tsc.wrapping_sub(self.base_tsc));
        let ns = (ticks * u128::from(self.multiplier)) >> self.shift;
        self.base_ns.wrapping_add(ns as u64)
    }
}

/// Sets `flags` (`map::READ`, or `map::READ | map::WRITE`) on each `[address, length]`
/// window of `process`, with one flush: the grouped revoke before the record check, and the
/// grant back when a window is used again (data-os §5). At most `windows::MAX` at a time.
pub fn windows_protect(process: &Process, windows: &[[u64; 2]], flags: u64) -> Result<()> {
    check(raw::windows_protect(process.0.raw(), windows, flags)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_reading_is_taken_only_under_a_steady_even_sequence() {
        let mut bytes = [0u8; page::LEN];
        bytes[page::BASE_NS..page::BASE_NS + 8].copy_from_slice(&1_000u64.to_le_bytes());
        bytes[page::BASE_TSC..page::BASE_TSC + 8].copy_from_slice(&500u64.to_le_bytes());
        bytes[page::MULTIPLIER..page::MULTIPLIER + 8].copy_from_slice(&3u64.to_le_bytes());
        bytes[page::SHIFT..page::SHIFT + 4].copy_from_slice(&1u32.to_le_bytes());
        let r = ClockReading::from_bytes(2, &bytes, 2).expect("steady");
        assert_eq!(r.nanos_at(600), 1_000 + (100 * 3 >> 1));
        assert_eq!(
            ClockReading::from_bytes(3, &bytes, 3),
            None,
            "odd: being written"
        );
        assert_eq!(ClockReading::from_bytes(2, &bytes, 4), None, "moved");
    }
}
