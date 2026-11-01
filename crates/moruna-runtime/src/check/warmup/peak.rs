//! The high-water marks a unit's peak memory is read from (SDD 24 p.4): what was in use at the
//! most, over the call, and never a reading before and after it.
//!
//! - **The arena**: `moruna_arena::Arena::host_peak`, exact, reset at the unit's start.
//! - **Moruna's counting allocator** ([`CountingAllocator`]): a program that installs it as its
//!   global allocator (the Python extension does, and the tests) has every heap byte the
//!   process allocates through Rust counted, exactly, with a mark reset at the unit's start.
//! - **The operating system's mark of the process**: on macOS the kernel ledger's
//!   `phys_footprint` high-water since `proc_reset_footprint_interval`; on Linux `VmHWM` since
//!   `/proc/self/clear_refs` was written `5` (resident memory, anonymous and file backed).
//! - **A packaged stage's child**: its cgroup's `memory.peak`, through
//!   `OutsideMemory::reset_peak` and `peak`.
//!
//! Each mark sees a different part of the memory a call uses, and none sees all of it: the
//! arena is pre-faulted, so its use never moves the process's mark; memory a C library's
//! allocator frees and reuses inside the process (macOS keeps freed large blocks resident) moves
//! neither the operating system's mark nor, when it is not Rust's, the counting allocator's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Process-wide markers cannot distinguish overlapping warm-up calls. Keep a short
/// coordination lock only while entering/leaving; never hold it across kernel code.
#[derive(Default)]
struct Intervals {
    active: usize,
    generation: u64,
}

static INTERVALS: Mutex<Intervals> = Mutex::new(Intervals {
    active: 0,
    generation: 0,
});

/// A marker interval. Overlap invalidates every participant, including the first one
/// after the other has left, so a later reset never masquerades as its original mark.
pub(crate) struct Interval {
    generation: u64,
    exclusive: bool,
    closed: bool,
}

impl Interval {
    /// Enter without blocking another warm-up's kernel invocation.
    pub(crate) fn begin() -> Result<Self, String> {
        let mut state = INTERVALS
            .lock()
            .map_err(|_| "warm-up marker state is poisoned")?;
        let generation = state
            .generation
            .checked_add(1)
            .ok_or("warm-up marker generation overflow")?;
        let active = state
            .active
            .checked_add(1)
            .ok_or("warm-up marker count overflow")?;
        let exclusive = state.active == 0;
        state.generation = generation;
        state.active = active;
        counting_arm();
        Ok(Self {
            generation,
            exclusive,
            closed: false,
        })
    }

    /// Whether no other interval has entered since this exclusive interval began.
    #[cfg(test)]
    pub(crate) fn valid(&self) -> Result<bool, String> {
        let state = INTERVALS
            .lock()
            .map_err(|_| "warm-up marker state is poisoned")?;
        Ok(self.exclusive && state.active == 1 && state.generation == self.generation)
    }

    /// Close the measurement and decide its validity in one atomic handoff. Later
    /// intervals may reset their marks without overlapping a completed measurement.
    pub(crate) fn finish(mut self) -> Result<bool, String> {
        let mut state = INTERVALS
            .lock()
            .map_err(|_| "warm-up marker state is poisoned")?;
        if state.active == 0 {
            return Err("warm-up marker interval count is broken".into());
        }
        let valid = self.exclusive && state.active == 1 && state.generation == self.generation;
        state.active -= 1;
        if state.active == 0 {
            counting_disarm();
        }
        self.closed = true;
        Ok(valid)
    }
}

impl Drop for Interval {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        match INTERVALS.lock() {
            Ok(mut state) if state.active > 0 => {
                state.active -= 1;
                if state.active == 0 {
                    counting_disarm();
                }
            }
            _ => {
                counting_disarm();
                tracing::error!(target: "warmup.peak", "warm-up marker interval accounting is broken");
            }
        }
    }
}

// Real marker fixtures share process-wide counters and must not reset one another.
#[cfg(test)]
pub(crate) static TEST_MARK_LOCK: Mutex<()> = Mutex::new(());

/// Whether the counting allocator counts: armed by warm-up for its length, so a program that
/// installs it pays one relaxed load per allocation otherwise.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Whether any allocation has gone through it: whether the program installed it.
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Bytes allocated less bytes freed while armed. Signed: a free of what was allocated before
/// arming takes it below zero, and only differences are read.
static LIVE: AtomicI64 = AtomicI64::new(0);
/// The most `LIVE` has been since the last [`counting_reset`].
static PEAK: AtomicI64 = AtomicI64::new(0);

/// The system allocator, counting the bytes in use and their high-water mark while warm-up
/// runs. A program opts in with `#[global_allocator] static A: CountingAllocator =
/// CountingAllocator;`; a library cannot install a global allocator for its embedder.
pub struct CountingAllocator;

fn counted(delta: i64) {
    if ARMED.load(Ordering::Relaxed) {
        let live = LIVE.fetch_add(delta, Ordering::AcqRel) + delta;
        if delta > 0 {
            PEAK.fetch_max(live, Ordering::AcqRel);
        }
    }
}

// SAFETY: every call is forwarded to `System` with the arguments it was given, so the
// allocator's contract is `System`'s; the counting touches only atomics.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        INSTALLED.store(true, Ordering::Relaxed);
        // SAFETY: forwarded unchanged; the caller upholds `alloc`'s contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            counted(layout.size() as i64);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        INSTALLED.store(true, Ordering::Relaxed);
        // SAFETY: forwarded unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            counted(layout.size() as i64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged; `ptr` was allocated by `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        counted(-(layout.size() as i64));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            counted(new_size as i64 - layout.size() as i64);
        }
        moved
    }
}

/// Whether the program installed [`CountingAllocator`].
pub(crate) fn counting_installed() -> bool {
    // An allocation is certain by the time warm-up asks: it has generated a batch.
    INSTALLED.load(Ordering::Relaxed)
}

/// Count from now.
pub(crate) fn counting_arm() {
    ARMED.store(true, Ordering::Release);
}

/// Stop counting.
pub(crate) fn counting_disarm() {
    ARMED.store(false, Ordering::Release);
}

/// Start a new mark at what is in use now; returns that baseline.
pub(crate) fn counting_reset() -> i64 {
    let live = LIVE.load(Ordering::Acquire);
    PEAK.store(live, Ordering::Release);
    PEAK.fetch_max(LIVE.load(Ordering::Acquire), Ordering::AcqRel);
    live
}

/// The mark since [`counting_reset`].
pub(crate) fn counting_peak() -> i64 {
    PEAK.load(Ordering::Acquire)
}

/// The operating system's high-water mark of this process.
#[derive(Debug)]
pub(crate) enum Process {
    /// macOS: `phys_footprint`'s interval peak.
    Footprint,
    /// Linux: `VmHWM`, reset through `clear_refs`.
    Resident,
    /// Controlled reset/read results for the real unit regression.
    #[cfg(test)]
    Controlled {
        reset: Result<u64, String>,
        peak: Result<u64, String>,
    },
}

impl Process {
    /// The mark this platform keeps, or why there is none.
    pub(crate) fn here() -> Result<Process, String> {
        if cfg!(target_vendor = "apple") {
            Ok(Process::Footprint)
        } else if cfg!(target_os = "linux") {
            Ok(Process::Resident)
        } else {
            Err("this platform keeps no resettable high-water mark of a process".into())
        }
    }

    /// What it measures, for the report.
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Process::Footprint => {
                "the process's phys_footprint high-water mark since \
                 proc_reset_footprint_interval"
            }
            Process::Resident => {
                "the process's VmHWM since clear_refs (resident, anonymous and file backed)"
            }
            #[cfg(test)]
            Process::Controlled { .. } => "the controlled process mark",
        }
    }

    /// Start a new mark; returns what the process holds now, the mark's baseline.
    pub(crate) fn reset(&self) -> Result<u64, String> {
        match self {
            Process::Footprint => footprint::reset(),
            Process::Resident => resident::reset(),
            #[cfg(test)]
            Process::Controlled { reset, .. } => reset.clone(),
        }
    }

    /// The mark since [`Process::reset`].
    pub(crate) fn peak(&self) -> Result<u64, String> {
        match self {
            Process::Footprint => footprint::peak(),
            Process::Resident => resident::peak(),
            #[cfg(test)]
            Process::Controlled { peak, .. } => peak.clone(),
        }
    }
}

#[cfg(target_vendor = "apple")]
mod footprint {
    unsafe extern "C" {
        /// libproc: start a new interval of `ri_interval_max_phys_footprint`. Exported by
        /// libSystem and declared in xnu's `libproc_internal.h`, not in the SDK's headers.
        fn proc_reset_footprint_interval(pid: libc::c_int) -> libc::c_int;
    }

    fn rusage() -> Result<libc::rusage_info_v4, String> {
        // SAFETY: `rusage_info_v4` is a C struct of integers; all zeros is a valid value, and
        // it is overwritten below before it is read.
        let mut info: libc::rusage_info_v4 = unsafe { core::mem::zeroed() };
        // SAFETY: `info` is a live `rusage_info_v4`, the struct `proc_pid_rusage` documents
        // for `RUSAGE_INFO_V4`; it writes only into it.
        let rc = unsafe {
            libc::proc_pid_rusage(
                std::process::id() as libc::c_int,
                libc::RUSAGE_INFO_V4,
                (&raw mut info).cast::<libc::rusage_info_t>(),
            )
        };
        if rc == 0 {
            Ok(info)
        } else {
            Err(format!(
                "proc_pid_rusage failed: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    pub(super) fn reset() -> Result<u64, String> {
        // SAFETY: takes a pid and touches nothing of this process's memory.
        let rc = unsafe { proc_reset_footprint_interval(std::process::id() as libc::c_int) };
        if rc != 0 {
            return Err(format!(
                "proc_reset_footprint_interval failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(rusage()?.ri_phys_footprint)
    }

    pub(super) fn peak() -> Result<u64, String> {
        Ok(rusage()?.ri_interval_max_phys_footprint)
    }
}

#[cfg(not(target_vendor = "apple"))]
mod footprint {
    pub(super) fn reset() -> Result<u64, String> {
        Err("phys_footprint is a macOS ledger".into())
    }

    pub(super) fn peak() -> Result<u64, String> {
        Err("phys_footprint is a macOS ledger".into())
    }
}

/// Linux's `VmHWM`. The parsing is plain text and is tested on every platform.
mod resident {
    /// A field of `/proc/self/status` in bytes (`VmHWM:   1234 kB`).
    pub(super) fn field(status: &str, name: &str) -> Result<u64, String> {
        let line = status
            .lines()
            .find(|l| l.starts_with(name) && l[name.len()..].starts_with(':'))
            .ok_or_else(|| format!("/proc/self/status has no {name}"))?;
        let mut words = line[name.len() + 1..].split_whitespace();
        let value: u64 = words
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("/proc/self/status: `{line}` has no number"))?;
        match words.next() {
            Some("kB") => Ok(value * 1024),
            other => Err(format!(
                "/proc/self/status: `{line}` is in {other:?}, not kB"
            )),
        }
    }

    fn status() -> Result<String, String> {
        std::fs::read_to_string("/proc/self/status").map_err(|e| format!("/proc/self/status: {e}"))
    }

    pub(super) fn reset() -> Result<u64, String> {
        std::fs::write("/proc/self/clear_refs", "5")
            .map_err(|e| format!("/proc/self/clear_refs: {e}"))?;
        field(&status()?, "VmRSS")
    }

    pub(super) fn peak() -> Result<u64, String> {
        field(&status()?, "VmHWM")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_fields_parse_in_bytes_and_refuse_otherwise() {
        let status = "Name:\tx\nVmHWM:\t    2048 kB\nVmRSS:\t  1024 kB\nVmBad:\t12 pages\n";
        assert_eq!(resident::field(status, "VmHWM"), Ok(2 << 20));
        assert_eq!(resident::field(status, "VmRSS"), Ok(1 << 20));
        assert!(
            resident::field(status, "VmSwap")
                .unwrap_err()
                .contains("no VmSwap")
        );
        assert!(
            resident::field(status, "VmBad")
                .unwrap_err()
                .contains("not kB")
        );
        assert!(
            resident::field("VmHWM: lots kB\n", "VmHWM")
                .unwrap_err()
                .contains("no number")
        );
        assert!(
            resident::field("VmHWMx: 1 kB\n", "VmHWM").is_err(),
            "a prefix is not the field"
        );
    }

    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    #[test]
    fn the_process_mark_sees_a_spike_that_is_gone() {
        let _serial = TEST_MARK_LOCK.lock().expect("marker fixture lock");
        let mark = Process::here().expect("a mark here");
        assert!(!mark.describe().is_empty());
        let before = mark.reset().expect("reset");
        let spike = 192usize << 20;
        // Fresh pages mapped and unmapped here, so the spike is gone when the mark is read and
        // no allocator's cache of freed memory can hide it.
        // SAFETY: an anonymous private mapping of `spike` bytes, written in full and unmapped
        // with the same length; nothing else refers to it.
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                spike,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            std::ptr::write_bytes(ptr.cast::<u8>(), 1, spike);
            assert_eq!(libc::munmap(ptr, spike), 0);
        }
        let peak = mark.peak().expect("peak");
        assert!(
            peak >= before + (spike as u64) * 9 / 10,
            "the mark {peak} holds the spike above {before}"
        );
    }

    #[test]
    fn the_counting_marks_reset_and_hold_a_spike() {
        let _serial = TEST_MARK_LOCK.lock().expect("marker fixture lock");
        counting_arm();
        let base = counting_reset();
        counted(1000);
        counted(-1000);
        assert_eq!(counting_peak(), base + 1000);
        let base = counting_reset();
        assert_eq!(counting_peak(), base);
        counting_disarm();
        counted(5000);
        assert_eq!(counting_peak(), base, "disarmed counts nothing");
    }

    #[test]
    fn cm_t68_overlapping_intervals_stay_invalid_until_both_leave() {
        let _serial = TEST_MARK_LOCK.lock().expect("marker fixture lock");
        let first = Interval::begin().expect("first interval");
        assert!(first.valid().expect("validity"));
        let second = Interval::begin().expect("second interval");
        assert!(!first.valid().expect("validity"));
        assert!(!second.valid().expect("validity"));
        assert!(!second.finish().expect("overlapping close"));
        assert!(
            !first.valid().expect("validity"),
            "a later reset cannot be undone"
        );
        assert!(
            ARMED.load(Ordering::Acquire),
            "the remaining interval still counts"
        );
        assert!(!first.finish().expect("overlapping close"));
        assert!(!ARMED.load(Ordering::Acquire));
        let independent = Interval::begin().expect("next independent interval");
        assert!(independent.valid().expect("validity"));
        assert!(independent.finish().expect("independent close"));
        let next = Interval::begin().expect("after completed measurement");
        assert!(next.finish().expect("independent handoff"));
        assert!(!ARMED.load(Ordering::Acquire));
    }
}
