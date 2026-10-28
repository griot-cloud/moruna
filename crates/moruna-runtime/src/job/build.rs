//! From a document to a `RunSpec` (MH 4.1): the precedence rule, strict mode, the range checks,
//! and each part built by the kind the document names, resolved through the registry of kinds
//! (SDD 18). The only place a `RunSpec` is built from a description.

use std::path::PathBuf;
use std::sync::Arc;

use moruna_discovery::DiscoveryInput;
use moruna_kernel::{
    ErrorPolicy, Guarantee, HostProfile, Kernel, MorunaError, Result, RunId, SizerKind,
};
use moruna_reactor::{AzureConfig, GcsConfig, ObjectStoreConfig, S3Config};

use super::refusal;
use super::translate::{self, ResumeArg};
use crate::kinds::{Context, Entry, Registry, Stage};
use crate::spec::RunSpec;
use moruna_job::{
    Declaration, ErrorPolicyDoc, Fact, GuaranteeDoc, HostProfileDoc, Isolation, JobSpec,
    ObjectStoreDoc, SizerDoc, SpecError,
};

/// The fields whose `null` an environment variable may fill, and the variable (MH 4.1).
pub const ENV_FIELDS: [(&str, &str); 5] = [
    ("budget.memory_bytes", "MORUNA_BUDGET"),
    ("budget.cpu", "MORUNA_CPU"),
    ("staging.dir", "MORUNA_SPILL_DIR"),
    ("staging.limit_bytes", "MORUNA_SPILL_LIMIT"),
    ("host_profile", "MORUNA_HOST_PROFILE"),
];

/// How a document is resolved (MH 4.1).
pub struct BuildOptions<'a> {
    /// Refuse any field the environment would fill (H8).
    pub strict: bool,
    /// The environment, as a lookup, so the tests need not mutate the process's.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Notes the caller already made (the Python surface's own clamps), which come first.
    pub notes: Vec<String>,
    /// What a `.pex` package's sandbox needs from the host (17 e.6).
    pub packages: crate::packaged::PackageHost,
    /// The host protocol's ports, when a peer is there to serve them (MH 4.10); a document that
    /// lists a host port is refused without one.
    pub host_ports: Option<std::sync::Arc<crate::host::ports::HostPorts>>,
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
    /// The document's kernels each stage carries, in stage order: one each, or several adjacent
    /// standard kernels fused into one stage (MH 4.9), which the task events tell as a group.
    pub stage_kernels: Vec<Vec<usize>>,
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

/// Build the facade's `RunSpec` from a document (MH 4.1), each part by the kind `kinds` resolves
/// its `kind` to (SDD 18 f.2). The order is the order a refusal is found in: the environment, the
/// run id, the kinds, strict mode and the contracts by the kinds' declarations, the source, the
/// sink, the kernels, the ranges, then resume, which reads the disk.
pub fn build(job: &JobSpec, kinds: &Registry, opts: BuildOptions<'_>) -> Result<Built> {
    // f.2: the precedence rule. A field set wins; a null field goes to discovery, which reads
    // its variable when one is set, and then to the default. Strict mode refuses the second.
    let env_resolved = env_reliance(job, opts.env);
    if opts.strict && !env_resolved.is_empty() {
        return Err(refusal(strict_refusal(&env_resolved)));
    }
    let mut notes = opts.notes;
    for (field, var) in &env_resolved {
        notes.push(format!("{field} resolved from {var}"));
    }

    let run_id = match &job.run_id {
        Some(hex) => Some(RunId::from_hex(hex).ok_or_else(|| {
            refusal(SpecError::new(
                "run_id",
                format!("`{hex}` is not 32 lowercase hex characters"),
            ))
        })?),
        None => None,
    };

    // KD-I1: every kind the document names, resolved before anything is built.
    let (source_declared, source_kind) = kinds.source("source", &job.source.kind)?;
    let (sink_declared, sink_kind) = kinds.sink("sink", &job.sink.kind)?;
    let mut kernel_kinds = Vec::with_capacity(job.kernels.len());
    for (index, doc) in job.kernels.iter().enumerate() {
        kernel_kinds.push(kinds.kernel(&format!("kernels[{index}]"), &doc.kind)?);
    }

    // KD-I6, 16 PK-I8: a hosted run loads no code by import path.
    if opts.strict {
        let named = std::iter::once(("source".to_string(), source_declared))
            .chain(
                kernel_kinds
                    .iter()
                    .enumerate()
                    .map(|(i, (declared, _))| (format!("kernels[{i}]"), *declared)),
            )
            .chain(std::iter::once(("sink".to_string(), sink_declared)));
        for (field, declared) in named {
            if declared.isolation == Isolation::Imported {
                return Err(refusal(imported_refusal(&field, declared)));
            }
        }
    }

    // KD-I8: contracts carried for no kind that reads or writes under them are refused.
    if !job.contracts.is_empty()
        && !std::iter::once(source_declared)
            .chain(kernel_kinds.iter().map(|(declared, _)| *declared))
            .chain(std::iter::once(sink_declared))
            .any(|declared| declared.needs.contracts == Fact::Yes)
    {
        return Err(refusal(SpecError::new(
            "contracts",
            "carried, and no kind of this document reads or writes under contracts",
        )));
    }

    let mut cx = Context::new(kinds.embedding())
        .packages(opts.packages)
        .host_ports(opts.host_ports)
        .contracts(job.contracts.clone());
    let source = source_kind.source(&Entry::new("source", None, &job.source), &mut cx)?;
    cx.set_source_targets(source.targets.clone());
    let sink = sink_kind.sink(&Entry::new("sink", None, &job.sink), &mut cx)?;
    translate::check_sink_not_source(&sink.target, &source.targets)?;

    // Each entry is built on its own; an entry whose stage joins the previous one becomes part of
    // it (MH 4.9), and stage ids are counted after that.
    let mut stages: Vec<Stage> = Vec::with_capacity(job.kernels.len());
    let mut stage_kernels: Vec<Vec<usize>> = Vec::with_capacity(job.kernels.len());
    for (index, (doc, (_, kind))) in job.kernels.iter().zip(&kernel_kinds).enumerate() {
        let field = format!("kernels[{index}]");
        let stage = kind.stage(&Entry::new(field, Some(index), doc), &mut cx)?;
        let joined = match (stages.last().and_then(|s| s.fuse.as_ref()), &stage.fuse) {
            (Some(last), Some(next)) => last.fuse(next.as_ref()),
            _ => None,
        };
        match (joined, stages.last_mut(), stage_kernels.last_mut()) {
            (Some(joined), Some(last), Some(members)) => {
                *last = joined;
                members.push(index);
            }
            _ => {
                stages.push(stage);
                stage_kernels.push(vec![index]);
            }
        }
    }
    notes.extend(cx.take_notes());
    let mut kernels: Vec<Arc<dyn Kernel>> = Vec::with_capacity(stages.len());
    let mut packaged = Vec::new();
    #[cfg(feature = "python")]
    let mut py_kernels = Vec::new();
    for (at, stage) in stages.into_iter().enumerate() {
        let id = (at + 1) as moruna_kernel::StageId;
        if let Some(placed) = stage.placed {
            placed(id);
        }
        if let Some(p) = stage.packaged {
            packaged.push(p);
        }
        #[cfg(feature = "python")]
        if let Some(py) = stage.python {
            py_kernels.push((id, py));
        }
        kernels.push(stage.kernel);
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
        return Err(refusal(SpecError::new(
            "budget.cpu",
            format!("`{cpu}` is not a positive number of cores"),
        )));
    }
    if let Some(elastic) = &job.budget.elastic
        && let (Some(max), Some(start)) = (elastic.memory_max_bytes, job.budget.memory_bytes)
        && max < start
    {
        return Err(refusal(SpecError::new(
            "budget.elastic.memory_max_bytes",
            format!("{max} is below budget.memory_bytes {start}"),
        )));
    }
    let host_profile = host_profile_of(job)?;
    let staging_dir = job.staging.dir.as_deref().map(PathBuf::from);
    // `"auto"` is resolved by the facade, after discovery, to the newest manifest this job wrote
    // (same document digest and kernels), or a fresh run when there is none (MH 4.7). It still
    // needs a staging directory to look in.
    let resume_arg = job.resume.as_deref().map(translate::resume_arg);
    let resume_auto = matches!(resume_arg, Some(translate::ResumeArg::Auto));
    // MH 4.7: with `staging.url`, the run works in `staging.dir` and copies each manifest to the
    // prefix; a resume by "auto" or by run id reads the copies into the directory first.
    if let Some(url) = &job.staging.url {
        let Some(dir) = &staging_dir else {
            return Err(refusal(SpecError::new(
                "staging.url",
                "needs staging.dir: the run works in a directory and copies its manifests to the \
                 prefix",
            )));
        };
        if !job.checkpoint.enabled {
            return Err(refusal(SpecError::new(
                "staging.url",
                "copies manifests, and checkpoint.enabled is false: there are none",
            )));
        }
        if matches!(
            resume_arg,
            Some(translate::ResumeArg::Auto | translate::ResumeArg::RunId(_))
        ) {
            let files = moruna_reactor::ObjectFiles::new(object_store_of(&job.object_store))?;
            std::fs::create_dir_all(dir).map_err(|e| MorunaError::Io {
                op: "create_dir",
                target: dir.display().to_string(),
                msg: e.to_string(),
            })?;
            notes.extend(crate::durable::fetch(&files, url, dir)?);
        }
    }
    let resume = if resume_auto {
        if staging_dir.is_none() {
            return Err(MorunaError::Resume(
                "resume \"auto\" needs staging.dir so the manifest can be found".into(),
            ));
        }
        None
    } else {
        translate::resolve_resume(resume_arg.as_ref(), staging_dir.as_deref())?
    };

    let discovery = DiscoveryInput {
        explicit_budget: job.budget.memory_bytes,
        explicit_cpu: job.budget.cpu,
        explicit_staging_dir: staging_dir.clone(),
        explicit_spill_limit: staging_limit,
        profile_override: host_profile.clone(),
    };

    let mut spec = RunSpec::new(source.spec, kernels, sink.spec);
    spec.engine_memory = source.memory;
    spec.packaged = packaged;
    spec.sink_memory = sink.memory;
    #[cfg(feature = "python")]
    {
        spec.py_kernels = py_kernels;
    }
    spec.budget = job.budget.memory_bytes;
    spec.cpu = job.budget.cpu;
    if let Some(elastic) = &job.budget.elastic {
        spec.elastic = crate::elastic::ElasticBudget {
            memory_max_bytes: elastic.memory_max_bytes,
            cpu_max: elastic
                .cpu_max
                .map(|cpus| u16::try_from(cpus.clamp(1, 1024)).unwrap_or(1024)),
        };
    }
    spec.trace_path = trace_path;
    spec.staging_dir = staging_dir;
    spec.staging_limit = staging_limit;
    spec.staging_url = job.staging.url.clone();
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
    spec.resume_auto = resume_auto;
    let digest = super::digest(job);
    spec.spec_digest = Some(digest.clone());
    spec.notes = notes;

    Ok(Built {
        spec,
        run_id,
        digest,
        discovery,
        env_resolved,
        stage_kernels,
    })
}

/// KD-I6: the strict-mode refusal of a kind whose code is imported by path (16 PK-I8).
fn imported_refusal(field: &str, declared: &Declaration) -> SpecError {
    SpecError::new(
        format!("{field}.kind"),
        format!(
            "strict mode loads no code by import path, and `{}` is imported from the \
             interpreter's path: a hosted run loads its kernels from packages (moruna package, \
             then {{\"kind\": \"package\", \"path\", \"fingerprint\"}}) or from the kinds the \
             program running it registers",
            declared.name
        ),
    )
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
                return Err(refusal(SpecError::new(
                    "staging.durable",
                    "disagrees with host_profile.durable_staging",
                )));
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
                return Err(refusal(SpecError::new(
                    "host_profile.staging_dir",
                    format!("needs an absolute path, found `{dir}`"),
                )));
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
