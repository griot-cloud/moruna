//! `VortexSource`: the Vortex file format as a source (sections e.7, e.8, f.8; SO-O1 built on
//! 2026-09-29).
//!
//! The plan is complete before the first read (SO-I1): every file's footer and the zone maps of
//! the projected columns are read in `new`, and the file is cut into splits at zone boundaries,
//! one split per run of zones of about `split_bytes` (e.7). A read opens the file over the
//! footer the plan kept (no footer I/O), scans the split's row range with the projection, and
//! answers every segment request by having the reactor land the bytes in an arena buffer
//! ([`io`]). A column whose segment is in a canonical encoding decodes as a view over those
//! bytes and is returned in place with no copy; every other buffer is decoded by Vortex and
//! copied into the arena once, which is the decode copy G-I2 allows (SO-I5, e.8).

mod io;
mod plan;
mod read;

use std::sync::Arc;

use moruna_kernel::{
    Allocator, BoxFuture, ObjectMetadata, Payload, Reactor, Result, RowRange, Source, SourceSchema,
    Split, Tier,
};
use vortex::VortexSessionDefault;
use vortex::io::runtime::BlockingRuntime;
use vortex::io::runtime::current::CurrentThreadRuntime;
use vortex::io::session::RuntimeSessionExt;
use vortex::session::VortexSession;

use crate::stats::{Counters, SourceStats};

/// The split size a Vortex source cuts toward when the configuration names none: the row
/// group a Moruna Parquet sink writes by default (`sink.row_group_bytes`, preamble section 5),
/// so a Vortex split and a Parquet split are about the same size (e.7).
pub const DEFAULT_SPLIT_BYTES: u64 = 128 << 20;

/// How a `VortexSource` is built (d.1).
#[derive(Clone, Debug, Default)]
pub struct VortexSourceConfig {
    /// Files or prefixes; a prefix (a local directory, or an object prefix) is listed at plan
    /// time for names ending `.vortex`.
    pub urls: Vec<String>,
    /// The projection, by column name; `None` is every column.
    pub columns: Option<Vec<String>>,
    /// The uncompressed bytes a split is cut toward (e.7); `None` is [`DEFAULT_SPLIT_BYTES`].
    pub split_bytes: Option<u64>,
}

/// The Vortex library's session and the executor its futures run on, one per source or sink.
/// The executor is driven only by the thread that blocks on it, so a Vortex read runs on the
/// thread that asked for it and spawns no thread of its own (g).
pub(crate) struct Engine {
    pub(crate) session: VortexSession,
    pub(crate) runtime: CurrentThreadRuntime,
}

impl Engine {
    pub(crate) fn new() -> Engine {
        let runtime = CurrentThreadRuntime::new();
        let session = VortexSession::default().with_handle(runtime.handle());
        Engine { session, runtime }
    }
}

/// A source over Vortex files (d.1).
pub struct VortexSource {
    reactor: Arc<dyn Reactor>,
    engine: Engine,
    files: Vec<plan::FileMeta>,
    entries: Vec<plan::Entry>,
    splits: Vec<Split>,
    schema: SourceSchema,
    counters: Counters,
}

impl VortexSource {
    /// Read every footer and every projected column's zone map now (the plan cache), so `plan`
    /// is a clone and is complete before the first read (SO-I1). A projection naming a column
    /// a file does not have, or files whose projected columns disagree on a type, is a `Plan`
    /// error here (h).
    ///
    /// Local paths and `file://` URLs are sized and listed with `std::fs` and their footers are
    /// read with `std::fs`, as a Parquet footer is; `meta` is used for object URLs only.
    pub fn new(
        cfg: VortexSourceConfig,
        reactor: Arc<dyn Reactor>,
        meta: Arc<dyn ObjectMetadata>,
    ) -> Result<VortexSource> {
        VortexSource::build(cfg, reactor, meta, None)
    }

    /// [`VortexSource::new`], able to read objects as well as local files: an object's footer
    /// and zone maps are read through `read_object` into buffers from `alloc` at plan time.
    /// `alloc` is used during construction only; reads allocate from the allocator `read` is
    /// given (SO-I3).
    pub fn with_allocator(
        cfg: VortexSourceConfig,
        reactor: Arc<dyn Reactor>,
        meta: Arc<dyn ObjectMetadata>,
        alloc: Arc<dyn Allocator>,
    ) -> Result<VortexSource> {
        VortexSource::build(cfg, reactor, meta, Some(alloc))
    }

    fn build(
        cfg: VortexSourceConfig,
        reactor: Arc<dyn Reactor>,
        meta: Arc<dyn ObjectMetadata>,
        alloc: Option<Arc<dyn Allocator>>,
    ) -> Result<VortexSource> {
        let engine = Engine::new();
        let counters = Counters::default();
        let planned = plan::plan(&cfg, &engine, &reactor, &meta, alloc.as_ref(), &counters)?;
        Counters::add(&counters.splits, planned.splits.len() as u64);
        Counters::add(
            &counters.bytes_planned,
            planned.splits.iter().map(|s| s.uncompressed_bytes).sum(),
        );
        tracing::info!(
            target: "source.plan",
            files = planned.files.len(),
            splits = planned.splits.len(),
            bytes = planned.splits.iter().map(|s| s.uncompressed_bytes).sum::<u64>(),
            skipped = 0u64,
            "vortex plan",
        );
        Ok(VortexSource {
            reactor,
            engine,
            files: planned.files,
            entries: planned.entries,
            splits: planned.splits,
            schema: planned.schema,
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
}

impl Source for VortexSource {
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
        let outcome = read::read(self, split, rows, alloc, tier);
        Box::pin(async move { outcome })
    }
}
