//! peQL, the contract-native engine, on both ends of a run (MH 4.5): a governed plan as the
//! source, a contract write as the sink.
//!
//! The source is the plan peQL makes for the caller: gated, with every shape that applies to
//! them in it (group and whole-result suppression, noise) and the privacy budgets charged when
//! it is planned. Rows that break a contract's row rules never leave its view: by default the
//! plan leaves them out and peQL counts them per rule, and a read may instead be refused when
//! any are there. Moruna runs that plan, sees only the batches it emits, and carries peQL's
//! resolution of every contract it read to the run's report. The sink is peQL's write path, in
//! parts: each morsel is conformed to the contract's row schema, enriched, flagged and written
//! as Parquet under the contract's layout, and on `finish` the write is committed and peQL's
//! report of it goes to the run's report as peQL wrote it. Rows that break the contract's row
//! rules are refused (nothing is committed), excluded (the others are committed and these are
//! kept in a directory of the run's own) or committed with a verdict that says so, as the job
//! asks.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use datafusion::arrow::array::RecordBatch;
use datafusion::config::Dialect;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::{SessionConfig, SessionContext};
use moruna_kernel::{
    BoxFuture, ExcludedRows, MorunaError, Payload, PayloadKind, PayloadSpec, Result, Seq, Sink,
    SinkSummary, SinkWrite, SourceRead, SourceSchema, TierPref,
};
use peql::{
    Caller, Engine, Location, OnReadViolation, OnWriteViolation, PeqlError, WriteMode, Writing,
};

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
    /// charged now, with rows that break a contract's row rules dealt with as `on_violation`
    /// says: left out and counted per rule (`Exclude`), or the read refused when the data holds
    /// any (`Refuse`). A refusal (an unknown contract, a `decide` rule, a failed guarantee, rows
    /// that break a rule under `Refuse`, an exhausted budget) is a `Plan` error carrying peQL's
    /// own words. What peQL decided for every contract the read names, the rows each view left
    /// out among it, is the source's read report (`{"contracts": [<peQL's Resolution>, …]}`).
    /// Its operators hold their working memory in `memory`, the share of the run's budget the
    /// facade gives the plan, and it is planned for as many partitions as that share's batches
    /// in flight can hold at once ([`crate::plan_source::partitions_for`]).
    pub fn peql(
        engine: &Engine,
        read: &PeqlRead,
        caller: &Caller,
        on_violation: OnReadViolation,
        memory: &PlanMemory,
    ) -> Result<PlanSource> {
        let partitions = crate::plan_source::partitions_for(memory.pool.in_flight());
        let runtime = Runtime::with_threads(partitions + 1)?;
        let partitions = Some(partitions);
        let planned = runtime
            .block_on(async {
                match read {
                    PeqlRead::Contract(name) => {
                        engine
                            .view_for(name, caller, partitions, on_violation)
                            .await
                    }
                    PeqlRead::Sql(sql) => {
                        engine.plan_for(sql, caller, partitions, on_violation).await
                    }
                }
            })
            .map_err(|e| plan_err(format!("peQL: {e}")))?;
        let report = serde_json::json!({ "contracts": planned.contracts });
        Ok(
            PlanSource::build(planned.plan, &planned.ctx, runtime, memory)?.with_read(SourceRead {
                engine: "peql".into(),
                report,
            }),
        )
    }
}

/// Where a contract's data is, as one string: a local path as written, an object URL, or, for
/// a binding peQL adds behind a feature of its own (an Iceberg table), peQL's key for it. Two
/// contracts with the same target write the same data, which is what a job document's sink
/// equals source rule compares. Cargo unifies features, so peQL's `iceberg` may be on in a host's
/// build without this crate's `peql-iceberg`; the last arm keeps this match whole either way.
pub fn location_target(location: &Location) -> peql::Result<String> {
    match location {
        Location::Local(path) => Ok(path.display().to_string()),
        Location::Object(object) => Ok(object.url(true)),
        #[allow(unreachable_patterns)]
        other => other.key(),
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

/// What a contract write does with rows that break its contract's row rules (peQL's
/// `OnWriteViolation`), and, when it excludes them, the local directory it keeps them in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OnViolation {
    /// Nothing is committed when any row breaks a rule; the run fails naming each rule and how
    /// many rows broke it.
    #[default]
    Refuse,
    /// The rows that pass are committed; the others are written as Parquet under this
    /// directory, each with the rules it broke (peQL's `_violations` column).
    Exclude(PathBuf),
    /// Every row is committed; peQL's verdict on the write says which rules rows broke.
    Warn,
}

impl OnViolation {
    /// peQL's name for it.
    pub fn peql(&self) -> OnWriteViolation {
        match self {
            OnViolation::Refuse => OnWriteViolation::Refuse,
            OnViolation::Exclude(_) => OnWriteViolation::Exclude,
            OnViolation::Warn => OnWriteViolation::Warn,
        }
    }
}

/// The rows a write excludes, kept as they come: one Parquet file per morsel that had any,
/// named by its sequence number, under a directory that held nothing when the write opened.
struct Excluded {
    dir: PathBuf,
    /// By sequence number, so the summary lists them in the order the run issued them.
    files: Mutex<Vec<(Seq, String)>>,
    rows: AtomicU64,
}

impl Excluded {
    /// The directory, made if absent and refused if it holds anything: rows from another write
    /// beside this one's would be taken for them.
    fn open(dir: &Path) -> Result<()> {
        let fail = |msg: String| {
            MorunaError::Sink(format!(
                "peQL: the directory excluded rows are kept in, {}: {msg}",
                dir.display()
            ))
        };
        std::fs::create_dir_all(dir).map_err(|e| fail(e.to_string()))?;
        let mut entries = std::fs::read_dir(dir).map_err(|e| fail(e.to_string()))?;
        if entries.next().is_some() {
            return Err(fail(
                "it already holds files, and a write keeps the rows it excludes in a directory \
                 of its own"
                    .into(),
            ));
        }
        Ok(())
    }

    /// One morsel's excluded rows, in a file of their own.
    fn keep(&self, seq: Seq, batch: &RecordBatch) -> Result<()> {
        let path = self.dir.join(format!("excluded-{seq:020}.parquet"));
        let fail = |msg: String| MorunaError::Io {
            op: "excluded rows",
            target: path.display().to_string(),
            msg,
        };
        let file = std::fs::File::create_new(&path).map_err(|e| fail(e.to_string()))?;
        let mut writer =
            ArrowWriter::try_new(file, batch.schema(), None).map_err(|e| fail(e.to_string()))?;
        writer.write(batch).map_err(|e| fail(e.to_string()))?;
        writer.close().map_err(|e| fail(e.to_string()))?;
        self.rows
            .fetch_add(batch.num_rows() as u64, Ordering::SeqCst);
        self.files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((seq, path.display().to_string()));
        Ok(())
    }

    /// What the summary says of them.
    fn rows(&self) -> ExcludedRows {
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner()).clone();
        files.sort();
        ExcludedRows {
            url: self.dir.display().to_string(),
            files: files.into_iter().map(|(_, path)| path).collect(),
            rows: self.rows.load(Ordering::SeqCst),
        }
    }
}

/// A contract write as a sink (MH 4.5): `Engine::begin_write` on `open`, one
/// `Engine::write_part` per morsel, `Engine::finish_write` on `finish`, which validates the rows
/// written, commits them and refreshes the manifest. Parts land in files of their own, so
/// morsels are written as they arrive, in any order and concurrently, each inside its piece of
/// `memory`.
///
/// Rows that break the contract's row rules are dealt with as `on_violation` says. Under
/// `Refuse` the write commits nothing when any does, and `finish` fails naming each rule and
/// how many rows broke it. Under `Exclude` each part hands back the rows it did not write, which
/// the sink keeps under the directory named, and the rest are committed. Under `Warn` every row
/// is committed. What peQL said about a committed write is the summary's `write`, `{"engine":
/// "peql", "report": <peQL's WriteReport>}`: its rows written, the rows that broke each rule
/// (`violations`), the rows excluded (`rows_excluded`) and its verdict, with the excluded rows'
/// files beside it (`excluded`).
pub struct PeqlSink {
    engine: Arc<Engine>,
    contract: String,
    mode: WriteMode,
    on_violation: OnWriteViolation,
    excluded: Option<Arc<Excluded>>,
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
        on_violation: OnViolation,
        memory: Arc<WriteMemory>,
    ) -> Result<PeqlSink> {
        engine
            .authorize_write(contract, caller)
            .map_err(|e| MorunaError::Sink(format!("peQL: {e}")))?;
        let excluded = match &on_violation {
            OnViolation::Exclude(dir) => Some(Arc::new(Excluded {
                dir: dir.clone(),
                files: Mutex::new(Vec::new()),
                rows: AtomicU64::new(0),
            })),
            OnViolation::Refuse | OnViolation::Warn => None,
        };
        Ok(PeqlSink {
            engine,
            contract: contract.to_string(),
            mode,
            on_violation: on_violation.peql(),
            excluded,
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

/// What a failed `finish_write` ends the run with. A conflict (the table moved while the run
/// wrote, so peQL refused to commit over it) and a refusal of rows that break the contract's
/// rules each say that nothing was committed, which a host needs to know, the refusal with each
/// rule broken and how many rows broke it; every other failure is peQL's own words.
fn finish_err(contract: &str, e: PeqlError) -> MorunaError {
    match e {
        PeqlError::Conflict(msg) => MorunaError::Sink(format!(
            "peQL: `{contract}` was not committed, because its table moved while this run wrote it: {msg}"
        )),
        PeqlError::Violated { violations, .. } => MorunaError::Sink(format!(
            "peQL: `{contract}` was not committed, because rows break its rules and this write \
             refuses them: {}",
            peql::violation::describe(&violations)
        )),
        other => sink_err(other),
    }
}

impl Sink for PeqlSink {
    fn open(&mut self, _schema: &SourceSchema) -> Result<()> {
        if let Some(excluded) = &self.excluded {
            Excluded::open(&excluded.dir)?;
        }
        let mut writing = self
            .runtime
            .block_on(
                self.engine
                    .begin_write(&self.contract, self.mode, self.on_violation),
            )
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

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
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
            let part = self
                .runtime
                .spawn(async move { engine.write_part(&writing, vec![batch]).await })
                .await
                .map_err(sink_err)?
                .map_err(sink_err)?;
            // Rows handed back are the excluding write's to keep; peQL holds none of them.
            if let Some(handed) = part.excluded.filter(|b| b.num_rows() > 0) {
                let excluded = self.excluded.clone().ok_or_else(|| {
                    MorunaError::Sink(format!(
                        "peQL handed back {} rows of a write that does not exclude any",
                        handed.num_rows()
                    ))
                })?;
                self.runtime
                    .spawn_blocking(move || excluded.keep(seq, &handed))
                    .await
                    .map_err(sink_err)??;
            }
            self.rows
                .fetch_add(part.rows_written as u64, Ordering::SeqCst);
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
            .map_err(|e| finish_err(&self.contract, e))?;
        let files = report.files.iter().map(|f| f.path.clone()).collect();
        let rows = report.rows_written as u64;
        let report = serde_json::to_value(&report).map_err(|e| {
            MorunaError::Sink(format!(
                "peQL: `{}` is committed, and its write report could not be serialised: {e}",
                self.contract
            ))
        })?;
        Ok(SinkSummary {
            rows,
            bytes: self.bytes.load(Ordering::SeqCst),
            files,
            write: Some(SinkWrite {
                engine: "peql".into(),
                report,
                excluded: self.excluded.as_ref().map(|e| e.rows()),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conflict says the write was not committed and keeps peQL's account of it; any other
    /// failure of `finish_write` is peQL's own words.
    #[test]
    fn a_conflict_says_nothing_was_committed() {
        let e = finish_err(
            "demo/big",
            PeqlError::Conflict("ns.big is at snapshot Some(7)".into()),
        );
        assert_eq!(
            e.to_string(),
            "sink: peQL: `demo/big` was not committed, because its table moved while this run \
             wrote it: ns.big is at snapshot Some(7)"
        );
        let e = finish_err("demo/big", PeqlError::Invalid("bad".into()));
        assert_eq!(e.to_string(), "sink: peQL: bad");
    }

    /// A local binding's target is its path as written.
    #[test]
    fn a_local_target_is_its_path() {
        let location = Location::Local(std::path::PathBuf::from("/data/big"));
        assert_eq!(location_target(&location).expect("a path"), "/data/big");
    }
}
