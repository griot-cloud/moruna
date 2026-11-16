//! Where each source's and each sink's time went (the gate's split, `arrow-core-0.7.md` §9):
//! a source's reads apart from its decodes, a sink's encodes apart from its writes.
//!
//! A source that decodes on workers (RB1) fetches its bytes on the drive and decodes them on a
//! worker, and a sink that encodes on workers encodes on a worker and puts the bytes on the
//! drive: each half is timed on its own. A source or a sink that does both in one call is timed
//! as one, all of it its I/O, with no CPU half.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// One source's or sink's times, as the run's report reads them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndTimes {
    /// Reads (a source) or writes (a sink) finished, failed ones included.
    pub operations: u64,
    /// Nanoseconds of its I/O: each read from its issue until its bytes were fetched, or each
    /// write from its put until it was done, summed over those in flight together. A read or a
    /// write done in one call with its decode or encode counts all of that call here. The
    /// probe's read and a resume's re-reads decode in their read, and count here too; an ordered
    /// sink's write includes its wait for the morsels before it.
    pub io_ns: u64,
    /// Nanoseconds decoding (a source) or encoding (a sink) on workers, summed; `None` when the
    /// end does both halves in one call, whose time is all in `io_ns`.
    pub codec_ns: Option<u64>,
}

/// Where each source's and each sink's time went, source 0 and sink 0 first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IoTimes {
    /// Each source's.
    pub sources: Vec<EndTimes>,
    /// Each sink's.
    pub sinks: Vec<EndTimes>,
}

/// One end's running times, shared by its drive and the workers that decode or encode for it.
#[derive(Debug, Default)]
pub(crate) struct Times {
    operations: AtomicU64,
    io_ns: AtomicU64,
    codec_ns: AtomicU64,
    /// Some decode or encode was timed apart from its I/O.
    apart: AtomicBool,
}

fn since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Times {
    /// I/O that started at `start` is over.
    pub(crate) fn io(&self, start: Instant) {
        self.io_ns.fetch_add(since(start), Ordering::Relaxed);
    }

    /// A decode or an encode that started at `start` is over.
    pub(crate) fn codec(&self, start: Instant) {
        self.codec_ns.fetch_add(since(start), Ordering::Relaxed);
        self.apart.store(true, Ordering::Relaxed);
    }

    /// A read or a write is over.
    pub(crate) fn done(&self) {
        self.operations.fetch_add(1, Ordering::Relaxed);
    }

    /// The times so far.
    pub(crate) fn snapshot(&self) -> EndTimes {
        EndTimes {
            operations: self.operations.load(Ordering::Relaxed),
            io_ns: self.io_ns.load(Ordering::Relaxed),
            codec_ns: self
                .apart
                .load(Ordering::Relaxed)
                .then(|| self.codec_ns.load(Ordering::Relaxed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn an_end_with_no_cpu_half_reports_none() {
        let t = Times::default();
        assert_eq!(t.snapshot(), EndTimes::default());
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(2));
        t.io(start);
        t.done();
        let s = t.snapshot();
        assert_eq!(s.operations, 1);
        assert!(s.io_ns >= 2_000_000, "{s:?}");
        assert_eq!(s.codec_ns, None, "nothing was timed apart");
    }

    #[test]
    fn each_half_is_timed_on_its_own() {
        let t = Times::default();
        let fetch = Instant::now();
        std::thread::sleep(Duration::from_millis(1));
        t.io(fetch);
        let decode = Instant::now();
        std::thread::sleep(Duration::from_millis(3));
        t.codec(decode);
        t.done();
        let s = t.snapshot();
        assert!(s.io_ns >= 1_000_000);
        assert!(s.codec_ns.unwrap() >= 3_000_000, "{s:?}");
        assert_eq!(s.operations, 1);
    }
}
