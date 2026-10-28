//! The job document: `RunSpec` as a file (MH 4.1).
//!
//! A run is described by one JSON document with `"moruna_spec": 1`. [`JobSpec`] is that
//! document, field for field. Reading one, writing one and its canonical form are here and
//! run nothing; building a run from it is `moruna-runtime`'s, the only place a run is built from a
//! description, whether the description came from a file (`moruna run`), from a socket (`moruna
//! serve`) or from the arguments of `moruna.run(...)` (12 f.3). One code path, so a run from a
//! file and the same run from Python are the same run (H1).
//!
//! Every optional field is `null` or absent when the document leaves it to the runtime; the
//! precedence rule of MH 4.1 says what fills it. The canonical form (MH 4.1, [`JobSpec::canonical_json`])
//! omits absent fields, so a document that spells a `null` and one that leaves the key out
//! have one digest.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// `checkpoint.interval_ms` when the document leaves it out.
pub const CHECKPOINT_INTERVAL_MS: u64 = 5000;

/// The one version of the document this build reads (MH 4.1).
pub const SPEC_VERSION: u32 = 1;

/// Why a document was refused, with the field it was refused for (MH 4.1). `moruna run`
/// exits 2 with this text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("spec refused: {field}: {reason}")]
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

/// One run, as a document (MH 4.1). Every field of the runtime's `RunSpec` has a field
/// here; `notes` and the Python kernel list are derived, not described.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    /// The document version; 1.
    pub moruna_spec: u32,
    /// The run's identity, 32 lowercase hex characters; absent: minted at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The input: a source kind and its settings.
    pub source: KindDoc,
    /// Stages 1..=n, in order, each a runnable kind and its settings; empty is a copy from source
    /// to sink.
    #[serde(default)]
    pub kernels: Vec<KindDoc>,
    /// The output: a sink kind and its settings.
    pub sink: KindDoc,
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
    /// The contracts the document's kinds read and write under (MH 4.5; a `datafusion` source, a
    /// `peql` sink), each as its compiler wrote it. They are the job's whole contract store: the engine holds them
    /// in memory for this run alone and reads no contract from its root, so a contract the job
    /// names and does not carry is refused by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<ContractEntry>,
}

/// One contract a job carries (`contracts[i]`, MH 4.5): its document, its compiled form exactly
/// as the compiler wrote it, and the function modules that form pins. Registered as given, never
/// compiled; whoever writes the document vouches that `compiled` is what `document` compiles to,
/// and the program running it may refuse any entry (the runtime's `Embedding::admit`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractEntry {
    /// The contract's document, as the engine reads one.
    pub document: serde_json::Value,
    /// The compiled contract: the compiler's bytes, which are UTF-8.
    pub compiled: String,
    /// The function modules the compiled contract pins, as the engine reads a bundled function.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<serde_json::Value>,
    /// The tenants it is published to for this run besides its owner; `public` is everyone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audiences: Vec<String>,
}

/// One part of a run as the document names it (MH 4.1, SDD 18 d.1): its `kind`, a string the
/// registry of kinds resolves when the document is built (KD-I1), and the rest of the object,
/// which is that kind's settings. A setting spelled `null` is a setting left out, so the two have
/// one canonical form (MH 4.1). What the settings may be is the kind's declaration (SDD 18 e.2);
/// the document itself knows no kind.
#[derive(Clone, Debug, PartialEq)]
pub struct KindDoc {
    /// The kind's name, as the registry knows it (`parquet`, `std`, `package`, ...).
    pub kind: String,
    /// Every other key of the entry, `null`s left out.
    pub settings: serde_json::Map<String, serde_json::Value>,
}

impl KindDoc {
    /// An entry of kind `kind` with `settings`; a `null` setting is left out.
    pub fn new(
        kind: impl Into<String>,
        mut settings: serde_json::Map<String, serde_json::Value>,
    ) -> KindDoc {
        settings.retain(|_, value| !value.is_null());
        KindDoc {
            kind: kind.into(),
            settings,
        }
    }

    /// An entry of kind `kind` whose settings are `settings` as its kind reads them. Refused,
    /// naming `kind`, when they are not an object.
    pub fn of<T: Serialize>(kind: impl Into<String>, settings: &T) -> Result<KindDoc, SpecError> {
        let kind = kind.into();
        match serde_json::to_value(settings) {
            Ok(serde_json::Value::Object(map)) => Ok(KindDoc::new(kind, map)),
            Ok(other) => Err(SpecError::new(
                kind,
                format!("a kind's settings are an object, not {other}"),
            )),
            Err(e) => Err(SpecError::new(kind, e.to_string())),
        }
    }

    /// An entry from its JSON: an object with a non-empty string `kind`.
    pub fn from_value(value: serde_json::Value) -> Result<KindDoc, String> {
        let serde_json::Value::Object(mut map) = value else {
            return Err(format!(
                "an entry is an object naming its `kind`, not {value}"
            ));
        };
        match map.remove("kind") {
            Some(serde_json::Value::String(kind)) if !kind.is_empty() => {
                Ok(KindDoc::new(kind, map))
            }
            Some(other) => Err(format!("`kind` is a kind's name, not {other}")),
            None => Err("missing `kind`: an entry names the kind it is".to_string()),
        }
    }
}

impl Serialize for KindDoc {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(self.settings.len() + 1))?;
        map.serialize_entry("kind", &self.kind)?;
        for (key, value) in &self.settings {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for KindDoc {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<KindDoc, D::Error> {
        KindDoc::from_value(serde_json::Value::deserialize(d)?).map_err(serde::de::Error::custom)
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
    /// How far the budget may follow the machine (MH 4.4); `RunSpec::elastic`.
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
    /// An object-store prefix every manifest is copied to as it is written, and read back from
    /// by a resume on any machine (MH 4.7): `s3://`, `gs://`, `az://` or `file://`. Needs `dir`,
    /// where the run works.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
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
            interval_ms: CHECKPOINT_INTERVAL_MS,
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
    /// The ids the task events name the source, each kernel and the sink by (MH 4.3), opaque
    /// to Moruna. Absent: `source`, `kernel-1` .. `kernel-n` and `sink`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<TaskIdsDoc>,
    /// Which attempt of its job this process is, as its host counts them; every event carries
    /// it, so a host can tell a later attempt's events from an earlier one's (MH 4.3). Absent:
    /// 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// The output store: the directory each attempt's captured output is written under (MH 4.3
    /// `log`), `<run_id>` replaced by the run's id. Absent: `moruna-<run_id>.logs` in the staging
    /// directory, else the working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<String>,
}

/// `report.tasks`: one id for the source, one per kernel in stage order, one for the sink.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIdsDoc {
    /// The source's.
    pub source: TaskRefDoc,
    /// Each kernel's, in stage order.
    pub kernels: Vec<TaskRefDoc>,
    /// The sink's.
    pub sink: TaskRefDoc,
}

/// One element's task: its id, and, when the host fused several of its own tasks into this one
/// element, theirs, which the events repeat so a host can show the group as the group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRefDoc {
    /// The task's id: 1 to 128 of ASCII letters, digits, `_`, `-`, `.` and `:`, starting with a
    /// letter or a digit.
    pub id: String,
    /// The host's tasks this element carries, by the same rule; empty when it is one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// Whether `id` may name a task in the events (MH 4.3).
pub fn is_task_id(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= 128
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

impl ReportDoc {
    /// The attempt the document names; 1 when it names none.
    pub fn attempt(&self) -> u32 {
        self.attempt.unwrap_or(1)
    }
}

/// The top-level keys of version 1, in the order MH 4.1 lists them.
const TOP_LEVEL: [&str; 19] = [
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
    "contracts",
];

impl JobSpec {
    /// A document over `source`, `kernels` and `sink` with every other field absent or at its
    /// default: what `moruna.run(source, kernels, sink)` describes (PY-I3).
    pub fn new(source: KindDoc, kernels: Vec<KindDoc>, sink: KindDoc) -> JobSpec {
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
            contracts: Vec::new(),
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
        let job: JobSpec =
            serde_json::from_value(value).map_err(|e| SpecError::new("spec", e.to_string()))?;
        job.check_report()?;
        Ok(job)
    }

    /// `report.tasks` names the chain as it is, each task once; `report.attempt` counts from 1.
    fn check_report(&self) -> Result<(), SpecError> {
        if self.report.attempt == Some(0) {
            return Err(SpecError::new(
                "report.attempt",
                "attempts are counted from 1",
            ));
        }
        let Some(tasks) = &self.report.tasks else {
            return Ok(());
        };
        if tasks.kernels.len() != self.kernels.len() {
            return Err(SpecError::new(
                "report.tasks.kernels",
                format!(
                    "{} ids for {} kernels: one id per kernel, in stage order",
                    tasks.kernels.len(),
                    self.kernels.len()
                ),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        let named = std::iter::once(("report.tasks.source".to_string(), &tasks.source))
            .chain(
                tasks
                    .kernels
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (format!("report.tasks.kernels[{i}]"), t)),
            )
            .chain(std::iter::once((
                "report.tasks.sink".to_string(),
                &tasks.sink,
            )));
        for (field, task) in named {
            if !is_task_id(&task.id) {
                return Err(SpecError::new(
                    format!("{field}.id"),
                    format!(
                        "{:?} is not a task id: 1 to 128 of ASCII letters, digits, `_`, `-`, `.` \
                         and `:`, starting with a letter or a digit",
                        task.id
                    ),
                ));
            }
            if !seen.insert(task.id.as_str()) {
                return Err(SpecError::new(
                    format!("{field}.id"),
                    format!("{:?} names two tasks", task.id),
                ));
            }
            if task.members.len() > 256 {
                return Err(SpecError::new(
                    format!("{field}.members"),
                    "at most 256 members",
                ));
            }
            if let Some(bad) = task.members.iter().find(|m| !is_task_id(m)) {
                return Err(SpecError::new(
                    format!("{field}.members"),
                    format!("{bad:?} is not a task id"),
                ));
            }
        }
        Ok(())
    }

    /// The document as indented JSON, for a person to read.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// The canonical form (MH 4.1): keys sorted, no whitespace, absent fields omitted.
    pub fn canonical_json(&self) -> String {
        match serde_json::to_value(self) {
            Ok(value) => crate::canonical::to_string(&value),
            Err(e) => format!("{{\"error\":\"{e}\"}}"),
        }
    }

    /// The canonical form of what the job is (MH 4.1): this document with `resume` and `report`
    /// removed. Those two say how this process continues and where it reports, not what the job
    /// is, so a resume of a job has the job's identity and a host may move the report without
    /// changing it. The runtime's content address is the `sha256` of this text.
    pub fn identity_json(&self) -> String {
        let mut identity = self.clone();
        identity.resume = None;
        identity.report = ReportDoc::default();
        identity.canonical_json()
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
        "source" | "sink" => read::<KindDoc>(key, section),
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
                read::<KindDoc>(&format!("kernels[{i}]"), item)?;
            }
            Ok(())
        }
        "contracts" => {
            let serde_json::Value::Array(items) = section else {
                return Err(SpecError::new(key, "a list of contracts"));
            };
            for (i, item) in items.iter().enumerate() {
                read::<ContractEntry>(&format!("contracts[{i}]"), item)?;
            }
            Ok(())
        }
        "run_id" | "resume" | "trace" | "profiles_dir" => read::<Option<String>>(key, section),
        "ordered" | "allow_gil" => read::<bool>(key, section),
        _ => Ok(()),
    }
}
