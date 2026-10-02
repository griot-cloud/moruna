//! What each stage's kernel asked for, per source, from the records' `alloc` (04 d.1, f.2; E13).
//!
//! Brackly, 2026-10-02: the figures are a basis for judging a function's design, whose ideal
//! asks for little or nothing outside Arrow, so the report shows per function what it asked for
//! from each source the allocator guard counts and the share of its peak that was not Arrow.

use moruna_kernel::{AllocCounts, TraceRecord};
use serde::Serialize;

/// One source's figures over a stage (04 d.1).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SourceAlloc {
    /// Bytes requested in total.
    pub requested_bytes: u64,
    /// Number of requests.
    pub requests: u64,
    /// The largest single request; `None` where nothing measures it (Arrow).
    pub largest_request_bytes: Option<u64>,
    /// The most one call held at once; `None` where nothing measures it (Python objects).
    pub peak_bytes: Option<u64>,
    /// Requests refused; `None` where the source has no hook (Arrow).
    pub refused: Option<u64>,
}

/// A stage's allocation figures (04 d.1 `StageReport::alloc`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StageAlloc {
    /// Whether the allocator guard could refuse this kernel's requests.
    pub refusal_on: bool,
    /// Python objects (CPython's allocator domains).
    pub python: SourceAlloc,
    /// NumPy data.
    pub numpy: SourceAlloc,
    /// Arrow, from the default pool's statistics.
    pub arrow: SourceAlloc,
    /// The most one call held at once, all sources (f.2).
    pub peak_bytes: u64,
    /// The part of that peak that was not Arrow.
    pub outside_arrow_peak_bytes: u64,
    /// `outside_arrow_peak_bytes / peak_bytes`; `None` when `peak_bytes` is 0.
    pub outside_arrow_fraction: Option<f64>,
}

#[derive(Default, Clone, Copy)]
struct SourceAcc {
    bytes: u64,
    requests: u64,
    largest: u64,
    peak: u64,
    refused: u64,
}

impl SourceAcc {
    fn add(&mut self, c: &AllocCounts) {
        self.bytes = self.bytes.saturating_add(c.bytes);
        self.requests = self.requests.saturating_add(c.requests);
        self.largest = self.largest.max(c.largest);
        self.peak = self.peak.max(c.peak);
        self.refused = self.refused.saturating_add(c.refused);
    }

    fn finish(self, largest: bool, peak: bool, refused: bool) -> SourceAlloc {
        SourceAlloc {
            requested_bytes: self.bytes,
            requests: self.requests,
            largest_request_bytes: largest.then_some(self.largest),
            peak_bytes: peak.then_some(self.peak),
            refused: refused.then_some(self.refused),
        }
    }
}

/// The running totals of one stage's measured records.
#[derive(Default, Clone)]
pub struct AllocAcc {
    measured: bool,
    refusal_on: bool,
    python: SourceAcc,
    numpy: SourceAcc,
    arrow: SourceAcc,
    /// The largest call total and that call's outside-Arrow part, with the record's order key
    /// so the choice between equal totals does not depend on the order chunks are read in.
    peak: Option<((u64, u64), u64, u64)>,
}

impl AllocAcc {
    /// Fold one record in; a record whose `alloc` is not measured changes nothing.
    pub fn add(&mut self, r: &TraceRecord) {
        let a = &r.alloc;
        if !a.measured {
            return;
        }
        self.measured = true;
        self.refusal_on |= a.refusal_on;
        self.python.add(&a.python);
        self.numpy.add(&a.numpy);
        self.arrow.add(&a.arrow);
        let rise = r.mem_anon_peak.saturating_sub(r.mem_anon_before);
        let total = rise.max(a.arrow.peak.saturating_add(a.numpy.peak));
        let outside = total - a.arrow.peak.min(total);
        let key = (r.t_start_ns, r.seq);
        let better = match self.peak {
            None => true,
            Some((k, t, _)) => total > t || (total == t && key < k),
        };
        if better {
            self.peak = Some((key, total, outside));
        }
    }

    /// The stage's figures; `None` when no record was measured.
    pub fn finish(self) -> Option<StageAlloc> {
        if !self.measured {
            return None;
        }
        let (peak, outside) = self.peak.map_or((0, 0), |(_, t, o)| (t, o));
        Some(StageAlloc {
            refusal_on: self.refusal_on,
            python: self.python.finish(true, false, true),
            numpy: self.numpy.finish(true, true, true),
            arrow: self.arrow.finish(false, true, false),
            peak_bytes: peak,
            outside_arrow_peak_bytes: outside,
            outside_arrow_fraction: (peak > 0).then(|| outside as f64 / peak as f64),
        })
    }
}

impl StageAlloc {
    /// The figures of a set of records of one stage (what `moruna check` reports, MH 4.9).
    pub fn of<'a>(records: impl IntoIterator<Item = &'a TraceRecord>) -> Option<StageAlloc> {
        let mut acc = AllocAcc::default();
        for r in records {
            acc.add(r);
        }
        acc.finish()
    }
}
