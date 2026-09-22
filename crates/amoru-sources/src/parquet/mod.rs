//! `ParquetSource`: footer-driven look-ahead, projection and row-range sub-splitting
//! (sections e.1, e.2, f.1 to f.4).
//!
//! The plan is complete before the first read (SO-I1): every footer is parsed in `new`, so
//! every split carries the row count, the uncompressed bytes and the null counts the footer
//! states for the projected columns (SO-I2), which is what the controller sizes morsels from
//! (D3). A read fetches the row group's projected byte ranges into arena buffers through the
//! reactor, decodes one batch, and copies the decoded Arrow buffers into arena buffers of the
//! requested tier: that copy is the one exception G-I2 grants (SO-I5).

mod decode_copy;
mod plan;
mod read;
mod reader;

use std::sync::Arc;

use amoru_kernel::{
    Allocator, BoxFuture, ObjectMetadata, Payload, Reactor, Result, RowRange, Source, SourceSchema,
    Split, Tier,
};

use crate::stats::{Counters, SourceStats};

/// A literal a `RowFilter` compares a column against.
#[derive(Clone, Debug, PartialEq)]
pub enum ScalarValue {
    /// A signed integer.
    I64(i64),
    /// An unsigned integer.
    U64(u64),
    /// A floating point number.
    F64(f64),
    /// A string.
    Str(String),
    /// A boolean.
    Bool(bool),
}

/// Row-group statistics predicate; only min/max pruning in v1 (d.1, f.2).
#[derive(Clone, Debug, PartialEq)]
pub enum RowFilter {
    /// Keep row groups that may hold a value greater than this.
    Gt(String, ScalarValue),
    /// Keep row groups that may hold a value less than this.
    Lt(String, ScalarValue),
    /// Keep row groups that may hold this value.
    Eq(String, ScalarValue),
}

impl RowFilter {
    /// The column the predicate names.
    pub(crate) fn column(&self) -> &str {
        match self {
            RowFilter::Gt(name, _) | RowFilter::Lt(name, _) | RowFilter::Eq(name, _) => name,
        }
    }
}

/// How a `ParquetSource` is built (d.1).
#[derive(Clone, Debug, Default)]
pub struct ParquetSourceConfig {
    /// Files or prefixes; a prefix is listed at plan time.
    pub urls: Vec<String>,
    /// The projection; `None` is every column.
    pub columns: Option<Vec<String>>,
    /// Row-group pruning on statistics; groups only, no row filtering in v1.
    pub filters: Vec<RowFilter>,
    /// A fallback when the scheduler passes no row range.
    pub batch_rows_hint: Option<u64>,
}

/// A source over Parquet files (d.1).
pub struct ParquetSource {
    reactor: Arc<dyn Reactor>,
    files: Vec<plan::FileMeta>,
    entries: Vec<plan::Entry>,
    splits: Vec<Split>,
    schema: SourceSchema,
    counters: Counters,
}

impl ParquetSource {
    /// Read every footer now (the plan cache), so `plan` is a clone and is complete before the
    /// first read (SO-I1). A projection naming a column no file has, or files under one prefix
    /// whose projected columns disagree on a type, is a `Plan` error here (h).
    ///
    /// `meta` is the reactor's `ObjectMetadata` (contracts d.9) and is used for object URLs
    /// only; a local path or a `file://` URL never touches it (SO-T16).
    pub fn new(
        cfg: ParquetSourceConfig,
        reactor: Arc<dyn Reactor>,
        meta: Arc<dyn ObjectMetadata>,
    ) -> Result<ParquetSource> {
        let counters = Counters::default();
        let files = plan::files(&cfg, &meta, &counters)?;
        let (entries, splits, skipped) = plan::splits(&cfg, &files, &counters)?;
        let schema = plan::schema(&files)?;
        Counters::add(&counters.splits, splits.len() as u64);
        Counters::add(&counters.groups_skipped, skipped);
        Counters::add(
            &counters.bytes_planned,
            splits.iter().map(|s| s.uncompressed_bytes).sum(),
        );
        tracing::info!(
            target: "source.plan",
            files = files.len(),
            splits = splits.len(),
            bytes = splits.iter().map(|s| s.uncompressed_bytes).sum::<u64>(),
            skipped,
            "parquet plan",
        );
        Ok(ParquetSource {
            reactor,
            files,
            entries,
            splits,
            schema,
            counters,
        })
    }

    /// What this source has done so far (j).
    pub fn stats(&self) -> SourceStats {
        self.counters.snapshot()
    }

    pub(crate) fn entry(&self, split: &Split) -> Result<(&plan::FileMeta, &plan::Entry)> {
        let entry = self
            .splits
            .iter()
            .position(|s| s.id == split.id)
            .and_then(|i| self.entries.get(i))
            .ok_or_else(|| {
                crate::util::source_err(split.id, "no such split in the plan (SO-I1)")
            })?;
        let file = self
            .files
            .get(entry.file)
            .ok_or_else(|| crate::util::source_err(split.id, "the plan names no such file"))?;
        Ok((file, entry))
    }

    pub(crate) fn reactor(&self) -> &Arc<dyn Reactor> {
        &self.reactor
    }

    pub(crate) fn counters(&self) -> &Counters {
        &self.counters
    }
}

impl Source for ParquetSource {
    fn schema(&self) -> SourceSchema {
        self.schema.clone()
    }

    fn plan(&self) -> Result<Vec<Split>> {
        Ok(self.splits.clone())
    }

    fn read(
        &self,
        split: &Split,
        rows: Option<RowRange>,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'_, Result<Payload>> {
        read::read(self, split, rows, alloc, tier)
    }
}

/// The decode copy, for the iterator source (f.5): the interpreter is its decoder, so the copy
/// of a Python-owned batch into the arena is the same one copy SO-I5 counts.
#[cfg(feature = "python")]
pub(crate) fn decode_copy_for_iterator(
    batch: &amoru_kernel::arrow::array::RecordBatch,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<(amoru_kernel::arrow::array::RecordBatch, u64)> {
    decode_copy::copy_batch(batch, alloc, tier)
}
