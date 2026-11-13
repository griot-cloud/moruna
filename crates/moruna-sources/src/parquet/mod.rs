//! `ParquetSource`: footer-driven look-ahead, projection and row-range sub-splitting
//! (sections e.1, e.2, f.1 to f.4).
//!
//! The plan is complete before the first read (SO-I1): every footer is parsed in `new`, so
//! every split carries the row count, the uncompressed bytes and the null counts the footer
//! states for the projected columns (SO-I2), which is what the controller sizes morsels from
//! (D3). A read fetches the row group's projected byte ranges into arena buffers through the
//! reactor, decodes one batch, and copies the decoded Arrow buffers into arena buffers of the
//! requested tier: that copy is the one exception G-I2 grants (SO-I5).

pub(crate) mod decode_copy;
mod hint;
mod pages;
mod plan;
mod read;
mod reader;

use std::sync::Arc;

use moruna_kernel::{
    Allocator, BoxFuture, Fetched, ObjectMetadata, Payload, Reactor, Result, RowRange, Source,
    SourceSchema, Split, SplitId, Tier,
};
use parquet::file::metadata::ParquetMetaData;

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
    /// Row groups with their pages located (`pages`), by split, most recent last; `None` for a
    /// group whose pages could not be located. Bounded by `PAGED_KEPT`.
    paged: std::sync::Mutex<std::collections::VecDeque<(SplitId, Option<Arc<ParquetMetaData>>)>>,
    /// The split the drive fetched last, while ranges of it remain to be read: the next range
    /// decodes from these bytes rather than fetching the group's columns again. Fetches run on
    /// the drive one at a time, so one is enough (f.3, RB3).
    last_fetch: std::sync::Mutex<Option<(SplitId, reader::ArenaChunks)>>,
}

/// How many row groups' page locations a source keeps: more than the reads a run has in flight.
const PAGED_KEPT: usize = 64;

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
        ParquetSource::build(cfg, reactor, meta, None)
    }

    /// [`ParquetSource::new`], able to read objects as well as local files: an `s3://`, `gs://`
    /// or `az://` URL's footer is read through `read_object` into a buffer from `alloc` at
    /// plan time, and each split's column chunks through `read_object` at read time (e.1, f.3;
    /// MH 4.6). `alloc` is used during construction only; reads allocate from the allocator
    /// `read` is given, as every read does (SO-I3).
    pub fn with_allocator(
        cfg: ParquetSourceConfig,
        reactor: Arc<dyn Reactor>,
        meta: Arc<dyn ObjectMetadata>,
        alloc: Arc<dyn Allocator>,
    ) -> Result<ParquetSource> {
        ParquetSource::build(cfg, reactor, meta, Some(alloc))
    }

    fn build(
        cfg: ParquetSourceConfig,
        reactor: Arc<dyn Reactor>,
        meta: Arc<dyn ObjectMetadata>,
        alloc: Option<Arc<dyn Allocator>>,
    ) -> Result<ParquetSource> {
        let counters = Counters::default();
        let files = plan::files(
            &cfg,
            &meta,
            &counters,
            alloc.as_ref().map(|alloc| (&reactor, alloc)),
        )?;
        let (entries, splits, skipped) = plan::splits(&cfg, &files, &counters)?;
        let schema = plan::schema(&files)?;
        Counters::add(&counters.splits, splits.len() as u64);
        Counters::add(&counters.groups_skipped, skipped);
        // A footer is untrusted input and its sizes may be anything (SO-I10).
        let bytes = splits
            .iter()
            .fold(0u64, |sum, s| sum.saturating_add(s.uncompressed_bytes));
        Counters::add(&counters.bytes_planned, bytes);
        tracing::info!(
            target: "source.plan",
            files = files.len(),
            splits = splits.len(),
            bytes,
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
            paged: std::sync::Mutex::new(std::collections::VecDeque::new()),
            last_fetch: std::sync::Mutex::new(None),
        })
    }

    /// `split`'s row group alone, with its pages located, for a ranged read (`pages`); `None`
    /// when they cannot be located from the fetched headers.
    pub(crate) fn paged(
        &self,
        split: &Split,
        file: &plan::FileMeta,
        entry: &plan::Entry,
        chunks: &reader::ArenaChunks,
    ) -> Option<Arc<ParquetMetaData>> {
        {
            let kept = self.paged.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, hit)) = kept.iter().find(|(id, _)| *id == split.id) {
                return hit.clone();
            }
        }
        // Two workers decoding ranges of one group at once may both locate it; the walk reads
        // headers only, and the second result is dropped.
        let built =
            pages::one_group_indexed(&file.metadata, entry.row_group, &file.projected, chunks);
        let mut kept = self.paged.lock().unwrap_or_else(|e| e.into_inner());
        if !kept.iter().any(|(id, _)| *id == split.id) {
            kept.push_back((split.id, built.clone()));
            if kept.len() > PAGED_KEPT {
                kept.pop_front();
            }
        }
        built
    }

    /// The fetched columns of `split` if the drive fetched them last; taking the split's final
    /// range (`last`) lets them go.
    pub(crate) fn fetched_last(&self, split: SplitId, last: bool) -> Option<reader::ArenaChunks> {
        let mut kept = self.last_fetch.lock().unwrap_or_else(|e| e.into_inner());
        let hit = match kept.as_ref() {
            Some((id, chunks)) if *id == split => Some(chunks.clone()),
            _ => None,
        };
        if hit.is_some() && last {
            *kept = None;
        }
        hit
    }

    /// Keep `chunks` for the next range of `split`, unless this was its final range.
    pub(crate) fn keep_fetched(&self, split: SplitId, chunks: &reader::ArenaChunks, last: bool) {
        *self.last_fetch.lock().unwrap_or_else(|e| e.into_inner()) =
            (!last).then(|| (split, chunks.clone()));
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

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        read::read(self, split, rows, alloc, tier)
    }

    fn decodes_on_workers(&self) -> bool {
        true
    }

    fn fetch<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Fetched>> {
        Box::pin(async move {
            let fetched = read::fetch(self, split, rows, alloc, tier).await?;
            Ok(Fetched(Box::new(fetched)))
        })
    }

    fn decode(
        &self,
        split: &Split,
        fetched: Fetched,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> Result<Payload> {
        let fetched = fetched.0.downcast::<read::ParquetFetched>().map_err(|_| {
            crate::util::source_err(split.id, "decode was given another source's fetch")
        })?;
        read::decode_fetched(self, split, *fetched, alloc, tier)
    }
}
