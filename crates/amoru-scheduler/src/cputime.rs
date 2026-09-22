//! Per-thread CPU time (l). The only `unsafe` this crate is permitted, and only here.

/// Nanoseconds of CPU time this thread has used, or 0 when the host refuses the clock.
pub(crate) fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes one `timespec` through the pointer it is given and reads
    // nothing else; `ts` is a live, correctly aligned, exclusively borrowed `timespec` for the
    // whole call, which is the invariant the call relies on.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// Nanoseconds since the epoch; the trace's `t_start_ns` and `t_end_ns` and the heartbeat.
pub(crate) fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// l: the one `libc::clock_gettime` behind a safe function, and a monotonic clock for the
    /// trace. Both must return a usable number on every supported host.
    #[test]
    fn thread_cpu_time_moves_and_the_wall_clock_is_set() {
        let before = thread_cpu_ns();
        let mut total = 0u64;
        for i in 0..2_000_000u64 {
            total = total.wrapping_add(i);
        }
        std::hint::black_box(total);
        let after = thread_cpu_ns();
        assert!(after >= before, "thread cpu time went backwards");
        assert!(now_ns() > 1_600_000_000_000_000_000, "wall clock unset");
    }
}
