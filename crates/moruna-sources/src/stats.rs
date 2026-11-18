//! `SourceStats` (section d.1) and the counters behind it (section j).

use std::sync::atomic::{AtomicU64, Ordering};

/// What a source has done so far. Read through `ParquetSource::stats`, `TensorSource::stats`
/// and `PyIteratorSource::stats`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceStats {
    /// Splits the plan produced.
    pub splits: u64,
    /// Uncompressed bytes the plan accounted for, over the projection.
    pub bytes_planned: u64,
    /// Reads issued.
    pub reads: u64,
    /// Row groups whose columns a Parquet read fetched from the file. A read of a later range
    /// of the same group decodes from that fetch and is not counted again (`keep_fetched`).
    pub group_fetches: u64,
    /// Parquet reads of a part of a row group, decoded.
    pub ranges_decoded: u64,
    /// Of `ranges_decoded`, those that decoded only the pages they cover, located from the
    /// pages' headers; the rest decoded every page before their rows to skip them.
    pub ranges_paged: u64,
    /// Batches a Parquet decode built outside the arena before copying them in.
    pub batches_decoded: u64,
    /// The most memory one of those batches held outside the arena (3A).
    pub largest_batch_bytes: u64,
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
    /// Bytes a `PySource` compacted out of a sliced batch before its decode copy (f.7); zero for
    /// every other source.
    pub compacted_bytes: u64,
    /// Payload bytes a read returned in the arena buffers the reactor landed the file's bytes
    /// in, with no decode copy: an Arrow IPC block decoded over its own buffer.
    pub zero_copy_bytes: u64,
}

/// The live counters; one per source instance, shared with every read in flight.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) splits: AtomicU64,
    pub(crate) bytes_planned: AtomicU64,
    pub(crate) reads: AtomicU64,
    pub(crate) group_fetches: AtomicU64,
    pub(crate) ranges_decoded: AtomicU64,
    pub(crate) ranges_paged: AtomicU64,
    pub(crate) batches_decoded: AtomicU64,
    pub(crate) largest_batch_bytes: AtomicU64,
    pub(crate) decode_bytes: AtomicU64,
    pub(crate) tensor_direct_reads: AtomicU64,
    pub(crate) tensor_buffered_reads: AtomicU64,
    pub(crate) footer_reads: AtomicU64,
    pub(crate) groups_skipped: AtomicU64,
    pub(crate) oversized_rows: AtomicU64,
    pub(crate) compacted_bytes: AtomicU64,
    pub(crate) zero_copy_bytes: AtomicU64,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    /// A decoded batch of `bytes`, held outside the arena until it is copied in.
    pub(crate) fn batch(&self, bytes: u64) {
        self.batches_decoded.fetch_add(1, Ordering::Relaxed);
        self.largest_batch_bytes.fetch_max(bytes, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> SourceStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        SourceStats {
            splits: get(&self.splits),
            bytes_planned: get(&self.bytes_planned),
            reads: get(&self.reads),
            group_fetches: get(&self.group_fetches),
            ranges_decoded: get(&self.ranges_decoded),
            ranges_paged: get(&self.ranges_paged),
            batches_decoded: get(&self.batches_decoded),
            largest_batch_bytes: get(&self.largest_batch_bytes),
            decode_bytes: get(&self.decode_bytes),
            tensor_direct_reads: get(&self.tensor_direct_reads),
            tensor_buffered_reads: get(&self.tensor_buffered_reads),
            footer_reads: get(&self.footer_reads),
            groups_skipped: get(&self.groups_skipped),
            oversized_rows: get(&self.oversized_rows),
            compacted_bytes: get(&self.compacted_bytes),
            zero_copy_bytes: get(&self.zero_copy_bytes),
        }
    }
}
