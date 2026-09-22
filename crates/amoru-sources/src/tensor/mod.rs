//! `TensorSource`: safetensors, NumPy `.npy` and the Amoru aligned binary format `AMB1`
//! (sections e.3, e.4).
//!
//! A read computes the enclosing page-aligned byte range of the rows it was asked for,
//! allocates one arena buffer of that length, has the reactor land the bytes in it, and
//! returns a `ManagedTensor` that is a safe view at an offset inside that buffer. Nothing is
//! copied, nothing is mapped and there is no `unsafe` here: an unaligned safetensors data
//! section only changes the view's `byte_offset` (SO-I5, S13).

mod npy;
mod plan;
mod read;
mod safetensors;

use std::sync::Arc;

use amoru_kernel::{
    Allocator, BoxFuture, DType, Payload, Reactor, Result, RowRange, Source, SourceSchema, Split,
    Tier,
};

use crate::stats::{Counters, SourceStats};

/// How a `TensorSource` is built (d.1).
#[derive(Clone, Debug, Default)]
pub struct TensorSourceConfig {
    /// Local files only in v1; an object URL is a `Plan` error.
    pub paths: Vec<std::path::PathBuf>,
    /// Names within a safetensors file; `None` is all of them, in file order.
    pub tensors: Option<Vec<String>>,
    /// A fallback when the scheduler passes no row range.
    pub slice_rows_hint: Option<u64>,
}

/// One planned tensor: where its bytes are and what shape they have (e.3).
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) path: std::path::PathBuf,
    pub(crate) name: String,
    pub(crate) dtype: DType,
    pub(crate) shape: Vec<i64>,
    pub(crate) data_offset: u64,
    pub(crate) file_len: u64,
}

impl Entry {
    /// Bytes per row along dimension 0; a rank 0 tensor is one row of one element.
    pub(crate) fn row_bytes(&self) -> u64 {
        let trailing: u64 = self.shape.iter().skip(1).map(|d| *d as u64).product();
        trailing * self.dtype.item_size() as u64
    }

    /// Rows: `shape[0]`, or 1 for a rank 0 tensor.
    pub(crate) fn rows(&self) -> u64 {
        self.shape.first().map_or(1, |d| *d as u64)
    }

    /// Payload bytes of the whole tensor.
    pub(crate) fn bytes(&self) -> u64 {
        let elements: u64 = self.shape.iter().map(|d| *d as u64).product();
        elements * self.dtype.item_size() as u64
    }
}

/// A source over tensor files: safetensors, `.npy` and `AMB1` (d.1).
pub struct TensorSource {
    reactor: Arc<dyn Reactor>,
    entries: Vec<Entry>,
    splits: Vec<Split>,
    schema: SourceSchema,
    counters: Counters,
}

impl TensorSource {
    /// Parse every header now, so `plan` is a clone and is complete before the first `read`
    /// (SO-I1). A path that is not a local file, a `.npy` in Fortran order, a header that
    /// does not parse or a file shorter than its header claims is a `Plan` error here.
    pub fn new(cfg: TensorSourceConfig, reactor: Arc<dyn Reactor>) -> Result<TensorSource> {
        let entries = plan::plan(&cfg)?;
        let splits = plan::splits(&entries);
        let schema = plan::schema(&entries)?;
        let counters = Counters::default();
        Counters::add(&counters.splits, splits.len() as u64);
        Counters::add(
            &counters.bytes_planned,
            splits.iter().map(|s| s.uncompressed_bytes).sum(),
        );
        tracing::info!(
            target: "source.plan",
            files = cfg.paths.len(),
            splits = splits.len(),
            bytes = splits.iter().map(|s| s.uncompressed_bytes).sum::<u64>(),
            skipped = 0,
            "tensor plan",
        );
        Ok(TensorSource {
            reactor,
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
}

impl Source for TensorSource {
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

impl TensorSource {
    pub(crate) fn entry(&self, split: &Split) -> Result<&Entry> {
        self.splits
            .iter()
            .position(|s| s.id == split.id)
            .and_then(|i| self.entries.get(i))
            .ok_or_else(|| crate::util::source_err(split.id, "no such split in the plan (SO-I1)"))
    }

    pub(crate) fn reactor(&self) -> &Arc<dyn Reactor> {
        &self.reactor
    }

    pub(crate) fn counters(&self) -> &Counters {
        &self.counters
    }
}
