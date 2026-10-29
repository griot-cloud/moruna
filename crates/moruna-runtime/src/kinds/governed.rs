//! The peQL ends of a run (MH 4.5): `datafusion` as a source kind, `peql` as a sink kind. One
//! engine per root per build, shared by both ends, so a run that reads one contract and writes
//! another on the same disk has one ledger and one audit log open. Its contracts are the ones
//! the document carries (`contracts`), registered in memory for this run alone: the engine reads
//! no contract from its root, and keeps none there.
//!
//! The engine is opened when the document is built, because what the sink equals source rule
//! compares is where each contract's files are, as the engine resolves its binding, and not the
//! contract's name: two contracts bound to the same files are one target.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use moruna_datafusion::engine::audit::JsonlAudit;
use moruna_datafusion::engine::budget::BudgetStore;
use moruna_datafusion::engine::store::Registered;
use moruna_datafusion::engine::{Caller, Engine, OnReadViolation, PeqlError, WriteMode};
use moruna_datafusion::{
    BudgetPool, OnViolation, PeqlRead, PeqlSink, PlanMemory, PlanSource, WriteMemory,
    location_target,
};
use moruna_kernel::{MorunaError, Result, Sink, Source};
use parcel_runtime::compiled::CompiledBytes;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{BuiltSink, BuiltSource, Context, Entry, SinkKind, SourceKind};
use crate::job::refusal;
use crate::spec::{EngineMemory, SinkSpec, SourceSpec};
use moruna_job::{
    ContractEntry, Declaration, Fact, Flow, Isolation, Needs, Role, SpecError, settings_schema,
};

/// The `datafusion` kind, a source: a governed DataFusion plan.
pub struct Datafusion;

/// The `peql` kind, a sink: a write under a contract.
pub struct Peql;

/// A `datafusion` source's settings: what peQL, opened on the disk at `root` with the job's
/// `contracts`, plans for `caller` over one contract or over SQL in which every table is a
/// contract. Exactly one of `contract` and `sql`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceSettings {
    /// The engine's root: the disk, with peQL's manifests, ledger and audit log under
    /// `<root>/_peql/`. Its contracts are the job's `contracts`, never a store on the disk.
    pub root: String,
    /// One contract, every row and column the caller may see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    /// SQL in which every table is a contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    /// The caller, as peQL reads it: `id`, `tenant`, `purpose`, and optionally `tier`,
    /// `clearance`, `classification`, `roles`, `now` and `other`.
    pub caller: serde_json::Map<String, serde_json::Value>,
    /// What the read does with rows that break a contract's row rules: `exclude` (absent)
    /// leaves them out and the run's report counts them per rule; `refuse` refuses the read
    /// when the data holds any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_violation: Option<String>,
}

/// A `peql` sink's settings: a write under a contract the job carries, by `caller`, who must be
/// the contract's owner. The manifest is refreshed when the run finishes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SinkSettings {
    /// The engine's root: the disk.
    pub root: String,
    /// The contract written under.
    pub contract: String,
    /// The writer, as peQL reads a caller.
    pub caller: serde_json::Map<String, serde_json::Value>,
    /// `append` (absent) or `overwrite`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// What the write does with rows that break the contract's row rules: `refuse` (absent)
    /// commits nothing when any does; `exclude` commits the others and keeps these under
    /// `excluded_url`; `warn` commits every row and its verdict says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_violation: Option<String>,
    /// The local directory an `exclude` write keeps the rows it excludes in, as Parquet with the
    /// rules each broke; empty or absent when the run starts. Only `exclude` takes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_url: Option<String>,
}

/// The engines a build opens, by root, each with the contracts the document carries; one value
/// per build, shared by its two ends ([`Context::shared`]).
#[derive(Clone)]
struct Engines {
    contracts: Vec<ContractEntry>,
    open: Arc<Mutex<HashMap<PathBuf, Arc<Engine>>>>,
}

impl Engines {
    fn of(cx: &mut Context) -> Engines {
        let contracts = cx.carried_contracts().to_vec();
        cx.shared(move || Engines {
            contracts,
            open: Arc::default(),
        })
    }

    /// The engine at `root`: its manifests, ledger and audit log on the disk, its contracts the
    /// document's, each admitted by the program and registered as compiled, in memory.
    fn open(&self, root: &PathBuf, cx: &Context) -> Result<Arc<Engine>> {
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
        // The program's resolution, when it has one, before anything asks where a contract is.
        if let Some(bindings) = cx.embedding().bindings(root)? {
            engine = engine.with_bindings(bindings);
        }
        for (index, entry) in self.contracts.iter().enumerate() {
            let field = format!("contracts[{index}]");
            cx.embedding().admit(index, entry)?;
            let (document, functions) = parts(&field, entry)?;
            let registered = engine
                .register_compiled(document, entry.compiled.as_bytes(), &functions)
                .map_err(|e| refusal(SpecError::new(&field, format!("peQL: {e}"))))?;
            for audience in &entry.audiences {
                engine.publish(registered.name(), audience).map_err(|e| {
                    refusal(SpecError::new(format!("{field}.audiences"), e.to_string()))
                })?;
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
    entry: &ContractEntry,
) -> Result<(
    parcel_core::ContractDoc,
    Vec<parcel_runtime::compiled::BundledFunction>,
)> {
    let document = serde_json::from_value(entry.document.clone())
        .map_err(|e| refusal(SpecError::new(format!("{field}.document"), e.to_string())))?;
    let functions = entry
        .functions
        .iter()
        .enumerate()
        .map(|(i, f)| {
            serde_json::from_value(f.clone()).map_err(|e| {
                refusal(SpecError::new(
                    format!("{field}.functions[{i}]"),
                    e.to_string(),
                ))
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok((document, functions))
}

/// The entry that carries `registered`, as a document writes it: what a caller that compiled a
/// contract with peQL puts in `contracts`.
pub fn entry_of(registered: &Registered) -> Result<ContractEntry> {
    let invalid = |e: String| MorunaError::Config {
        name: "peql",
        msg: format!("`{}`: {e}", registered.name()),
    };
    let compiled = registered.compilation.to_bytes().map_err(invalid)?;
    Ok(ContractEntry {
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

fn root_of(entry: &Entry<'_>, root: &str) -> Result<PathBuf> {
    if root.trim().is_empty() {
        return Err(entry.refuse("root", "empty: the engine's root is the disk it reads"));
    }
    Ok(crate::job::translate::local_path(root))
}

/// Where a contract's files are, as its binding resolves them: the path or the object URL the
/// sink equals source rule compares. A contract peQL does not know is refused here, by the field
/// that names it.
fn target(engine: &Engine, field: &str, contract: &str) -> Result<Option<String>> {
    let location = engine.location(contract).map_err(|e| match e {
        PeqlError::UnknownContract(name) => refusal(SpecError::new(
            field,
            format!("`{name}` is not among the contracts the document carries (`contracts`)"),
        )),
        e => refusal(SpecError::new(field, format!("peQL: {e}"))),
    })?;
    location
        .as_ref()
        .map(location_target)
        .transpose()
        .map_err(|e| refusal(SpecError::new(field, format!("peQL: {e}"))))
}

fn caller_of(
    entry: &Entry<'_>,
    caller: &serde_json::Map<String, serde_json::Value>,
) -> Result<Caller> {
    serde_json::from_value(serde_json::Value::Object(caller.clone()))
        .map_err(|e| entry.refuse("caller", format!("not a caller: {e}")))
}

/// What a read does with rows that break a contract's row rules: `exclude` (absent) leaves them
/// out of the contract's view and counts them per rule; `refuse` refuses the read when the data
/// holds any. A read is never only warned.
fn on_read_of(entry: &Entry<'_>, on_violation: Option<&str>) -> Result<OnReadViolation> {
    match on_violation.unwrap_or("exclude") {
        "exclude" => Ok(OnReadViolation::Exclude),
        "refuse" => Ok(OnReadViolation::Refuse),
        other => Err(entry.refuse(
            "on_violation",
            format!("unknown on_violation `{other}` (use \"exclude\" or \"refuse\")"),
        )),
    }
}

/// Whether a write replaces the contract's data: `append` (absent) or `overwrite`.
fn mode_of(entry: &Entry<'_>, mode: Option<&str>) -> Result<WriteMode> {
    match mode.unwrap_or("append") {
        "append" => Ok(WriteMode::Append),
        "overwrite" => Ok(WriteMode::Overwrite),
        other => Err(entry.refuse(
            "mode",
            format!("unknown mode `{other}` (use \"append\" or \"overwrite\")"),
        )),
    }
}

/// What a write does with rows that break its contract's row rules: `refuse` (absent) commits
/// nothing when any does; `exclude` commits the others and keeps these under `excluded_url`, a
/// local directory, which it needs and which no other mode takes; `warn` commits every row.
fn on_write_of(
    entry: &Entry<'_>,
    on_violation: Option<&str>,
    excluded_url: Option<&str>,
) -> Result<OnViolation> {
    let on_write = match on_violation.unwrap_or("refuse") {
        "refuse" => OnViolation::Refuse,
        "warn" => OnViolation::Warn,
        "exclude" => {
            let url = excluded_url.ok_or_else(|| {
                entry.refuse(
                    "excluded_url",
                    "absent: a write that excludes rows keeps them in a directory it names",
                )
            })?;
            return Ok(OnViolation::Exclude(excluded_dir(entry, url)?));
        }
        other => {
            return Err(entry.refuse(
                "on_violation",
                format!("unknown on_violation `{other}` (use \"refuse\", \"exclude\" or \"warn\")"),
            ));
        }
    };
    match excluded_url {
        Some(_) => Err(entry.refuse(
            "excluded_url",
            "given, and this write excludes no rows: only `\"on_violation\": \"exclude\"` keeps \
             any",
        )),
        None => Ok(on_write),
    }
}

/// The directory a write keeps the rows it excludes in: on the run's own disk, a bare path or a
/// `file://` URL.
fn excluded_dir(entry: &Entry<'_>, url: &str) -> Result<PathBuf> {
    let url = url.trim();
    if url.is_empty() {
        return Err(entry.refuse("excluded_url", "empty: a directory is named"));
    }
    if let Some(at) = url.find("://")
        && !url[..at].eq_ignore_ascii_case("file")
    {
        return Err(entry.refuse(
            "excluded_url",
            format!(
                "`{url}` is not on the run's disk: excluded rows are kept in a local directory"
            ),
        ));
    }
    Ok(crate::job::translate::local_path(url))
}

/// Excluded rows are kept apart from the data: not where the contract keeps its files, nor
/// where a source reads, nor in a directory that holds either.
fn check_excluded(
    entry: &Entry<'_>,
    excluded_url: &str,
    written: &str,
    sources: &[String],
) -> Result<()> {
    use crate::job::translate::normalise_target;
    let excluded = normalise_target(excluded_url);
    for (what, other) in std::iter::once(("the contract's files", written))
        .chain(sources.iter().map(|s| ("a source", s.as_str())))
    {
        let other = normalise_target(other);
        if excluded == other
            || excluded.starts_with(&format!("{other}/"))
            || other.starts_with(&format!("{excluded}/"))
        {
            return Err(entry.refuse(
                "excluded_url",
                format!(
                    "`{excluded_url}` resolves to `{excluded}`, which overlaps {what} at \
                     `{other}`: excluded rows are kept apart from the data"
                ),
            ));
        }
    }
    Ok(())
}

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
            "the source's plan operators held at most {} of the {} bytes they could reserve, and \
             were refused {} times, which a spilling operator answers by spilling",
            self.peak(),
            self.capacity(),
            self.refusals()
        )
    }
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

fn caller_schema_example() -> serde_json::Value {
    json!({"id": "ana", "tenant": "demo", "purpose": "analytics"})
}

impl SourceKind for Datafusion {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "datafusion".into(),
            role: Role::Source,
            summary: "A governed DataFusion plan over the contracts the job carries".into(),
            settings: settings_schema::<SourceSettings>(
                "datafusion",
                vec![
                    json!({"root": "/disk", "contract": "sales/orders",
                           "caller": caller_schema_example()}),
                    json!({"root": "/disk", "sql": "SELECT region, sum(total) FROM \"sales/orders\" GROUP BY region",
                           "caller": caller_schema_example(), "on_violation": "refuse"}),
                ],
            ),
            inputs: Flow::Files,
            outputs: Flow::Batches,
            needs: Needs {
                contracts: Fact::Yes,
                ..Needs::nothing()
            },
            deterministic: Fact::Yes,
            preserves_columns: Fact::BySettings,
            isolation: Isolation::Builtin,
        }
    }

    fn source(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSource> {
        let settings: SourceSettings = entry.settings()?;
        let (read, field) = match (&settings.contract, &settings.sql) {
            (Some(c), None) if !c.is_empty() => {
                (PeqlRead::Contract(c.clone()), entry.at("contract"))
            }
            (None, Some(s)) if !s.trim().is_empty() => (PeqlRead::Sql(s.clone()), entry.at("sql")),
            _ => {
                return Err(refusal(SpecError::new(
                    entry.field().to_string(),
                    "a datafusion source names exactly one of `contract` and `sql`",
                )));
            }
        };
        let on_violation = on_read_of(entry, settings.on_violation.as_deref())?;
        let root = root_of(entry, &settings.root)?;
        let caller = caller_of(entry, &settings.caller)?;
        let names = read.contracts().map_err(|e| entry.refuse("sql", e))?;
        let engines = Engines::of(cx);
        let engine = engines.open(&root, cx)?;
        let mut targets = Vec::new();
        for name in names {
            targets.extend(target(&engine, &field, &name)?);
        }
        // Sized by the facade before the source is built (MH 4.5); nothing may be held until then.
        let pool = Arc::new(BudgetPool::new(0));
        let memory = Arc::clone(&pool);
        Ok(BuiltSource {
            spec: SourceSpec::Build(Box::new(move |ctx| {
                let memory = PlanMemory {
                    pool: memory,
                    spill_dir: ctx.discovered.profile.staging_dir.clone(),
                };
                Ok(Arc::new(PlanSource::peql(
                    &engine,
                    &read,
                    &caller,
                    on_violation,
                    &memory,
                )?) as Arc<dyn Source>)
            })),
            targets,
            memory: Some(pool as Arc<dyn EngineMemory>),
        })
    }
}

impl SinkKind for Peql {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "peql".into(),
            role: Role::Sink,
            summary: "A write under a contract the job carries, by the contract's owner".into(),
            settings: settings_schema::<SinkSettings>(
                "peql",
                vec![
                    json!({"root": "/disk", "contract": "sales/orders_clean",
                           "caller": caller_schema_example()}),
                    json!({"root": "file:///disk", "contract": "sales/orders_clean",
                           "caller": caller_schema_example(), "mode": "overwrite",
                           "on_violation": "exclude", "excluded_url": "/scratch/excluded"}),
                ],
            ),
            inputs: Flow::Batches,
            outputs: Flow::Files,
            needs: Needs {
                contracts: Fact::Yes,
                ..Needs::nothing()
            },
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: Isolation::Builtin,
        }
    }

    fn sink(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSink> {
        let settings: SinkSettings = entry.settings()?;
        if settings.contract.is_empty() {
            return Err(entry.refuse("contract", "empty: a write is under a contract"));
        }
        let mode = mode_of(entry, settings.mode.as_deref())?;
        let on_violation = on_write_of(
            entry,
            settings.on_violation.as_deref(),
            settings.excluded_url.as_deref(),
        )?;
        let root = root_of(entry, &settings.root)?;
        let caller = caller_of(entry, &settings.caller)?;
        let engines = Engines::of(cx);
        let engine = engines.open(&root, cx)?;
        let contract = settings.contract;
        let written = target(&engine, &entry.at("contract"), &contract)?.ok_or_else(|| {
            entry.refuse(
                "contract",
                format!("`{contract}` is served from a table, and only files are written"),
            )
        })?;
        if let Some(excluded) = &settings.excluded_url {
            check_excluded(entry, excluded, &written, cx.source_targets())?;
        }
        // Sized by the facade before the sink is built (MH 4.5).
        let memory = Arc::new(WriteMemory::new(crate::config::SINK_CONCURRENCY));
        let held = Arc::clone(&memory);
        Ok(BuiltSink {
            spec: SinkSpec::Build(Box::new(move |_ctx| {
                Ok(Box::new(PeqlSink::new(
                    engine,
                    &contract,
                    &caller,
                    mode,
                    on_violation,
                    held,
                )?) as Box<dyn Sink>)
            })),
            target: written,
            memory: Some(memory as Arc<dyn EngineMemory>),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::job::{BuildOptions, build};
    use crate::kinds::conformance::{self, Harness};
    use crate::kinds::{Registry, Standalone};
    use moruna_job::{JobSpec, KindDoc};

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn opts() -> BuildOptions<'static> {
        BuildOptions {
            strict: false,
            env: &no_env,
            notes: Vec::new(),
            packages: crate::packaged::PackageHost::default(),
            host_ports: None,
        }
    }

    fn doc(value: Value) -> KindDoc {
        KindDoc::from_value(value).expect("an entry")
    }

    fn refusal(job: &JobSpec) -> String {
        match build(
            &moruna_job::Job::from_v1(job),
            &Registry::standard(),
            opts(),
        ) {
            Err(MorunaError::Config { msg, .. }) => msg,
            Err(other) => panic!("not a document refusal: {other}"),
            Ok(_) => panic!("built"),
        }
    }

    /// A disk and two contracts compiled for it, carried as a document carries them.
    fn carried(disk: &std::path::Path) -> Vec<ContractEntry> {
        let compiler = Engine::in_memory(disk);
        let schema = moruna_kernel::arrow::datatypes::Schema::new(vec![
            moruna_kernel::arrow::datatypes::Field::new(
                "id",
                moruna_kernel::arrow::datatypes::DataType::Int64,
                false,
            ),
        ]);
        let mut carried = Vec::new();
        for (name, dir) in [("demo/big", "big/"), ("demo/copy", "copy/")] {
            let contract = format!(
                "contract: {name}\nversion: 1\nowner: demo\nbinding: {{parquet: {dir}}}\n\
                 expose:\n  - {{name: id, type: int64}}\n"
            );
            let registered = compiler
                .register_contract(&contract, &schema)
                .expect("compiles");
            carried.push(entry_of(&registered).expect("an entry"));
        }
        carried
    }

    /// MH 4.5: the governed ends of a run. The document reads and writes them, refuses a
    /// malformed one naming the field, opens the engine to resolve where each contract's files
    /// are, and keeps excluded rows apart from the data.
    #[test]
    fn governed_sources_and_sinks_are_documents() {
        let text = r#"{"moruna_spec":1,
            "source":{"kind":"datafusion","root":"/disk","contract":"demo/big",
                      "caller":{"id":"gus","tenant":"partner","purpose":"analytics"}},
            "sink":{"kind":"peql","root":"file:///disk","contract":"demo/copy","mode":"overwrite",
                    "caller":{"id":"ana","tenant":"demo","purpose":"analytics"}}}"#;
        let job = JobSpec::from_json(text).expect("parses");
        let again = JobSpec::from_json(&job.canonical_json()).expect("canonical parses");
        assert_eq!(again, job);
        assert!(job.canonical_json().contains(r#""kind":"datafusion""#));

        let caller = json!({"id": "a", "tenant": "demo", "purpose": "analytics"});
        let with_source = |contract: Option<&str>, sql: Option<&str>, root: &str| {
            let mut j = job.clone();
            j.source = doc(
                json!({"kind": "datafusion", "root": root, "contract": contract,
                                  "sql": sql, "caller": caller}),
            );
            j
        };
        for bad in [
            with_source(Some("a"), Some("SELECT 1"), "/disk"),
            with_source(None, None, "/disk"),
            with_source(Some(""), None, "/disk"),
        ] {
            assert!(refusal(&bad).contains("source: a datafusion source names exactly one"));
        }
        assert!(refusal(&with_source(Some("a"), None, " ")).contains("source.root"));
        let with_sink = |contract: &str, mode: Option<&str>, root: &str| {
            let mut j = job.clone();
            j.source = doc(json!({"kind": "parquet", "url": "/in"}));
            j.sink = doc(json!({"kind": "peql", "root": root, "contract": contract,
                                "caller": caller, "mode": mode}));
            j
        };
        assert!(refusal(&with_sink("", None, "/disk")).contains("sink.contract"));
        assert!(refusal(&with_sink("c", Some("upsert"), "/disk")).contains("unknown mode"));
        assert!(refusal(&with_sink("c", None, "")).contains("sink.root"));

        // What a read and a write do with rows that break a contract's row rules, refused by
        // field before any engine is opened.
        for warn in ["warn", "Exclude", ""] {
            let mut j = with_source(Some("a"), None, "/disk");
            j.source.settings.insert("on_violation".into(), json!(warn));
            let msg = refusal(&j);
            assert!(
                msg.contains(&format!(
                    "source.on_violation: unknown on_violation `{warn}` (use \"exclude\" or \
                     \"refuse\")"
                )),
                "{msg}"
            );
        }
        let with_write_violation = |on_violation: Option<&str>, excluded_url: Option<&str>| {
            let mut j = with_sink("c", None, "/disk");
            j.sink = doc(json!({"kind": "peql", "root": "/disk", "contract": "c",
                                "caller": caller, "on_violation": on_violation,
                                "excluded_url": excluded_url}));
            j
        };
        let msg = refusal(&with_write_violation(Some("drop"), None));
        assert!(
            msg.contains(
                "sink.on_violation: unknown on_violation `drop` (use \"refuse\", \"exclude\" or \
                 \"warn\")"
            ),
            "{msg}"
        );
        let msg = refusal(&with_write_violation(Some("exclude"), None));
        assert!(msg.contains("sink.excluded_url: absent"), "{msg}");
        for given in [None, Some("refuse"), Some("warn")] {
            let msg = refusal(&with_write_violation(given, Some("/scratch/excluded")));
            assert!(msg.contains("sink.excluded_url: given"), "{msg}");
        }
        for remote in ["s3://bucket/excluded", "gs://b/x", ""] {
            let msg = refusal(&with_write_violation(Some("exclude"), Some(remote)));
            assert!(msg.contains("sink.excluded_url"), "{remote}: {msg}");
        }
        let excluding = with_write_violation(Some("exclude"), Some("/scratch/excluded"));
        let text = excluding.canonical_json();
        assert!(
            text.contains(r#""on_violation":"exclude""#)
                && text.contains(r#""excluded_url":"/scratch/excluded""#),
            "{text}"
        );
        assert_eq!(JobSpec::from_json(&text).expect("parses"), excluding);
        assert!(
            !with_sink("c", None, "/disk")
                .canonical_json()
                .contains("on_violation")
        );

        // The engine is opened when the document is built, to resolve where each contract's
        // files are: a disk, and both contracts carried by the document.
        let disk = std::env::temp_dir().join(format!("moruna-kinds-peql-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&disk);
        let root = disk.display().to_string();
        let mut job = job.clone();
        job.contracts = carried(&disk);
        let again = JobSpec::from_json(&job.canonical_json()).expect("carried contracts parse");
        assert_eq!(again, job, "carried contracts round-trip");
        job.source.settings.insert("root".into(), json!(root));
        job.sink
            .settings
            .insert("root".into(), json!(format!("file://{root}")));
        let built = build(
            &moruna_job::Job::from_v1(&job),
            &Registry::standard(),
            opts(),
        )
        .expect("builds");
        assert!(built.spec.spec_digest.is_some());
        assert!(built.spec.engine_memory.len() == 1 && built.spec.sink_memory.len() == 1);
        let mut not_a_caller = job.clone();
        not_a_caller
            .sink
            .settings
            .insert("caller".into(), json!({}));
        assert!(refusal(&not_a_caller).contains("sink.caller: not a caller"));
        let mut bad_sql = job.clone();
        bad_sql.source.settings.remove("contract");
        bad_sql.source.settings.insert("sql".into(), json!("SELEC"));
        assert!(refusal(&bad_sql).contains("source.sql"));
        let mut unknown = job.clone();
        unknown
            .source
            .settings
            .insert("contract".into(), json!("demo/none"));
        assert!(refusal(&unknown).contains("is not among the contracts the document carries"));
        // Excluded rows are kept apart from the data: not under the contract's files, nor over a
        // source's, nor in a directory that holds them.
        for (dir, overlaps) in [
            (format!("{root}/copy/excluded"), "the contract's files"),
            (format!("{root}/big"), "a source"),
            (root.clone(), "the contract's files"),
        ] {
            let mut excluding = job.clone();
            excluding
                .sink
                .settings
                .insert("on_violation".into(), json!("exclude"));
            excluding
                .sink
                .settings
                .insert("excluded_url".into(), json!(dir));
            let msg = refusal(&excluding);
            assert!(
                msg.contains("sink.excluded_url") && msg.contains(&format!("overlaps {overlaps}")),
                "{dir}: {msg}"
            );
        }
        let mut excluding = job.clone();
        excluding
            .sink
            .settings
            .insert("on_violation".into(), json!("exclude"));
        excluding.sink.settings.insert(
            "excluded_url".into(),
            json!(disk.join("scratch").join("excluded").display().to_string()),
        );
        build(
            &moruna_job::Job::from_v1(&excluding),
            &Registry::standard(),
            opts(),
        )
        .expect("kept apart, it builds");
        let _ = std::fs::remove_dir_all(&disk);
    }

    /// KD-T4 for the governed kinds: both pass their ports' suites over examples that name the
    /// contracts a fixture carries.
    #[test]
    fn kd_t4_governed_conformance() {
        let disk = std::env::temp_dir().join(format!("moruna-kinds-suite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&disk);
        let contracts = carried(&disk);
        let root = disk.display().to_string();
        let caller = json!({"id": "demo", "tenant": "demo", "purpose": "analytics"});
        let make =
            move || crate::kinds::Context::new(Arc::new(Standalone)).contracts(contracts.clone());
        let make_sink = make.clone();
        let source = Harness::new().context(make).examples(vec![
            json!({"root": root, "contract": "demo/big", "caller": caller}),
            json!({"root": root, "sql": "SELECT id FROM \"demo/big\"", "caller": caller,
                   "on_violation": "refuse"}),
        ]);
        let passed = conformance::source(&Datafusion, &source).unwrap_or_else(|v| panic!("{v}"));
        assert!(passed.cases > 4, "{passed:?}");
        let sink = Harness::new().context(make_sink).examples(vec![
            json!({"root": root, "contract": "demo/copy", "caller": caller}),
            json!({"root": format!("file://{root}"), "contract": "demo/copy", "caller": caller,
                   "mode": "overwrite", "on_violation": "exclude",
                   "excluded_url": disk.join("excluded").display().to_string()}),
        ]);
        let passed = conformance::sink(&Peql, &sink).unwrap_or_else(|v| panic!("{v}"));
        assert!(passed.cases > 4, "{passed:?}");
        let _ = std::fs::remove_dir_all(&disk);
    }
}
