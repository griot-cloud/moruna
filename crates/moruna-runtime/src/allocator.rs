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

/// Moruna's own large buffers kept for reuse while a run lasts (2026-11-14).
///
/// What the run allocates in Rust outside the arena (a source's decode buffers, a sink's encode
/// and compression buffers, the adapters' copies) comes from the process allocator. Under F8.9
/// glibc maps every block of 256 KiB or more on its own and unmaps it on `free`, and musl (the
/// engine on the microkernel) does the same, so buffers taken and freed morsel after morsel were
/// faulted in and zeroed by the kernel each time: on the benchmark VM the kernel's time was 17%
/// of Moruna's Enrich CPU against 6% for the plain loop, whose pyarrow keeps a pool. A program
/// that installs [`crate::check::warmup::CountingAllocator`] (the Python extension, the engine
/// binary) allocates such blocks at a class size, the next of 1, 1.25, 1.5 and 1.75 times a power
/// of two, so a freed block serves any later request of its class; while a run lasts, a freed
/// block is kept, up to a sixteenth of the run's ceiling, and the run's end gives every kept
/// block back. Outside a run nothing is kept. It is Moruna's own code, so it is the same on
/// every C library; F8.9's concern, a process allocator holding freed memory past the ceiling
/// (mimalloc held 1.5 to 4.7 times it), is met by the bound and by the run's end.
pub(crate) mod reuse {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The smallest block allocated at a class size.
    pub(crate) const MIN: usize = 256 << 10;
    /// The largest alignment a class block is given.
    const ALIGN: usize = 16;
    /// Classes per power of two.
    const STEPS: usize = 4;
    /// Classes up to 2^63.
    const CLASSES: usize = 64 * STEPS;
    /// The most blocks a class keeps: its list then never grows past 32 KiB, so growing it never
    /// asks for a block of a class, whose lock this thread may hold.
    const PER_CLASS: usize = 4096;

    /// What the cache may hold now: zero outside a run.
    static LIMIT: AtomicU64 = AtomicU64::new(0);
    /// What it holds.
    static HELD: AtomicU64 = AtomicU64::new(0);
    /// Requests it served, for the report.
    static HITS: AtomicU64 = AtomicU64::new(0);
    #[allow(clippy::declare_interior_mutable_const)]
    const EMPTY: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    static KEPT: [Mutex<Vec<usize>>; CLASSES] = [EMPTY; CLASSES];

    /// The class and capacity of a block of `size` bytes and `align`, when it is one.
    pub(crate) fn class_of(size: usize, align: usize) -> Option<(usize, usize)> {
        if size < MIN || align > ALIGN {
            return None;
        }
        let power = (usize::BITS - 1 - size.leading_zeros()) as usize;
        let base = 1usize << power;
        (0..=STEPS).find_map(|step| {
            let capacity = base.checked_add((base / STEPS) * step)?;
            (capacity >= size).then_some((power * STEPS + step, capacity))
        })
    }

    fn capacity_of(class: usize) -> usize {
        let base = 1usize << (class / STEPS);
        base + (base / STEPS) * (class % STEPS)
    }

    /// A block for `layout` when it is of a class: a kept one, else a fresh one of the class's
    /// capacity. `None` when it is not of a class (the caller allocates it as asked).
    ///
    /// # Safety
    /// As `GlobalAlloc::alloc`.
    pub(crate) unsafe fn alloc(layout: Layout, zeroed: bool) -> Option<*mut u8> {
        let (class, capacity) = class_of(layout.size(), layout.align())?;
        let kept = KEPT[class].lock().unwrap_or_else(|e| e.into_inner()).pop();
        if let Some(block) = kept {
            HELD.fetch_sub(capacity as u64, Ordering::AcqRel);
            HITS.fetch_add(1, Ordering::Relaxed);
            let block = block as *mut u8;
            if zeroed {
                // SAFETY: a kept block of `capacity >= layout.size()` bytes nobody holds.
                unsafe { block.write_bytes(0, layout.size()) };
            }
            return Some(block);
        }
        let whole = Layout::from_size_align(capacity, layout.align()).ok()?;
        // SAFETY: a non-zero size and a valid alignment.
        Some(unsafe {
            if zeroed {
                System.alloc_zeroed(whole)
            } else {
                System.alloc(whole)
            }
        })
    }

    /// Free `ptr` of `layout` when it is of a class: kept while a run allows, else given back.
    /// False when it is not of a class (the caller frees it as asked).
    ///
    /// # Safety
    /// As `GlobalAlloc::dealloc`, for a block [`alloc`] gave.
    pub(crate) unsafe fn dealloc(ptr: *mut u8, layout: Layout) -> bool {
        let Some((class, capacity)) = class_of(layout.size(), layout.align()) else {
            return false;
        };
        let room = {
            let mut kept = KEPT[class].lock().unwrap_or_else(|e| e.into_inner());
            let room = kept.len() < PER_CLASS
                && HELD
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                        let limit = LIMIT.load(Ordering::Acquire);
                        (held + capacity as u64 <= limit).then_some(held + capacity as u64)
                    })
                    .is_ok();
            if room {
                kept.push(ptr as usize);
            }
            room
        };
        if !room && let Ok(whole) = Layout::from_size_align(capacity, layout.align()) {
            // SAFETY: the block `alloc` made at the class's capacity.
            unsafe { System.dealloc(ptr, whole) };
        }
        true
    }

    /// Whether a block of `old` stays where it is when resized to `new` bytes: both of the same
    /// class.
    pub(crate) fn same_class(old: Layout, new: usize) -> Option<bool> {
        match (
            class_of(old.size(), old.align()),
            class_of(new, old.align()),
        ) {
            (None, None) => None,
            (Some((a, _)), Some((b, _))) => Some(a == b),
            _ => Some(false),
        }
    }

    /// Keep freed blocks up to `bytes` from now: a run has started.
    pub(crate) fn keep_up_to(bytes: u64) {
        LIMIT.store(bytes, Ordering::Release);
    }

    /// Keep nothing more and give every kept block back: the run has ended. Returns the requests
    /// the kept blocks served since the last end.
    pub(crate) fn end() -> u64 {
        LIMIT.store(0, Ordering::Release);
        for (class, kept) in KEPT.iter().enumerate() {
            let blocks = std::mem::take(&mut *kept.lock().unwrap_or_else(|e| e.into_inner()));
            let capacity = capacity_of(class);
            for block in blocks {
                HELD.fetch_sub(capacity as u64, Ordering::AcqRel);
                if let Ok(whole) = Layout::from_size_align(capacity, ALIGN) {
                    // SAFETY: a kept block `alloc` made at this capacity; an alignment of at most
                    // `ALIGN` frees the same way (`free`) on every platform `System` serves.
                    unsafe { System.dealloc(block as *mut u8, whole) };
                }
            }
        }
        HITS.swap(0, Ordering::AcqRel)
    }

    /// What the cache holds now.
    #[cfg(test)]
    pub(crate) fn held() -> u64 {
        HELD.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod reuse_tests {
    use super::reuse::*;
    use std::alloc::Layout;

    #[test]
    fn a_class_holds_its_request_and_wastes_a_quarter_at_most() {
        assert_eq!(class_of(MIN - 1, 8), None);
        assert_eq!(
            class_of(MIN, 64),
            None,
            "an alignment above 16 is not a class's"
        );
        for size in [MIN, MIN + 1, 300 << 10, 1 << 20, 7_000_000, 1 << 30] {
            let (class, capacity) = class_of(size, 8).expect("a class");
            assert!(
                capacity >= size && capacity <= size + size / 4 + 1,
                "{size}"
            );
            assert_eq!(class_of(capacity, 8).map(|(c, _)| c), Some(class));
        }
        // 1 MiB + 1 is of the 1.25 MiB class, as is 1.2 MiB.
        assert_eq!(
            same_class(
                Layout::from_size_align((1 << 20) + 1, 8).unwrap(),
                1_200_000
            ),
            Some(true)
        );
        assert_eq!(
            same_class(Layout::from_size_align(1 << 20, 8).unwrap(), 2 << 20),
            Some(false)
        );
        assert_eq!(
            same_class(Layout::from_size_align(100, 8).unwrap(), 200),
            None
        );
    }

    #[test]
    fn blocks_are_kept_only_while_a_run_allows_and_all_go_back_at_its_end() {
        let layout = Layout::from_size_align(1 << 20, 8).unwrap();
        // SAFETY: blocks this module made, each freed once.
        unsafe {
            let a = alloc(layout, false).expect("a class block");
            assert!(!a.is_null());
            assert!(dealloc(a, layout));
            assert_eq!(held(), 0, "outside a run nothing is kept");
            keep_up_to(8 << 20);
            let b = alloc(layout, false).expect("a class block");
            assert!(dealloc(b, layout));
            assert!(held() > 0, "kept while the run allows");
            let c = alloc(layout, true).expect("served from the kept block");
            assert_eq!(c, b);
            assert!(
                std::slice::from_raw_parts(c, layout.size())
                    .iter()
                    .all(|&x| x == 0)
            );
            assert!(dealloc(c, layout));
            assert!(end() >= 1);
            assert_eq!(held(), 0, "the run's end gives every block back");
        }
    }
}
