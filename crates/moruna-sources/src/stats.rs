//! `SourceStats` (section d.1) and the counters behind it (section j).

use std::sync::atomic::{AtomicU64, Ordering};

/// What a source has done so far. Read through `ParquetSource::stats`,
/// `TensorSource::stats` and `PyIteratorSource::stats`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceStats {
    /// Splits the plan produced.
    pub splits: u64,
    /// Uncompressed bytes the plan accounted for, over the projection.
    pub bytes_planned: u64,
    /// Reads issued.
    pub reads: u64,
    /// Payload bytes copied by the CPU out of a decoder and into the arena (SO-I5).
    pub decode_bytes: u64,
    /// Tensor reads the reactor served on a direct path.
    pub tensor_direct_reads: u64,
    /// Tensor reads the reactor served on a buffered path.
    pub tensor_buffered_reads: u64,
    /// Ranged reads spent on file footers.
    pub footer_reads: u64,
    /// Row groups a `RowFilter` proved empty (f.2).
    pub groups_skipped: u64,
    /// One-row payloads returned above the range's target (SO-I9).
    pub oversized_rows: u64,
}

/// The live counters; one per source instance, shared with every read in flight.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) splits: AtomicU64,
    pub(crate) bytes_planned: AtomicU64,
    pub(crate) reads: AtomicU64,
    pub(crate) decode_bytes: AtomicU64,
    pub(crate) tensor_direct_reads: AtomicU64,
    pub(crate) tensor_buffered_reads: AtomicU64,
    pub(crate) footer_reads: AtomicU64,
    pub(crate) groups_skipped: AtomicU64,
    pub(crate) oversized_rows: AtomicU64,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> SourceStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        SourceStats {
            splits: get(&self.splits),
            bytes_planned: get(&self.bytes_planned),
            reads: get(&self.reads),
            decode_bytes: get(&self.decode_bytes),
            tensor_direct_reads: get(&self.tensor_direct_reads),
            tensor_buffered_reads: get(&self.tensor_buffered_reads),
            footer_reads: get(&self.footer_reads),
            groups_skipped: get(&self.groups_skipped),
            oversized_rows: get(&self.oversized_rows),
        }
    }
}
