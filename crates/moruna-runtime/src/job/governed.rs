//! The peQL ends of a run (MH 4.5): `"kind": "datafusion"` as the source, `"kind": "peql"` as
//! the sink. One engine per root per run, shared by both ends, so a run that reads one contract
//! and writes another on the same disk has one ledger and one audit log open. Its contracts are
//! the ones the document carries (`contracts`), registered in memory for this run alone: the
//! engine reads no contract from its root, and keeps none there.
//!
//! The engine is opened when the document is built, because what the sink equals source rule
//! compares is where each contract's files are, as the engine resolves its binding, and not the
//! contract's name: two contracts bound to the same files are one target.

use std::sync::Arc;

use super::SpecError;
use super::build::KernelLoader;
use crate::spec::{EngineMemory, SinkSpec, SourceSpec};
use moruna_kernel::Result;

/// A source's builder, the data locations it reads, and the memory its plan holds.
pub(super) type Read = (SourceSpec, Vec<String>, Option<Arc<dyn EngineMemory>>);

/// A sink's builder, the data location it writes, and the memory its writes hold.
pub(super) type Write = (SinkSpec, String, Option<Arc<dyn EngineMemory>>);

/// The engines a run opens, by root, each with the contracts the document carries. Opened at
/// the lifecycle's "sources and sinks built" step.
#[derive(Clone, Default)]
pub(super) struct Engines {
    contracts: Vec<super::ContractEntry>,
    #[cfg(feature = "peql")]
    open: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                std::path::PathBuf,
                std::sync::Arc<moruna_datafusion::engine::Engine>,
            >,
        >,
    >,
}

impl Engines {
    /// The engines of a run that carries `contracts`.
    pub(super) fn carrying(contracts: &[super::ContractEntry]) -> Engines {
        Engines {
            contracts: contracts.to_vec(),
            #[cfg(feature = "peql")]
            open: Default::default(),
        }
    }

    /// A document that carries contracts and has no governed end to read them is refused: the
    /// contracts would be carried for nothing.
    pub(super) fn check_used(&self, governed: bool) -> Result<()> {
        if !self.contracts.is_empty() && !governed {
            return Err(SpecError::new(
                "contracts",
                "carried, and no `datafusion` source or `peql` sink reads or writes under them",
            )
            .into());
        }
        Ok(())
    }
}

/// A `datafusion` source reads exactly one of a contract and SQL.
fn check_read(contract: &Option<String>, sql: &Option<String>) -> Result<()> {
    match (contract, sql) {
        (Some(c), None) if !c.is_empty() => Ok(()),
        (None, Some(s)) if !s.trim().is_empty() => Ok(()),
        _ => Err(SpecError::new(
            "source",
            "a datafusion source names exactly one of `contract` and `sql`",
        )
        .into()),
    }
}

fn root_of(field: &str, root: &str) -> Result<std::path::PathBuf> {
    if root.trim().is_empty() {
        return Err(SpecError::new(field, "empty: the engine's root is the disk it reads").into());
    }
    Ok(super::translate::local_path(root))
}

/// Whether a write replaces the contract's data: `append` (absent) or `overwrite`.
fn overwrite_of(mode: &Option<String>) -> Result<bool> {
    match mode.as_deref().unwrap_or("append") {
        "append" => Ok(false),
        "overwrite" => Ok(true),
        other => Err(SpecError::new(
            "sink.mode",
            format!("unknown mode `{other}` (use \"append\" or \"overwrite\")"),
        )
        .into()),
    }
}

/// The source's builder, where the contracts it reads keep their files, and its plan's memory.
pub(super) fn source(
    root: &str,
    contract: &Option<String>,
    sql: &Option<String>,
    caller: &serde_json::Map<String, serde_json::Value>,
    loader: &dyn KernelLoader,
    engines: &Engines,
) -> Result<Read> {
    check_read(contract, sql)?;
    let root = root_of("source.root", root)?;
    imp::source(root, contract, sql, caller, loader, engines)
}

/// The sink's builder, where the contract it writes keeps its files, and its writes' memory.
pub(super) fn sink(
    root: &str,
    contract: &str,
    caller: &serde_json::Map<String, serde_json::Value>,
    mode: &Option<String>,
    loader: &dyn KernelLoader,
    engines: &Engines,
) -> Result<Write> {
    if contract.is_empty() {
        return Err(SpecError::new("sink.contract", "empty: a write is under a contract").into());
    }
    let overwrite = overwrite_of(mode)?;
    let root = root_of("sink.root", root)?;
    imp::sink(root, contract, caller, overwrite, loader, engines)
}

#[cfg(feature = "peql")]
pub use imp::entry_of;

#[cfg(feature = "peql")]
mod imp {
    use std::path::PathBuf;
    use std::sync::Arc;

    use moruna_datafusion::engine::audit::JsonlAudit;
    use moruna_datafusion::engine::budget::BudgetStore;
    use moruna_datafusion::engine::store::Registered;
    use moruna_datafusion::engine::{Caller, Engine, PeqlError, WriteMode};
    use moruna_datafusion::{
        BudgetPool, PeqlRead, PeqlSink, PlanMemory, PlanSource, WriteMemory, location_target,
    };
    use moruna_kernel::{MorunaError, Result, Sink, Source};
    use parcel_runtime::compiled::CompiledBytes;

    use super::{Engines, KernelLoader, Read, SpecError, Write};
    use crate::spec::{EngineMemory, SinkSpec, SourceSpec};

    /// The facade sizes a plan's pool; this is how it reaches it.
    impl EngineMemory for BudgetPool {
        fn set_limit(&self, bytes: u64) {
            BudgetPool::set_limit(self, bytes);
        }

        fn morsel_max(&self) -> Option<u64> {
            None
        }

        fn note(&self) -> String {
            format!(
                "the source's plan operators held at most {} of the {} bytes they could \
                 reserve, and were refused {} times, which a spilling operator answers by \
                 spilling",
                self.peak(),
                self.capacity(),
                self.refusals()
            )
        }
    }

    /// Where a contract's files are, as its binding resolves them: the path or the object
    /// URL the sink equals source rule compares. A contract peQL does not know is refused here,
    /// by the field that names it.
    fn target(engine: &Engine, field: &str, contract: &str) -> Result<Option<String>> {
        let location = engine.location(contract).map_err(|e| match e {
            PeqlError::UnknownContract(name) => SpecError::new(
                field,
                format!("`{name}` is not among the contracts the document carries (`contracts`)"),
            ),
            e => SpecError::new(field, format!("peQL: {e}")),
        })?;
        location
            .as_ref()
            .map(location_target)
            .transpose()
            .map_err(|e| SpecError::new(field, format!("peQL: {e}")).into())
    }

    fn caller_of(
        field: &str,
        caller: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Caller> {
        serde_json::from_value(serde_json::Value::Object(caller.clone()))
            .map_err(|e| SpecError::new(field, format!("not a caller: {e}")).into())
    }

    impl Engines {
        /// The engine at `root`: its manifests, ledger and audit log on the disk, its contracts
        /// the document's, each admitted by the loader and registered as compiled, in memory.
        fn open(&self, root: &PathBuf, loader: &dyn KernelLoader) -> Result<Arc<Engine>> {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(engine) = open.get(root) {
                return Ok(Arc::clone(engine));
            }
            let config = |msg: String| MorunaError::Config { name: "peql", msg };
            let state = root.join("_peql");
            std::fs::create_dir_all(&state)
                .map_err(|e| config(format!("the engine's state at {}: {e}", state.display())))?;
            let budgets = BudgetStore::open(state.join("budgets.json"))
                .map_err(|e| config(format!("the ledger at {}: {e}", state.display())))?;
            let mut engine = Engine::in_memory(root.clone())
                .with_budgets(Arc::new(budgets))
                .with_audit(Arc::new(JsonlAudit::new(state.join("audit.jsonl"))));
            // The loader's resolution, when it has one, before anything asks where a contract is.
            if let Some(bindings) = loader.bindings(root)? {
                engine = engine.with_bindings(bindings);
            }
            for (index, entry) in self.contracts.iter().enumerate() {
                let field = format!("contracts[{index}]");
                loader.admit(index, entry)?;
                let (document, functions) = parts(&field, entry)?;
                let registered = engine
                    .register_compiled(document, entry.compiled.as_bytes(), &functions)
                    .map_err(|e| SpecError::new(&field, format!("peQL: {e}")))?;
                for audience in &entry.audiences {
                    engine
                        .publish(registered.name(), audience)
                        .map_err(|e| SpecError::new(format!("{field}.audiences"), e.to_string()))?;
                }
            }
            let engine = Arc::new(engine);
            open.insert(root.clone(), Arc::clone(&engine));
            Ok(engine)
        }
    }

    /// An entry's document and function modules, as the engine reads them.
    fn parts(
        field: &str,
        entry: &super::super::ContractEntry,
    ) -> Result<(
        parcel_core::ContractDoc,
        Vec<parcel_runtime::compiled::BundledFunction>,
    )> {
        let document = serde_json::from_value(entry.document.clone())
            .map_err(|e| SpecError::new(format!("{field}.document"), e.to_string()))?;
        let functions = entry
            .functions
            .iter()
            .enumerate()
            .map(|(i, f)| {
                serde_json::from_value(f.clone())
                    .map_err(|e| SpecError::new(format!("{field}.functions[{i}]"), e.to_string()))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((document, functions))
    }

    /// The entry that carries `registered`, as a document writes it: what a caller that compiled
    /// a contract with peQL puts in `contracts`.
    pub fn entry_of(registered: &Registered) -> Result<super::super::ContractEntry> {
        let invalid = |e: String| MorunaError::Config {
            name: "peql",
            msg: format!("`{}`: {e}", registered.name()),
        };
        let compiled = registered.compilation.to_bytes().map_err(invalid)?;
        Ok(super::super::ContractEntry {
            document: serde_json::to_value(&registered.doc).map_err(|e| invalid(e.to_string()))?,
            compiled: String::from_utf8(compiled).map_err(|e| invalid(e.to_string()))?,
            functions: registered
                .functions
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| invalid(e.to_string()))?,
            audiences: Vec::new(),
        })
    }

    pub(super) fn source(
        root: PathBuf,
        contract: &Option<String>,
        sql: &Option<String>,
        caller: &serde_json::Map<String, serde_json::Value>,
        loader: &dyn KernelLoader,
        engines: &Engines,
    ) -> Result<Read> {
        let caller = caller_of("source.caller", caller)?;
        let (read, field) = match (contract, sql) {
            (Some(name), _) => (PeqlRead::Contract(name.clone()), "source.contract"),
            (None, sql) => (PeqlRead::Sql(sql.clone().unwrap_or_default()), "source.sql"),
        };
        let names = read
            .contracts()
            .map_err(|e| SpecError::new("source.sql", e))?;
        let engine = engines.open(&root, loader)?;
        let mut targets = Vec::new();
        for name in names {
            targets.extend(target(&engine, field, &name)?);
        }
        // Sized by the facade before the source is built (MH 4.5); nothing may be held until then.
        let pool = Arc::new(BudgetPool::new(0));
        let memory = Arc::clone(&pool);
        Ok((
            SourceSpec::Build(Box::new(move |ctx| {
                let memory = PlanMemory {
                    pool: memory,
                    spill_dir: ctx.discovered.profile.staging_dir.clone(),
                };
                Ok(
                    Arc::new(PlanSource::peql(&engine, &read, &caller, &memory)?)
                        as Arc<dyn Source>,
                )
            })),
            targets,
            Some(pool as Arc<dyn EngineMemory>),
        ))
    }

    /// The facade sizes a write's memory; this is how it reaches it.
    impl EngineMemory for WriteMemory {
        fn set_limit(&self, bytes: u64) {
            WriteMemory::set_limit(self, bytes);
        }

        /// A part is one morsel, written inside its piece of the share.
        fn morsel_max(&self) -> Option<u64> {
            Some(self.per_part())
        }

        fn note(&self) -> String {
            format!(
                "the sink's writes held their parts to {} bytes each, {} in flight",
                self.per_part(),
                crate::config::SINK_CONCURRENCY
            )
        }
    }

    pub(super) fn sink(
        root: PathBuf,
        contract: &str,
        caller: &serde_json::Map<String, serde_json::Value>,
        overwrite: bool,
        loader: &dyn KernelLoader,
        engines: &Engines,
    ) -> Result<Write> {
        let caller = caller_of("sink.caller", caller)?;
        let mode = if overwrite {
            WriteMode::Overwrite
        } else {
            WriteMode::Append
        };
        let engine = engines.open(&root, loader)?;
        let written = target(&engine, "sink.contract", contract)?.ok_or_else(|| {
            SpecError::new(
                "sink.contract",
                format!("`{contract}` is served from a table, and only files are written"),
            )
        })?;
        let contract = contract.to_string();
        // Sized by the facade before the sink is built (MH 4.5).
        let memory = Arc::new(WriteMemory::new(crate::config::SINK_CONCURRENCY));
        let held = Arc::clone(&memory);
        Ok((
            SinkSpec::Build(Box::new(move |_ctx| {
                Ok(
                    Box::new(PeqlSink::new(engine, &contract, &caller, mode, held)?)
                        as Box<dyn Sink>,
                )
            })),
            written,
            Some(memory as Arc<dyn EngineMemory>),
        ))
    }
}

#[cfg(not(feature = "peql"))]
mod imp {
    use super::{Engines, KernelLoader, Read, SpecError};
    use moruna_kernel::Result;

    const ABSENT: &str = "this build of moruna has no peQL bridge (feature `peql`)";

    pub(super) fn source(
        _root: std::path::PathBuf,
        _contract: &Option<String>,
        _sql: &Option<String>,
        _caller: &serde_json::Map<String, serde_json::Value>,
        _loader: &dyn KernelLoader,
        _engines: &Engines,
    ) -> Result<Read> {
        Err(SpecError::new("source.kind", ABSENT).into())
    }

    pub(super) fn sink(
        _root: std::path::PathBuf,
        _contract: &str,
        _caller: &serde_json::Map<String, serde_json::Value>,
        _overwrite: bool,
        _loader: &dyn KernelLoader,
        _engines: &Engines,
    ) -> Result<super::Write> {
        Err(SpecError::new("sink.kind", ABSENT).into())
    }
}
