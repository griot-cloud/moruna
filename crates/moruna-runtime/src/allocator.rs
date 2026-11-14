//! The process allocator, told to give freed memory back (F8.9).
//!
//! Everything outside the arena is allocated by the platform's allocator, and the ceiling is
//! about the whole process, so what matters is not only what is allocated but what a free gives
//! back. glibc keeps a heap per thread (up to eight per core) and serves a request from its heap
//! unless it is at least the mmap threshold, which starts at 128 KiB and climbs, each time a
//! mapped buffer is freed, to as much as 32 MiB; after that a batch of a few mebibytes freed by
//! any of a plan's threads stays in that thread's heap. Measured under glibc at 256 MiB, a
//! grouped aggregation that spills held 30 MB more than on macOS, whose allocator returns large
//! freed buffers at once. The runtime fixes the threshold, which stops it climbing, so every
//! buffer of 256 KiB or more is mapped on its own and unmapped when it is freed, and holds glibc
//! to four heaps. On every other platform this is nothing.
//!
//! What the heaps keep at their tops (2026-11-14) is the other half. glibc trims a heap's top
//! once it has more than 128 KiB free there and grows it a page at a time, so the buffers below
//! the threshold, taken and freed morsel after morsel, were faulted in and zeroed by the kernel
//! each time: on the benchmark VM (Linux, 8 CPUs) 21 of Occupancy's 53 CPU-seconds and 14 of
//! Enrich's 45 were the kernel's, against 8 and 2 for the plain loop. With the threshold left at
//! 256 KiB and the tops kept, Occupancy took 5.4 s, not 7.4, with 0.6 million page faults, not
//! 6.9; Enrich 5.5 s, not 6.2. [`keep_heap_tops`] keeps up to a sixteenth of the ceiling free at a
//! heap's top and grows a heap in steps of a sixty-fourth of it (at most 64 MiB): what is kept is
//! a share of the budget, and resident, and the controller counts it as it counts any resident
//! memory (11 f.3), so a small budget keeps little and F8.9's peaks stand.

/// Buffers at least this large are mapped on their own and returned to the kernel when freed.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const MMAP_THRESHOLD_BYTES: libc::c_int = 256 << 10;

/// The most heaps glibc keeps.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const ARENA_MAX: libc::c_int = 4;

/// Set once per process, before the first run's baseline is sampled.
pub(crate) fn give_freed_memory_back() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        // SAFETY: `mallopt` takes two integers and changes allocator parameters; glibc
        // documents both options as safe to set at any time from any thread.
        unsafe {
            libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD_BYTES);
            libc::mallopt(libc::M_ARENA_MAX, ARENA_MAX);
        }
    });
}

/// The least kept free at a heap's top, whatever the ceiling.
const TRIM_MIN: u64 = 1 << 20;
/// The most: `mallopt` takes an `int`.
const TRIM_MAX: u64 = i32::MAX as u64;
/// The largest step a heap grows by.
const PAD_MAX: u64 = 64 << 20;

/// What [`keep_heap_tops`] keeps for a run under `ceiling`: the bytes free at a heap's top that
/// stay, and the step a heap grows by.
pub(crate) fn heap_tops_for(ceiling: u64) -> (u64, u64) {
    let trim = (ceiling / 16).clamp(TRIM_MIN, TRIM_MAX);
    let pad = (ceiling / 64).min(PAD_MAX);
    (trim, pad)
}

/// Keep what a heap frees at its top for the next morsel, for a run under `ceiling` (2026-11-14).
/// `Some((trim, pad))` where it was set, on glibc; `None` elsewhere.
pub(crate) fn keep_heap_tops(ceiling: u64) -> Option<(u64, u64)> {
    let (trim, pad) = heap_tops_for(ceiling);
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let as_int = |bytes: u64| libc::c_int::try_from(bytes).unwrap_or(libc::c_int::MAX);
        // SAFETY: as in `give_freed_memory_back`: `mallopt` changes allocator parameters, and
        // glibc documents both options as safe to set at any time from any thread.
        let set = unsafe {
            libc::mallopt(libc::M_TRIM_THRESHOLD, as_int(trim)) == 1
                && libc::mallopt(libc::M_TOP_PAD, as_int(pad)) == 1
        };
        set.then_some((trim, pad))
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = (trim, pad);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_heap_tops_kept_are_a_share_of_the_ceiling() {
        assert_eq!(heap_tops_for(256 << 20), (16 << 20, 4 << 20));
        assert_eq!(heap_tops_for(24 << 30), (1536 << 20, PAD_MAX));
        assert_eq!(heap_tops_for(4 << 20), (TRIM_MIN, 64 << 10));
        assert_eq!(heap_tops_for(u64::MAX).0, TRIM_MAX);
    }

    #[test]
    fn keep_heap_tops_is_set_only_on_glibc() {
        let kept = keep_heap_tops(8 << 30);
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            assert_eq!(kept, Some(heap_tops_for(8 << 30)));
        } else {
            assert_eq!(kept, None);
        }
    }
}
