//! `PlanSource`: a DataFusion plan as a Moruna source (MH 4.5).
//!
//! Splits follow the plan's output partitions: a partition is one split, or, when it is larger
//! than [`SPLIT_BYTES`], consecutive runs of its rows of about that size, as a Parquet file is
//! row groups. A split's rows are counted when the source is built, by one pass over the plan
//! that keeps nothing but counts, because the source contract (d.6) gives a split its row count
//! before any read and the source drive issues row ranges from it. A read then streams its
//! partition: the partition runs on the source's own Tokio runtime (DataFusion's operators
//! spawn onto one), batches cross a channel one at a time, and the rows of the range are copied
//! into the arena. A partition is never held whole; the most any partition has in flight is
//! the batch in the channel, the batch its producer is computing, and the remainder of the
//! last batch a range cut.
//!
//! Ranges of one partition arrive in order, and one live stream per partition serves them. A
//! range that starts behind the stream (an eviction's re-read) runs the partition again, from
//! a fresh copy of the plan, and skips to the range; only a repeatable plan is re-read, since
//! the facade disables eviction for one that is not.
//!
//! A plan is repeatable when its partitions come out in the same order every time: file scans,
//! filters and projections are; an exchange (`RepartitionExec`), a merge of partitions in
//! arrival order (`CoalescePartitionsExec`) or a volatile function (noise, `random()`) is not.
//! A plan that is not repeatable is read as one partition, merged first, so that its row count
//! is the whole result's, which does not depend on the order rows arrive in, and so that an
//! exchange is never left buffering every other partition while one is read.

use std::sync::Arc;

use datafusion::common::tree_node::{TreeNode, TreeNodeRecursion};
use datafusion::config::ConfigNonZeroUsize;
use datafusion::error::DataFusionError;
use datafusion::execution::TaskContext;
use datafusion::execution::disk_manager::{DiskManagerBuilder, DiskManagerMode};
use datafusion::execution::memory_pool::MemoryPool;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::logical_expr::LogicalPlan;
use datafusion::physical_expr_common::physical_expr::is_volatile;
use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
use datafusion::physical_plan::execution_plan::reset_plan_states;
use datafusion::physical_plan::{
    ExecutionPlan, ExecutionPlanProperties, StatisticsArgs, StatisticsContext,
};
use datafusion::prelude::SessionContext;
use futures::StreamExt;
use moruna_kernel::arrow::array::{Array, RecordBatch, UInt32Array};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use moruna_kernel::{
    Allocator, BoxFuture, MorunaError, Payload, Result, RowRange, Source, SourceSchema, Split,
    SplitId, Tier,
};
use tokio::sync::{Mutex, mpsc};

use crate::memory::PlanMemory;

/// Batches a partition's producer may compute ahead of the reads.
const AHEAD: usize = 1;

/// The most a split holds, as the plan's batches measure it: a partition larger than this is
/// several splits. The source drive sizes a read by the split it is in, so a partition read as
/// one split of a whole result would be read in morsels of whatever size the drive's target
/// is, however large, where a file is read at most a row group at a time. `morsel.min_bytes`:
/// a plan that cannot be re-read has its queued morsels staged, and at the smallest budget a
/// larger split's staging buffer could not always be found (F8.5, H6 at 256 MiB).
pub const SPLIT_BYTES: u64 = 4 << 20;

/// Where a split's rows are: its partition, and the partition row it starts at.
#[derive(Clone, Copy, Debug)]
struct Place {
    partition: usize,
    first: u64,
    /// The partition's rows, so the read that reaches its end can check nothing follows.
    partition_rows: u64,
}

type Batches = mpsc::Receiver<std::result::Result<RecordBatch, DataFusionError>>;

/// A DataFusion plan read as a source: its output partitions as splits (MH 4.5).
pub struct PlanSource {
    plan: Arc<dyn ExecutionPlan>,
    task: Arc<TaskContext>,
    runtime: Runtime,
    schema: SourceSchema,
    splits: Vec<Split>,
    /// By split id.
    places: Vec<Place>,
    repeatable: bool,
    /// The live stream of each partition, opened by the first read of it.
    streams: Vec<Mutex<Option<Stream>>>,
}

impl PlanSource {
    /// A physical plan, run in `ctx`'s session with its operators' memory in `memory`.
    pub fn new(
        plan: Arc<dyn ExecutionPlan>,
        ctx: &SessionContext,
        memory: &PlanMemory,
    ) -> Result<PlanSource> {
        PlanSource::build(plan, ctx, Runtime::new()?, memory)
    }

    /// A logical plan, planned in `ctx`'s session and run with its operators' memory in
    /// `memory`.
    pub fn from_logical(
        plan: LogicalPlan,
        ctx: &SessionContext,
        memory: &PlanMemory,
    ) -> Result<PlanSource> {
        let runtime = Runtime::new()?;
        let state = ctx.state();
        let physical = runtime
            .block_on(state.create_physical_plan(&plan))
            .map_err(|e| plan_err(format!("planning: {e}")))?;
        PlanSource::build(physical, ctx, runtime, memory)
    }

    pub(crate) fn build(
        plan: Arc<dyn ExecutionPlan>,
        ctx: &SessionContext,
        runtime: Runtime,
        memory: &PlanMemory,
    ) -> Result<PlanSource> {
        // The session's object stores and caches, with the run's pool in place of the session's
        // unbounded one and the staging directory as the place a spilling operator writes.
        let state = ctx.state();
        let spill = match &memory.spill_dir {
            Some(dir) => DiskManagerMode::Directories(vec![dir.clone()]),
            None => DiskManagerMode::Disabled,
        };
        let env = RuntimeEnvBuilder::from_runtime_env(state.runtime_env())
            .with_memory_pool(Arc::clone(&memory.pool) as Arc<dyn MemoryPool>)
            .with_disk_manager_builder(DiskManagerBuilder::default().with_mode(spill))
            .build_arc()
            .map_err(|e| plan_err(format!("the plan's memory: {e}")))?;
        let mut state = SessionStateBuilder::new_from_existing(state)
            .with_runtime_env(env)
            .build();
        // The batches in flight are the other half of the plan's share: every partition that
        // runs at once holds a few, whatever the pool says, so their size is set from it.
        state.config_mut().options_mut().execution.batch_size =
            ConfigNonZeroUsize::try_new(batch_rows(&plan, memory.pool.in_flight()))
                .map_err(|e| plan_err(e.to_string()))?;
        // A file scan's partitions steal files from one another while they run together, so
        // which rows a partition yields depends on timing, and a partition run alone reads
        // every file. Splits are partitions, counted once and read later, perhaps one at a
        // time: each partition reads its own files.
        state
            .config_mut()
            .options_mut()
            .execution
            .enable_file_stream_work_stealing = false;
        let task = Arc::new(TaskContext::from(&state));
        let repeatable = deterministic(&plan);
        let plan: Arc<dyn ExecutionPlan> =
            if !repeatable && plan.output_partitioning().partition_count() > 1 {
                Arc::new(CoalescePartitionsExec::new(plan))
            } else {
                plan
            };
        let schema = unpacked(&plan.schema());
        let partitions = runtime.block_on(count(&plan, &task))?;
        let streams = partitions.iter().map(|_| Mutex::new(None)).collect();
        let (splits, places) = divide(&partitions);
        Ok(PlanSource {
            plan: reset_plan_states(plan).map_err(|e| plan_err(e.to_string()))?,
            task,
            runtime,
            schema: SourceSchema::Table(schema),
            splits,
            places,
            repeatable,
            streams,
        })
    }

    /// Open a partition's stream on a fresh copy of the plan: an operator keeps the state of
    /// its execution, so no copy is executed twice, and a re-read never touches the live one.
    fn reopen(&self, partition: usize) -> Result<Stream> {
        let plan = reset_plan_states(Arc::clone(&self.plan))
            .map_err(|e| source_err(partition as SplitId, e.to_string()))?;
        Ok(Stream::open(
            plan,
            partition,
            Arc::clone(&self.task),
            &self.runtime,
        ))
    }

    async fn range(&self, split: &Split, range: RowRange) -> Result<Vec<RecordBatch>> {
        let place = self
            .places
            .get(split.id as usize)
            .copied()
            .ok_or_else(|| source_err(split.id, "the plan has no such split"))?;
        let rows = RowRange {
            start: place.first + range.start,
            end: place.first + range.end,
        };
        let slot = &self.streams[place.partition];
        let mut live = slot.lock().await;
        if live.as_ref().is_some_and(|s| s.offset > rows.start) {
            drop(live);
            let mut again = self.reopen(place.partition)?;
            return again.take(split.id, rows).await;
        }
        let stream = match live.as_mut() {
            Some(stream) => stream,
            None => live.insert(self.reopen(place.partition)?),
        };
        let pieces = stream.take(split.id, rows).await?;
        if rows.end >= place.partition_rows {
            stream.finish(split.id).await?;
            *live = None;
        }
        Ok(pieces)
    }
}

impl Source for PlanSource {
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
        Box::pin(async move {
            let range = rows.unwrap_or(RowRange {
                start: 0,
                end: split.rows,
            });
            let pieces = self.range(split, range).await?;
            let SourceSchema::Table(schema) = &self.schema else {
                return Err(plan_err("a plan's schema is a table"));
            };
            let batch = compact(schema, &pieces).map_err(|e| source_err(split.id, e))?;
            let (batch, copied) = moruna_sources::copy_batch(&batch, alloc, tier)?;
            alloc.note_payload_copy(copied);
            Payload::table(batch)
        })
    }

    fn repeatable(&self) -> bool {
        self.repeatable
    }
}

/// One partition as it is being read: its producer's channel, and where the reads are.
struct Stream {
    rx: Batches,
    /// The rows of the last batch a range cut that no range has taken yet.
    pending: Option<RecordBatch>,
    /// The partition row the next batch (or `pending`) starts at.
    offset: u64,
}

impl Stream {
    /// Run `partition` of `plan` on the runtime, one batch ahead of the reads.
    fn open(
        plan: Arc<dyn ExecutionPlan>,
        partition: usize,
        task: Arc<TaskContext>,
        runtime: &Runtime,
    ) -> Stream {
        let (tx, rx) = mpsc::channel(AHEAD);
        runtime.spawn(async move {
            let mut batches = match plan.execute(partition, task) {
                Ok(batches) => batches,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            while let Some(batch) = batches.next().await {
                let failed = batch.is_err();
                if tx.send(batch).await.is_err() || failed {
                    return;
                }
            }
        });
        Stream {
            rx,
            pending: None,
            offset: 0,
        }
    }

    async fn next(&mut self, split: SplitId) -> Result<Option<RecordBatch>> {
        if let Some(batch) = self.pending.take() {
            return Ok(Some(batch));
        }
        match self.rx.recv().await {
            Some(Ok(batch)) => Ok(Some(batch)),
            Some(Err(e)) => Err(source_err(split, e.to_string())),
            None => Ok(None),
        }
    }

    /// The rows `range` of the partition, as slices of the batches that hold them; rows before
    /// it are skipped.
    async fn take(&mut self, split: SplitId, range: RowRange) -> Result<Vec<RecordBatch>> {
        let mut pieces = Vec::new();
        while self.offset < range.end {
            let Some(batch) = self.next(split).await? else {
                return Err(source_err(
                    split,
                    format!(
                        "the partition ended at row {}, before row {} it counted",
                        self.offset, range.end
                    ),
                ));
            };
            let rows = batch.num_rows() as u64;
            let (first, last) = (self.offset, self.offset + rows);
            if last <= range.start {
                self.offset = last;
                continue;
            }
            let from = range.start.saturating_sub(first);
            let to = range.end.min(last) - first;
            pieces.push(batch.slice(from as usize, (to - from) as usize));
            if to < rows {
                self.pending = Some(batch.slice(to as usize, (rows - to) as usize));
            }
            self.offset = first + to;
        }
        Ok(pieces)
    }

    /// After the last range: the partition must have no rows beyond what was counted.
    async fn finish(&mut self, split: SplitId) -> Result<()> {
        while let Some(batch) = self.next(split).await? {
            if batch.num_rows() > 0 {
                return Err(source_err(
                    split,
                    format!(
                        "the partition has rows beyond the {} it counted: the plan is not \
                         deterministic in its row count",
                        self.offset
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// The Tokio runtime a plan runs on. DataFusion's operators spawn onto the current runtime,
/// and the source drive polls a read with a waker of its own, so a plan needs a runtime of its
/// own. Dropped without waiting for its tasks, so that dropping a source or sink anywhere,
/// inside another runtime included, is safe.
pub(crate) struct Runtime(Option<tokio::runtime::Runtime>);

impl Runtime {
    pub(crate) fn new() -> Result<Runtime> {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 8));
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("moruna-plan")
            .enable_all()
            .build()
            .map(|rt| Runtime(Some(rt)))
            .map_err(|e| plan_err(format!("a runtime for the plan: {e}")))
    }
}

impl std::ops::Deref for Runtime {
    type Target = tokio::runtime::Runtime;
    fn deref(&self) -> &tokio::runtime::Runtime {
        self.0.as_ref().expect("the runtime lives until drop")
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// What one running partition of a scan holds whatever its batch size, measured on a governed
/// view over Parquet row groups of 4 MB: the column chunks it fetched, the pages it
/// decompressed, and the batches in its exchanges (F8.8, about 9.5 MB at 64-row batches and 16
/// MB at DataFusion's default).
const PARTITION_BYTES: u64 = 64 << 20;

/// The partitions a plan is planned for inside `in_flight` bytes of batches (MH 4.5): every
/// partition of a stage runs at once, so as many as `PARTITION_BYTES` each fit, at least one
/// and at most one per core.
pub(crate) fn partitions_for(in_flight: u64) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
    (in_flight / PARTITION_BYTES).clamp(1, cores.max(1)) as usize
}

/// Batches one running partition may hold at once: the one its scan is decoding, the ones
/// queued in the exchanges above it, and the one its consumer holds.
const BATCHES_PER_PARTITION: u64 = 4;
/// The bytes a row of a variable-width column is assumed to take when the plan's statistics do
/// not say: a scan over files the statistics cover (Parquet, what a governed plan reads) is
/// measured instead.
const VARIABLE_WIDTH_BYTES: u64 = 256;
/// The fewest rows a batch is cut to, so a very wide row still moves in batches worth the
/// per-batch cost, and DataFusion's own default, the most.
const BATCH_ROWS: (u64, u64) = (64, 8192);

/// The session's batch size for `plan` inside `in_flight` bytes (MH 4.5): every partition that
/// runs at once holds `BATCHES_PER_PARTITION` batches, so a batch is `in_flight` divided by the
/// most partitions any stage of the plan runs and by that depth, in rows of the widest row the
/// plan's scans produce. A plan with more partitions, or wider rows, gets smaller batches; its
/// operators' own state is the pool's, not this.
fn batch_rows(plan: &Arc<dyn ExecutionPlan>, in_flight: u64) -> usize {
    let mut partitions = 1u64;
    let mut width = 1u64;
    let _ = plan.apply(|node| {
        partitions = partitions.max(node.output_partitioning().partition_count() as u64);
        if node.children().is_empty() {
            width = width.max(row_width(node));
        }
        Ok(TreeNodeRecursion::Continue)
    });
    let per_batch = in_flight / (partitions * BATCHES_PER_PARTITION);
    (per_batch / width).clamp(BATCH_ROWS.0, BATCH_ROWS.1) as usize
}

/// The bytes a row of a leaf's output takes: its statistics' byte size over its rows when both
/// are known, otherwise its schema's fixed widths with `VARIABLE_WIDTH_BYTES` for the rest.
fn row_width(leaf: &Arc<dyn ExecutionPlan>) -> u64 {
    if let Ok(stats) = StatisticsContext::new().compute(leaf.as_ref(), &StatisticsArgs::new())
        && let (Some(bytes), Some(rows)) = (
            stats.total_byte_size.get_value(),
            stats.num_rows.get_value(),
        )
        && *rows > 0
    {
        return (*bytes as u64).div_ceil(*rows as u64).max(1);
    }
    leaf.schema()
        .fields()
        .iter()
        .map(|f| {
            f.data_type()
                .primitive_width()
                .map_or(VARIABLE_WIDTH_BYTES, |w| w as u64)
        })
        .sum::<u64>()
        .max(1)
}

/// Cut each partition into splits of about [`SPLIT_BYTES`], numbered in partition order. A
/// split's bytes are its share of its partition's, so they are estimates once a partition is
/// cut, and its null counts are unknown.
fn divide(partitions: &[Split]) -> (Vec<Split>, Vec<Place>) {
    let mut splits = Vec::new();
    let mut places = Vec::new();
    for (partition, whole) in partitions.iter().enumerate() {
        let per_row = whole.uncompressed_bytes.div_ceil(whole.rows.max(1)).max(1);
        let rows_each = (SPLIT_BYTES / per_row).max(1);
        if whole.rows <= rows_each {
            places.push(Place {
                partition,
                first: 0,
                partition_rows: whole.rows,
            });
            splits.push(Split {
                id: (splits.len()) as SplitId,
                ..whole.clone()
            });
            continue;
        }
        let share = |bytes: u64, rows: u64| {
            (u128::from(bytes) * u128::from(rows) / u128::from(whole.rows)) as u64
        };
        let mut first = 0;
        while first < whole.rows {
            let rows = rows_each.min(whole.rows - first);
            places.push(Place {
                partition,
                first,
                partition_rows: whole.rows,
            });
            splits.push(Split {
                id: splits.len() as SplitId,
                rows,
                uncompressed_bytes: share(whole.uncompressed_bytes, rows),
                estimated: true,
                column_bytes: whole.column_bytes.iter().map(|b| share(*b, rows)).collect(),
                null_counts: vec![None; whole.null_counts.len()],
                sub_splittable: true,
            });
            first += rows;
        }
    }
    (splits, places)
}

/// Count every partition's rows and bytes: one pass over the plan, keeping nothing.
async fn count(plan: &Arc<dyn ExecutionPlan>, task: &Arc<TaskContext>) -> Result<Vec<Split>> {
    let counting = reset_plan_states(Arc::clone(plan)).map_err(|e| plan_err(e.to_string()))?;
    let columns = plan.schema().fields().len();
    let mut tasks = Vec::new();
    for partition in 0..plan.output_partitioning().partition_count() {
        let counting = Arc::clone(&counting);
        let task = Arc::clone(task);
        tasks.push(tokio::spawn(async move {
            let mut batches = counting.execute(partition, task)?;
            let mut split = Split {
                id: partition as SplitId,
                rows: 0,
                uncompressed_bytes: 0,
                estimated: false,
                column_bytes: vec![0; columns],
                null_counts: vec![Some(0); columns],
                sub_splittable: true,
            };
            while let Some(batch) = batches.next().await {
                let batch = batch?;
                split.rows += batch.num_rows() as u64;
                for (i, column) in batch.columns().iter().enumerate() {
                    let bytes = column_bytes(column.as_ref());
                    split.uncompressed_bytes += bytes;
                    split.column_bytes[i] += bytes;
                    if let Some(Some(nulls)) = split.null_counts.get_mut(i) {
                        *nulls += column.null_count() as u64;
                    }
                }
            }
            Ok::<Split, DataFusionError>(split)
        }));
    }
    let mut splits = Vec::with_capacity(tasks.len());
    for (partition, task) in tasks.into_iter().enumerate() {
        let split = task
            .await
            .map_err(|e| source_err(partition as SplitId, e.to_string()))?
            .map_err(|e| source_err(partition as SplitId, e.to_string()))?;
        splits.push(split);
    }
    Ok(splits)
}

/// The bytes a column's rows occupy once compacted.
fn column_bytes(column: &dyn Array) -> u64 {
    column
        .to_data()
        .get_slice_memory_size()
        .unwrap_or_else(|_| column.get_array_memory_size()) as u64
}

/// True when the plan's partitions come out in the same order every run.
pub(crate) fn deterministic(plan: &Arc<dyn ExecutionPlan>) -> bool {
    let mut fixed = true;
    let _ = plan.apply(|node| {
        if matches!(node.name(), "RepartitionExec" | "CoalescePartitionsExec") {
            fixed = false;
        }
        let _ = node.apply_expressions(&mut |e| {
            if is_volatile(e) {
                fixed = false;
            }
            Ok(TreeNodeRecursion::Continue)
        });
        Ok(if fixed {
            TreeNodeRecursion::Continue
        } else {
            TreeNodeRecursion::Stop
        })
    });
    fixed
}

/// The schema a payload has: the arena copy unpacks dictionaries (the decode copy, f.4).
fn unpacked(schema: &SchemaRef) -> SchemaRef {
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|f| match f.data_type() {
            DataType::Dictionary(_, values) => f.as_ref().clone().with_data_type(*values.clone()),
            _ => f.as_ref().clone(),
        })
        .collect();
    Arc::new(Schema::new(fields).with_metadata(schema.metadata().clone()))
}

/// The slices of one range as one compact batch: only the range's rows, not the buffers of the
/// batches they were cut from, so the arena copy is the size of the range.
fn compact(schema: &SchemaRef, pieces: &[RecordBatch]) -> std::result::Result<RecordBatch, String> {
    let batch = match pieces {
        [] => return Ok(RecordBatch::new_empty(Arc::clone(schema))),
        [one] => {
            let all = UInt32Array::from_iter_values(0..one.num_rows() as u32);
            moruna_kernel::arrow::compute::take_record_batch(one, &all)
        }
        many => moruna_kernel::arrow::compute::concat_batches(&many[0].schema(), many),
    };
    batch.map_err(|e| format!("compacting a range: {e}"))
}

pub(crate) fn plan_err(msg: impl Into<String>) -> MorunaError {
    MorunaError::Plan(msg.into())
}

fn source_err(split: SplitId, msg: impl Into<String>) -> MorunaError {
    MorunaError::Source {
        split,
        msg: msg.into(),
    }
}
