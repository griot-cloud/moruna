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
    /// All splits, in delivery order, before any read. Called once per run; on a
    /// resumed run the result must equal the plan recorded in the manifest
    /// (checked by a digest over split ids and row counts, placement e.5; a
    /// mismatch is `Resume`).
    fn plan(&self) -> crate::Result<Vec<Split>>;
    /// Read a split (or a row range of it) into buffers from `alloc` in `tier`,
    /// returning a resident payload. Runs on the reactor; must not block a worker.
    /// Deterministic for a given `(split, rows)` for the lifetime of the input
    /// (CT-I12): the same call returns the same rows in the same order, when
    /// `repeatable()` is true.
    fn read(
        &self,
        split: &Split,
        rows: Option<RowRange>,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'_, crate::Result<Payload>>;
    /// True when `plan` and `read` satisfy CT-I12. A source that pulls from a
    /// one-shot iterator returns false; the runtime then disables Q0 eviction
    /// (stages Q0 instead) and refuses `resume` for the run. Default true, which
    /// every file-backed source satisfies by construction.
    fn repeatable(&self) -> bool {
        true
    }
}
