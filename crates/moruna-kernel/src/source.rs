//! The source contract (contracts d.6).

use crate::BoxFuture;
use crate::buffer::Allocator;
use crate::ids::SplitId;
use crate::payload::{Payload, SourceSchema};
use crate::tier::Tier;

/// A unit the source can read independently, with metadata known before reading.
#[derive(Clone, Debug)]
pub struct Split {
    /// Identifier, unique within a run.
    pub id: SplitId,
    /// Rows in the split.
    pub rows: u64,
    /// Uncompressed bytes: exact from metadata or estimated (see `estimated`).
    pub uncompressed_bytes: u64,
    /// True when `uncompressed_bytes` and `column_bytes` are estimates.
    pub estimated: bool,
    /// Bytes per projected column; empty for tensors.
    pub column_bytes: Vec<u64>,
    /// Nulls per projected column, where the metadata says.
    pub null_counts: Vec<Option<u64>>,
    /// Can `read` take a `RowRange` narrower than the split?
    pub sub_splittable: bool,
}

/// A half-open row range within a split.
#[derive(Copy, Clone, Debug)]
pub struct RowRange {
    /// First row, inclusive.
    pub start: u64,
    /// Last row, exclusive.
    pub end: u64,
}

/// Where a run's input comes from (component 7).
pub trait Source: Send + Sync {
    /// The schema of this source's output.
    fn schema(&self) -> SourceSchema;
    /// All finite splits, in delivery order, before any read. Streaming sources instead
    /// return an initial size estimate and end only when `read_next` returns `None`.
    /// Called once per run; on a
    /// resumed run the result must equal the plan recorded in the manifest
    /// (checked by a digest over split ids and row counts, placement e.5; a
    /// mismatch is `Resume`).
    fn plan(&self) -> crate::Result<Vec<Split>>;
    /// Read a split (or a row range of it) into buffers from `alloc` in `tier`,
    /// returning a resident payload. Runs on the reactor; must not block a worker.
    /// Deterministic for a given `(split, rows)` for the lifetime of the input
    /// (CT-I12): the same call returns the same rows in the same order, when
    /// `repeatable()` is true.
    /// The allocator is borrowed for the future's whole life, not just for the call:
    /// a source that learns its sizes only after decoding (Parquet does) must allocate
    /// after an await, and with the elided lifetime it could not, which forced the
    /// whole read to be synchronous and cost the concurrency `readahead.splits` exists
    /// to buy (E10, component 7, 2026-09-22).
    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, crate::Result<Payload>>;
    /// Opt in to one-at-a-time streaming reads instead of exhausting the finite plan.
    /// Such sources must be non-repeatable; their plan is only an initial size estimate.
    fn streaming(&self) -> bool {
        false
    }
    /// Pull one batch. Only `None` means EOF: an empty batch is still a batch.
    /// The scheduler retains and drains this original future before stopping the drive.
    fn read_next<'a>(
        &'a self,
        _alloc: &'a dyn Allocator,
        _tier: Tier,
    ) -> BoxFuture<'a, crate::Result<Option<Payload>>> {
        Box::pin(async {
            Err(crate::MorunaError::Config {
                name: "source",
                msg: "this finite source does not support streaming reads".into(),
            })
        })
    }
    /// True when `plan` and `read` satisfy CT-I12. A source that pulls from a
    /// one-shot iterator returns false; the runtime then disables Q0 eviction
    /// (stages Q0 instead) and refuses `resume` for the run. Default true, which
    /// every file-backed source satisfies by construction.
    fn repeatable(&self) -> bool {
        true
    }
    /// What the engine behind the source said about the read it planned, for a source that
    /// reads through an engine which governs its reads (a peQL contract); `None` for every
    /// other source. Known once the source is built.
    fn read_report(&self) -> Option<SourceRead> {
        None
    }
}

/// A read as the engine that planned it reports it, in that engine's own words: Moruna
/// carries the report to the run's report and neither reads nor judges it.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceRead {
    /// The engine: `peql`.
    pub engine: String,
    /// The engine's report of the read, as the engine serialises it.
    pub report: serde_json::Value,
}
