//! The Python kinds (SDD 18 f.1, 12 f.3): the only code that knows how a document's Python parts
//! become a run's. Two programs register them, each its own adapters:
//!
//! - the command (`moruna run`, `moruna serve`, `moruna kinds` with the Python adapter compiled
//!   in) registers [`Imported`], the `python` kernel kind that imports the module a document names;
//! - the library (`moruna.run`) registers [`Given`], the `python` kernel kind that hands back the
//!   kernels it was given, and [`Iterable`], [`UserSource`] and [`UserSink`] for the iterable and
//!   the user's `moruna.Source` and `moruna.Sink` it holds.
//!
//! Every other kind is the runtime's ([`Registry::standard`]).

use std::sync::{Arc, Mutex};

use moruna_adapters::PyKernel;
use moruna_kernel::{Kernel, MorunaError, Result, SourceSchema};
use moruna_runtime::job::SpecError;
use moruna_runtime::kinds::{
    BuiltSink, BuiltSource, Context, Declaration, Entry, Fact, Flow, Isolation, Needs, Registry,
    Role, RunnableKind, SinkKind, SourceKind, Stage, check_pin, settings_schema,
};
use pyo3::prelude::*;
use pyo3::types::PyAny;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::kernel::{PyKernelHandle, build};

/// A `python` kernel entry's settings (MH 4.1): the callable by module and name, and the
/// decorator's arguments (12 d.2), which apply to a callable that is not already decorated.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// A module name on the interpreter's path, or a path to a `.py` file.
    pub module: String,
    /// The attribute of the module that is the kernel; dotted for a nested attribute.
    pub callable: String,
    /// The fingerprint the loaded kernel must have, hex, optionally prefixed `algo:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The decorator's `stateful`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stateful: Option<bool>,
    /// The decorator's `instances`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<u16>,
    /// The decorator's `device_memory`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_memory: Option<bool>,
    /// The decorator's `accepts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepts: Option<String>,
    /// The decorator's `tier`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// The decorator's `releases_gil`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub releases_gil: Option<bool>,
    /// The decorator's `expected_amplification`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_amplification: Option<f64>,
    /// The decorator's `preferred_rows`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_rows: Option<u64>,
    /// The decorator's `resume`: `reinit`, `checkpoint` or `forbid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// The decorator's `state_bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_bytes: Option<u64>,
    /// The decorator's `memory_guard` (E13): whether the allocator guard may refuse this
    /// kernel's requests past the ceiling; absent means on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_guard: Option<bool>,
}

impl Settings {
    /// An entry naming `callable` of `module`, with no hints.
    pub fn named(module: impl Into<String>, callable: impl Into<String>) -> Settings {
        Settings {
            module: module.into(),
            callable: callable.into(),
            ..Settings::default()
        }
    }

    /// The names of the decorator hints these settings set, in document order.
    pub fn hints_set(&self) -> Vec<&'static str> {
        let flags: [(&'static str, bool); 11] = [
            ("stateful", self.stateful.is_some()),
            ("instances", self.instances.is_some()),
            ("device_memory", self.device_memory.is_some()),
            ("accepts", self.accepts.is_some()),
            ("tier", self.tier.is_some()),
            ("releases_gil", self.releases_gil.is_some()),
            (
                "expected_amplification",
                self.expected_amplification.is_some(),
            ),
            ("preferred_rows", self.preferred_rows.is_some()),
            ("resume", self.resume.is_some()),
            ("state_bytes", self.state_bytes.is_some()),
            ("memory_guard", self.memory_guard.is_some()),
        ];
        flags
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| name)
            .collect()
    }
}

/// The `python` kernel kind's declaration, for an adapter whose code comes from `isolation`.
fn kernel_declaration(isolation: Isolation) -> Declaration {
    Declaration {
        name: "python".into(),
        role: Role::Kernel,
        summary: "A Python callable, by module and name".into(),
        settings: settings_schema::<Settings>(
            "python",
            vec![
                json!({"module": "/job/kernels.py", "callable": "shout"}),
                json!({"module": "mypackage.kernels", "callable": "Embed",
                       "stateful": true, "instances": 2, "expected_amplification": 1.5}),
            ],
        ),
        inputs: Flow::Batches,
        outputs: Flow::Batches,
        needs: Needs {
            gpu: Fact::BySettings,
            ..Needs::nothing()
        },
        deterministic: Fact::No,
        preserves_columns: Fact::No,
        isolation,
    }
}

/// A stage of a Python kernel, with the adapter's type for `bind_allocator` and `gil_state`.
fn python_stage(kernel: Arc<PyKernel>) -> Stage {
    let mut stage = Stage::of(Arc::clone(&kernel) as Arc<dyn Kernel>);
    stage.python = Some(kernel);
    stage
}

/// The command's `python` kernel kind: imports the module an entry names and finds its callable
/// (MH 4.2). Code imported by path, so a strict build refuses it (16 PK-I8, KD-I6).
pub struct Imported;

impl RunnableKind for Imported {
    fn declaration(&self) -> Declaration {
        kernel_declaration(Isolation::Imported)
    }

    fn stage(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<Stage> {
        let doc: Settings = entry.settings()?;
        if doc.module.is_empty() {
            return Err(entry.refuse("module", "a Python kernel names its module"));
        }
        if doc.callable.is_empty() {
            return Err(entry.refuse("callable", "a Python kernel names its callable"));
        }
        let (module, callable) = (doc.module.as_str(), doc.callable.as_str());
        let kernel = Python::attach(|py| -> Result<Arc<PyKernel>> {
            let object = import(py, module)
                .map_err(|e| entry.refuse("module", format!("`{module}`: {e}")))?;
            let mut target = object;
            for part in callable.split('.') {
                target = target.getattr(part).map_err(|e| {
                    entry.refuse("callable", format!("`{callable}` in `{module}`: {e}"))
                })?;
            }
            if let Ok(handle) = target.cast::<PyKernelHandle>() {
                let set = doc.hints_set();
                if let Some(first) = set.first() {
                    return Err(entry.refuse(
                        first,
                        format!(
                            "`{callable}` is already decorated with @moruna.kernel, whose \
                             arguments are its hints; the document sets {}",
                            set.join(", ")
                        ),
                    ));
                }
                return Ok(Arc::clone(&handle.get().kernel));
            }
            let handle = build(
                py,
                &target,
                doc.stateful.unwrap_or(false),
                usize::from(doc.instances.unwrap_or(1)),
                doc.device_memory.unwrap_or(false),
                doc.accepts.as_deref().unwrap_or("table"),
                doc.tier.as_deref().unwrap_or("host"),
                doc.releases_gil,
                doc.expected_amplification,
                doc.preferred_rows,
                doc.resume.as_deref().unwrap_or("reinit"),
                doc.state_bytes,
                moruna_kernel::declare::Declared::default(),
                None,
                None,
                doc.memory_guard.unwrap_or(true),
            )
            .map_err(|e| SpecError::new(entry.field().to_string(), e.to_string()))?;
            Ok(handle.kernel)
        })?;
        check_pin(entry, doc.fingerprint.as_deref(), kernel.as_ref())?;
        Ok(python_stage(kernel))
    }
}

/// A module by name, or a `.py` file by path, imported once per path.
fn import<'py>(py: Python<'py>, module: &str) -> PyResult<Bound<'py, PyAny>> {
    let is_file = module.ends_with(".py") || module.contains('/');
    if !is_file {
        return Ok(py.import(module)?.into_any());
    }
    let sys_modules = py.import("sys")?.getattr("modules")?;
    let name: String = format!(
        "_moruna_job_{}",
        module
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    );
    if let Ok(loaded) = sys_modules.get_item(&name) {
        return Ok(loaded);
    }
    let util = py.import("importlib.util")?;
    let spec = util.call_method1("spec_from_file_location", (&name, module))?;
    if spec.is_none() {
        return Err(pyo3::exceptions::PyImportError::new_err(format!(
            "`{module}` is not a Python file"
        )));
    }
    let loaded = util.call_method1("module_from_spec", (&spec,))?;
    sys_modules.set_item(&name, &loaded)?;
    if let Err(e) = spec
        .getattr("loader")?
        .call_method1("exec_module", (&loaded,))
    {
        let _ = sys_modules.del_item(&name);
        return Err(e);
    }
    Ok(loaded)
}

/// What the command runs with: the runtime's kinds and [`Imported`].
pub fn command() -> Result<Registry> {
    let mut kinds = Registry::standard();
    kinds.register_kernel(Arc::new(Imported))?;
    Ok(kinds)
}

/// The library's `python` kernel kind: the kernels `moruna.run` was given, handed back by the
/// entry's place in `kernels`. Objects the caller holds, not code imported by path.
pub struct Given {
    /// Entry by entry; `None` for an entry another kind builds.
    pub kernels: Vec<Option<Arc<PyKernel>>>,
}

impl RunnableKind for Given {
    fn declaration(&self) -> Declaration {
        kernel_declaration(Isolation::Caller)
    }

    fn stage(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<Stage> {
        let doc: Settings = entry.settings()?;
        let index = entry.index().unwrap_or(usize::MAX);
        let kernel = self.kernels.get(index).cloned().flatten().ok_or_else(|| {
            MorunaError::Plan(format!("{}: no such kernel was passed", entry.field()))
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
    }
}

/// Take the object `moruna.run` was given, once.
fn take<T>(slot: &Mutex<Option<T>>, what: &str) -> Result<T> {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .ok_or_else(|| MorunaError::Plan(format!("{what} was named without its object")))
}

/// The library's `iterator` source kind: the Python iterable `moruna.run` was given, with its
/// schema.
pub struct Iterable {
    /// The iterable and its schema, taken once.
    pub object: Mutex<Option<(Py<PyAny>, SourceSchema)>>,
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
        let (iterator, schema) = take(&self.object, "an IteratorSource")?;
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

/// The library's `python` source kind: the user's `moruna.Source` subclass instance (07 e.6).
pub struct UserSource {
    /// The instance, taken once.
    pub object: Mutex<Option<Py<PyAny>>>,
}

impl SourceKind for UserSource {
    fn declaration(&self) -> Declaration {
        held_declaration("python", Role::Source, "A moruna.Source the caller holds")
    }

    fn source(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSource> {
        let Nothing {} = entry.settings()?;
        let object = take(&self.object, "a moruna.Source")?;
        // `PySource::new` calls the user's `plan()` at the lifecycle's "sources built" step, so a
        // plan error surfaces before a sink is opened or a morsel is read (12 f.1).
        Ok(BuiltSource {
            spec: moruna_runtime::SourceSpec::Build(Box::new(move |ctx| {
                Ok(Arc::new(moruna_sources::PySource::new(
                    object,
                    ctx.reactor.clone(),
                )?))
            })),
            targets: Vec::new(),
            memory: None,
        })
    }
}

/// The library's `python` sink kind: the user's `moruna.Sink` subclass instance (08 f.10). It
/// names no location of its own, so the sink equals source rule has nothing to compare.
pub struct UserSink {
    /// The instance, taken once.
    pub object: Mutex<Option<Py<PyAny>>>,
}

impl SinkKind for UserSink {
    fn declaration(&self) -> Declaration {
        held_declaration("python", Role::Sink, "A moruna.Sink the caller holds")
    }

    fn sink(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSink> {
        let Nothing {} = entry.settings()?;
        let object = take(&self.object, "a moruna.Sink")?;
        Ok(BuiltSink {
            spec: moruna_runtime::SinkSpec::Build(Box::new(move |_ctx| {
                Ok(Box::new(moruna_sinks::PySink::new(object)?) as Box<dyn moruna_kernel::Sink>)
            })),
            target: String::new(),
            memory: None,
        })
    }
}

/// What `moruna.run` runs with: the runtime's kinds, [`Given`] over its kernels, and the
/// objects it holds as their kinds.
pub fn library(
    kernels: Vec<Option<Arc<PyKernel>>>,
    iterable: Option<(Py<PyAny>, SourceSchema)>,
    source: Option<Py<PyAny>>,
    sink: Option<Py<PyAny>>,
) -> Result<Registry> {
    let mut kinds = Registry::standard();
    kinds.register_kernel(Arc::new(Given { kernels }))?;
    kinds.register_source(Arc::new(Iterable {
        object: Mutex::new(iterable),
    }))?;
    kinds.register_source(Arc::new(UserSource {
        object: Mutex::new(source),
    }))?;
    kinds.register_sink(Arc::new(UserSink {
        object: Mutex::new(sink),
    }))?;
    Ok(kinds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_runtime::kinds::conformance::{self, Harness};

    /// The decorator's hints an entry sets are listed in document order, and the allocator
    /// guard's switch is one of them (PY-T21, E13).
    #[test]
    fn hints_are_listed() {
        let mut doc = Settings::named("m", "k");
        assert!(doc.hints_set().is_empty());
        doc.stateful = Some(true);
        doc.instances = Some(2);
        doc.device_memory = Some(false);
        doc.accepts = Some("table".into());
        doc.tier = Some("host".into());
        doc.releases_gil = Some(true);
        doc.expected_amplification = Some(1.0);
        doc.preferred_rows = Some(10);
        doc.resume = Some("reinit".into());
        doc.state_bytes = Some(1);
        assert_eq!(doc.hints_set().len(), 10);
        doc.memory_guard = Some(false);
        assert_eq!(doc.hints_set().last(), Some(&"memory_guard"));
        let json = serde_json::to_value(&doc).expect("serialises");
        assert_eq!(json["memory_guard"], serde_json::Value::Bool(false));
        let back: Settings = serde_json::from_value(json).expect("reads back");
        assert_eq!(back.memory_guard, Some(false));
    }

    /// KD-T4 for the Python kinds' declarations: each is one the registry accepts, and the
    /// command's registry adds `python` to the runtime's kinds.
    #[test]
    fn kd_t4_python_declarations() {
        for isolation in [Isolation::Imported, Isolation::Caller] {
            kernel_declaration(isolation)
                .check(Role::Kernel)
                .unwrap_or_else(|e| panic!("{e}"));
        }
        let kinds = command().expect("the command's kinds");
        let (declared, _) = kinds.kernel("kernels[0]", "python").expect("python");
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
