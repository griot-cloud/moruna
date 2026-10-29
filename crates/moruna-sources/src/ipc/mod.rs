//! `ArrowIpcSource`: Arrow IPC files as a source (sections e.9, e.10; SO-O2, KA9, 2026-10-28).
//!
//! Only the file format is read: the `ARROW1` magic, the footer, and the record batch blocks the
//! footer lists. A stream is refused by name. The plan reads every footer and the message header
//! of every block (the row count is in the header, so no body is read at plan time, SO-I2), and
//! cuts each file into splits of whole blocks. A read lands a block's bytes in an arena buffer
//! through the reactor, puts its message through `moruna_kernel::ipc`'s check before arrow-ipc
//! sees it (CT-I13, SO-I10), and decodes the batch over that very buffer: nothing is mapped and
//! a block that is one split is returned with no copy at all. The sink writes every body buffer
//! at a page boundary (08 e.3), so a read of a Moruna file is the enclosing page range and the
//! buffers sit where the arena's alignment wants them.

mod plan;
mod read;

use std::sync::Arc;

use moruna_kernel::{
    Allocator, BoxFuture, Payload, Reactor, Result, RowRange, Source, SourceSchema, Split, Tier,
};

use crate::stats::{Counters, SourceStats};

/// How an `ArrowIpcSource` is built (d.1).
#[derive(Clone, Debug, Default)]
pub struct ArrowIpcSourceConfig {
    /// Files or directories, local: a directory is listed for names ending `.arrow`, in name
    /// order.
    pub urls: Vec<String>,
    /// The projection, by column name; `None` is every column. The output keeps the file's
    /// order.
    pub columns: Option<Vec<String>>,
    /// The rows a split is cut toward, from whole record batch blocks of one file; `None` is
    /// one split per block (e.9).
    pub split_rows: Option<u64>,
}

/// A source over Arrow IPC files (d.1).
pub struct ArrowIpcSource {
    reactor: Arc<dyn Reactor>,
    files: Vec<plan::FileMeta>,
    entries: Vec<plan::Entry>,
    splits: Vec<Split>,
    schema: SourceSchema,
    counters: Counters,
}

impl ArrowIpcSource {
    /// Read every footer and every block's message header now (the plan cache), so `plan` is a
    /// clone and is complete before the first read (SO-I1). A file that is a stream, files whose
    /// schemas differ, a projection naming a column a file lacks and a URL that is not local
    /// are `Plan` errors; a file that is cut short or whose footer or block headers are wrong
    /// is a `Source` error naming the file and the block (SO-I10).
    pub fn new(cfg: ArrowIpcSourceConfig, reactor: Arc<dyn Reactor>) -> Result<ArrowIpcSource> {
        let counters = Counters::default();
        let planned = plan::plan(&cfg, &counters)?;
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
            "arrow ipc plan",
        );
        Ok(ArrowIpcSource {
            reactor,
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

impl Source for ArrowIpcSource {
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
        Box::pin(read::read(self, split, rows, alloc, tier))
    }
}
