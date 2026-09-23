//! `FakeTrace`, the `TraceSink` and `TraceTail` fake of contracts d.15.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_kernel::{Result, StageId, TraceRecord, TraceSink, TraceTail};

struct Inner {
    records: Mutex<VecDeque<TraceRecord>>,
    capacity: usize,
    flush_calls: AtomicU64,
    finish_calls: AtomicU64,
}

/// The trace as a test sees it: records in memory, oldest dropped once `capacity` is reached.
///
/// Knobs: `capacity(n)`. Observables: `records()`, `flush_calls()`, `finish_calls()`.
///
/// `TraceSink` has `record` and `flush` only (d.13); `finish` belongs to the trace writer of
/// component 4, so `finish_calls()` counts this fake's own inherent `finish()` (escalation on
/// the pull request that adds this crate).
#[derive(Clone)]
pub struct FakeTrace {
    inner: Arc<Inner>,
}

impl Default for FakeTrace {
    fn default() -> Self {
        FakeTrace::new()
    }
}

impl FakeTrace {
    /// A trace that keeps the last 4096 records.
    pub fn new() -> FakeTrace {
        FakeTrace {
            inner: Arc::new(Inner {
                records: Mutex::new(VecDeque::new()),
                capacity: 4096,
                flush_calls: AtomicU64::new(0),
                finish_calls: AtomicU64::new(0),
            }),
        }
    }

    /// Knob: how many records to keep before the oldest is dropped.
    pub fn capacity(self, n: usize) -> FakeTrace {
        let records = std::mem::take(&mut *self.lock());
        let mut kept: VecDeque<TraceRecord> = records;
        while kept.len() > n {
            kept.pop_front();
        }
        FakeTrace {
            inner: Arc::new(Inner {
                records: Mutex::new(kept),
                capacity: n,
                flush_calls: AtomicU64::new(self.flush_calls()),
                finish_calls: AtomicU64::new(self.finish_calls()),
            }),
        }
    }

    /// Observable: every record kept, oldest first.
    pub fn records(&self) -> Vec<TraceRecord> {
        self.lock().iter().cloned().collect()
    }

    /// Observable: how many times `flush` was called.
    pub fn flush_calls(&self) -> u64 {
        self.inner.flush_calls.load(Ordering::SeqCst)
    }

    /// Observable: how many times this fake's `finish` was called.
    pub fn finish_calls(&self) -> u64 {
        self.inner.finish_calls.load(Ordering::SeqCst)
    }

    /// The finish the `TraceSink` trait does not have; counted by `finish_calls`.
    pub fn finish(&self) -> Vec<TraceRecord> {
        self.inner.finish_calls.fetch_add(1, Ordering::SeqCst);
        self.records()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<TraceRecord>> {
        self.inner.records.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl TraceSink for FakeTrace {
    fn record(&self, r: TraceRecord) {
        let mut records = self.lock();
        records.push_back(r);
        while records.len() > self.inner.capacity {
            records.pop_front();
        }
    }

    fn flush(&self) -> Result<()> {
        self.inner.flush_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl TraceTail for FakeTrace {
    fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord> {
        let records = self.lock();
        let matching: Vec<TraceRecord> = records
            .iter()
            .filter(|r| r.stage == stage)
            .cloned()
            .collect();
        let start = matching.len().saturating_sub(n);
        matching[start..].to_vec()
    }
}
