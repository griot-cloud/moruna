//! The worker time a unit used: the CPU time of the threads that ran it, measured as the
//! scheduler measures a worker's (SDD 10, `cputime`), so warm-up's worker time and a run's are
//! one quantity.

/// Nanoseconds of CPU time this thread has used; `None` when the host refuses the clock, so
/// that a refusal is never read as a unit that took no time.
#[cfg(unix)]
pub(crate) fn thread_cpu_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes one `timespec` through the pointer it is given and reads
    // nothing else; `ts` is a live, aligned, exclusively borrowed `timespec` for the whole call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (rc == 0).then(|| {
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64)
    })
}

/// No thread clock off Unix: worker time is not measured, and says so.
#[cfg(not(unix))]
pub(crate) fn thread_cpu_ns() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_thread_clock_moves_with_work() {
        let before = super::thread_cpu_ns().expect("a thread clock");
        let mut x = 0u64;
        for i in 0..5_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        assert!(super::thread_cpu_ns().expect("a thread clock") > before);
    }
}
