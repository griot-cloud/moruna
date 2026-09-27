//! peQL, the contract-native engine, on both ends of a run (MH 4.5): a governed plan as the
//! source, a contract write as the sink.
//!
//! The source is the plan peQL makes for the caller: gated, with every shape that applies to
//! them in it (group and whole-result suppression, noise) and the privacy budgets charged when
//! it is planned. Moruna runs that plan and sees only the batches it emits. The sink is peQL's
//! write path, in parts: each morsel is conformed to the contract's row schema, enriched,
//! flagged and written as Parquet under the contract's layout, and the manifest (the
//! validation verdict a query is later answered from) is refreshed once, on `finish`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use datafusion::config::Dialect;
use datafusion::prelude::{SessionConfig, SessionContext};
use moruna_kernel::{
    BoxFuture, MorunaError, Payload, PayloadKind, PayloadSpec, Result, Seq, Sink, SinkSummary,
    SourceSchema, TierPref,
};
use peql::{Caller, Engine, WriteMode, Writing};

use crate::memory::PlanMemory;
use crate::plan_source::{PlanSource, Runtime, plan_err};

/// What a run reads from peQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeqlRead {
    /// One contract, every row and column the caller may see (`Engine::view`).
    Contract(String),
    /// SQL in which every table is a contract (`Engine::plan`).
    Sql(String),
}

impl PeqlRead {
    /// The contracts the read names, without planning it: the SQL is parsed, nothing is
    /// resolved. What a job document compares its sink against.
    pub fn contracts(&self) -> std::result::Result<Vec<String>, String> {
        match self {
            PeqlRead::Contract(name) => Ok(vec![name.clone()]),
            PeqlRead::Sql(sql) => {
                // Identifiers as peQL's own session reads them: not normalised.
                let config = SessionConfig::new()
                    .set_bool("datafusion.sql_parser.enable_ident_normalization", false);
                let state = SessionContext::new_with_config(config).state();
                let statement = state
                    .sql_to_statement(sql, &Dialect::Generic)
                    .map_err(|e| e.to_string())?;
                let tables = state
                    .resolve_table_references(&statement)
                    .map_err(|e| e.to_string())?;
                let mut names: Vec<String> = tables.iter().map(|t| t.table().to_string()).collect();
                names.sort();
                names.dedup();
                Ok(names)
            }
        }
    }
}

impl PlanSource {
    /// The plan peQL makes for `caller` (MH 4.5): resolved, gated and shaped, its budgets
    /// charged now. A refusal (an unknown contract, a `decide` rule, a failed guarantee, an
    /// exhausted budget) is a `Plan` error carrying peQL's own words.
    /// Its operators hold their working memory in `memory`, the share of the run's budget the
    /// facade gives the plan, and it is planned for as many partitions as that share's batches
    /// in flight can hold at once ([`crate::plan_source::partitions_for`]).
    pub fn peql(
        engine: &Engine,
        read: &PeqlRead,
        caller: &Caller,
        memory: &PlanMemory,
    ) -> Result<PlanSource> {
        let partitions = crate::plan_source::partitions_for(memory.pool.in_flight());
        let runtime = Runtime::with_threads(partitions + 1)?;
        let partitions = Some(partitions);
        let planned = runtime
            .block_on(async {
                match read {
                    PeqlRead::Contract(name) => engine.view_for(name, caller, partitions).await,
                    PeqlRead::Sql(sql) => engine.plan_for(sql, caller, partitions).await,
                }
            })
            .map_err(|e| plan_err(format!("peQL: {e}")))?;
        PlanSource::build(planned.plan, &planned.ctx, runtime, memory)
    }
}

/// The memory a contract write holds outside the arena, inside the run's budget (MH 4.5).
///
/// The run gives the sink a share of its budget before the sink opens; the share is divided
/// among the parts that may be written at once, and each part is written inside its piece
/// (`Writing::with_memory`): its row groups, file buffers, bloom filters and a clustered
/// layout's sort. A part is one morsel, so a piece is also the largest morsel the sink takes.
#[derive(Debug)]
pub struct WriteMemory {
    share: AtomicU64,
    parts: u64,
}

impl WriteMemory {
    /// A share of nothing yet, divided among `parts` writes in flight.
    pub fn new(parts: u16) -> WriteMemory {
        WriteMemory {
            share: AtomicU64::new(0),
            parts: u64::from(parts.max(1)),
        }
    }

    /// Set the share. Parts are sized when the sink opens and keep that size.
    pub fn set_limit(&self, bytes: u64) {
        self.share.store(bytes, Ordering::SeqCst);
    }

    /// The share in force.
    pub fn limit(&self) -> u64 {
        self.share.load(Ordering::SeqCst)
    }

    /// What one part in flight may hold.
    pub fn per_part(&self) -> u64 {
        self.limit() / self.parts
    }

    /// The parts that may be written at once.
    pub fn parts(&self) -> u64 {
        self.parts
    }
}

/// A contract write as a sink (MH 4.5): `Engine::begin_write` on `open`, one
/// `Engine::write_part` per morsel, `Engine::finish_write` on `finish`, which validates the
/// data and refreshes the manifest. Parts land in files of their own, so morsels are written as
/// they arrive, in any order and concurrently, each inside its piece of `memory`.
pub struct PeqlSink {
    engine: Arc<Engine>,
    contract: String,
    mode: WriteMode,
    memory: Arc<WriteMemory>,
    runtime: Runtime,
    writing: Option<Arc<Writing>>,
    rows: AtomicU64,
    bytes: AtomicU64,
}

impl PeqlSink {
    /// A write under `contract` by `caller`, refused here unless the caller may write it
    /// (`Engine::authorize_write`: the contract's owner tenant, or anyone for a contract with
    /// no owner).
    pub fn new(
        engine: Arc<Engine>,
        contract: &str,
        caller: &Caller,
        mode: WriteMode,
        memory: Arc<WriteMemory>,
    ) -> Result<PeqlSink> {
        engine
            .authorize_write(contract, caller)
            .map_err(|e| MorunaError::Sink(format!("peQL: {e}")))?;
        Ok(PeqlSink {
            engine,
            contract: contract.to_string(),
            mode,
            runtime: Runtime::with_threads(usize::try_from(memory.parts()).unwrap_or(2))?,
            memory,
            writing: None,
            rows: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        })
    }
}

fn sink_err(e: impl std::fmt::Display) -> MorunaError {
    MorunaError::Sink(format!("peQL: {e}"))
}

impl Sink for PeqlSink {
    fn open(&mut self, _schema: &SourceSchema) -> Result<()> {
        let mut writing = self
            .runtime
            .block_on(self.engine.begin_write(&self.contract, self.mode))
            .map_err(sink_err)?;
        if let Ok(per_part) = usize::try_from(self.memory.per_part())
            && per_part > 0
        {
            writing = writing.with_memory(per_part);
        }
        self.writing = Some(Arc::new(writing));
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn write(&self, _seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let Payload::Table(batch, _) = payload else {
                return Err(MorunaError::Sink(
                    "peQL writes tables, and this morsel is a tensor".into(),
                ));
            };
            let writing = self
                .writing
                .clone()
                .ok_or_else(|| MorunaError::Sink("write before open".into()))?;
            let engine = Arc::clone(&self.engine);
            let bytes = batch.get_array_memory_size() as u64;
            let rows = self
                .runtime
                .spawn(async move { engine.write_part(&writing, vec![batch]).await })
                .await
                .map_err(sink_err)?
                .map_err(sink_err)?;
            self.rows.fetch_add(rows as u64, Ordering::SeqCst);
            self.bytes.fetch_add(bytes, Ordering::SeqCst);
            Ok(())
        })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        let writing = self
            .writing
            .take()
            .ok_or_else(|| MorunaError::Sink("finish before open".into()))?;
        let writing = Arc::try_unwrap(writing)
            .map_err(|_| MorunaError::Sink("finish while a write is in flight".into()))?;
        let report = self
            .runtime
            .block_on(self.engine.finish_write(writing))
            .map_err(sink_err)?;
        if !report.verdict.valid {
            return Err(MorunaError::Sink(format!(
                "peQL: `{}` is written but not servable: {}",
                self.contract,
                report.verdict.breached.join(", ")
            )));
        }
        let files = self
            .engine
            .manifest(&self.contract)
            .map_err(sink_err)?
            .map(|m| m.files.into_iter().map(|f| f.path).collect())
            .unwrap_or_default();
        Ok(SinkSummary {
            rows: report.rows_written as u64,
            bytes: self.bytes.load(Ordering::SeqCst),
            files,
        })
    }
}
