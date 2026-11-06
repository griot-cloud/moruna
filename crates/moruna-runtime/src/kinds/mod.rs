//! The kinds a run is made of (SDD 18): one port per role, one declaration per kind, and the
//! registry a job document's `kind` strings are resolved through.
//!
//! A document names each part of a run by a string: `source.kind`, `kernels[i].kind`,
//! `sink.kind`. The registry resolves the string to the adapter registered under it for that role,
//! or refuses naming the field and the kinds of that role it knows (KD-I1). An adapter is the only
//! code that knows its technology (decision AP1): it declares what it is (e.1) and builds its part
//! from the entry's settings, which it reads with the type its settings schema is derived from
//! (e.2, KD-I4). Nothing outside the registry and the adapters matches over kinds (KD-I3); a new
//! kind is a new adapter and a call to register it, and nothing else changes (KD-I5).

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::job::refusal;
use crate::packaged::{PackageHost, Packaged};
use crate::spec::{EngineMemory, SinkSpec, SourceSpec};
use moruna_job::{ContractEntry, Declaration, KindDoc, LISTING_VERSION, Listing, Role, SpecError};
use moruna_kernel::{Kernel, MorunaError, Result};
use serde::de::DeserializeOwned;

pub mod arrow_ipc;
pub mod conformance;
#[cfg(feature = "peql")]
pub mod governed;
pub mod image;
pub mod package;
pub mod parquet;
pub mod ports;
pub mod served;
pub mod standard;
pub mod tensor;
pub mod vortex;

/// One entry of the document as its adapter reads it: where it is (`source`, `kernels[2]`,
/// `sink`), so a refusal names its field (MH 4.1), and what it says.
pub struct Entry<'d> {
    field: String,
    index: Option<usize>,
    doc: &'d KindDoc,
}

impl<'d> Entry<'d> {
    /// The entry `doc` at `field`; `index` is its place among the tasks of its role, in
    /// document order (a version 1 document's place in `kernels`, for a kernel).
    pub fn new(field: impl Into<String>, index: Option<usize>, doc: &'d KindDoc) -> Entry<'d> {
        Entry {
            field: field.into(),
            index,
            doc,
        }
    }

    /// Its field: `source`, `kernels[i]` or `sink`.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The field of its setting `name`: `source.url`, `kernels[1].fingerprint`.
    pub fn at(&self, name: &str) -> String {
        format!("{}.{name}", self.field)
    }

    /// Its place among the tasks of its role, in document order.
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    /// What the document says.
    pub fn doc(&self) -> &KindDoc {
        self.doc
    }

    /// Its settings, read as `T` (KD-I4): refused naming the entry when they are not what `T`
    /// reads, an unknown setting included.
    pub fn settings<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(serde_json::Value::Object(self.doc.settings.clone())).map_err(|e| {
            refusal(SpecError::new(
                self.field.clone(),
                format!("`{}` settings: {e}", self.doc.kind),
            ))
        })
    }

    /// A refusal of its setting `name` for `reason`.
    pub fn refuse(&self, name: &str, reason: impl Into<String>) -> MorunaError {
        refusal(SpecError::new(self.at(name), reason))
    }
}

/// What the program running a document supplies besides its kinds (d.1): the hooks a kind may
/// ask of it. Every method has the answer of a program that supplies nothing.
pub trait Embedding: Send + Sync {
    /// Authorize required historical logical outputs for this checked job and actual run.
    /// Hosted CLI/SDK/MCP adapters use this native seam; no default authority is invented.
    fn historical_rows(
        &self,
        job: &moruna_job::Job,
        parts: &crate::job::Parts,
        run_id: moruna_kernel::RunId,
    ) -> Result<Option<Arc<crate::history::Historical>>> {
        let _ = (job, parts, run_id);
        Ok(None)
    }

    /// Live scoped checkpoint writer. Missing authority refuses portable publication;
    /// no credentials or storage grants are inferred from a job URL.
    fn checkpoints(
        &self,
        job: &moruna_job::Job,
        run_id: moruna_kernel::RunId,
    ) -> Result<Option<Arc<crate::portable::Publisher>>> {
        let _ = (job, run_id);
        Ok(None)
    }

    /// Live scoped reader for retained task output; absent is an explicit unavailable port.
    fn historical_reader(&self) -> Result<Option<Arc<crate::history::ReadPort>>> {
        Ok(None)
    }

    /// Where the contracts of the governed engine opened at `root` keep their files, when the
    /// program knows more than the engine does: an authorized read-only root another party's
    /// contract is bound to, say. `None` is the engine's own resolution.
    #[cfg(feature = "peql")]
    fn bindings(
        &self,
        root: &std::path::Path,
    ) -> Result<Option<Arc<dyn moruna_datafusion::engine::BindingResolver>>> {
        let _ = root;
        Ok(None)
    }

    /// Whether the run may register entry `index` of the document's `contracts` (MH 4.5). A
    /// program that knows where the contracts it trusts come from refuses the others here.
    fn admit(&self, index: usize, contract: &ContractEntry) -> Result<()> {
        let _ = (index, contract);
        Ok(())
    }

    /// The handler of the guest port `name` (`{"guest": "<name>"}`, MH 4.10): what the program
    /// answers inside the machine. `None` refuses a document that names one, by name.
    fn guest_port(&self, name: &str) -> Option<Arc<dyn moruna_kernel::GuestPort>> {
        let _ = name;
        None
    }
}

/// A program that supplies nothing beyond its kinds: what `moruna` itself is.
pub struct Standalone;

impl Embedding for Standalone {}

/// What a kind builds its part with (d.1): the run's contracts and hosts, the notes the build
/// reports, and state the kinds of one build share (one engine per root for both ends of a run,
/// say).
pub struct Context {
    packages: PackageHost,
    host_ports: Option<Arc<crate::host::ports::HostPorts>>,
    contracts: Vec<ContractEntry>,
    embedding: Arc<dyn Embedding>,
    notes: Vec<String>,
    source_targets: Vec<String>,
    shared: HashMap<TypeId, Box<dyn Any + Send>>,
    pulled: Option<std::path::PathBuf>,
    args: serde_json::Map<String, serde_json::Value>,
    calls: Vec<(usize, Arc<moruna_kernel::StagePorts>)>,
}

impl Context {
    /// A context for a build in a program that supplies `embedding`, with no contracts, no host
    /// ports, the default package host and no notes.
    pub fn new(embedding: Arc<dyn Embedding>) -> Context {
        Context {
            packages: PackageHost::default(),
            host_ports: None,
            contracts: Vec::new(),
            embedding,
            notes: Vec::new(),
            source_targets: Vec::new(),
            shared: HashMap::new(),
            pulled: None,
            args: serde_json::Map::new(),
            calls: Vec::new(),
        }
    }

    /// The OCI layout the runner pulled the job's registry images into (SDD 19 d.2).
    pub fn pulled(mut self, pulled: Option<std::path::PathBuf>) -> Context {
        self.pulled = pulled;
        self
    }

    /// The run's argument values, checked against the job's declaration.
    pub fn run_args(mut self, args: serde_json::Map<String, serde_json::Value>) -> Context {
        self.args = args;
        self
    }

    /// Where the job's registry images were pulled to, when the runner pulled any.
    pub fn pulled_images(&self) -> Option<&std::path::Path> {
        self.pulled.as_deref()
    }

    /// The run's argument values, by name.
    pub fn args(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.args
    }

    /// The ports of a source or a sink (MH 4.10), for task `task` (its place in the job): made
    /// now, bound to the run when it starts, and counted in the report beside the stages'.
    pub fn task_calls(
        &mut self,
        task: usize,
        routes: std::collections::BTreeMap<String, moruna_kernel::Route>,
    ) -> Arc<moruna_kernel::StagePorts> {
        let ports = moruna_kernel::StagePorts::new(0, routes);
        self.calls.push((task, Arc::clone(&ports)));
        ports
    }

    /// The ports sources and sinks made, by task.
    pub fn take_task_calls(&mut self) -> Vec<(usize, Arc<moruna_kernel::StagePorts>)> {
        std::mem::take(&mut self.calls)
    }

    /// What a `.pex` package's sandbox needs from the host (17 e.6).
    pub fn packages(mut self, packages: PackageHost) -> Context {
        self.packages = packages;
        self
    }

    /// The host protocol's ports, when a peer is there to serve them (MH 4.10).
    pub fn host_ports(mut self, ports: Option<Arc<crate::host::ports::HostPorts>>) -> Context {
        self.host_ports = ports;
        self
    }

    /// The contracts the job carries (MH 4.5).
    pub fn contracts(mut self, contracts: Vec<ContractEntry>) -> Context {
        self.contracts = contracts;
        self
    }

    /// The package host.
    pub fn package_host(&self) -> &PackageHost {
        &self.packages
    }

    /// The host ports, when there is a peer.
    pub fn host_port_hub(&self) -> Option<&Arc<crate::host::ports::HostPorts>> {
        self.host_ports.as_ref()
    }

    /// The contracts the job carries.
    pub fn carried_contracts(&self) -> &[ContractEntry] {
        &self.contracts
    }

    /// The program's hooks.
    pub fn embedding(&self) -> &dyn Embedding {
        self.embedding.as_ref()
    }

    /// The notes the build reports (12 f.3), for a kind's clamps.
    pub fn notes(&mut self) -> &mut Vec<String> {
        &mut self.notes
    }

    /// Take the notes.
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// Where the run's source reads, once the source is built: what a sink keeps apart from.
    pub fn source_targets(&self) -> &[String] {
        &self.source_targets
    }

    /// Record where the source reads.
    pub fn set_source_targets(&mut self, targets: Vec<String>) {
        self.source_targets = targets;
    }

    /// The build's one value of type `T`, made by `make` the first time a kind asks.
    pub fn shared<T: Any + Clone + Send>(&mut self, make: impl FnOnce() -> T) -> T {
        if let Some(found) = self
            .shared
            .get(&TypeId::of::<T>())
            .and_then(|held| held.downcast_ref::<T>())
        {
            return found.clone();
        }
        let made = make();
        self.shared
            .insert(TypeId::of::<T>(), Box::new(made.clone()));
        made
    }
}

/// A source kind's part (d.2): its builder, the locations it reads for the sink equals source
/// rule (12 f.3), and the memory its engine holds outside the arena when it runs operators of its
/// own (MH 4.5).
pub struct BuiltSource {
    /// The builder.
    pub spec: SourceSpec,
    /// Where it reads.
    pub targets: Vec<String>,
    /// Its engine's memory.
    pub memory: Option<Arc<dyn EngineMemory>>,
}

/// A sink kind's part (d.2): its builder, where it writes (empty for a sink that names no
/// location of its own), and its engine's memory.
pub struct BuiltSink {
    /// The builder.
    pub spec: SinkSpec,
    /// Where it writes.
    pub target: String,
    /// Its engine's memory.
    pub memory: Option<Arc<dyn EngineMemory>>,
}

/// A runnable kind's stage (d.2): the kernel, and what the facade binds to the run for it.
pub struct Stage {
    /// The kernel the stage runs.
    pub kernel: Arc<dyn Kernel>,
    /// How it joins the next stage into one, when it can (MH 4.9).
    pub fuse: Option<Arc<dyn Fuse>>,
    /// The packaged kernel the facade binds to the run's gate, sampler and limits (16, 17).
    pub packaged: Option<Packaged>,
    /// The ports the stage calls (MH 4.10): the build makes them once the stage's id is known,
    /// hands them to the kernel, and the run binds them to its cancel and its waits.
    pub ports: Option<ports::StageCalls>,
    /// The kernel as the Python adapter's type, for `bind_allocator` and `gil_state` (05 d.1).
    #[cfg(feature = "python")]
    pub python: Option<Arc<moruna_adapters::PyKernel>>,
}

impl Stage {
    /// A stage that runs `kernel` and needs nothing else.
    pub fn of(kernel: Arc<dyn Kernel>) -> Stage {
        Stage {
            kernel,
            fuse: None,
            packaged: None,
            ports: None,
            #[cfg(feature = "python")]
            python: None,
        }
    }
}

/// A stage that may join the next into one (MH 4.9). The build asks the earlier stage with the
/// later one's; a kind joins only stages it knows, and says `None` to any other.
pub trait Fuse: Send + Sync {
    /// The two stages as one, when their combination is one.
    fn fuse(&self, next: &dyn Fuse) -> Option<Stage>;
    /// Itself, so a kind can recognise its own.
    fn as_any(&self) -> &dyn Any;
}

/// The port of a source kind (d.2).
pub trait SourceKind: Send + Sync {
    /// What it is (e.1).
    fn declaration(&self) -> Declaration;
    /// Its part for `entry` (KD-I4).
    fn source(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSource>;
}

/// The port of a runnable kind: a stage of the chain (d.2).
pub trait RunnableKind: Send + Sync {
    /// What it is (e.1).
    fn declaration(&self) -> Declaration;
    /// Its stage for `entry` (KD-I4).
    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage>;
}

/// The port of a sink kind (d.2).
pub trait SinkKind: Send + Sync {
    /// What it is (e.1).
    fn declaration(&self) -> Declaration;
    /// Its part for `entry` (KD-I4).
    fn sink(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSink>;
}

/// The pin a kernel entry gives, checked against the kernel it built (MH 4.1): `fingerprint` is
/// hex, optionally prefixed `algo:`, and a mismatch is refused naming the field and both values.
pub fn check_pin(entry: &Entry<'_>, pinned: Option<&str>, kernel: &dyn Kernel) -> Result<()> {
    let Some(pinned) = pinned else {
        return Ok(());
    };
    let want = pinned
        .rsplit(':')
        .next()
        .unwrap_or(pinned)
        .to_ascii_lowercase();
    let have = kernel.fingerprint().to_hex();
    if want != have {
        return Err(entry.refuse(
            "fingerprint",
            format!("the document pins {want} and the loaded function's is {have}"),
        ));
    }
    Ok(())
}

struct Registered<K: ?Sized> {
    declaration: Declaration,
    kind: Arc<K>,
}

impl<K: ?Sized> Clone for Registered<K> {
    fn clone(&self) -> Registered<K> {
        Registered {
            declaration: self.declaration.clone(),
            kind: Arc::clone(&self.kind),
        }
    }
}

/// The kinds a program can run (d.1, KD-I1): its sources, runnable kinds and sinks by name, and
/// its hooks. A clone shares the adapters: what a command line adds to it for one invocation
/// (`--kinds`) leaves the program's own registry as it was.
#[derive(Clone)]
pub struct Registry {
    sources: BTreeMap<String, Registered<dyn SourceKind>>,
    kernels: BTreeMap<String, Registered<dyn RunnableKind>>,
    sinks: BTreeMap<String, Registered<dyn SinkKind>>,
    embedding: Arc<dyn Embedding>,
}

impl Registry {
    /// A registry with no kinds, in a program that supplies nothing else.
    pub fn empty() -> Registry {
        Registry {
            sources: BTreeMap::new(),
            kernels: BTreeMap::new(),
            sinks: BTreeMap::new(),
            embedding: Arc::new(Standalone),
        }
    }

    /// Every kind this build of the runtime has (f.1): the Arrow IPC, Parquet, Vortex and tensor sources;
    /// the Parquet, Vortex, tensor and Arrow IPC sinks; the standard kernels and function
    /// packages; and, with the `peql` feature, the `datafusion` source and the `peql` sink. No
    /// `rust` kind is among them (KD-I7). A program adds its own with the `register_` calls.
    pub fn standard() -> Registry {
        let mut kinds = Registry::empty();
        kinds.put_source(Arc::new(arrow_ipc::ArrowIpc));
        kinds.put_source(Arc::new(parquet::Parquet));
        kinds.put_source(Arc::new(vortex::Vortex));
        kinds.put_source(Arc::new(tensor::Tensor));
        kinds.put_source(Arc::new(image::Image));
        kinds.put_kernel(Arc::new(standard::Std));
        kinds.put_kernel(Arc::new(package::Package));
        kinds.put_kernel(Arc::new(image::Image));
        kinds.put_sink(Arc::new(parquet::Parquet));
        kinds.put_sink(Arc::new(vortex::Vortex));
        kinds.put_sink(Arc::new(tensor::Tensor));
        kinds.put_sink(Arc::new(arrow_ipc::ArrowIpc));
        kinds.put_sink(Arc::new(image::Image));
        #[cfg(feature = "peql")]
        {
            kinds.put_source(Arc::new(governed::Datafusion));
            kinds.put_sink(Arc::new(governed::Peql));
        }
        kinds
    }

    /// The same kinds in a program that supplies `embedding`.
    pub fn with_embedding(mut self, embedding: Arc<dyn Embedding>) -> Registry {
        self.embedding = embedding;
        self
    }

    /// The program's hooks.
    pub fn embedding(&self) -> Arc<dyn Embedding> {
        Arc::clone(&self.embedding)
    }

    fn put_source(&mut self, kind: Arc<dyn SourceKind>) {
        let declaration = kind.declaration();
        self.sources
            .insert(declaration.name.clone(), Registered { declaration, kind });
    }

    fn put_kernel(&mut self, kind: Arc<dyn RunnableKind>) {
        let declaration = kind.declaration();
        self.kernels
            .insert(declaration.name.clone(), Registered { declaration, kind });
    }

    fn put_sink(&mut self, kind: Arc<dyn SinkKind>) {
        let declaration = kind.declaration();
        self.sinks
            .insert(declaration.name.clone(), Registered { declaration, kind });
    }

    /// Register a source kind (KD-I2): refused when its declaration is not one the registry
    /// accepts, or when a source kind of that name is registered already.
    pub fn register_source(&mut self, kind: Arc<dyn SourceKind>) -> Result<()> {
        let declaration = kind.declaration();
        admissible(
            &declaration,
            Role::Source,
            self.sources.contains_key(&declaration.name),
        )?;
        self.put_source(kind);
        Ok(())
    }

    /// Register a runnable kind (KD-I2), as [`Registry::register_source`]. A kind its embedder
    /// serves is declared ([`Registry::declare`]), not registered with an adapter of its own.
    pub fn register_kernel(&mut self, kind: Arc<dyn RunnableKind>) -> Result<()> {
        let declaration = kind.declaration();
        admissible(
            &declaration,
            Role::Kernel,
            self.kernels.contains_key(&declaration.name),
        )?;
        if declaration.served.is_some() {
            return Err(MorunaError::Config {
                name: "kinds",
                msg: format!(
                    "`{}` is served by a port, and a served kind is declared (Registry::declare), \
                     not registered with an adapter: Moruna runs every served kind the same way",
                    declaration.name
                ),
            });
        }
        self.put_kernel(kind);
        Ok(())
    }

    /// Declare a kind the program running Moruna serves through a port (KD-I11): a declaration
    /// with `isolation: embedder` and `served`, which one adapter ([`served::Served`]) runs by
    /// handing each batch to the port. Refused, `Config { name: "kinds" }`, as any registration
    /// is (KD-I2), when a kernel kind of its name is registered already (a built-in one
    /// included), when it is not a served kind, and when its settings schema uses what the
    /// validator its settings are checked with does not read (SDD 22 JB-I7).
    pub fn declare(&mut self, declaration: Declaration) -> Result<()> {
        admissible(
            &declaration,
            Role::Kernel,
            self.kernels.contains_key(&declaration.name),
        )?;
        moruna_job::schema::admit(&declaration.settings).map_err(|fault| MorunaError::Config {
            name: "kinds",
            msg: format!(
                "`{}`'s settings schema cannot be checked against: {fault}",
                declaration.name
            ),
        })?;
        self.put_kernel(Arc::new(served::Served::new(declaration)?));
        Ok(())
    }

    /// Register a sink kind (KD-I2), as [`Registry::register_source`].
    pub fn register_sink(&mut self, kind: Arc<dyn SinkKind>) -> Result<()> {
        let declaration = kind.declaration();
        admissible(
            &declaration,
            Role::Sink,
            self.sinks.contains_key(&declaration.name),
        )?;
        self.put_sink(kind);
        Ok(())
    }

    /// Replace the source kind of its name with `kind`, which declares itself exactly as the
    /// kind it replaces: a program that runs some tasks of a kind from objects it holds and the
    /// rest as the kind does. Refused when no kind of that name is registered, or the
    /// declarations differ.
    pub fn replace_source(&mut self, kind: Arc<dyn SourceKind>) -> Result<()> {
        let declaration = kind.declaration();
        same(
            self.sources.get(&declaration.name).map(|r| &r.declaration),
            &declaration,
            Role::Source,
        )?;
        self.put_source(kind);
        Ok(())
    }

    /// [`Registry::replace_source`] for a runnable kind.
    pub fn replace_kernel(&mut self, kind: Arc<dyn RunnableKind>) -> Result<()> {
        let declaration = kind.declaration();
        same(
            self.kernels.get(&declaration.name).map(|r| &r.declaration),
            &declaration,
            Role::Kernel,
        )?;
        self.put_kernel(kind);
        Ok(())
    }

    /// [`Registry::replace_source`] for a sink kind.
    pub fn replace_sink(&mut self, kind: Arc<dyn SinkKind>) -> Result<()> {
        let declaration = kind.declaration();
        same(
            self.sinks.get(&declaration.name).map(|r| &r.declaration),
            &declaration,
            Role::Sink,
        )?;
        self.put_sink(kind);
        Ok(())
    }

    /// The source kind a document's `{field}.kind` names, or the refusal of KD-I1.
    pub fn source(&self, field: &str, name: &str) -> Result<(&Declaration, &dyn SourceKind)> {
        match self.sources.get(name) {
            Some(found) => Ok((&found.declaration, found.kind.as_ref())),
            None => Err(unknown(field, Role::Source, name, self.sources.keys())),
        }
    }

    /// The runnable kind a document's `{field}.kind` names, or the refusal of KD-I1.
    pub fn kernel(&self, field: &str, name: &str) -> Result<(&Declaration, &dyn RunnableKind)> {
        match self.kernels.get(name) {
            Some(found) => Ok((&found.declaration, found.kind.as_ref())),
            None => Err(unknown(field, Role::Kernel, name, self.kernels.keys())),
        }
    }

    /// The sink kind a document's `{field}.kind` names, or the refusal of KD-I1.
    pub fn sink(&self, field: &str, name: &str) -> Result<(&Declaration, &dyn SinkKind)> {
        match self.sinks.get(name) {
            Some(found) => Ok((&found.declaration, found.kind.as_ref())),
            None => Err(unknown(field, Role::Sink, name, self.sinks.keys())),
        }
    }

    /// What `moruna kinds` prints (e.3): every declaration, by role, in name order.
    pub fn listing(&self) -> Listing {
        Listing {
            moruna_kinds: LISTING_VERSION,
            moruna: env!("CARGO_PKG_VERSION").to_string(),
            sources: self
                .sources
                .values()
                .map(|r| r.declaration.clone())
                .collect(),
            kernels: self
                .kernels
                .values()
                .map(|r| r.declaration.clone())
                .collect(),
            sinks: self.sinks.values().map(|r| r.declaration.clone()).collect(),
        }
    }
}

fn same(registered: Option<&Declaration>, declaration: &Declaration, role: Role) -> Result<()> {
    match registered {
        Some(found) if found == declaration => Ok(()),
        Some(_) => Err(MorunaError::Config {
            name: "kinds",
            msg: format!(
                "the {} kind `{}` is replaced by one that declares itself differently",
                role.as_str(),
                declaration.name
            ),
        }),
        None => Err(MorunaError::Config {
            name: "kinds",
            msg: format!(
                "no {} kind `{}` is registered to replace",
                role.as_str(),
                declaration.name
            ),
        }),
    }
}

fn admissible(declaration: &Declaration, role: Role, taken: bool) -> Result<()> {
    let refuse = |msg: String| MorunaError::Config { name: "kinds", msg };
    declaration.check(role).map_err(refuse)?;
    if taken {
        return Err(refuse(format!(
            "a {} kind `{}` is registered already",
            role.as_str(),
            declaration.name
        )));
    }
    Ok(())
}

/// The refusal of a kind no adapter is registered for (KD-I1): the field, the name, and the kinds
/// of that role this build has.
fn unknown<'a>(
    field: &str,
    role: Role,
    name: &str,
    known: impl Iterator<Item = &'a String>,
) -> MorunaError {
    let known: Vec<&str> = known.map(String::as_str).collect();
    let has = if known.is_empty() {
        format!("this build has no {} kinds", role.as_str())
    } else {
        format!("this build {} {}", role.verb(), known.join(", "))
    };
    refusal(SpecError::new(
        format!("{field}.kind"),
        format!("no {} kind `{name}` is registered; {has}", role.as_str()),
    ))
}

#[cfg(test)]
mod tests;
