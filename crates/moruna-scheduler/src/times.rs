//! Stream P: where each worker's time goes, for the run's report. A worker decodes a source's
//! batches and encodes a sink's (each offloaded by its drive, RB1), runs the functions' units,
//! idles when nothing is ready, sits parked when the controller has not made it active, and
//! runs the jobs of a stage's start and end. The drives' own threads decode and encode a job no
//! worker would take. Monotonic timers, always on: a few clock reads a unit.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// One thread's time, in nanoseconds, by what it did.
#[derive(Debug, Default)]
pub(crate) struct WorkerTimes {
    pub(crate) decode: AtomicU64,
    pub(crate) encode: AtomicU64,
    pub(crate) functions: AtomicU64,
    pub(crate) idle: AtomicU64,
    pub(crate) parked: AtomicU64,
    pub(crate) jobs: AtomicU64,
}

impl WorkerTimes {
    fn seconds(&self) -> [f64; 6] {
        [
            &self.decode,
            &self.encode,
            &self.functions,
            &self.idle,
            &self.parked,
            &self.jobs,
        ]
        .map(|t| t.load(Ordering::Relaxed) as f64 / 1e9)
    }
}

/// Every worker's time, and the drives'.
#[derive(Debug)]
pub(crate) struct Times {
    workers: Vec<WorkerTimes>,
    drives: WorkerTimes,
}

std::thread_local! {
    /// The worker this thread is, set as its loop starts.
    static WORKER: Cell<Option<u16>> = const { Cell::new(None) };
}

/// Nanoseconds since `start`.
pub(crate) fn since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Times {
    pub(crate) fn new(workers: usize) -> Times {
        Times {
            workers: (0..workers).map(|_| WorkerTimes::default()).collect(),
            drives: WorkerTimes::default(),
        }
    }

    /// This thread is worker `worker` from now on.
    pub(crate) fn mark(worker: u16) {
        WORKER.with(|w| w.set(Some(worker)));
    }

    /// The times of worker `worker`.
    pub(crate) fn of(&self, worker: u16) -> &WorkerTimes {
        self.workers
            .get(usize::from(worker))
            .unwrap_or(&self.drives)
    }

    /// The times of the thread this runs on: its worker's, or the drives'.
    pub(crate) fn here(&self) -> &WorkerTimes {
        match WORKER.with(Cell::get) {
            Some(worker) => self.of(worker),
            None => &self.drives,
        }
    }

    /// The report's notes: one line a worker that did anything, and one for the drives when
    /// they decoded or encoded themselves.
    pub(crate) fn notes(&self) -> Vec<String> {
        let line = |who: String, s: [f64; 6]| {
            format!(
                "{who}: decode {:.2} s, encode {:.2} s, functions {:.2} s, idle {:.2} s, parked {:.2} s, start and end {:.2} s",
                s[0], s[1], s[2], s[3], s[4], s[5]
            )
        };
        let mut notes: Vec<String> = self
            .workers
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.seconds()))
            .filter(|(_, s)| s.iter().any(|v| *v > 0.0))
            .map(|(i, s)| line(format!("engine worker {i}"), s))
            .collect();
        let drives = self.drives.seconds();
        if drives[0] > 0.0 || drives[1] > 0.0 {
            notes.push(format!(
                "engine drives (a decode or encode no worker took): decode {:.2} s, encode {:.2} s",
                drives[0], drives[1]
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sc_p_a_thread_s_time_goes_to_its_worker_or_the_drives() {
        let times = std::sync::Arc::new(Times::new(2));
        times
            .here()
            .decode
            .fetch_add(3_000_000_000, Ordering::Relaxed);
        let on_worker = std::sync::Arc::clone(&times);
        std::thread::spawn(move || {
            Times::mark(1);
            on_worker
                .here()
                .encode
                .fetch_add(1_500_000_000, Ordering::Relaxed);
            on_worker
                .of(1)
                .idle
                .fetch_add(250_000_000, Ordering::Relaxed);
        })
        .join()
        .unwrap();
        let notes = times.notes();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(
            notes[0],
            "engine worker 1: decode 0.00 s, encode 1.50 s, functions 0.00 s, idle 0.25 s, parked 0.00 s, start and end 0.00 s"
        );
        assert!(notes[1].contains("decode 3.00 s"), "{notes:?}");
        assert!(since(Instant::now()) < 1_000_000_000);
    }
}
