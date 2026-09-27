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
