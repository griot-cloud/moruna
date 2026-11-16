//! The data path's mechanisms (ABI 2.6; `architecture/data-os.md` §2), for the engine.
//!
//! These signatures are frozen for the parallel streams (`architecture/data-os-streams.md`):
//! stream A implements the kernel's side, and streams C and P call them. Until stream A
//! lands, each answers `Error::BadSyscall`, and a caller falls back to today's path.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

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

/// Runs the calling thread on CPU `cpu` only, or where it runs now with `None`: with a
/// CPU, the call returns on it. A thread holds no handle to itself, so this is
/// [`thread_set_cpu`] for the thread making the call.
pub fn set_current_cpu(cpu: Option<u32>) -> Result<()> {
    let target = cpu.map_or(cpu::ANY, u64::from);
    check(raw::call2(nr::THREAD_SET_CPU, 0, target).0).map(|_| ())
}

/// The CPU running the calling thread, read with no call: the kernel writes each CPU's
/// number to its `IA32_TSC_AUX`, which `rdtscp` returns. A thread on no pinned CPU may
/// be elsewhere by the time this returns.
#[cfg(target_arch = "x86_64")]
pub fn current_cpu() -> u32 {
    let mut cpu = 0;
    // SAFETY: `rdtscp` reads the time-stamp counter and `IA32_TSC_AUX` and writes `cpu`.
    unsafe { core::arch::x86_64::__rdtscp(&mut cpu) };
    cpu
}

/// The time-stamp counter, which the clock page scales.
#[cfg(target_arch = "x86_64")]
fn tsc() -> u64 {
    // SAFETY: `rdtsc` only reads the time-stamp counter.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The clock page `clock_page` maps: monotonic nanoseconds on [`crate::now`]'s clock,
/// read with no call (data-os C8). The kernel keeps the page mapped unless the process
/// unmaps it itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClockPage {
    address: u64,
}

impl ClockPage {
    /// Maps the page (once a process; a second call finds the same page).
    pub fn map() -> Result<ClockPage> {
        clock_page().map(|address| ClockPage { address })
    }

    /// Its address.
    pub fn address(&self) -> u64 {
        self.address
    }

    /// The page's fields, read under its sequence.
    pub fn reading(&self) -> ClockReading {
        // SAFETY: the kernel mapped the whole page readable at `address`, aligned to a
        // page, and keeps it there (see the type).
        unsafe { read_page(self.address as *const u8) }
    }

    /// Nanoseconds since the clock started, as `clock_get` returns them.
    #[cfg(target_arch = "x86_64")]
    pub fn now(&self) -> u64 {
        self.reading().nanos_at(tsc())
    }
}

/// Reads the clock page at `base`, retrying while the kernel writes it: the sequence,
/// the fields, the sequence again, which must be the same and even.
///
/// # Safety
///
/// `base` must point at `clock_page::LEN` readable bytes aligned to 8, for the call.
unsafe fn read_page(base: *const u8) -> ClockReading {
    // SAFETY: each offset is inside the page and aligned to its word (the caller's
    // contract and `clock_page`'s layout); the words are only ever read here.
    let word = |at: usize| unsafe { &*(base.add(at) as *const AtomicU64) };
    // SAFETY: as for `word`.
    let shift = unsafe { &*(base.add(page::SHIFT) as *const AtomicU32) };
    loop {
        let before = word(page::SEQUENCE).load(Ordering::Acquire);
        let reading = ClockReading {
            base_ns: word(page::BASE_NS).load(Ordering::Relaxed),
            base_tsc: word(page::BASE_TSC).load(Ordering::Relaxed),
            multiplier: word(page::MULTIPLIER).load(Ordering::Relaxed),
            shift: shift.load(Ordering::Relaxed),
        };
        fence(Ordering::Acquire);
        let after = word(page::SEQUENCE).load(Ordering::Relaxed);
        if before == after && before & 1 == 0 {
            return reading;
        }
        core::hint::spin_loop();
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
        assert_eq!(r.nanos_at(600), 1_000 + ((100 * 3) >> 1));
        assert_eq!(
            ClockReading::from_bytes(3, &bytes, 3),
            None,
            "odd: being written"
        );
        assert_eq!(ClockReading::from_bytes(2, &bytes, 4), None, "moved");
    }

    /// A page's words, as the kernel lays them out (`clock_page`).
    fn page_words(sequence: u64) -> [u64; page::LEN / 8] {
        let mut words = [0u64; page::LEN / 8];
        words[page::SEQUENCE / 8] = sequence;
        words[page::BASE_NS / 8] = 7;
        words[page::BASE_TSC / 8] = 1_000;
        words[page::MULTIPLIER / 8] = 3 << 40;
        words[page::SHIFT / 8] = 40;
        words
    }

    #[test]
    fn the_page_is_read_in_place_under_its_sequence() {
        let words = page_words(2);
        // SAFETY: a whole page of words, aligned to 8, alive for the call.
        let r = unsafe { read_page(words.as_ptr() as *const u8) };
        assert_eq!(
            r,
            ClockReading {
                base_ns: 7,
                base_tsc: 1_000,
                multiplier: 3 << 40,
                shift: 40,
            }
        );
        // Three nanoseconds a tick, from 7 ns at tick 1000.
        assert_eq!(r.nanos_at(1_010), 37);
        // A counter read before the base (another CPU's, a little behind) wraps as the
        // kernel's own clock does, rather than panicking.
        let _ = r.nanos_at(999);
    }

    #[test]
    fn a_reading_has_the_bytes_and_the_words_agree() {
        let words = page_words(4);
        let mut bytes = [0u8; page::LEN];
        for (i, w) in words.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        // SAFETY: as above.
        let in_place = unsafe { read_page(words.as_ptr() as *const u8) };
        assert_eq!(ClockReading::from_bytes(4, &bytes, 4), Some(in_place));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_counter_and_the_cpu_are_read_with_no_call() {
        let a = tsc();
        let b = tsc();
        assert!(b >= a, "one CPU's counter does not go back");
        // Linux numbers CPUs in the low twelve bits of IA32_TSC_AUX, Moruna in all of it.
        assert!(current_cpu() & 0xfff < 4096);
    }
}
