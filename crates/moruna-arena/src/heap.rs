//! The C heap's handling of freed memory (02 f.9, 2026-11-14).
//!
//! The arena holds the batches; everything else a run allocates comes from the C heap: a source's
//! decode and a sink's encode buffers, the adapters' copies, and a Python kernel's NumPy arrays,
//! which NumPy takes from `malloc` through the allocator guard (05 f.11). glibc's heap hands a
//! block above its mmap threshold (dynamic, from 128 KiB) to the operating system on every `free`
//! and maps it fresh on the next `malloc`, and returns the top of its heaps above the trim
//! threshold, so a run that frees and takes the same buffers morsel after morsel pays a page fault
//! and the kernel's zeroing of every page each time. On the benchmark VM (Linux, 8 CPUs) that was
//! 21 of Occupancy's 53 CPU-seconds and 14 of Enrich's 45, against 8 and 2 for the plain loop,
//! whose pyarrow allocates from its own pool. Keeping freed memory for reuse took Occupancy from
//! 7.4 s to 5.4 s and its page faults from 6.9 to 0.6 million; Enrich from 6.2 s to 5.5 s.
//!
//! [`keep_freed`] fixes the mmap threshold at 32 MiB, the most glibc allows, so blocks below it
//! stay in the heap; raises the trim threshold to an eighth of the ceiling (bounded), so what a
//! run frees between morsels stays for the next one rather than going back to the system, and
//! what it keeps is a share of the budget the controller already counts as resident (11 f.3);
//! and pads each heap extension, so a heap grows in steps rather than a page at a time.

/// What [`keep_freed`] set, for the run's notes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HeapPolicy {
    /// Blocks below this many bytes come from the heap and stay in it when freed.
    pub mmap_threshold: u64,
    /// Free memory at the top of a heap is given back to the system only above this.
    pub trim_threshold: u64,
    /// Each heap extension asks for this much more than it needs.
    pub top_pad: u64,
}

/// glibc's largest mmap threshold on a 64-bit host (`DEFAULT_MMAP_THRESHOLD_MAX`).
pub const MMAP_THRESHOLD: u64 = 32 << 20;
/// The pad of every heap extension.
pub const TOP_PAD: u64 = 64 << 20;
/// The smallest trim threshold, whatever the ceiling.
const TRIM_MIN: u64 = 16 << 20;
/// The largest trim threshold: `mallopt` takes an `int`.
const TRIM_MAX: u64 = i32::MAX as u64;

/// The trim threshold for a run under `ceiling`: an eighth of it, between 16 MiB and the most
/// `mallopt` accepts.
pub fn trim_for(ceiling: u64) -> u64 {
    (ceiling / 8).clamp(TRIM_MIN, TRIM_MAX)
}

/// Keep what the C heap frees for reuse, for a run under `ceiling`. `None` where the heap is
/// not glibc's, or where it refused a parameter (each is set or none is reported).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn keep_freed(ceiling: u64) -> Option<HeapPolicy> {
    let policy = HeapPolicy {
        mmap_threshold: MMAP_THRESHOLD,
        trim_threshold: trim_for(ceiling),
        top_pad: TOP_PAD,
    };
    let as_int = |bytes: u64| libc::c_int::try_from(bytes).unwrap_or(libc::c_int::MAX);
    // SAFETY: `mallopt` sets a parameter of the C heap and touches no memory of the caller's;
    // each value is in the range glibc documents for it, and the call is thread-safe (it takes
    // every arena's lock).
    let set = unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, as_int(policy.mmap_threshold)) == 1
            && libc::mallopt(libc::M_TRIM_THRESHOLD, as_int(policy.trim_threshold)) == 1
            && libc::mallopt(libc::M_TOP_PAD, as_int(policy.top_pad)) == 1
    };
    set.then_some(policy)
}

/// Keep what the C heap frees for reuse: not glibc's heap here, so nothing is set.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn keep_freed(_ceiling: u64) -> Option<HeapPolicy> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trim_threshold_is_an_eighth_of_the_ceiling_within_bounds() {
        assert_eq!(trim_for(1 << 30), 128 << 20);
        assert_eq!(trim_for(64 << 20), TRIM_MIN);
        assert_eq!(trim_for(64 << 30), TRIM_MAX);
    }

    #[test]
    fn keep_freed_reports_what_it_set_where_the_heap_is_glibc() {
        let policy = keep_freed(8 << 30);
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            assert_eq!(
                policy,
                Some(HeapPolicy {
                    mmap_threshold: MMAP_THRESHOLD,
                    trim_threshold: 1 << 30,
                    top_pad: TOP_PAD,
                })
            );
        } else {
            assert_eq!(policy, None);
        }
    }
}
