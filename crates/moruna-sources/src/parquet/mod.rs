//! `ParquetSource`: footer-driven look-ahead, projection and row-range sub-splitting
//! (sections e.1, e.2, f.1 to f.4).
//!
//! The plan is complete before the first read (SO-I1): every footer is parsed in `new`, so
//! every split carries the row count, the uncompressed bytes and the null counts the footer
//! states for the projected columns (SO-I2), which is what the controller sizes morsels from
//! (D3). A read fetches the row group's projected byte ranges into arena buffers through the
//! reactor, decodes one batch, and copies the decoded Arrow buffers into arena buffers of the
//! requested tier: that copy is the one exception G-I2 grants (SO-I5).

mod append;
pub(crate) mod decode_copy;
mod decoders;
mod hint;
mod pages;
mod parts;
mod plan;
mod read;
mod reader;

use std::sync::Arc;

use moruna_kernel::{
    Allocator, BoxFuture, Fetched, ObjectMetadata, Payload, Reactor, Result, RowRange, Source,
    SourceSchema, Split, SplitId, Tier,
};
use parquet::file::metadata::ParquetMetaData;

pub use decoders::{
    ARENA_LEAST_BUFFER_BYTES, BYTE_ARRAY_COLUMN_DECODE_BYTES, FIXED_COLUMN_DECODE_BYTES,
    decode_fixed_bytes, read_least_arena_bytes,
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
    /// Row groups with their pages located (`pages`), by split, most recent last; `None` for a
    /// group whose pages could not be located. Bounded by `PAGED_KEPT`.
    paged: std::sync::Mutex<std::collections::VecDeque<(SplitId, Option<Arc<ParquetMetaData>>)>>,
    /// The row groups whose columns are being fetched, or were fetched while ranges of them
    /// remain to be read: a range decodes from its group's fetch rather than fetching the
    /// group's columns again, and shares the group's decode state (`parts`, 6.3l; f.3, RB3).
    /// Fetches are in flight together (6.3t), so a range whose group is still being fetched
    /// waits for that fetch rather than starting another.
    fetches: std::sync::Mutex<std::collections::HashMap<SplitId, FetchSlot>>,
    /// The most decoded bytes one decode builds outside the arena at once (3A); `None` decodes
    /// each range in one batch.
    decode_batch_bytes: Option<u64>,
    /// The run's decode batch, read at each read in place of `decode_batch_bytes` (6.3d).
    decode_batch: Option<DecodeBatch>,
}

/// A row group's fetch, as the source keeps it for the group's ranges (6.3t).
enum FetchSlot {
    /// A range is fetching it; these ranges wait for it.
    Pending(Vec<std::task::Waker>),
    /// Fetched, for the ranges still to read it.
    Ready(parts::GroupFetch),
}

/// What [`ParquetSource::claim_fetch`] gives a range.
pub(crate) enum Claim {
    /// The group's fetch, which another range made.
    Ready(parts::GroupFetch),
    /// No range is fetching the group: this one does, and ends it with
    /// [`ParquetSource::fetched`].
    Mine,
}

/// How many row groups' page locations a source keeps: more than the reads a run has in flight.
const PAGED_KEPT: usize = 64;

/// A decode batch the run sets once it has sized its arena (3A, items 6.3a and 6.3d): it knows
/// the room its decoders have above the arena only after the sinks' writers have claimed
/// theirs, which needs the schema the source gives. The source reads it at each read. Clones
/// share it.
#[derive(Clone, Debug, Default)]
pub struct DecodeBatch(Arc<std::sync::atomic::AtomicU64>);

impl DecodeBatch {
    /// A batch not set yet: each range decodes in one batch until it is.
    pub fn new() -> DecodeBatch {
        DecodeBatch::default()
    }

    /// Decode in batches of at most `bytes` decoded bytes; `None` keeps one batch a range.
    pub fn set(&self, bytes: Option<u64>) {
        self.0
            .store(bytes.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
    }

    /// The batch set, `None` for one batch a range.
    pub fn get(&self) -> Option<u64> {
        Some(self.0.load(std::sync::atomic::Ordering::SeqCst)).filter(|&bytes| bytes > 0)
    }
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
            fetches: std::sync::Mutex::new(std::collections::HashMap::new()),
            decode_batch_bytes: None,
            decode_batch: None,
        })
    }

    /// Decode each read in batches of at most `bytes` decoded bytes, each copied into the
    /// read's arena buffers as it comes (3A, 6.3a), rather than the whole range at once: the run
    /// gives it the headroom above its arena over the workers that may decode at once, so the
    /// decodes in flight hold no more outside the arena than it left for them. `None` keeps one
    /// batch a range.
    pub fn decode_batches_of(mut self, bytes: Option<u64>) -> ParquetSource {
        self.decode_batch_bytes = bytes.filter(|&bytes| bytes > 0);
        self
    }

    /// Decode each read in batches of what `batch` holds when the read starts (6.3d): the run
    /// sets it once it has sized its arena, after the source is built.
    pub fn decode_batches_from(mut self, batch: DecodeBatch) -> ParquetSource {
        self.decode_batch = Some(batch);
        self
    }

    pub(crate) fn decode_batch_bytes(&self) -> Option<u64> {
        match &self.decode_batch {
            Some(batch) => batch.get(),
            None => self.decode_batch_bytes,
        }
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

    /// The fetch of `split`'s group for a range of it: its fetch when a range already fetched
    /// it, a wait while one is fetching it, or the claim to fetch it ([`Claim::Mine`]), which
    /// the claimant ends with [`ParquetSource::fetched`]. Taking the split's final range
    /// (`last`) lets the source's hold on the fetch go; the ranges in flight keep it until they
    /// have decoded.
    pub(crate) fn claim_fetch(
        &self,
        split: SplitId,
        last: bool,
    ) -> impl std::future::Future<Output = Claim> + '_ {
        std::future::poll_fn(move |cx| {
            let mut fetches = self.fetches.lock().unwrap_or_else(|e| e.into_inner());
            match fetches.get_mut(&split) {
                Some(FetchSlot::Ready(fetch)) => {
                    let fetch = fetch.clone();
                    if last {
                        fetches.remove(&split);
                    }
                    std::task::Poll::Ready(Claim::Ready(fetch))
                }
                Some(FetchSlot::Pending(waiting)) => {
                    waiting.push(cx.waker().clone());
                    std::task::Poll::Pending
                }
                None => {
                    fetches.insert(split, FetchSlot::Pending(Vec::new()));
                    std::task::Poll::Ready(Claim::Mine)
                }
            }
        })
    }

    /// The claimed fetch of `split`'s group has ended: kept for the group's other ranges when it
    /// fetched and this was not the final range, forgotten otherwise (a failed fetch is claimed
    /// again by the next range that needs it). Every range waiting for it is woken.
    pub(crate) fn fetched(&self, split: SplitId, fetch: Option<&parts::GroupFetch>, last: bool) {
        let mut fetches = self.fetches.lock().unwrap_or_else(|e| e.into_inner());
        let waiting = match fetches.remove(&split) {
            Some(FetchSlot::Pending(waiting)) => waiting,
            Some(FetchSlot::Ready(_)) | None => Vec::new(),
        };
        if let (Some(fetch), false) = (fetch, last) {
            fetches.insert(split, FetchSlot::Ready(fetch.clone()));
        }
        drop(fetches);
        for waker in waiting {
            waker.wake();
        }
    }

    /// Whether a row range of `split` decodes only its own pages (3A): every projected column
    /// is unrepeated, so its pages can be located from their headers, and they were not found
    /// unlocatable when a range of this group was decoded before.
    fn pages_locatable(&self, split: &Split) -> bool {
        let Ok((file, _)) = self.entry(split) else {
            return false;
        };
        if !pages::locatable(&file.metadata, &file.projected) {
            return false;
        }
        let kept = self.paged.lock().unwrap_or_else(|e| e.into_inner());
        !kept
            .iter()
            .any(|(id, located)| *id == split.id && located.is_none())
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

    /// A range of a row group shares the group's fetch (`claim_fetch`) and decodes only its own
    /// pages when they can be located; one whose pages cannot be is read whole.
    fn range_reads_own_rows(&self, split: &Split) -> bool {
        split.sub_splittable && self.pages_locatable(split)
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, Int64Array, ListArray, RecordBatch};
    use arrow::datatypes::Int32Type;

    /// One row group of an id and a list column, written to a directory of its own.
    fn file(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("moruna-parquet-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the directory");
        let path = dir.join("lists.parquet");
        let batch = RecordBatch::try_from_iter([
            (
                "id",
                Arc::new(Int64Array::from_iter_values(0..64)) as ArrayRef,
            ),
            (
                "tags",
                Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(
                    (0..64).map(|r| Some((0..r % 3).map(Some).collect::<Vec<_>>())),
                )) as ArrayRef,
            ),
        ])
        .expect("the batch");
        let file = std::fs::File::create(&path).expect("the file");
        let mut writer =
            parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None).expect("the writer");
        writer.write(&batch).expect("the rows");
        writer.close().expect("closing");
        path
    }

    fn source(path: &std::path::Path, columns: Option<Vec<String>>) -> ParquetSource {
        let reactor = moruna_testkit::FakeReactor::new();
        ParquetSource::new(
            ParquetSourceConfig {
                urls: vec![path.display().to_string()],
                columns,
                ..ParquetSourceConfig::default()
            },
            Arc::new(reactor.clone()),
            Arc::new(reactor),
        )
        .expect("the source")
    }

    #[test]
    fn a_range_reads_own_rows_only_where_its_pages_can_be_located() {
        let path = file("own-rows");
        // A repeated column's header gives no rows, so its pages cannot be located (3A).
        let all = source(&path, None);
        let split = all.plan().expect("the plan").remove(0);
        assert!(split.sub_splittable);
        assert!(!all.range_reads_own_rows(&split));

        // Without it, they can; until a range of the group finds they cannot.
        let ids = source(&path, Some(vec!["id".to_string()]));
        let split = ids.plan().expect("the plan").remove(0);
        assert!(ids.range_reads_own_rows(&split));
        ids.paged
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back((split.id, None));
        assert!(!ids.range_reads_own_rows(&split));

        // A split this source did not plan is not one it can read part of.
        let stranger = Split { id: 99, ..split };
        assert!(!ids.range_reads_own_rows(&stranger));
        std::fs::remove_dir_all(path.parent().expect("its directory")).ok();
    }
}
