//! The data path's mechanisms (`architecture/data-os.md` §2, stream A), the part of them
//! that is arithmetic and so is tested on the host. The kernel's `clock.rs`, `datapath.rs`,
//! `memory.rs` and `vm.rs` call it.
//!
//! - [`clock`]: the scale from the time-stamp counter to nanoseconds, which the kernel's
//!   clock and the clock page share, and the page's bytes (C8);
//! - [`frames`]: an object's frames in aligned runs of a large page, falling back to
//!   single frames, and whether frames make one run (C4);
//! - [`windows`]: `windows_protect`'s list and flags (C1);
//! - [`place`]: an address at a given offset past a large page's boundary (C4).

#![no_std]
#![forbid(unsafe_code)]

/// The clock's scale and the clock page (`moruna_abi::clock_page`).
pub mod clock {
    use moruna_abi::clock_page;

    /// Counter ticks to nanoseconds: multiply by a multiplier, then shift right by this.
    pub const SHIFT: u32 = 40;

    /// The multiplier for a counter of `hz` ticks a second: nanoseconds a tick, scaled by
    /// `2^SHIFT`, rounded to the nearest; `None` for no counter (zero hertz).
    pub fn multiplier(hz: u64) -> Option<u64> {
        if hz == 0 {
            return None;
        }
        let scaled = (1_000_000_000u128 << SHIFT) + u128::from(hz) / 2;
        Some(u64::try_from(scaled / u128::from(hz)).unwrap_or(u64::MAX))
    }

    /// Nanoseconds for `ticks` at `multiplier`: the clock page's formula, as the page's
    /// reader computes it (`moruna_sys::datapath::ClockReading::nanos_at`).
    pub fn scale(ticks: u64, multiplier: u64) -> u64 {
        let ns = (u128::from(ticks) * u128::from(multiplier)) >> SHIFT;
        u64::try_from(ns).unwrap_or(u64::MAX - 1)
    }

    /// The bytes the clock page starts with, up to its last field: an even sequence (so it
    /// reads as written), zero nanoseconds at `origin`, and the scale.
    pub fn page(origin: u64, multiplier: u64) -> [u8; clock_page::SHIFT + 4] {
        let mut bytes = [0u8; clock_page::SHIFT + 4];
        let mut put = |at: usize, value: &[u8]| bytes[at..at + value.len()].copy_from_slice(value);
        put(clock_page::SEQUENCE, &2u64.to_le_bytes());
        put(clock_page::BASE_NS, &0u64.to_le_bytes());
        put(clock_page::BASE_TSC, &origin.to_le_bytes());
        put(clock_page::MULTIPLIER, &multiplier.to_le_bytes());
        put(clock_page::SHIFT, &SHIFT.to_le_bytes());
        bytes
    }

    /// The sequence [`page`] writes: even, so the page reads as written.
    pub const FIRST_SEQUENCE: u64 = 2;

    /// The realtime base (`clock_page::REALTIME_BASE`, ABI 2.8) of a wall clock that reads
    /// `utc_ns` when the monotonic clock reads `now`: UTC nanoseconds at the monotonic zero.
    /// `None` for a time at or before the monotonic clock's start, which no base says.
    pub fn realtime_base(utc_ns: u64, now: u64) -> Option<u64> {
        utc_ns.checked_sub(now).filter(|base| *base != 0)
    }

    /// The writes that give the page the realtime base `base` from `source`
    /// (`clock_page::realtime`) under its sequence, each an offset and the eight bytes there,
    /// in order: the sequence made odd, the base, the source, the sequence even again, so a
    /// reader sees both change or neither. `sequence` is the page's sequence now, even; it is
    /// `sequence + 2` after.
    pub fn realtime_writes(sequence: u64, base: u64, source: u64) -> [(usize, [u8; 8]); 4] {
        [
            (clock_page::SEQUENCE, (sequence + 1).to_le_bytes()),
            (clock_page::REALTIME_BASE, base.to_le_bytes()),
            (clock_page::REALTIME_SOURCE, source.to_le_bytes()),
            (clock_page::SEQUENCE, (sequence + 2).to_le_bytes()),
        ]
    }

    /// How long the kernel waits at boot for the CMOS clock to answer and hold still
    /// (`architecture/moruna-os-system.md`, section 1B: no wait without a deadline in boot
    /// code): 100 ms. A CMOS clock sets its update flag for at most about 2 ms a second, so
    /// one that is there answers well within it; a machine with none loses 100 ms of boot.
    pub const CMOS_WAIT_NS: u64 = 100_000_000;

    /// Looks at `ready`, pausing between looks, until it answers true or, after a look that
    /// answered false, the clock `now` reads `deadline` or later; whether it answered true.
    /// There is always at least one look, even past the deadline.
    pub fn wait_until(
        now: impl Fn() -> u64,
        deadline: u64,
        mut ready: impl FnMut() -> bool,
    ) -> bool {
        loop {
            if ready() {
                return true;
            }
            if now() >= deadline {
                return false;
            }
            core::hint::spin_loop();
        }
    }
}

/// An object's frames in runs of a large page.
pub mod frames {
    /// Pushes `pages` frames onto `out`: a run of `run` frames from `take_run` while a
    /// whole run remains and it has one, and single frames from `take_one` otherwise.
    /// With no run to be had it makes the same frames one at a time, so it fails only
    /// where `take_one` fails, with that error, and the frames pushed so far stay in `out`
    /// for the caller to drop.
    pub fn fill<F, R, E>(
        out: &mut impl Extend<F>,
        pages: usize,
        run: usize,
        mut take_run: impl FnMut() -> Option<R>,
        mut take_one: impl FnMut() -> Result<F, E>,
    ) -> Result<(), E>
    where
        R: IntoIterator<Item = F>,
    {
        let mut made = 0;
        while made < pages {
            let whole = run > 1 && pages - made >= run;
            match whole.then(&mut take_run).flatten() {
                Some(frames) => {
                    out.extend(frames);
                    made += run;
                }
                None => {
                    out.extend(core::iter::once(take_one()?));
                    made += 1;
                }
            }
        }
        Ok(())
    }

    /// Whether the physical addresses `paddrs` are one run: `run` pages of `page` bytes,
    /// contiguous and in order, the first aligned to the run's size.
    pub fn one_run(paddrs: impl IntoIterator<Item = u64>, run: usize, page: u64) -> bool {
        let size = run as u64 * page;
        let mut count = 0usize;
        let mut first = 0;
        for (i, paddr) in paddrs.into_iter().enumerate() {
            if i == 0 {
                if size == 0 || !paddr.is_multiple_of(size) {
                    return false;
                }
                first = paddr;
            } else if Some(paddr) != first.checked_add(i as u64 * page) {
                return false;
            }
            count += 1;
        }
        count == run
    }
}

/// `windows_protect`'s arguments.
pub mod windows {
    use moruna_abi::{Error, map, read_u64, windows};

    /// Whether `flags` is one the call takes: `map::READ`, or `map::READ | map::WRITE`.
    pub fn flags(flags: u64) -> Result<(), Error> {
        if flags == map::READ || flags == map::READ | map::WRITE {
            Ok(())
        } else {
            Err(Error::InvalidArgs)
        }
    }

    /// The byte length of `count` windows: `InvalidArgs` past `windows::MAX`.
    pub fn bytes(count: u64) -> Result<usize, Error> {
        match usize::try_from(count) {
            Ok(n) if n <= windows::MAX => Ok(n * windows::RANGE_SIZE),
            _ => Err(Error::InvalidArgs),
        }
    }

    /// The windows `bytes` holds, as `(address, length)`, onto `out`; `InvalidArgs` for a
    /// length that is not whole windows.
    pub fn parse(bytes: &[u8], out: &mut impl Extend<(u64, u64)>) -> Result<(), Error> {
        if !bytes.len().is_multiple_of(windows::RANGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        for entry in bytes.as_chunks::<{ windows::RANGE_SIZE }>().0 {
            let word = |at: usize| read_u64(entry, at).ok_or(Error::InvalidArgs);
            out.extend(core::iter::once((word(0)?, word(8)?)));
        }
        Ok(())
    }
}

/// Where a mapping goes.
pub mod place {
    /// The first address at or above `at` that is `phase` past a multiple of `align` (a
    /// power of two, `phase` below it); `None` past the end of the address space.
    pub fn aligned(at: u64, phase: u64, align: u64) -> Option<u64> {
        let here = (at & !(align - 1)) | phase;
        if here >= at {
            Some(here)
        } else {
            here.checked_add(align)
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::{vec, vec::Vec};

    use moruna_abi::{Error, clock_page, map, windows as abi_windows};

    use super::*;

    #[test]
    fn the_scale_turns_ticks_into_nanoseconds() {
        assert_eq!(clock::multiplier(0), None);
        // A 1 GHz counter: one nanosecond a tick, exactly.
        let m = clock::multiplier(1_000_000_000).unwrap();
        assert_eq!(m, 1 << clock::SHIFT);
        assert_eq!(clock::scale(123_456_789, m), 123_456_789);
        // A 2.5 GHz counter: 0.4 ns a tick. The rounded multiplier keeps an hour within
        // 10 ns (a part in 10^11), and a second within one.
        let m = clock::multiplier(2_500_000_000).unwrap();
        let ns = clock::scale(3_600 * 2_500_000_000u64, m);
        assert!(ns.abs_diff(3_600_000_000_000) <= 10, "{ns}");
        assert!(clock::scale(2_500_000_000, m).abs_diff(1_000_000_000) <= 1);
        // A slow counter still scales, and a huge count saturates rather than wrapping.
        assert_eq!(
            clock::scale(3, clock::multiplier(1_000).unwrap()),
            3_000_000
        );
        assert_eq!(clock::scale(u64::MAX, u64::MAX), u64::MAX - 1);
        // A counter faster than a tick a femtosecond still has a multiplier.
        assert!(clock::multiplier(u64::MAX).is_some());
    }

    #[test]
    fn a_wall_clock_s_base_is_its_time_at_the_monotonic_zero() {
        assert_eq!(
            clock::realtime_base(1_700_000_000_000_000_000, 5_000),
            Some(1_699_999_999_999_995_000)
        );
        assert_eq!(
            clock::realtime_base(10, 10),
            None,
            "the monotonic clock's start"
        );
        assert_eq!(
            clock::realtime_base(9, 10),
            None,
            "before the monotonic clock's start"
        );
        assert_eq!(clock::realtime_base(11, 10), Some(1));
    }

    fn word(page: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(page[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn a_new_base_and_its_source_change_together_between_an_odd_and_an_even_sequence() {
        let mut page = clock::page(1, 2).to_vec();
        page.resize(clock_page::LEN, 0);
        assert_eq!(word(&page, clock_page::SEQUENCE), clock::FIRST_SEQUENCE);
        assert_eq!(word(&page, clock_page::REALTIME_BASE), 0);
        assert_eq!(
            word(&page, clock_page::REALTIME_SOURCE),
            clock_page::realtime::UNSET,
            "the wall clock starts unset"
        );
        let mut sequence = clock::FIRST_SEQUENCE;
        // The machine's clock at boot, then `clock_set_realtime` again and again.
        for (base, source) in [
            (40, clock_page::realtime::MACHINE),
            (42, clock_page::realtime::SET),
            (43, clock_page::realtime::SET),
        ] {
            let mut seen = Vec::new();
            for (at, bytes) in clock::realtime_writes(sequence, base, source) {
                page[at..at + 8].copy_from_slice(&bytes);
                seen.push(word(&page, clock_page::SEQUENCE));
            }
            // Odd while the base and the source change, even after: a reader retries across.
            assert_eq!(
                seen,
                [sequence + 1, sequence + 1, sequence + 1, sequence + 2]
            );
            sequence += 2;
            assert_eq!(word(&page, clock_page::REALTIME_BASE), base);
            assert_eq!(word(&page, clock_page::REALTIME_SOURCE), source);
        }
        // The scale is untouched.
        assert_eq!(word(&page, clock_page::BASE_TSC), 1);
        assert_eq!(word(&page, clock_page::MULTIPLIER), 2);
    }

    #[test]
    fn the_page_reads_as_the_scale() {
        let m = clock::multiplier(3_000_000_000).unwrap();
        let bytes = clock::page(777, m);
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        assert_eq!(word(clock_page::SEQUENCE), 2);
        assert_eq!(word(clock_page::BASE_NS), 0);
        assert_eq!(word(clock_page::BASE_TSC), 777);
        assert_eq!(word(clock_page::MULTIPLIER), m);
        let shift = u32::from_le_bytes(
            bytes[clock_page::SHIFT..clock_page::SHIFT + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(shift, clock::SHIFT);
        // The reader's formula on the page's fields is the kernel's clock.
        let tsc = 777 + 3_000_000_000;
        let read = word(clock_page::BASE_NS)
            + ((u128::from(tsc - word(clock_page::BASE_TSC)) * u128::from(m)) >> shift) as u64;
        assert_eq!(read, clock::scale(tsc - 777, m));
        assert!(read.abs_diff(1_000_000_000) <= 1, "{read}");
    }

    /// A clock that reads `start` first and `step` nanoseconds more at each reading after.
    fn ticking(start: u64, step: u64) -> impl Fn() -> u64 {
        let next = core::cell::Cell::new(start);
        move || {
            let now = next.get();
            next.set(now.saturating_add(step));
            now
        }
    }

    #[test]
    fn a_cmos_clock_that_answers_is_read_well_within_its_deadline() {
        assert_eq!(clock::CMOS_WAIT_NS, 100_000_000);
        // An update in progress for 2 ms, the longest a CMOS clock takes, looked at every
        // 10 microseconds.
        let looks = core::cell::Cell::new(0u64);
        assert!(clock::wait_until(
            ticking(0, 10_000),
            clock::CMOS_WAIT_NS,
            || {
                looks.set(looks.get() + 1);
                looks.get() > 200
            }
        ));
        assert_eq!(looks.get(), 201);
        // Not updating at all: one look.
        let looks = core::cell::Cell::new(0);
        assert!(clock::wait_until(ticking(0, 1), 0, || {
            looks.set(looks.get() + 1);
            true
        }));
        assert_eq!(looks.get(), 1);
    }

    #[test]
    fn a_machine_with_no_cmos_clock_gives_up_at_the_deadline() {
        // Ports with nothing behind them read as all ones: always updating. With the
        // deadline 100 ms after a boot clock read at 5 s, the wait ends at the first look at
        // or past it.
        let start = 5_000_000_000;
        let looks = core::cell::Cell::new(0u64);
        assert!(!clock::wait_until(
            ticking(start, 1_000_000),
            start + clock::CMOS_WAIT_NS,
            || {
                looks.set(looks.get() + 1);
                false
            }
        ));
        assert_eq!(looks.get(), 101);
        // A deadline already passed: one look, and it ends.
        let looks = core::cell::Cell::new(0);
        assert!(!clock::wait_until(ticking(10, 1), 5, || {
            looks.set(looks.get() + 1);
            false
        }));
        assert_eq!(looks.get(), 1);
        // A clock that saturates still reaches the deadline.
        assert!(!clock::wait_until(
            ticking(u64::MAX - 1, 5),
            u64::MAX,
            || false
        ));
    }

    /// A frame allocator for the tests: runs while `runs` lasts, then none; single frames
    /// numbered from 1000, failing after `singles`.
    struct Frames {
        runs: usize,
        singles: usize,
        next_run: u64,
        next_one: u64,
    }

    impl Frames {
        fn new(runs: usize, singles: usize) -> Frames {
            Frames {
                runs,
                singles,
                next_run: 0,
                next_one: 1000,
            }
        }

        fn fill(&mut self, pages: usize, run: usize) -> (Vec<u64>, Result<(), Error>) {
            let mut out = Vec::new();
            let cell = core::cell::RefCell::new(self);
            let result = frames::fill(
                &mut out,
                pages,
                run,
                || {
                    let mut f = cell.borrow_mut();
                    if f.runs == 0 {
                        return None;
                    }
                    f.runs -= 1;
                    let first = f.next_run;
                    f.next_run += run as u64;
                    Some(first..first + run as u64)
                },
                || {
                    let mut f = cell.borrow_mut();
                    if f.singles == 0 {
                        return Err(Error::NoMemory);
                    }
                    f.singles -= 1;
                    f.next_one += 1;
                    Ok(f.next_one - 1)
                },
            );
            (out, result)
        }
    }

    #[test]
    fn frames_come_in_runs_while_runs_last() {
        // Two whole runs and a tail of three.
        let (out, result) = Frames::new(9, 9).fill(2 * 4 + 3, 4);
        assert_eq!(result, Ok(()));
        assert_eq!(out, vec![0, 1, 2, 3, 4, 5, 6, 7, 1000, 1001, 1002]);
    }

    #[test]
    fn with_no_run_free_every_frame_comes_alone() {
        // No run at all: the same number of frames, one at a time, as the old loop made.
        let (out, result) = Frames::new(0, 100).fill(10, 4);
        assert_eq!(result, Ok(()));
        assert_eq!(out, (1000..1010).collect::<Vec<_>>());
        // One run, then none: a run, then single frames for the rest.
        let (out, result) = Frames::new(1, 100).fill(10, 4);
        assert_eq!(result, Ok(()));
        assert_eq!(out, vec![0, 1, 2, 3, 1000, 1001, 1002, 1003, 1004, 1005]);
        // Fewer pages than a run never asks for one.
        let (out, result) = Frames::new(5, 2).fill(2, 4);
        assert_eq!(result, Ok(()));
        assert_eq!(out, vec![1000, 1001]);
        // A run of one page is a single frame.
        let (out, _) = Frames::new(5, 3).fill(3, 1);
        assert_eq!(out, vec![1000, 1001, 1002]);
    }

    #[test]
    fn it_fails_only_where_a_single_frame_fails() {
        // Exactly the pages: no failure. One more page than single frames: the error,
        // and what was made stays for the caller to drop.
        let (out, result) = Frames::new(0, 3).fill(3, 4);
        assert_eq!((out.len(), result), (3, Ok(())));
        let (out, result) = Frames::new(0, 3).fill(4, 4);
        assert_eq!(
            (out, result),
            (vec![1000, 1001, 1002], Err(Error::NoMemory))
        );
        let (out, result) = Frames::new(1, 0).fill(4, 4);
        assert_eq!((out.len(), result), (4, Ok(())));
        let (_, result) = Frames::new(0, 0).fill(0, 4);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn a_run_is_contiguous_aligned_and_whole() {
        let page = 4096;
        let run = |first: u64, n: usize| (0..n as u64).map(move |i| first + i * page);
        assert!(frames::one_run(run(0x20_0000, 512), 512, page));
        assert!(
            !frames::one_run(run(0x20_1000, 512), 512, page),
            "misaligned"
        );
        assert!(!frames::one_run(run(0x20_0000, 511), 512, page), "short");
        assert!(!frames::one_run(run(0x20_0000, 513), 512, page), "long");
        let gap = run(0x20_0000, 256).chain(run(0x20_0000 + 257 * page, 256));
        assert!(!frames::one_run(gap, 512, page), "a gap");
        assert!(!frames::one_run([], 512, page), "none");
        assert!(!frames::one_run(run(0, 1), 0, page), "a run of nothing");
        let wraps = [!(2 * page - 1), 0];
        assert!(!frames::one_run(wraps, 2, page), "past the top");
    }

    #[test]
    fn windows_take_read_or_read_write_and_whole_entries() {
        assert_eq!(windows::flags(map::READ), Ok(()));
        assert_eq!(windows::flags(map::READ | map::WRITE), Ok(()));
        for bad in [
            0,
            map::WRITE,
            map::READ | map::EXECUTE,
            map::READ | map::HUGE,
        ] {
            assert_eq!(windows::flags(bad), Err(Error::InvalidArgs), "{bad}");
        }
        assert_eq!(windows::bytes(0), Ok(0));
        assert_eq!(
            windows::bytes(abi_windows::MAX as u64),
            Ok(abi_windows::MAX * abi_windows::RANGE_SIZE)
        );
        assert_eq!(
            windows::bytes(abi_windows::MAX as u64 + 1),
            Err(Error::InvalidArgs)
        );
        assert_eq!(windows::bytes(u64::MAX), Err(Error::InvalidArgs));
        let mut bytes = Vec::new();
        for (a, l) in [(0x1000u64, 0x2000u64), (0x8000, 0x1000)] {
            bytes.extend_from_slice(&a.to_le_bytes());
            bytes.extend_from_slice(&l.to_le_bytes());
        }
        let mut out = Vec::new();
        assert_eq!(windows::parse(&bytes, &mut out), Ok(()));
        assert_eq!(out, vec![(0x1000, 0x2000), (0x8000, 0x1000)]);
        let mut out = Vec::new();
        assert_eq!(
            windows::parse(&bytes[..20], &mut out),
            Err(Error::InvalidArgs)
        );
    }

    #[test]
    fn placement_finds_the_offset_past_a_boundary() {
        let large = 2 << 20;
        assert_eq!(place::aligned(0x20_0000, 0, large), Some(0x20_0000));
        assert_eq!(place::aligned(0x20_0001, 0, large), Some(0x40_0000));
        assert_eq!(place::aligned(0x20_0000, 0x3000, large), Some(0x20_3000));
        assert_eq!(place::aligned(0x20_4000, 0x3000, large), Some(0x40_3000));
        assert_eq!(place::aligned(u64::MAX - 10, 0, large), None);
    }
}
