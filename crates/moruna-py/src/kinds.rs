//! The Python kinds (SDD 18 f.1, 12 f.3): the only code that knows how a job's Python parts
//! become a run's.
//!
//! A task of the user's code is an `image` task, whatever its role. `moruna.run` holds the
//! objects of the tasks it was given, and registers the `image` kinds of every role as
//! [`LocalKernel`], [`LocalSource`] and [`LocalSink`]: a task whose object the run holds runs as
//! that object, in this process, built or not; any other runs its image, as the runtime's
//! `image` kind does. It registers [`Iterable`] for the iterables it holds, declares the kinds
//! the program serves itself (`@moruna.kind`, SDD 18 KD-I11), each [`PyDeclaredTask`]'s
//! declaration with its guest port answered by the Python callable that serves it ([`PyServe`]),
//! and answers the needs of image tasks as guest ports, each by what the resolver supplied
//! ([`PyNeed`]). The command (`moruna run`) registers the runtime's kinds with those needs.
//!
//! Every other kind is the runtime's ([`Registry::standard`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moruna_adapters::PyKernel;
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, settings_schema};
use moruna_kernel::{Answer, GuestCall, GuestPort, Kernel, MorunaError, Result, SourceSchema};
use moruna_runtime::kinds::image::Image;
use moruna_runtime::kinds::{
    BuiltSink, BuiltSource, Context, Embedding, Entry, Registry, RunnableKind, SinkKind,
    SourceKind, Stage,
};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyModule};
use serde::Deserialize;
use serde_json::json;

/// A stage of a Python function, with the adapter's type for `bind_allocator` and `gil_state`.
fn python_stage(kernel: Arc<PyKernel>) -> Stage {
    let mut stage = Stage::of(Arc::clone(&kernel) as Arc<dyn Kernel>);
    stage.python = Some(kernel);
    stage
}

/// The `image` function kind of `moruna.run`: a task whose function the run holds is that
/// function, by its place among the run's functions; any other runs its image.
pub struct LocalKernel {
    /// Function by function; `None` for one the run does not hold.
    pub kernels: Vec<Option<Arc<PyKernel>>>,
}

impl RunnableKind for LocalKernel {
    fn declaration(&self) -> Declaration {
        RunnableKind::declaration(&Image)
    }

    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage> {
        let held = entry
            .index()
            .and_then(|at| self.kernels.get(at).cloned().flatten());
        match held {
            Some(kernel) => {
                let _: moruna_runtime::kinds::image::Settings = entry.settings()?;
                Ok(python_stage(kernel))
            }
            None => Image.stage(entry, cx),
        }
    }
}

/// Take the object `moruna.run` was given for the entry at its role's place `entry.index()`,
/// once; `None` when it holds none there.
fn take<T>(slots: &Mutex<Vec<Option<T>>>, entry: &Entry<'_>) -> Option<T> {
    entry.index().and_then(|at| {
        slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(at)
            .and_then(Option::take)
    })
}

/// The `image` source kind of `moruna.run`: a task whose `moruna.Source` the run holds is that
/// object (07 e.6); any other runs its image.
pub struct LocalSource {
    /// Each source's instance, by its place among the run's sources, taken once.
    pub objects: Mutex<Vec<Option<Py<PyAny>>>>,
}

impl SourceKind for LocalSource {
    fn declaration(&self) -> Declaration {
        SourceKind::declaration(&Image)
    }

    fn source(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSource> {
        match take(&self.objects, entry) {
            Some(object) => {
                let _: moruna_runtime::kinds::image::Settings = entry.settings()?;
                Ok(user_source(object))
            }
            None => Image.source(entry, cx),
        }
    }
}

/// The `image` sink kind of `moruna.run`: a task whose `moruna.Sink` the run holds is that
/// object (08 f.10), its target what its `target` attribute names; any other runs its image.
pub struct LocalSink {
    /// Each sink's instance, by its place among the run's sinks, taken once.
    pub objects: Mutex<Vec<Option<Py<PyAny>>>>,
}

impl SinkKind for LocalSink {
    fn declaration(&self) -> Declaration {
        SinkKind::declaration(&Image)
    }

    fn sink(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSink> {
        match take(&self.objects, entry) {
            Some(object) => {
                let _: moruna_runtime::kinds::image::Settings = entry.settings()?;
                Ok(user_sink(object))
            }
            None => Image.sink(entry, cx),
        }
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
        title: None,
        description: None,
        takes: None,
        gives: None,
        requires: Vec::new(),
        provenance: None,
    }
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
        let (iterator, schema) = take(&self.objects, entry).ok_or_else(|| {
            MorunaError::Plan(format!(
                "{}: an IteratorSource was named without its object",
                entry.field()
            ))
        })?;
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

/// A need of an image task, answered by what the resolver supplied for it: a Python callable
/// taking the request's bytes and returning the answer's (`moruna._job._Served`). An exception
/// is the port's refusal, in its words.
pub struct PyNeed {
    call: Py<PyAny>,
}

impl PyNeed {
    /// The need answered by `call`.
    pub fn new(call: Py<PyAny>) -> PyNeed {
        PyNeed { call }
    }
}

impl GuestPort for PyNeed {
    fn call(&self, request: Vec<u8>, _call: &GuestCall) -> Answer {
        Python::attach(|py| {
            let answered = self
                .call
                .bind(py)
                .call1((PyBytes::new(py, &request),))
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
    library(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
    )?
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
    let mut registry = library(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
    )
    .map_err(to_err)?;
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

/// What `moruna.run` runs with: the runtime's kinds, the `image` kinds of every role over the
/// objects it holds (each at its task's place among its role's: `kernels` by function,
/// `iterables` and `sources` by source, `sinks` by sink), the kinds the program serves
/// (`served`, each declaration with the callable that answers its guest port), declared in the
/// order given (the same kind given twice is declared once, and two declarations of one name are
/// refused), and the needs of its image tasks, each a guest port answered by what the resolver
/// supplied (`needs`).
pub fn library(
    kernels: Vec<Option<Arc<PyKernel>>>,
    iterables: Vec<Option<Held>>,
    sources: Vec<Option<Py<PyAny>>>,
    sinks: Vec<Option<Py<PyAny>>>,
    served: Vec<(Declaration, Py<PyAny>)>,
    needs: BTreeMap<String, Py<PyAny>>,
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
    for (need, call) in needs {
        if ports.contains_key(&need) {
            return Err(MorunaError::Config {
                name: "ports",
                msg: format!("`{need}` is a need of an image task and a served kind's port"),
            });
        }
        ports.insert(need, Arc::new(PyNeed::new(call)));
    }
    let mut kinds = Registry::standard().with_embedding(Arc::new(Serving(ports)));
    kinds.replace_kernel(Arc::new(LocalKernel { kernels }))?;
    kinds.replace_source(Arc::new(LocalSource {
        objects: Mutex::new(sources),
    }))?;
    kinds.replace_sink(Arc::new(LocalSink {
        objects: Mutex::new(sinks),
    }))?;
    kinds.register_source(Arc::new(Iterable {
        objects: Mutex::new(iterables),
    }))?;
    for declaration in declared {
        kinds.declare(declaration)?;
    }
    Ok(kinds)
}

/// What the command (`moruna run`, `moruna serve`) runs with: the runtime's kinds, the needs of
/// its image tasks answered by what the resolver `moruna run --resolver` names supplied.
pub fn command(needs: BTreeMap<String, Py<PyAny>>) -> Result<Registry> {
    let ports: BTreeMap<String, Arc<dyn GuestPort>> = needs
        .into_iter()
        .map(|(need, call)| (need, Arc::new(PyNeed::new(call)) as Arc<dyn GuestPort>))
        .collect();
    Ok(Registry::standard().with_embedding(Arc::new(Serving(ports))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The library's `image` kinds declare themselves as the runtime's do, so a job checked
    /// and planned here is checked and planned as a runner would; with nothing held, a task runs
    /// its image.
    #[test]
    fn the_library_image_kinds_are_the_runtimes() {
        let kinds = library(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
        )
        .expect("the library's kinds");
        let standard = Registry::standard();
        for role in [Role::Source, Role::Kernel, Role::Sink] {
            let (ours, theirs) = match role {
                Role::Source => (
                    kinds.source("t", "image").unwrap().0,
                    standard.source("t", "image").unwrap().0,
                ),
                Role::Kernel => (
                    kinds.kernel("t", "image").unwrap().0,
                    standard.kernel("t", "image").unwrap().0,
                ),
                Role::Sink => (
                    kinds.sink("t", "image").unwrap().0,
                    standard.sink("t", "image").unwrap().0,
                ),
            };
            assert_eq!(ours, theirs);
            ours.check(role).unwrap_or_else(|e| panic!("{e}"));
        }
        assert!(
            kinds.kernel("t", "python").is_err(),
            "no python kind is left"
        );
    }
}
