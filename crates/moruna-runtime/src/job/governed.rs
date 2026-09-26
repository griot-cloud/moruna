//! The peQL ends of a run (MH 4.5): `"kind": "datafusion"` as the source, `"kind": "peql"` as
//! the sink. One engine per root per run, shared by both ends, so a run that reads one contract
//! and writes another on the same disk has one store, one ledger and one audit log open.

use super::SpecError;
use crate::spec::{SinkSpec, SourceSpec};
use moruna_kernel::Result;

/// The engines a run opens, by root. Opened at the lifecycle's "sources and sinks built" step.
#[derive(Clone, Default)]
pub(super) struct Engines {
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

/// The source's builder and the contracts it reads.
pub(super) fn source(
    root: &str,
    contract: &Option<String>,
    sql: &Option<String>,
    caller: &serde_json::Map<String, serde_json::Value>,
    engines: &Engines,
) -> Result<(SourceSpec, Vec<String>)> {
    check_read(contract, sql)?;
    let root = root_of("source.root", root)?;
    imp::source(root, contract, sql, caller, engines)
}

/// The sink's builder and the contract it writes.
pub(super) fn sink(
    root: &str,
    contract: &str,
    caller: &serde_json::Map<String, serde_json::Value>,
    mode: &Option<String>,
    engines: &Engines,
) -> Result<(SinkSpec, String)> {
    if contract.is_empty() {
        return Err(SpecError::new("sink.contract", "empty: a write is under a contract").into());
    }
    let overwrite = overwrite_of(mode)?;
    let root = root_of("sink.root", root)?;
    imp::sink(root, contract, caller, overwrite, engines)
}

#[cfg(feature = "peql")]
mod imp {
    use std::path::PathBuf;
    use std::sync::Arc;

    use moruna_datafusion::engine::{Caller, Engine, WriteMode};
    use moruna_datafusion::{PeqlRead, PeqlSink, PlanSource};
    use moruna_kernel::{MorunaError, Result, Sink, Source};

    use super::{Engines, SpecError};
    use crate::spec::{SinkSpec, SourceSpec};

    /// Where a contract's data is, as the sink equals source rule compares targets.
    fn target(root: &std::path::Path, contract: &str) -> String {
        format!("peql://{}#{contract}", root.display())
    }

    fn caller_of(
        field: &str,
        caller: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Caller> {
        serde_json::from_value(serde_json::Value::Object(caller.clone()))
            .map_err(|e| SpecError::new(field, format!("not a caller: {e}")).into())
    }

    impl Engines {
        fn open(&self, root: &PathBuf) -> Result<Arc<Engine>> {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(engine) = open.get(root) {
                return Ok(Arc::clone(engine));
            }
            let engine = Arc::new(Engine::open(root).map_err(|e| MorunaError::Config {
                name: "peql",
                msg: format!("opening the engine at {}: {e}", root.display()),
            })?);
            open.insert(root.clone(), Arc::clone(&engine));
            Ok(engine)
        }
    }

    pub(super) fn source(
        root: PathBuf,
        contract: &Option<String>,
        sql: &Option<String>,
        caller: &serde_json::Map<String, serde_json::Value>,
        engines: &Engines,
    ) -> Result<(SourceSpec, Vec<String>)> {
        let caller = caller_of("source.caller", caller)?;
        let read = match (contract, sql) {
            (Some(name), _) => PeqlRead::Contract(name.clone()),
            (None, sql) => PeqlRead::Sql(sql.clone().unwrap_or_default()),
        };
        let targets = read
            .contracts()
            .map_err(|e| SpecError::new("source.sql", e))?
            .iter()
            .map(|c| target(&root, c))
            .collect();
        let engines = engines.clone();
        Ok((
            SourceSpec::Build(Box::new(move |_ctx| {
                let engine = engines.open(&root)?;
                Ok(Arc::new(PlanSource::peql(&engine, &read, &caller)?) as Arc<dyn Source>)
            })),
            targets,
        ))
    }

    pub(super) fn sink(
        root: PathBuf,
        contract: &str,
        caller: &serde_json::Map<String, serde_json::Value>,
        overwrite: bool,
        engines: &Engines,
    ) -> Result<(SinkSpec, String)> {
        let caller = caller_of("sink.caller", caller)?;
        let mode = if overwrite {
            WriteMode::Overwrite
        } else {
            WriteMode::Append
        };
        let written = target(&root, contract);
        let contract = contract.to_string();
        let engines = engines.clone();
        Ok((
            SinkSpec::Build(Box::new(move |_ctx| {
                let engine = engines.open(&root)?;
                Ok(Box::new(PeqlSink::new(engine, &contract, &caller, mode)?) as Box<dyn Sink>)
            })),
            written,
        ))
    }
}

#[cfg(not(feature = "peql"))]
mod imp {
    use super::{Engines, SpecError};
    use crate::spec::{SinkSpec, SourceSpec};
    use moruna_kernel::Result;

    const ABSENT: &str = "this build of moruna has no peQL bridge (feature `peql`)";

    pub(super) fn source(
        _root: std::path::PathBuf,
        _contract: &Option<String>,
        _sql: &Option<String>,
        _caller: &serde_json::Map<String, serde_json::Value>,
        _engines: &Engines,
    ) -> Result<(SourceSpec, Vec<String>)> {
        Err(SpecError::new("source.kind", ABSENT).into())
    }

    pub(super) fn sink(
        _root: std::path::PathBuf,
        _contract: &str,
        _caller: &serde_json::Map<String, serde_json::Value>,
        _overwrite: bool,
        _engines: &Engines,
    ) -> Result<(SinkSpec, String)> {
        Err(SpecError::new("sink.kind", ABSENT).into())
    }
}
