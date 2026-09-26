//! The job document: `RunSpec` as a file (MH 4.1).
//!
//! A run is described by one JSON document with `"moruna_spec": 1`. [`JobSpec`] is that
//! document, field for field; [`build::build`] turns it into the facade's [`crate::RunSpec`],
//! and it is the only place a `RunSpec` is built from a description, whether the description
//! came from a file (`moruna run`), from a socket (`moruna serve`) or from the arguments of
//! `moruna.run(...)` (12 f.3). One code path, so a run from a file and the same run from Python
//! are the same run (H1).
//!
//! Every optional field is `null` or absent when the document leaves it to the runtime; the
//! precedence rule of MH 4.1 says what fills it. The canonical form (MH 4.1, [`canonical`])
//! omits absent fields, so a document that spells a `null` and one that leaves the key out
//! have one digest.

pub mod build;
pub mod canonical;
pub mod translate;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use build::{BuildOptions, Built, KernelLoader, LoadedKernel, NoKernels, build};

/// The one version of the document this build reads (MH 4.1).
pub const SPEC_VERSION: u32 = 1;

/// Why a document was refused, with the field it was refused for (MH 4.1). `moruna run`
/// exits 2 with this text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecError {
    /// The field, as a dotted path from the document's root (`budget.memory_bytes`,
    /// `kernels[1].module`), or `spec` for the document as a whole.
    pub field: String,
    /// What is wrong with it.
    pub reason: String,
}

impl SpecError {
    /// A refusal of `field` for `reason`.
    pub fn new(field: impl Into<String>, reason: impl Into<String>) -> SpecError {
        SpecError {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

impl core::fmt::Display for SpecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "spec refused: {}: {}", self.field, self.reason)
    }
}

impl From<SpecError> for moruna_kernel::MorunaError {
    fn from(e: SpecError) -> moruna_kernel::MorunaError {
        moruna_kernel::MorunaError::Config {
            name: "spec",
            msg: e.to_string(),
        }
    }
}

/// One run, as a document (MH 4.1). Every field of [`crate::RunSpec`] has a field
/// here; `notes` and the Python kernel list are derived, not described.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    /// The document version; 1.
    pub moruna_spec: u32,
    /// The run's identity, 32 lowercase hex characters; absent: minted at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The input.
    pub source: SourceDoc,
    /// Stages 1..=n, in order; empty is a copy from source to sink.
    #[serde(default)]
    pub kernels: Vec<KernelDoc>,
    /// The output.
    pub sink: SinkDoc,
    /// Memory and CPU.
    #[serde(default)]
    pub budget: BudgetDoc,
    /// The disk tier.
    #[serde(default)]
    pub staging: StagingDoc,
    /// Credentials and endpoints for object URLs.
    #[serde(default)]
    pub object_store: ObjectStoreDoc,
    /// The manifest cadence.
    #[serde(default)]
    pub checkpoint: CheckpointDoc,
    /// `"auto"`, a run id or a manifest path; absent: a fresh run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// What happens after a kernel error.
    #[serde(default)]
    pub error_policy: ErrorPolicyDoc,
    /// Deliver morsels to the sink in sequence order.
    #[serde(default)]
    pub ordered: bool,
    /// Which decision function sizes morsels.
    #[serde(default)]
    pub sizer: SizerDoc,
    /// Where the trace file goes; absent keeps the trace in memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    /// The profile store; absent is the preamble's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles_dir: Option<String>,
    /// The platform's declared guarantees; absent: discovery probes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_profile: Option<HostProfileDoc>,
    /// Run Python kernels serialised under a GIL interpreter rather than refusing.
    #[serde(default)]
    pub allow_gil: bool,
    /// Where the report goes.
    #[serde(default)]
    pub report: ReportDoc,
}

/// Files or prefixes: one string or a list of them. The canonical form writes one as a
/// string and several as a list (MH 4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Urls(pub Vec<String>);

impl Serialize for Urls {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0.as_slice() {
            [one] => s.serialize_str(one),
            many => many.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Urls {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Urls, D::Error> {
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

/// Where the input comes from (MH 4.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceDoc {
    /// Parquet files or prefixes, local or in an object store.
    Parquet {
        /// `file://`, `s3://`, `gs://`, `az://` or a bare local path.
        url: Urls,
        /// Projection and row-group pruning.
        #[serde(default)]
        options: ParquetSourceOptions,
    },
    /// Safetensors or aligned binary tensor files.
    Tensor {
        /// Local paths or `file://` URLs.
        url: Urls,
        /// Which tensors.
        #[serde(default)]
        options: TensorSourceOptions,
    },
    /// A Python iterable. Library only: the iterable is an object in the caller's process and
    /// cannot be named in a file, so a document from a file or a socket is refused (MH 4.1).
    Iterator,
}

/// `source.options` for `parquet`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParquetSourceOptions {
    /// The projection; absent is every column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// Row-group predicates, each `[column, op, value]` with `op` one of `>`, `<`, `==`.
    pub filters: Vec<FilterDoc>,
}

/// One row-group predicate: `[column, op, value]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilterDoc(pub String, pub String, pub serde_json::Value);

/// `source.options` for `tensor`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TensorSourceOptions {
    /// Tensor names; absent is every tensor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tensors: Option<Vec<String>>,
}

/// Where the output goes (MH 4.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SinkDoc {
    /// Parquet files under a prefix.
    Parquet {
        /// `file://`, `s3://`, `gs://`, `az://` or a bare local directory.
        url: String,
        /// Sizes and compression.
        #[serde(default)]
        options: ParquetSinkOptions,
    },
    /// Tensor files.
    Tensor {
        /// A local directory.
        url: String,
        /// Format and naming.
        #[serde(default)]
        options: TensorSinkOptions,
    },
    /// Page-aligned Arrow IPC files.
    ArrowIpc {
        /// A local directory.
        url: String,
        /// Sizes.
        #[serde(default)]
        options: ArrowIpcSinkOptions,
    },
}

/// `sink.options` for `parquet`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParquetSinkOptions {
    /// `sink.row_group_bytes`; absent is 128 MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_group_bytes: Option<u64>,
    /// `sink.file_bytes`; absent is 1 GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bytes: Option<u64>,
    /// `zstd` (absent), `snappy`, `gzip`, `lz4` or `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression: Option<String>,
}

/// `sink.options` for `tensor`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TensorSinkOptions {
    /// `mrb1` (absent) or `safetensors`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// One file per morsel rather than one per size bound.
    pub one_file_per_morsel: bool,
    /// The tensor name; absent is `tensor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `sink.options` for `arrow_ipc`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArrowIpcSinkOptions {
    /// `sink.file_bytes`; absent is 1 GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bytes: Option<u64>,
}

/// Which language a kernel is written in (MH 4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelKindDoc {
    /// A Python callable, by module and name.
    Python,
    /// A Rust kernel by crate and symbol. Reserved in version 1 and refused.
    Rust,
}

/// One stage (MH 4.1). A Python kernel is named by `module` and `callable`; the hints are the
/// decorator's arguments (12 d.2) and apply to a callable that is not already decorated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelDoc {
    /// `python` or `rust`.
    pub kind: KernelKindDoc,
    /// A module name on the interpreter's path, or a path to a `.py` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// The attribute of the module that is the kernel; dotted for a nested attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callable: Option<String>,
    /// Reserved for `rust`.
    #[serde(default, rename = "crate", skip_serializing_if = "Option::is_none")]
    pub krate: Option<String>,
    /// Reserved for `rust`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
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
}

impl KernelDoc {
    /// A Python kernel named by module and callable, with no hints.
    pub fn python(module: impl Into<String>, callable: impl Into<String>) -> KernelDoc {
        KernelDoc {
            kind: KernelKindDoc::Python,
            module: Some(module.into()),
            callable: Some(callable.into()),
            krate: None,
            symbol: None,
            fingerprint: None,
            stateful: None,
            instances: None,
            device_memory: None,
            accepts: None,
            tier: None,
            releases_gil: None,
            expected_amplification: None,
            preferred_rows: None,
            resume: None,
            state_bytes: None,
        }
    }

    /// The names of the decorator hints this entry sets, in document order.
    pub fn hints_set(&self) -> Vec<&'static str> {
        let mut set = Vec::new();
        let flags: [(&'static str, bool); 10] = [
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
        ];
        for (name, on) in flags {
            if on {
                set.push(name);
            }
        }
        set
    }
}

/// `budget` (MH 4.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetDoc {
    /// The host memory ceiling in bytes; absent: discovered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// CPUs, in cores; absent: discovered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f64>,
    /// How far the budget may follow the machine (MH 4.4). Read and validated here; the
    /// elasticity itself is F8.2's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elastic: Option<ElasticDoc>,
}

/// `budget.elastic` (MH 4.4).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ElasticDoc {
    /// The largest memory ceiling the run may grow to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    /// The largest CPU count the run may grow to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_max: Option<u32>,
}

/// `staging` (MH 4.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StagingDoc {
    /// Where staging segments and the manifest go; absent: discovered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// `budget.disk` in bytes; absent: 20% of the free space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
    /// The directory survives the machine (`durable_staging=present`, MH 4.7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable: Option<bool>,
}

/// `object_store` (MH 4.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObjectStoreDoc {
    /// S3 and S3-compatible endpoints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Doc>,
    /// Google Cloud Storage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gcs: Option<GcsDoc>,
    /// Azure Blob Storage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub azure: Option<AzureDoc>,
    /// Root for `file://` URLs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_root: Option<String>,
    /// Permit plain-HTTP endpoints.
    pub allow_http: bool,
}

/// `object_store.s3`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct S3Doc {
    /// Custom endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Region.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Access key id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    /// Secret access key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,
    /// Session token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// Default bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
}

/// `object_store.gcs`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GcsDoc {
    /// Service account file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_account_path: Option<String>,
    /// Service account key as JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_account_json: Option<String>,
    /// Default bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
}

/// `object_store.azure`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AzureDoc {
    /// Storage account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Access key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_key: Option<String>,
    /// Default container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

/// `checkpoint` (MH 4.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointDoc {
    /// Write the manifest periodically.
    pub enabled: bool,
    /// The cadence, clamped to 500 to 60,000 ms with a note.
    pub interval_ms: u64,
    /// Keep the run directory and the final manifest after a completed run.
    pub keep: bool,
}

impl Default for CheckpointDoc {
    fn default() -> CheckpointDoc {
        CheckpointDoc {
            enabled: true,
            interval_ms: crate::config::CHECKPOINT_INTERVAL_MS,
            keep: false,
        }
    }
}

/// `error_policy`: `"terminate"`, `"skip"` or `{"budget": n}` (contracts d.11).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPolicyDoc {
    /// End the run with a diagnostic.
    #[default]
    Terminate,
    /// Skip the morsel and carry on.
    Skip,
    /// Skip up to this many morsels, then terminate.
    Budget(u32),
}

/// `sizer`: `"rule"` or `"learned"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizerDoc {
    /// The rule sizer.
    #[default]
    Rule,
    /// The learned sizer, with its fallback.
    Learned,
}

/// A platform's declaration about one guarantee (contracts d.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuaranteeDoc {
    /// Declared present; discovery verifies it.
    Present,
    /// Declared absent; nothing is probed.
    Absent,
}

/// `host_profile`: the keys of `MORUNA_HOST_PROFILE` (03 e.1), as an object.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostProfileDoc {
    /// Huge pages available to back the arena.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub huge_pages: Option<GuaranteeDoc>,
    /// `mlock` permitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memlock: Option<GuaranteeDoc>,
    /// io_uring permitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_uring: Option<GuaranteeDoc>,
    /// The staging directory accepts `O_DIRECT`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_io: Option<GuaranteeDoc>,
    /// GPUDirect Storage present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gds: Option<GuaranteeDoc>,
    /// RDMA present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rdma: Option<GuaranteeDoc>,
    /// The staging directory outlives the machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable_staging: Option<GuaranteeDoc>,
    /// Where staging goes, as the platform declares it; absolute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staging_dir: Option<String>,
}

impl HostProfileDoc {
    /// True when every guarantee is declared, so nothing is left for the environment variable
    /// to fill underneath (03 d.1, `profile_override`).
    pub fn is_complete(&self) -> bool {
        [
            self.huge_pages,
            self.memlock,
            self.io_uring,
            self.direct_io,
            self.gds,
            self.rdma,
            self.durable_staging,
        ]
        .iter()
        .all(Option::is_some)
    }
}

/// `report` (MH 4.1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReportDoc {
    /// A peer to connect to and speak the protocol of MH 4.3 with; `unix://` or `vsock://`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// Where the report file goes; `<run_id>` is replaced by the run's id. Absent:
    /// `moruna-<run_id>.report.json` in the staging directory, else the working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// The top-level keys of version 1, in the order MH 4.1 lists them.
const TOP_LEVEL: [&str; 18] = [
    "moruna_spec",
    "run_id",
    "source",
    "kernels",
    "sink",
    "budget",
    "staging",
    "object_store",
    "checkpoint",
    "resume",
    "error_policy",
    "ordered",
    "sizer",
    "trace",
    "profiles_dir",
    "host_profile",
    "allow_gil",
    "report",
];

impl JobSpec {
    /// A document over `source`, `kernels` and `sink` with every other field absent or at its
    /// default: what `moruna.run(source, kernels, sink)` describes (PY-I3).
    pub fn new(source: SourceDoc, kernels: Vec<KernelDoc>, sink: SinkDoc) -> JobSpec {
        JobSpec {
            moruna_spec: SPEC_VERSION,
            run_id: None,
            source,
            kernels,
            sink,
            budget: BudgetDoc::default(),
            staging: StagingDoc::default(),
            object_store: ObjectStoreDoc::default(),
            checkpoint: CheckpointDoc::default(),
            resume: None,
            error_policy: ErrorPolicyDoc::default(),
            ordered: false,
            sizer: SizerDoc::default(),
            trace: None,
            profiles_dir: None,
            host_profile: None,
            allow_gil: false,
            report: ReportDoc::default(),
        }
    }

    /// Parse a document (MH 4.1). The version is checked before anything else, so a document
    /// from a later Moruna is refused for its version and not for a field this build has never
    /// heard of; then every top-level key is checked by name, and each section is read on its
    /// own so a refusal names the section it came from.
    pub fn from_json(text: &str) -> Result<JobSpec, SpecError> {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| SpecError::new("spec", format!("not a JSON document: {e}")))?;
        JobSpec::from_value(value)
    }

    /// [`JobSpec::from_json`] for a document already parsed.
    pub fn from_value(value: serde_json::Value) -> Result<JobSpec, SpecError> {
        let serde_json::Value::Object(map) = &value else {
            return Err(SpecError::new("spec", "a document is a JSON object"));
        };
        match map.get("moruna_spec") {
            None => {
                return Err(SpecError::new(
                    "moruna_spec",
                    "missing; a job document says which version it is (1)",
                ));
            }
            Some(serde_json::Value::Number(n)) if n.as_u64() == Some(u64::from(SPEC_VERSION)) => {}
            Some(other) => {
                return Err(SpecError::new(
                    "moruna_spec",
                    format!("this build reads version {SPEC_VERSION}, the document says {other}"),
                ));
            }
        }
        for (key, section) in map {
            if !TOP_LEVEL.contains(&key.as_str()) {
                return Err(SpecError::new(
                    key.clone(),
                    format!("not a field of moruna_spec {SPEC_VERSION}"),
                ));
            }
            check_section(key, section)?;
        }
        serde_json::from_value(value).map_err(|e| SpecError::new("spec", e.to_string()))
    }

    /// The document as indented JSON, for a person to read.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// The canonical form (MH 4.1): keys sorted, no whitespace, absent fields omitted.
    pub fn canonical_json(&self) -> String {
        match serde_json::to_value(self) {
            Ok(value) => canonical::to_string(&value),
            Err(e) => format!("{{\"error\":\"{e}\"}}"),
        }
    }

    /// The content address (MH 4.1): `sha256:` and the hex digest of the canonical form
    /// of this document with `resume` and `report` removed. Those two say how this process
    /// continues and where it reports, not what the job is, so a resume of a job has the job's
    /// digest and a host may move the report without changing it.
    pub fn digest(&self) -> String {
        let mut identity = self.clone();
        identity.resume = None;
        identity.report = ReportDoc::default();
        canonical::sha256_hex(identity.canonical_json().as_bytes())
    }
}

/// Read one top-level section into its type, so a refusal names the section.
fn check_section(key: &str, section: &serde_json::Value) -> Result<(), SpecError> {
    fn read<T: serde::de::DeserializeOwned>(
        key: &str,
        section: &serde_json::Value,
    ) -> Result<(), SpecError> {
        serde_json::from_value::<T>(section.clone())
            .map(|_| ())
            .map_err(|e| SpecError::new(key, e.to_string()))
    }
    match key {
        "source" => read::<SourceDoc>(key, section),
        "sink" => read::<SinkDoc>(key, section),
        "budget" => read::<BudgetDoc>(key, section),
        "staging" => read::<StagingDoc>(key, section),
        "object_store" => read::<ObjectStoreDoc>(key, section),
        "checkpoint" => read::<CheckpointDoc>(key, section),
        "error_policy" => read::<ErrorPolicyDoc>(key, section),
        "sizer" => read::<SizerDoc>(key, section),
        "host_profile" => read::<Option<HostProfileDoc>>(key, section),
        "report" => read::<ReportDoc>(key, section),
        "kernels" => {
            let serde_json::Value::Array(items) = section else {
                return Err(SpecError::new(key, "a list of kernels"));
            };
            for (i, item) in items.iter().enumerate() {
                read::<KernelDoc>(&format!("kernels[{i}]"), item)?;
            }
            Ok(())
        }
        "run_id" | "resume" | "trace" | "profiles_dir" => read::<Option<String>>(key, section),
        "ordered" | "allow_gil" => read::<bool>(key, section),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
