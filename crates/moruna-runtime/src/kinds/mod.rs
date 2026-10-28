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

use moruna_kernel::{Kernel, MorunaError, Result, StageId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::job::{ContractEntry, KindDoc, SpecError};
use crate::packaged::{PackageHost, Packaged};
use crate::spec::{EngineMemory, SinkSpec, SourceSpec};

pub mod arrow_ipc;
pub mod conformance;
#[cfg(feature = "peql")]
pub mod governed;
pub mod image;
pub mod package;
pub mod parquet;
pub mod ports;
pub mod standard;
pub mod tensor;
pub mod vortex;

/// The format version of `moruna kinds` (the [`Listing`]).
pub const LISTING_VERSION: u32 = 1;

/// The JSON Schema dialect every settings schema is written in (decision KD1).
pub const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Which part of a run a kind is, and so which port its adapter implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The input ([`SourceKind`]).
    Source,
    /// A stage between them ([`RunnableKind`]).
    Kernel,
    /// The output ([`SinkKind`]).
    Sink,
}

impl Role {
    /// The role's name in a refusal and in the listing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Source => "source",
            Role::Kernel => "kernel",
            Role::Sink => "sink",
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Role::Source => "reads",
            Role::Kernel => "runs",
            Role::Sink => "writes",
        }
    }
}

/// What a kind takes in or gives out (e.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    /// Arrow record batches (or tensors), as morsels.
    Batches,
    /// Files: what a source reads or a sink writes.
    Files,
    /// Nothing the run carries: an object the caller holds, say.
    Nothing,
}

/// A fact about a kind that holds always, never, or as its settings say (e.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fact {
    /// Always.
    Yes,
    /// Never.
    No,
    /// As the entry's settings say (an object URL needs the network, a projection drops
    /// columns).
    BySettings,
}

/// Where a kind's code comes from and where it runs (e.1, MB3's isolation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Code compiled into the program running the document, in its process.
    Builtin,
    /// Code imported by path from the interpreter's path into the process: what a hosted run
    /// (strict mode) refuses (16 PK-I8, KD-I6).
    Imported,
    /// An object the library caller handed over, run in the caller's process; never named by a
    /// document from a file or a socket.
    Caller,
    /// Code from a package, run apart from the process: a `.wasm` in its sandbox, a `.pex` in a
    /// confined child (17, decision FP1).
    Confined,
}

/// What a kind may need from the machine and the run (e.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Needs {
    /// Connections out of the process (an object store), always reached through the reactor's
    /// clients and never by the kind's own sockets.
    pub network: Fact,
    /// Destinations the kind needs whatever its settings, as `host` or `host:port`; empty for
    /// every kind whose destinations are its settings'.
    pub egress: Vec<String>,
    /// A GPU.
    pub gpu: Fact,
    /// A named port it calls (`call(port, bytes)`, MH 4.10), a model's say.
    pub model_port: Fact,
    /// The contracts the job carries (`contracts`, MH 4.5).
    pub contracts: Fact,
}

impl Needs {
    /// Nothing at all.
    pub fn nothing() -> Needs {
        Needs {
            network: Fact::No,
            egress: Vec::new(),
            gpu: Fact::No,
            model_port: Fact::No,
            contracts: Fact::No,
        }
    }
}

/// What a kind is (e.1): what an embedder reads to know what can run and how to configure it, and
/// what the build and the planner read instead of matching on its name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    /// The name a document's `kind` names it by (KD-I2).
    pub name: String,
    /// The port it implements.
    pub role: Role,
    /// One line on what it is.
    pub summary: String,
    /// Its settings: a JSON Schema (2020-12) of the entry's keys other than `kind`, with
    /// `examples` (e.2, decision KD1).
    pub settings: serde_json::Value,
    /// What it takes in.
    pub inputs: Flow,
    /// What it gives out.
    pub outputs: Flow,
    /// What it may need.
    pub needs: Needs,
    /// Whether the same input and settings give the same output rows.
    pub deterministic: Fact,
    /// Whether the columns it is given pass through unchanged (for a source, whether it yields
    /// the stored columns as stored; for a sink, whether it writes every column it is given).
    pub preserves_columns: Fact,
    /// Where its code comes from and runs.
    pub isolation: Isolation,
}

impl Declaration {
    /// Whether this declares a kind of `role` the registry accepts (KD-I2): a kind name; the
    /// role; a summary; flows a part of that role can have; a closed object schema in the
    /// dialect of KD1 with at least one example, none of whose properties is `kind`.
    pub fn check(&self, role: Role) -> core::result::Result<(), String> {
        if !is_kind_name(&self.name) {
            return Err(format!(
                "{:?} is not a kind name: 1 to 64 of lowercase ASCII letters, digits, `_`, `-` \
                 and `.`, starting with a letter",
                self.name
            ));
        }
        if self.role != role {
            return Err(format!(
                "`{}` declares the role {} and is registered as a {}",
                self.name,
                self.role.as_str(),
                role.as_str()
            ));
        }
        if self.summary.trim().is_empty() {
            return Err(format!("`{}` has no summary", self.name));
        }
        let flows_fit = match role {
            Role::Source => self.inputs != Flow::Batches && self.outputs == Flow::Batches,
            Role::Kernel => self.inputs == Flow::Batches && self.outputs == Flow::Batches,
            Role::Sink => self.inputs == Flow::Batches && self.outputs != Flow::Batches,
        };
        if !flows_fit {
            return Err(format!(
                "`{}` takes {:?} and gives {:?}, which a {} does not: a source gives batches, a \
                 kernel takes and gives batches, a sink takes batches",
                self.name,
                self.inputs,
                self.outputs,
                role.as_str()
            ));
        }
        let schema = &self.settings;
        let fail = |what: &str| Err(format!("`{}`'s settings schema {what}", self.name));
        if schema.get("$schema").and_then(|v| v.as_str()) != Some(SCHEMA_DIALECT) {
            return fail(&format!("is not in the dialect {SCHEMA_DIALECT}"));
        }
        if schema.get("type").and_then(|v| v.as_str()) != Some("object") {
            return fail("does not describe an object");
        }
        if schema.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
            return fail("is not closed (`additionalProperties: false`)");
        }
        if schema
            .get("properties")
            .and_then(|p| p.as_object())
            .is_some_and(|p| p.contains_key("kind"))
        {
            return fail("has a property `kind`, which is the document's, not a setting");
        }
        match schema.get("examples").and_then(|v| v.as_array()) {
            Some(examples) if !examples.is_empty() && examples.iter().all(|e| e.is_object()) => {
                Ok(())
            }
            _ => fail("has no `examples`, or one that is not an object"),
        }
    }

    /// The examples of its settings schema.
    pub fn examples(&self) -> Vec<serde_json::Value> {
        self.settings
            .get("examples")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }
}

/// The settings schema of a kind that reads its settings as `T` (e.2): JSON Schema 2020-12
/// derived from `T`, titled with the kind's name, with `examples`.
pub fn settings_schema<T: schemars::JsonSchema>(
    name: &str,
    examples: Vec<serde_json::Value>,
) -> serde_json::Value {
    let mut schema = schemars::SchemaGenerator::default().into_root_schema_for::<T>();
    if let Some(object) = schema.as_object_mut() {
        object.insert("title".into(), serde_json::Value::String(name.to_string()));
        object.insert("examples".into(), serde_json::Value::Array(examples));
    }
    schema.to_value()
}

/// Whether `name` may name a kind (KD-I2): 1 to 64 of lowercase ASCII letters, digits, `_`, `-`
/// and `.`, starting with a letter.
pub fn is_kind_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

/// Files or prefixes: one string or a list of them (MH 4.1). Written as a string when it is one
/// and as a list when it is several, and read either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Urls(pub Vec<String>);

impl Serialize for Urls {
    fn serialize<S: Serializer>(&self, s: S) -> core::result::Result<S::Ok, S::Error> {
        match self.0.as_slice() {
            [one] => s.serialize_str(one),
            many => many.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Urls {
    fn deserialize<D: Deserializer<'de>>(d: D) -> core::result::Result<Urls, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }
        Ok(match OneOrMany::deserialize(d)? {
            OneOrMany::One(one) => Urls(vec![one]),
            OneOrMany::Many(many) => Urls(many),
        })
    }
}

impl schemars::JsonSchema for Urls {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Urls".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut any_of = serde_json::Map::new();
        any_of.insert(
            "anyOf".into(),
            serde_json::json!([
                {"type": "string"},
                {"type": "array", "items": {"type": "string"}}
            ]),
        );
        any_of.insert(
            "description".into(),
            serde_json::Value::String("One location, or a list of them.".into()),
        );
        schemars::Schema::from(any_of)
    }
}

/// One entry of the document as its adapter reads it: where it is (`source`, `kernels[2]`,
/// `sink`), so a refusal names its field (MH 4.1), and what it says.
pub struct Entry<'d> {
    field: String,
    index: Option<usize>,
    doc: &'d KindDoc,
}

impl<'d> Entry<'d> {
    /// The entry `doc` at `field`; `index` is its place in `kernels`, for a kernel.
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

    /// Its place in `kernels`, for a kernel.
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
            SpecError::new(
                self.field.clone(),
                format!("`{}` settings: {e}", self.doc.kind),
            )
            .into()
        })
    }

    /// A refusal of its setting `name` for `reason`.
    pub fn refuse(&self, name: &str, reason: impl Into<String>) -> MorunaError {
        SpecError::new(self.at(name), reason).into()
    }
}

/// What the program running a document supplies besides its kinds (d.1): the hooks a kind may
/// ask of it. Every method has the answer of a program that supplies nothing.
pub trait Embedding: Send + Sync {
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
    sink_target: String,
    shared: HashMap<TypeId, Box<dyn Any + Send>>,
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
            sink_target: String::new(),
            shared: HashMap::new(),
        }
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

    /// Where the run's sink writes, once the sink is built: the store a kernel that keeps
    /// something of its own (an image's unpacked root) keeps it beside. Empty before then, and
    /// for a sink with no location.
    pub fn sink_target(&self) -> &str {
        &self.sink_target
    }

    /// Record where the sink writes.
    pub fn set_sink_target(&mut self, target: String) {
        self.sink_target = target;
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
    /// Called once with the stage's id when the chain is settled: a stage that calls ports
    /// learns its id this way (MH 4.10).
    pub placed: Option<Box<dyn FnOnce(StageId) + Send>>,
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
            placed: None,
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
            format!("the document pins {want} and the loaded kernel's is {have}"),
        ));
    }
    Ok(())
}

struct Registered<K: ?Sized> {
    declaration: Declaration,
    kind: Arc<K>,
}

/// The kinds a program can run (d.1, KD-I1): its sources, runnable kinds and sinks by name, and
/// its hooks.
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

    /// Every kind this build of the runtime has (f.1): the Parquet, Vortex and tensor sources;
    /// the Parquet, Vortex, tensor and Arrow IPC sinks; the standard kernels and function
    /// packages; and, with the `peql` feature, the `datafusion` source and the `peql` sink. No
    /// `rust` kind is among them (KD-I7). A program adds its own with the `register_` calls.
    pub fn standard() -> Registry {
        let mut kinds = Registry::empty();
        kinds.put_source(Arc::new(parquet::Parquet));
        kinds.put_source(Arc::new(vortex::Vortex));
        kinds.put_source(Arc::new(tensor::Tensor));
        kinds.put_kernel(Arc::new(standard::Std));
        kinds.put_kernel(Arc::new(package::Package));
        kinds.put_kernel(Arc::new(image::Image));
        kinds.put_sink(Arc::new(parquet::Parquet));
        kinds.put_sink(Arc::new(vortex::Vortex));
        kinds.put_sink(Arc::new(tensor::Tensor));
        kinds.put_sink(Arc::new(arrow_ipc::ArrowIpc));
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

    /// Register a runnable kind (KD-I2), as [`Registry::register_source`].
    pub fn register_kernel(&mut self, kind: Arc<dyn RunnableKind>) -> Result<()> {
        let declaration = kind.declaration();
        admissible(
            &declaration,
            Role::Kernel,
            self.kernels.contains_key(&declaration.name),
        )?;
        self.put_kernel(kind);
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
    SpecError::new(
        format!("{field}.kind"),
        format!("no {} kind `{name}` is registered; {has}", role.as_str()),
    )
    .into()
}

/// Every declaration of a registry (e.3): what `moruna kinds` prints, and what an embedder reads
/// to know what can run and how to configure it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    /// The listing's format version ([`LISTING_VERSION`]).
    pub moruna_kinds: u32,
    /// The Moruna that listed them.
    pub moruna: String,
    /// The source kinds.
    pub sources: Vec<Declaration>,
    /// The runnable kinds.
    pub kernels: Vec<Declaration>,
    /// The sink kinds.
    pub sinks: Vec<Declaration>,
}

impl Listing {
    /// The listing as indented JSON, as [`crate::JobSpec::to_json_pretty`] writes a document.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

#[cfg(test)]
mod tests;
