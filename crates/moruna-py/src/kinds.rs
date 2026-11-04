//! The Python kinds (SDD 18 f.1, 12 f.3): the only code that knows how a document's Python parts
//! become a run's. Two programs register them, each its own adapters:
//!
//! - the command (`moruna run`, `moruna serve`, `moruna kinds` with the Python adapter compiled
//!   in) registers [`Imported`], the `python` kinds of every role, which import the code a task
//!   names (`ref`), construct it with its `params` and supply its `needs` through a resolver;
//! - the library (`moruna.run`) registers [`Given`], the `python` kernel kind that hands back the
//!   kernels it was given, and [`Iterable`], [`UserSource`] and [`UserSink`] for the iterable and
//!   the user's `moruna.Source` and `moruna.Sink` it holds, and declares the kinds the program
//!   serves itself (`@moruna.kind`, SDD 18 KD-I11): each [`PyDeclaredTask`]'s declaration, its
//!   guest port answered by the Python callable that serves it ([`PyServe`]).
//!
//! Every other kind is the runtime's ([`Registry::standard`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moruna_adapters::PyKernel;
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, settings_schema};
use moruna_kernel::{Answer, GuestCall, GuestPort, Kernel, MorunaError, Result, SourceSchema};
use moruna_runtime::kinds::{
    BuiltSink, BuiltSource, Context, Embedding, Entry, Registry, RunnableKind, SinkKind,
    SourceKind, Stage, check_pin,
};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyModule};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::kernel::PyKernelHandle;

/// A `python` task's settings, whatever its role: the code by reference, the data its
/// constructor takes, the names of what the runner must supply it, and the fingerprint that
/// pins its code.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// `module:attribute`: a `moruna.Source`, `moruna.Function` or `moruna.Sink` class, which
    /// the runner constructs with `params`, or a function decorated with `@moruna.function`;
    /// the module is a dotted name on the interpreter's path, the attribute dotted for a nested
    /// one.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The constructor's arguments by name: data only (JSON values).
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub params: serde_json::Map<String, serde_json::Value>,
    /// The names of what the runner must supply the task (`needs`), each passed to it under its
    /// name; Moruna does not read them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// The fingerprint the loaded code must have, hex, optionally prefixed `algo:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// The `python` kind's declaration for `role`, for an adapter whose code comes from
/// `isolation`.
fn python_declaration(role: Role, isolation: Isolation) -> Declaration {
    let (summary, inputs, outputs) = match role {
        Role::Source => (
            "A moruna.Source, by reference",
            Flow::Nothing,
            Flow::Batches,
        ),
        Role::Kernel => (
            "A moruna.Function, by reference",
            Flow::Batches,
            Flow::Batches,
        ),
        Role::Sink => ("A moruna.Sink, by reference", Flow::Batches, Flow::Nothing),
    };
    Declaration {
        name: "python".into(),
        role,
        summary: summary.into(),
        settings: settings_schema::<Settings>(
            "python",
            vec![
                json!({"ref": "mypackage.functions:Shout"}),
                json!({"ref": "mypackage.functions:Enrich", "params": {"table": "orders"},
                       "needs": ["warehouse"]}),
            ],
        ),
        inputs,
        outputs,
        needs: Needs {
            gpu: if role == Role::Kernel {
                Fact::BySettings
            } else {
                Fact::No
            },
            ..Needs::nothing()
        },
        deterministic: Fact::No,
        preserves_columns: Fact::No,
        isolation,
        served: None,
    }
}

/// A stage of a Python function, with the adapter's type for `bind_allocator` and `gil_state`.
fn python_stage(kernel: Arc<PyKernel>) -> Stage {
    let mut stage = Stage::of(Arc::clone(&kernel) as Arc<dyn Kernel>);
    stage.python = Some(kernel);
    stage
}

/// The object a `python` task's settings name, made ready to run by `moruna._job._materialize`:
/// the class constructed with its params, its fingerprint checked, its needs supplied by the
/// resolver `resolver` names (`module:attribute`, the command's `--resolver`). A refusal names
/// the field and why.
fn materialize(entry: &Entry<'_>, role: Role, resolver: Option<&str>) -> Result<Py<PyAny>> {
    let doc: Settings = entry.settings()?;
    if doc.reference.is_empty() {
        return Err(entry.refuse("ref", "a python task names its code as `module:attribute`"));
    }
    let settings = serde_json::to_string(&doc).map_err(|e| entry.refuse("ref", e.to_string()))?;
    Python::attach(|py| {
        py.import("moruna._job")
            .and_then(|m| m.getattr("_materialize"))
            .and_then(|f| f.call1((role.as_str(), entry.field(), settings, resolver)))
            .map(Bound::unbind)
            .map_err(|e| entry.refuse("ref", e.value(py).to_string()))
    })
}

/// The command's `python` kinds: each imports the code a task's settings name, constructs it
/// with its params, checks its fingerprint and supplies its needs through the resolver the
/// command was given (`--resolver`). Code imported by path, so a strict build refuses them (16
/// PK-I8, KD-I6).
#[derive(Clone, Default)]
pub struct Imported {
    /// `module:attribute` of the resolver that supplies needs; `None`: a task with needs is
    /// refused.
    pub resolver: Option<String>,
}

impl RunnableKind for Imported {
    fn declaration(&self) -> Declaration {
        python_declaration(Role::Kernel, Isolation::Imported)
    }

    fn stage(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<Stage> {
        let object = materialize(entry, Role::Kernel, self.resolver.as_deref())?;
        let kernel = Python::attach(|py| {
            object
                .bind(py)
                .cast::<PyKernelHandle>()
                .map(|handle| Arc::clone(&handle.get().kernel))
                .map_err(|_| entry.refuse("ref", "does not name a moruna.Function"))
        })?;
        Ok(python_stage(kernel))
    }
}

impl SourceKind for Imported {
    fn declaration(&self) -> Declaration {
        python_declaration(Role::Source, Isolation::Imported)
    }

    fn source(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSource> {
        let object = materialize(entry, Role::Source, self.resolver.as_deref())?;
        Ok(user_source(object))
    }
}

impl SinkKind for Imported {
    fn declaration(&self) -> Declaration {
        python_declaration(Role::Sink, Isolation::Imported)
    }

    fn sink(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSink> {
        let object = materialize(entry, Role::Sink, self.resolver.as_deref())?;
        Ok(user_sink(object))
    }
}

/// A user's `moruna.Source` as a built source. `PySource::new` calls the user's `plan()` at the
/// lifecycle's "sources built" step, so a plan error surfaces before a sink is opened or a
/// morsel is read (12 f.1).
fn user_source(object: Py<PyAny>) -> BuiltSource {
    BuiltSource {
        spec: moruna_runtime::SourceSpec::Build(Box::new(move |ctx| {
            Ok(Arc::new(moruna_sources::PySource::new(
                object,
                ctx.reactor.clone(),
            )?))
        })),
        targets: Vec::new(),
        memory: None,
    }
}

/// A user's `moruna.Sink` as a built sink, its target what its `target` attribute names (a
/// string), or none.
fn user_sink(object: Py<PyAny>) -> BuiltSink {
    let target = Python::attach(|py| {
        object
            .bind(py)
            .getattr("target")
            .ok()
            .and_then(|t| t.extract::<String>().ok())
            .unwrap_or_default()
    });
    BuiltSink {
        spec: moruna_runtime::SinkSpec::Build(Box::new(move |_ctx| {
            Ok(Box::new(moruna_sinks::PySink::new(object)?) as Box<dyn moruna_kernel::Sink>)
        })),
        target,
        memory: None,
    }
}

/// What the command runs with: the runtime's kinds and the [`Imported`] `python` kinds, whose
/// needs `resolver` (`module:attribute`) supplies.
pub fn command(resolver: Option<String>) -> Result<Registry> {
    let mut kinds = Registry::standard();
    let imported = Arc::new(Imported { resolver });
    kinds.register_kernel(Arc::clone(&imported) as Arc<dyn RunnableKind>)?;
    kinds.register_source(Arc::clone(&imported) as Arc<dyn SourceKind>)?;
    kinds.register_sink(imported as Arc<dyn SinkKind>)?;
    Ok(kinds)
}

/// The library's `python` kernel kind: the kernels `moruna.run` was given, handed back by the
/// entry's place among the run's kernels. Objects the caller holds, not code imported by path.
pub struct Given {
    /// Entry by entry; `None` for an entry another kind builds.
    pub kernels: Vec<Option<Arc<PyKernel>>>,
}

impl RunnableKind for Given {
    fn declaration(&self) -> Declaration {
        python_declaration(Role::Kernel, Isolation::Caller)
    }

    fn stage(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<Stage> {
        let doc: Settings = entry.settings()?;
        let index = entry.index().unwrap_or(usize::MAX);
        let kernel = self.kernels.get(index).cloned().flatten().ok_or_else(|| {
            MorunaError::Plan(format!("{}: no such function was passed", entry.field()))
        })?;
        check_pin(entry, doc.fingerprint.as_deref(), kernel.as_ref())?;
        Ok(python_stage(kernel))
    }
}

/// Settings of a kind that has none: the object it stands for is the caller's.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Nothing {}

/// The declaration of a source or sink kind whose part is an object the library caller holds.
fn held_declaration(name: &str, role: Role, summary: &str) -> Declaration {
    Declaration {
        name: name.into(),
        role,
        summary: summary.into(),
        settings: settings_schema::<Nothing>(name, vec![json!({})]),
        inputs: if role == Role::Source {
            Flow::Nothing
        } else {
            Flow::Batches
        },
        outputs: if role == Role::Source {
            Flow::Batches
        } else {
            Flow::Nothing
        },
        needs: Needs::nothing(),
        deterministic: Fact::No,
        preserves_columns: Fact::No,
        isolation: Isolation::Caller,
        served: None,
    }
}

/// Take the object `moruna.run` was given for the entry at its role's place `entry.index()`,
/// once.
fn take<T>(slots: &Mutex<Vec<Option<T>>>, entry: &Entry<'_>, what: &str) -> Result<T> {
    entry
        .index()
        .and_then(|at| {
            slots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(at)
                .and_then(Option::take)
        })
        .ok_or_else(|| {
            MorunaError::Plan(format!(
                "{}: {what} was named without its object",
                entry.field()
            ))
        })
}

/// A Python iterable and the schema it promises.
pub type Held = (Py<PyAny>, SourceSchema);

/// The library's `iterator` source kind: the Python iterables `moruna.run` was given, with their
/// schemas.
pub struct Iterable {
    /// Each source's iterable and its schema, by its place among the run's sources, taken once.
    pub objects: Mutex<Vec<Option<Held>>>,
}

impl SourceKind for Iterable {
    fn declaration(&self) -> Declaration {
        held_declaration(
            "iterator",
            Role::Source,
            "A Python iterable the caller holds",
        )
    }

    fn source(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSource> {
        let Nothing {} = entry.settings()?;
        let (iterator, schema) = take(&self.objects, entry, "an IteratorSource")?;
        Ok(BuiltSource {
            spec: moruna_runtime::SourceSpec::Build(Box::new(move |ctx| {
                Ok(Arc::new(moruna_sources::PyIteratorSource::new(
                    iterator,
                    schema,
                    ctx.reactor.clone(),
                )?))
            })),
            targets: Vec::new(),
            memory: None,
        })
    }
}

/// The library's `python` source kind: the user's `moruna.Source` instances `moruna.run` holds
/// (07 e.6), each by its place among the run's sources.
pub struct UserSource {
    /// Each source's instance, by its place among the run's sources, taken once.
    pub objects: Mutex<Vec<Option<Py<PyAny>>>>,
}

impl SourceKind for UserSource {
    fn declaration(&self) -> Declaration {
        python_declaration(Role::Source, Isolation::Caller)
    }

    fn source(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSource> {
        let _: Settings = entry.settings()?;
        Ok(user_source(take(&self.objects, entry, "a moruna.Source")?))
    }
}

/// The library's `python` sink kind: the user's `moruna.Sink` instances (08 f.10). It names the
/// target the sink's `target` attribute gives, or none, which the sink equals source rule skips.
pub struct UserSink {
    /// Each sink's instance, by its place among the run's sinks, taken once.
    pub objects: Mutex<Vec<Option<Py<PyAny>>>>,
}

impl SinkKind for UserSink {
    fn declaration(&self) -> Declaration {
        python_declaration(Role::Sink, Isolation::Caller)
    }

    fn sink(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSink> {
        let _: Settings = entry.settings()?;
        Ok(user_sink(take(&self.objects, entry, "a moruna.Sink")?))
    }
}

/// A task of a kind the program serves itself (`@moruna.kind`, SDD 18 KD-I11): what calling a
/// `moruna.DeclaredKind` with settings returns and `moruna.run` takes as a kernel. The kind's
/// declaration, the task's settings, and the Python callable that answers its port.
#[pyclass(frozen, module = "moruna._core", name = "DeclaredTask")]
pub struct PyDeclaredTask {
    pub(crate) declaration: Declaration,
    pub(crate) settings: serde_json::Map<String, serde_json::Value>,
    pub(crate) serve: Py<PyAny>,
}

#[pymethods]
impl PyDeclaredTask {
    /// The kind's name.
    #[getter]
    fn kind(&self) -> String {
        self.declaration.name.clone()
    }

    /// The task's settings, as JSON.
    #[getter]
    fn settings(&self) -> String {
        moruna_job::canonical_json(&serde_json::Value::Object(self.settings.clone()))
    }
}

/// A Python callable that serves a kind's port: each request handed to
/// `moruna._kinds._answer(serve, request)` on the call's own thread, which reads the batch with
/// pyarrow, calls `serve(batch, settings)` and writes what it returns as an Arrow IPC stream. An
/// exception is the port's refusal, in the exception's words, and fails the task with it.
pub struct PyServe {
    serve: Py<PyAny>,
}

impl GuestPort for PyServe {
    fn call(&self, request: Vec<u8>, _call: &GuestCall) -> Answer {
        Python::attach(|py| {
            let answered = py
                .import("moruna._kinds")
                .and_then(|module| module.getattr("_answer"))
                .and_then(|answer| answer.call1((self.serve.bind(py), PyBytes::new(py, &request))))
                .and_then(|reply| reply.extract::<Vec<u8>>());
            match answered {
                Ok(bytes) => Answer::Ok(bytes),
                Err(e) => Answer::Error(e.to_string()),
            }
        })
    }
}

/// The program `moruna.run` is, to the kinds it runs: the guest ports of the kinds it serves.
struct Serving(BTreeMap<String, Arc<dyn GuestPort>>);

impl Embedding for Serving {
    fn guest_port(&self, name: &str) -> Option<Arc<dyn GuestPort>> {
        self.0.get(name).cloned()
    }
}

/// A declaration from its JSON, checked as `Registry::declare` checks it: a kind the program
/// serves, under a name no kind of the library's has, with a settings schema the validator reads.
fn declared_kind(text: &str) -> Result<Declaration> {
    let declaration: Declaration = serde_json::from_str(text).map_err(|e| MorunaError::Config {
        name: "kinds",
        msg: format!("not a declaration: {e}"),
    })?;
    library(Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new())?
        .declare(declaration.clone())?;
    Ok(declaration)
}

/// `moruna._core.declare_kind(declaration)`: the declaration `@moruna.kind` built, checked, as
/// canonical JSON; refused with `ConfigError` naming why.
#[pyfunction]
fn declare_kind(py: Python<'_>, declaration: &str) -> PyResult<String> {
    let declared = declared_kind(declaration)
        .map_err(|e| crate::errors::to_py_err(py, &e, crate::errors::Attachments::default()))?;
    let value = serde_json::to_value(&declared).map_err(|e| {
        pyo3::exceptions::PyValueError::new_err(format!("the declaration does not serialise: {e}"))
    })?;
    Ok(moruna_job::canonical_json(&value))
}

/// `moruna._core.declared_task(declaration, settings, serve)`: a task of the declared kind with
/// these settings, served by `serve`. The settings are checked when the run is built.
#[pyfunction]
fn declared_task(
    py: Python<'_>,
    declaration: &str,
    settings: &str,
    serve: Bound<'_, PyAny>,
) -> PyResult<PyDeclaredTask> {
    let to_err =
        |e: MorunaError| crate::errors::to_py_err(py, &e, crate::errors::Attachments::default());
    let declaration = declared_kind(declaration).map_err(to_err)?;
    let settings = match serde_json::from_str::<serde_json::Value>(settings) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => {
            return Err(to_err(MorunaError::Config {
                name: "kinds",
                msg: format!("`{}`'s settings are a JSON object", declaration.name),
            }));
        }
    };
    if !serve.is_callable() {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "`{}` is served by a callable over pyarrow batches",
            declaration.name
        )));
    }
    Ok(PyDeclaredTask {
        declaration,
        settings,
        serve: serve.unbind(),
    })
}

/// `moruna._core.kinds(declarations)`: what `moruna.kinds()` returns, as JSON: the listing of
/// the kinds `moruna.run` can run, with the declared kinds among them.
#[pyfunction]
fn kinds(py: Python<'_>, declarations: Vec<String>) -> PyResult<String> {
    let to_err =
        |e: MorunaError| crate::errors::to_py_err(py, &e, crate::errors::Attachments::default());
    let mut registry =
        library(Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()).map_err(to_err)?;
    for text in &declarations {
        let declaration = declared_kind(text).map_err(to_err)?;
        registry.declare(declaration).map_err(to_err)?;
    }
    Ok(registry.listing().to_json())
}

/// The kinds' functions and classes on `moruna._core`.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDeclaredTask>()?;
    m.add_function(wrap_pyfunction!(declare_kind, m)?)?;
    m.add_function(wrap_pyfunction!(declared_task, m)?)?;
    m.add_function(wrap_pyfunction!(kinds, m)?)?;
    Ok(())
}

/// What `moruna.run` runs with: the runtime's kinds, [`Given`] over its kernels, the objects it
/// holds as their kinds (each at its task's place among its role's: `iterables` and `sources` by
/// source, `sinks` by sink), and the kinds the program serves (`served`, each declaration with
/// the callable that answers its guest port), declared in the order given; the same kind given
/// twice is declared once, and two declarations of one name are refused.
pub fn library(
    kernels: Vec<Option<Arc<PyKernel>>>,
    iterables: Vec<Option<Held>>,
    sources: Vec<Option<Py<PyAny>>>,
    sinks: Vec<Option<Py<PyAny>>>,
    served: Vec<(Declaration, Py<PyAny>)>,
) -> Result<Registry> {
    let mut ports: BTreeMap<String, Arc<dyn GuestPort>> = BTreeMap::new();
    let mut declared: Vec<Declaration> = Vec::new();
    for (declaration, serve) in served {
        if let Some(same) = declared.iter().find(|d| d.name == declaration.name) {
            if *same != declaration {
                return Err(MorunaError::Config {
                    name: "kinds",
                    msg: format!(
                        "two kinds called `{}` are given, declared differently",
                        declaration.name
                    ),
                });
            }
            continue;
        }
        let port = declaration
            .served
            .as_ref()
            .and_then(|s| s.port.guest.clone())
            .ok_or_else(|| MorunaError::Config {
                name: "kinds",
                msg: format!(
                    "`{}` is served in this process, by a guest port",
                    declaration.name
                ),
            })?;
        ports.insert(port, Arc::new(PyServe { serve }));
        declared.push(declaration);
    }
    let mut kinds = Registry::standard().with_embedding(Arc::new(Serving(ports)));
    kinds.register_kernel(Arc::new(Given { kernels }))?;
    kinds.register_source(Arc::new(Iterable {
        objects: Mutex::new(iterables),
    }))?;
    kinds.register_source(Arc::new(UserSource {
        objects: Mutex::new(sources),
    }))?;
    kinds.register_sink(Arc::new(UserSink {
        objects: Mutex::new(sinks),
    }))?;
    for declaration in declared {
        kinds.declare(declaration)?;
    }
    Ok(kinds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_runtime::kinds::conformance::{self, Harness};

    /// A `python` task's settings are its reference, its params, its needs and its pin, and
    /// nothing else: an unknown field is refused, and empty params and needs are not written.
    #[test]
    fn python_settings_round_trip() {
        let doc: Settings = serde_json::from_value(json!({
            "ref": "pkg.mod:Enrich", "params": {"table": "orders"},
            "needs": ["warehouse.read"], "fingerprint": "sha256:00"
        }))
        .expect("settings");
        assert_eq!(doc.reference, "pkg.mod:Enrich");
        assert_eq!(doc.needs, vec!["warehouse.read".to_string()]);
        let back = serde_json::to_value(&doc).expect("serialises");
        assert_eq!(back["ref"], "pkg.mod:Enrich");
        let bare: Settings = serde_json::from_value(json!({"ref": "m:f"})).expect("bare");
        assert_eq!(
            serde_json::to_value(&bare).expect("bare"),
            json!({"ref": "m:f"})
        );
        assert!(serde_json::from_value::<Settings>(json!({"ref": "m:f", "module": "m"})).is_err());
    }

    /// KD-T4 for the Python kinds' declarations: each is one the registry accepts, and the
    /// command's registry adds `python` to the runtime's kinds.
    #[test]
    fn kd_t4_python_declarations() {
        for isolation in [Isolation::Imported, Isolation::Caller] {
            for role in [Role::Source, Role::Kernel, Role::Sink] {
                python_declaration(role, isolation)
                    .check(role)
                    .unwrap_or_else(|e| panic!("{e}"));
            }
        }
        let kinds = command(None).expect("the command's kinds");
        let (declared, _) = kinds.kernel("tasks[1]", "python").expect("python");
        assert_eq!(declared.isolation, Isolation::Imported);
        let (declared, _) = kinds.source("tasks[0]", "python").expect("python");
        assert_eq!(declared.isolation, Isolation::Imported);
        let (declared, _) = kinds.sink("tasks[2]", "python").expect("python");
        assert_eq!(declared.isolation, Isolation::Imported);
        // The library's kernel kind hands back what it was given; with nothing given, every
        // example is refused for want of a kernel, which the suite reports as KD-C3.
        let violation = conformance::kernel(
            &Given {
                kernels: Vec::new(),
            },
            &Harness::new(),
        )
        .expect_err("no kernels were given");
        assert_eq!(violation.property, "KD-C3");
    }
}
