//! From a document to a `RunSpec` (MH 4.1): the precedence rule, strict mode, the range checks
//! and the component builders. The only place a `RunSpec` is built from a description.

use std::path::PathBuf;
use std::sync::Arc;

use moruna_discovery::DiscoveryInput;
use moruna_kernel::{
    ErrorPolicy, Guarantee, HostProfile, Kernel, MorunaError, Result, RunId, SizerKind, Source,
};
use moruna_reactor::{AzureConfig, GcsConfig, ObjectStoreConfig, S3Config};
use moruna_sinks::{
    ArrowIpcSink, ArrowIpcSinkConfig, ParquetSink, ParquetSinkConfig, TensorFormat, TensorSink,
    TensorSinkConfig,
};
use moruna_sources::{
    ParquetSource, ParquetSourceConfig, RowFilter, ScalarValue, TensorSource, TensorSourceConfig,
};
use parquet::basic::{Compression, ZstdLevel};

use super::translate::{self, ResumeArg};
use super::{
    ErrorPolicyDoc, FilterDoc, GuaranteeDoc, HostProfileDoc, JobSpec, KernelDoc, KernelKindDoc,
    ObjectStoreDoc, SinkDoc, SizerDoc, SourceDoc, SpecError,
};
use crate::spec::{RunSpec, SinkSpec, SourceSpec};

/// The fields whose `null` an environment variable may fill, and the variable (MH 4.1).
pub const ENV_FIELDS: [(&str, &str); 5] = [
    ("budget.memory_bytes", "MORUNA_BUDGET"),
    ("budget.cpu", "MORUNA_CPU"),
    ("staging.dir", "MORUNA_SPILL_DIR"),
    ("staging.limit_bytes", "MORUNA_SPILL_LIMIT"),
    ("host_profile", "MORUNA_HOST_PROFILE"),
];

/// A kernel a loader produced for one stage.
pub struct LoadedKernel {
    /// The kernel, as the facade runs it.
    pub kernel: Arc<dyn Kernel>,
    /// The same kernel as the Python adapter's type, when it is a Python kernel, for
    /// `bind_allocator` and `gil_state` (05 d.1).
    #[cfg(feature = "python")]
    pub python: Option<Arc<moruna_adapters::PyKernel>>,
}

/// What turns a kernel entry into a kernel (MH 4.2). The Python surface's loader imports the
/// module a file names; `moruna.run`'s hands back the objects it was given.
pub trait KernelLoader {
    /// The kernel for `doc`, which is entry `index` of `kernels` (stage `index + 1`).
    fn load(&self, index: usize, doc: &KernelDoc) -> Result<LoadedKernel>;

    /// The source for `"kind": "iterator"`, which only a library caller can supply.
    fn iterator_source(&self) -> Result<SourceSpec> {
        Err(SpecError::new(
            "source.kind",
            "`iterator` names a Python iterable in the caller's process and is library only; a \
             document names a parquet or tensor source",
        )
        .into())
    }
}

/// A loader with no kernels at all: every kernel entry is refused. What a build without the
/// Python adapter runs with.
pub struct NoKernels;

impl KernelLoader for NoKernels {
    fn load(&self, index: usize, _doc: &KernelDoc) -> Result<LoadedKernel> {
        Err(SpecError::new(
            format!("kernels[{index}]"),
            "this build of moruna has no Python adapter, so it runs no Python kernels",
        )
        .into())
    }
}

/// How a document is resolved (MH 4.1).
pub struct BuildOptions<'a> {
    /// Refuse any field the environment would fill (H8).
    pub strict: bool,
    /// The environment, as a lookup, so the tests need not mutate the process's.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Notes the caller already made (the Python surface's own clamps), which come first.
    pub notes: Vec<String>,
}

/// The process environment, for [`BuildOptions::env`].
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// A document, built.
pub struct Built {
    /// What the facade runs.
    pub spec: RunSpec,
    /// `run_id` from the document, when it named one.
    pub run_id: Option<RunId>,
    /// The document's content address (MH 4.1).
    pub digest: String,
    /// What discovery is asked, so a caller can report the limits before the run (MH 4.3,
    /// `hello`).
    pub discovery: DiscoveryInput,
    /// Every field the environment filled, as `(field, variable)`.
    pub env_resolved: Vec<(&'static str, &'static str)>,
}

/// The fields of `job` left `null` that an environment variable would fill (MH 4.1).
pub fn env_reliance(
    job: &JobSpec,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    for (field, var) in ENV_FIELDS {
        let unset = match field {
            "budget.memory_bytes" => job.budget.memory_bytes.is_none(),
            "budget.cpu" => job.budget.cpu.is_none(),
            "staging.dir" => job.staging.dir.is_none(),
            "staging.limit_bytes" => job.staging.limit_bytes.is_none(),
            // Discovery reads the variable underneath a declared profile and takes from it every
            // guarantee the profile leaves undeclared (03 d.1), so only a complete declaration
            // leaves nothing to the variable.
            _ => !job
                .host_profile
                .as_ref()
                .is_some_and(HostProfileDoc::is_complete),
        };
        if unset && env(var).is_some() {
            out.push((field, var));
        }
    }
    out
}

/// The strict-mode refusal (H8, MH 4.1): every field the environment would fill, named.
pub fn strict_refusal(reliance: &[(&'static str, &'static str)]) -> SpecError {
    let fields: Vec<&str> = reliance.iter().map(|(f, _)| *f).collect();
    let pairs: Vec<String> = reliance
        .iter()
        .map(|(f, v)| format!("{f} from {v}"))
        .collect();
    SpecError::new(
        fields.join(", "),
        format!(
            "strict mode resolves no field from the environment, and this document would \
             resolve {}; set the field in the document or unset the variable",
            pairs.join(", ")
        ),
    )
}

/// Build the facade's `RunSpec` from a document (MH 4.1). The order is the order a refusal is
/// found in: the environment, the run id, the source, the sink, the kernels, the ranges, then
/// resume, which reads the disk.
pub fn build(job: &JobSpec, loader: &dyn KernelLoader, opts: BuildOptions<'_>) -> Result<Built> {
    let mut notes = opts.notes;

    // f.2: the precedence rule. A field set wins; a null field goes to discovery, which reads
    // its variable when one is set, and then to the default. Strict mode refuses the second.
    let env_resolved = env_reliance(job, opts.env);
    if opts.strict && !env_resolved.is_empty() {
        return Err(strict_refusal(&env_resolved).into());
    }
    for (field, var) in &env_resolved {
        notes.push(format!("{field} resolved from {var}"));
    }

    let run_id = match &job.run_id {
        Some(hex) => Some(RunId::from_hex(hex).ok_or_else(|| {
            SpecError::new(
                "run_id",
                format!("`{hex}` is not 32 lowercase hex characters"),
            )
        })?),
        None => None,
    };

    let (source, source_targets) = source_of(&job.source, loader)?;
    let (sink, sink_target) = sink_of(&job.sink, &mut notes)?;
    translate::check_sink_not_source(&sink_target, &source_targets)?;

    let mut kernels: Vec<Arc<dyn Kernel>> = Vec::with_capacity(job.kernels.len());
    #[cfg(feature = "python")]
    let mut py_kernels = Vec::new();
    for (index, doc) in job.kernels.iter().enumerate() {
        if doc.kind == KernelKindDoc::Rust {
            return Err(SpecError::new(
                format!("kernels[{index}].kind"),
                "rust kernels are reserved in moruna_spec 1",
            )
            .into());
        }
        let loaded = loader.load(index, doc)?;
        if let Some(pinned) = &doc.fingerprint {
            let want = pinned
                .rsplit(':')
                .next()
                .unwrap_or(pinned)
                .to_ascii_lowercase();
            let have = loaded.kernel.fingerprint().to_hex();
            if want != have {
                return Err(SpecError::new(
                    format!("kernels[{index}].fingerprint"),
                    format!("the document pins {want} and the loaded kernel's is {have}"),
                )
                .into());
            }
        }
        #[cfg(feature = "python")]
        if let Some(py) = loaded.python {
            py_kernels.push(((index + 1) as moruna_kernel::StageId, py));
        }
        kernels.push(loaded.kernel);
    }

    let checkpoint_interval_ms = translate::clamp(
        "checkpoint.interval_ms",
        job.checkpoint.interval_ms,
        translate::CHECKPOINT_INTERVAL_MS,
        &mut notes,
    );
    let staging_limit = job.staging.limit_bytes.map(|limit| {
        translate::clamp(
            "budget.disk",
            limit,
            (translate::STAGING_LIMIT_MIN, u64::MAX),
            &mut notes,
        )
    });
    let trace_path = match &job.trace {
        Some(value) => Some(translate::trace_path(value)?),
        None => None,
    };
    if let Some(cpu) = job.budget.cpu
        && !(cpu.is_finite() && cpu > 0.0)
    {
        return Err(SpecError::new(
            "budget.cpu",
            format!("`{cpu}` is not a positive number of cores"),
        )
        .into());
    }
    if let Some(elastic) = &job.budget.elastic {
        if let (Some(max), Some(start)) = (elastic.memory_max_bytes, job.budget.memory_bytes)
            && max < start
        {
            return Err(SpecError::new(
                "budget.elastic.memory_max_bytes",
                format!("{max} is below budget.memory_bytes {start}"),
            )
            .into());
        }
        notes.push(
            "budget.elastic is recorded; this build sizes the run once, at start".to_string(),
        );
    }
    let host_profile = host_profile_of(job)?;
    let staging_dir = job.staging.dir.as_deref().map(PathBuf::from);
    let resume = translate::resolve_resume(
        job.resume.as_deref().map(translate::resume_arg).as_ref(),
        staging_dir.as_deref(),
    )?;

    let discovery = DiscoveryInput {
        explicit_budget: job.budget.memory_bytes,
        explicit_cpu: job.budget.cpu,
        explicit_staging_dir: staging_dir.clone(),
        explicit_spill_limit: staging_limit,
        profile_override: host_profile.clone(),
    };

    let mut spec = RunSpec::new(source, kernels, sink);
    #[cfg(feature = "python")]
    {
        spec.py_kernels = py_kernels;
    }
    spec.budget = job.budget.memory_bytes;
    spec.cpu = job.budget.cpu;
    spec.trace_path = trace_path;
    spec.staging_dir = staging_dir;
    spec.staging_limit = staging_limit;
    spec.error_policy = match job.error_policy {
        ErrorPolicyDoc::Terminate => ErrorPolicy::Terminate,
        ErrorPolicyDoc::Skip => ErrorPolicy::Skip,
        ErrorPolicyDoc::Budget(n) => ErrorPolicy::Budget(n),
    };
    spec.ordered = job.ordered;
    spec.sizer = match job.sizer {
        SizerDoc::Rule => SizerKind::Rule,
        SizerDoc::Learned => SizerKind::Learned,
    };
    spec.profiles_dir = job.profiles_dir.as_deref().map(PathBuf::from);
    spec.object_store = object_store_of(&job.object_store);
    spec.host_profile = host_profile;
    spec.allow_gil = job.allow_gil;
    spec.checkpoint = job.checkpoint.enabled;
    spec.checkpoint_interval_ms = checkpoint_interval_ms;
    spec.checkpoint_keep = job.checkpoint.keep;
    spec.resume = resume;
    let digest = job.digest();
    spec.spec_digest = Some(digest.clone());
    spec.notes = notes;

    Ok(Built {
        spec,
        run_id,
        digest,
        discovery,
        env_resolved,
    })
}

/// The source's builder and the URLs it reads, for the sink equals source rule.
fn source_of(doc: &SourceDoc, loader: &dyn KernelLoader) -> Result<(SourceSpec, Vec<String>)> {
    match doc {
        SourceDoc::Parquet { url, options } => {
            if url.0.is_empty() {
                return Err(
                    SpecError::new("source.url", "empty: a source needs at least one url").into(),
                );
            }
            let mut filters = Vec::with_capacity(options.filters.len());
            for (i, filter) in options.filters.iter().enumerate() {
                filters.push(row_filter(i, filter)?);
            }
            let cfg = ParquetSourceConfig {
                urls: url.0.clone(),
                columns: options.columns.clone(),
                filters,
                batch_rows_hint: None,
            };
            let targets = url.0.clone();
            Ok((
                SourceSpec::Build(Box::new(move |ctx| {
                    let meta = ctx.object_metadata()?;
                    Ok(
                        Arc::new(ParquetSource::new(cfg, ctx.reactor.clone(), meta)?)
                            as Arc<dyn Source>,
                    )
                })),
                targets,
            ))
        }
        SourceDoc::Tensor { url, options } => {
            if url.0.is_empty() {
                return Err(SpecError::new(
                    "source.url",
                    "empty: a source needs at least one path",
                )
                .into());
            }
            let cfg = TensorSourceConfig {
                paths: url.0.iter().map(|u| translate::local_path(u)).collect(),
                tensors: options.tensors.clone(),
                slice_rows_hint: None,
            };
            let targets = url.0.clone();
            Ok((
                SourceSpec::Build(Box::new(move |ctx| {
                    Ok(Arc::new(TensorSource::new(cfg, ctx.reactor.clone())?) as Arc<dyn Source>)
                })),
                targets,
            ))
        }
        SourceDoc::Iterator => Ok((loader.iterator_source()?, Vec::new())),
    }
}

/// `[column, op, value]` to a row-group predicate (07 d.1).
fn row_filter(index: usize, doc: &FilterDoc) -> Result<RowFilter> {
    let field = || format!("source.options.filters[{index}]");
    let FilterDoc(column, op, value) = doc;
    let scalar = match value {
        serde_json::Value::Bool(b) => ScalarValue::Bool(*b),
        serde_json::Value::String(s) => ScalarValue::Str(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ScalarValue::I64(i)
            } else if let Some(u) = n.as_u64() {
                ScalarValue::U64(u)
            } else {
                ScalarValue::F64(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        other => {
            return Err(SpecError::new(
                field(),
                format!("a filter value is a number, a string or a boolean, not {other}"),
            )
            .into());
        }
    };
    Ok(match op.as_str() {
        ">" | "gt" => RowFilter::Gt(column.clone(), scalar),
        "<" | "lt" => RowFilter::Lt(column.clone(), scalar),
        "==" | "=" | "eq" => RowFilter::Eq(column.clone(), scalar),
        other => {
            return Err(SpecError::new(
                field(),
                format!("unknown operator `{other}` (use \">\", \"<\" or \"==\")"),
            )
            .into());
        }
    })
}

/// The sink's builder and where it writes.
fn sink_of(doc: &SinkDoc, notes: &mut Vec<String>) -> Result<(SinkSpec, String)> {
    match doc {
        SinkDoc::Parquet { url, options } => {
            let url = translate::local_url(url);
            let row_group_bytes = translate::clamp_row_group_bytes(
                options
                    .row_group_bytes
                    .unwrap_or(translate::DEFAULT_ROW_GROUP_BYTES),
                notes,
            );
            let file_bytes = translate::clamp_file_bytes(
                options.file_bytes.unwrap_or(translate::DEFAULT_FILE_BYTES),
                notes,
            );
            let compression =
                parse_compression(options.compression.as_deref().unwrap_or("zstd"))
                    .map_err(|reason| SpecError::new("sink.options.compression", reason))?;
            let target = url.clone();
            Ok((
                SinkSpec::Build(Box::new(move |ctx| {
                    let cfg = ParquetSinkConfig {
                        url,
                        row_group_bytes,
                        file_bytes,
                        compression,
                        writer_props: None,
                    };
                    // The run id goes into every file's footer (08 e.2).
                    let sink = ParquetSink::new(cfg, ctx.reactor.clone(), ctx.alloc.clone())?
                        .with_run_id(ctx.run_id);
                    Ok(Box::new(sink) as Box<dyn moruna_kernel::Sink>)
                })),
                target,
            ))
        }
        SinkDoc::Tensor { url, options } => {
            let format = parse_tensor_format(options.format.as_deref().unwrap_or("mrb1"))
                .map_err(|reason| SpecError::new("sink.options.format", reason))?;
            let cfg = TensorSinkConfig {
                path: translate::local_path(url),
                format,
                one_file_per_morsel: options.one_file_per_morsel,
                name: options.name.clone().unwrap_or_else(|| "tensor".to_string()),
            };
            Ok((
                SinkSpec::Build(Box::new(move |ctx| {
                    Ok(Box::new(TensorSink::new(
                        cfg,
                        ctx.reactor.clone(),
                        ctx.alloc.clone(),
                    )?) as Box<dyn moruna_kernel::Sink>)
                })),
                url.clone(),
            ))
        }
        SinkDoc::ArrowIpc { url, options } => {
            let file_bytes = translate::clamp_file_bytes(
                options.file_bytes.unwrap_or(translate::DEFAULT_FILE_BYTES),
                notes,
            );
            let cfg = ArrowIpcSinkConfig {
                path: translate::local_path(url),
                file_bytes,
            };
            Ok((
                SinkSpec::Build(Box::new(move |ctx| {
                    Ok(Box::new(ArrowIpcSink::new(
                        cfg,
                        ctx.reactor.clone(),
                        ctx.alloc.clone(),
                    )?) as Box<dyn moruna_kernel::Sink>)
                })),
                url.clone(),
            ))
        }
    }
}

/// A Parquet compression by the name a document and `ParquetSink(compression=)` use.
pub fn parse_compression(name: &str) -> core::result::Result<Compression, String> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "zstd" => Compression::ZSTD(ZstdLevel::default()),
        "snappy" => Compression::SNAPPY,
        "gzip" => Compression::GZIP(Default::default()),
        "lz4" => Compression::LZ4_RAW,
        "none" | "uncompressed" => Compression::UNCOMPRESSED,
        other => {
            return Err(format!(
                "unknown compression `{other}` (use \"zstd\", \"snappy\", \"gzip\", \"lz4\" or \"none\")"
            ));
        }
    })
}

/// The name [`parse_compression`] reads for a compression, so a built sink can be described.
pub fn compression_name(compression: Compression) -> &'static str {
    match compression {
        Compression::SNAPPY => "snappy",
        Compression::GZIP(_) => "gzip",
        Compression::LZ4_RAW | Compression::LZ4 => "lz4",
        Compression::UNCOMPRESSED => "none",
        _ => "zstd",
    }
}

/// A tensor file format by the name a document and `TensorSink(format=)` use.
pub fn parse_tensor_format(name: &str) -> core::result::Result<TensorFormat, String> {
    match name.to_ascii_lowercase().as_str() {
        "mrb1" => Ok(TensorFormat::Amb1),
        "safetensors" => Ok(TensorFormat::SafeTensors),
        other => Err(format!(
            "unknown tensor format `{other}` (use \"mrb1\" or \"safetensors\")"
        )),
    }
}

/// The name [`parse_tensor_format`] reads for a format.
pub fn tensor_format_name(format: TensorFormat) -> &'static str {
    match format {
        TensorFormat::Amb1 => "mrb1",
        TensorFormat::SafeTensors => "safetensors",
    }
}

/// `host_profile` and `staging.durable` to the profile discovery is handed (03 d.1). `durable`
/// is `durable_staging` spelled where a host thinks of it; the two may not disagree.
fn host_profile_of(job: &JobSpec) -> Result<Option<HostProfile>> {
    fn guarantee(g: Option<GuaranteeDoc>) -> Guarantee {
        match g {
            Some(GuaranteeDoc::Present) => Guarantee::Present,
            Some(GuaranteeDoc::Absent) => Guarantee::Absent,
            None => Guarantee::Unknown,
        }
    }
    let durable = job.staging.durable.map(|d| {
        if d {
            GuaranteeDoc::Present
        } else {
            GuaranteeDoc::Absent
        }
    });
    let doc = match (&job.host_profile, durable) {
        (None, None) => return Ok(None),
        (None, Some(d)) => HostProfileDoc {
            durable_staging: Some(d),
            ..HostProfileDoc::default()
        },
        (Some(p), None) => p.clone(),
        (Some(p), Some(d)) => {
            if let Some(declared) = p.durable_staging
                && declared != d
            {
                return Err(SpecError::new(
                    "staging.durable",
                    "disagrees with host_profile.durable_staging",
                )
                .into());
            }
            HostProfileDoc {
                durable_staging: Some(d),
                ..p.clone()
            }
        }
    };
    let staging_dir = match &doc.staging_dir {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if !path.is_absolute() {
                return Err(SpecError::new(
                    "host_profile.staging_dir",
                    format!("needs an absolute path, found `{dir}`"),
                )
                .into());
            }
            Some(path)
        }
        None => None,
    };
    Ok(Some(HostProfile {
        huge_pages: guarantee(doc.huge_pages),
        memlock: guarantee(doc.memlock),
        io_uring: guarantee(doc.io_uring),
        direct_io_staging: guarantee(doc.direct_io),
        gds: guarantee(doc.gds),
        rdma: guarantee(doc.rdma),
        staging_dir,
        durable_staging: guarantee(doc.durable_staging),
    }))
}

fn object_store_of(doc: &ObjectStoreDoc) -> ObjectStoreConfig {
    ObjectStoreConfig {
        s3: doc.s3.as_ref().map(|s| S3Config {
            endpoint: s.endpoint.clone(),
            region: s.region.clone(),
            access_key_id: s.access_key_id.clone(),
            secret_access_key: s.secret_access_key.clone(),
            session_token: s.session_token.clone(),
            bucket: s.bucket.clone(),
        }),
        gcs: doc.gcs.as_ref().map(|g| GcsConfig {
            service_account_path: g.service_account_path.as_deref().map(PathBuf::from),
            service_account_json: g.service_account_json.clone(),
            bucket: g.bucket.clone(),
        }),
        azure: doc.azure.as_ref().map(|a| AzureConfig {
            account: a.account.clone(),
            access_key: a.access_key.clone(),
            container: a.container.clone(),
        }),
        local_root: doc.local_root.as_deref().map(PathBuf::from),
        allow_http: doc.allow_http,
    }
}

/// The shape of a resume argument, for a caller that must know it before building (the host
/// session reads the manifest's run id to name the report file).
pub fn resume_shape(job: &JobSpec) -> Option<ResumeArg> {
    job.resume.as_deref().map(translate::resume_arg)
}

/// The error a document refusal is, for a caller that has only a `MorunaError`.
pub fn is_spec_refusal(error: &MorunaError) -> bool {
    matches!(
        error,
        MorunaError::Config { .. } | MorunaError::Plan(_) | MorunaError::Convert(_)
    )
}
