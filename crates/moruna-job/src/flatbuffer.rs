//! The job document's FlatBuffers form (SDD 22 e.4): [`Job::from_flatbuffer`], which verifies
//! untrusted bytes before reading them, and [`Job::to_flatbuffer`].

use serde_json::Value;

use crate::doc::{
    AzureDoc, BudgetDoc, CheckpointDoc, ContractEntry, ElasticDoc, ErrorPolicyDoc, GcsDoc,
    GuaranteeDoc, HostProfileDoc, ObjectStoreDoc, S3Doc, SizerDoc, StagingDoc,
};
use crate::flex;
use crate::generated::moruna::job::fb;
use crate::job::{Edge, JOB_VERSION, Job, PortRef, Reporting, Task};
use crate::kind::{Flow, Role};

/// The FlatBuffers file identifier of a job document, at bytes 4 to 8 of every buffer.
pub const JOB_IDENTIFIER: &str = "MJB2";

/// Why bytes are not a job document this build reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// The buffer does not carry the job document's file identifier.
    #[error("not a job document: the bytes do not carry the identifier {JOB_IDENTIFIER}")]
    Identifier,
    /// The FlatBuffers verifier refused the bytes: truncated, out of bounds, misaligned or past
    /// the verifier's limits.
    #[error("not a job document: {0}")]
    Malformed(String),
    /// A document of a version this build does not read.
    #[error("job refused: version: this build reads version {JOB_VERSION}, the document says {0}")]
    Version(u32),
    /// A field that verified and does not hold what the document says it holds: settings that
    /// are not a FlexBuffers map, an enum value this build does not know.
    #[error("job refused: {field}: {reason}")]
    Field {
        /// The field, as a path from the document's root (`tasks[2].settings`).
        field: String,
        /// What is wrong with it.
        reason: String,
    },
}

impl JobError {
    fn field(field: impl Into<String>, reason: impl Into<String>) -> JobError {
        JobError::Field {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

/// How far the verifier walks a buffer before it refuses it: deep enough for any document,
/// shallow enough that a hostile one cannot make it work without bound.
fn verifier_options() -> flatbuffers::VerifierOptions {
    flatbuffers::VerifierOptions {
        max_depth: 64,
        max_tables: 1 << 20,
        max_apparent_size: 1 << 31,
        ignore_missing_null_terminator: false,
    }
}

impl Job {
    /// Read a job document from FlatBuffers bytes, which may be anyone's: the verifier checks
    /// every offset, length and required field before anything is read, and each task's
    /// settings and each contract are decoded from FlexBuffers and refused by field when they
    /// are not what the document says they are. Nothing panics on any input.
    pub fn from_flatbuffer(bytes: &[u8]) -> Result<Job, JobError> {
        if !is_job_flatbuffer(bytes) {
            return Err(JobError::Identifier);
        }
        let root = fb::root_as_job_with_opts(&verifier_options(), bytes)
            .map_err(|e| JobError::Malformed(e.to_string()))?;
        if root.version() != JOB_VERSION {
            return Err(JobError::Version(root.version()));
        }
        let mut tasks = Vec::new();
        for (i, task) in root.tasks().iter().flatten().enumerate() {
            tasks.push(read_task(i, task)?);
        }
        let mut edges = Vec::new();
        for (i, edge) in root.edges().iter().flatten().enumerate() {
            edges.push(Edge {
                from: endpoint(edge.from()),
                to: endpoint(edge.to()),
                flow: flow(edge.flow()).ok_or_else(|| {
                    JobError::field(
                        format!("edges[{i}].flow"),
                        format!("{} is not a flow", edge.flow().0),
                    )
                })?,
                barrier: edge.barrier(),
            });
        }
        let mut contracts = Vec::new();
        for (i, contract) in root.contracts().iter().flatten().enumerate() {
            contracts.push(read_contract(i, contract)?);
        }
        Ok(Job {
            id: root.id().map(str::to_string),
            tasks,
            edges,
            budget: root.budget().map(read_budget).unwrap_or_default(),
            staging: root.staging().map(read_staging).unwrap_or_default(),
            object_store: root
                .object_store()
                .map(read_object_store)
                .unwrap_or_default(),
            checkpoint: root
                .checkpoint()
                .map(|c| CheckpointDoc {
                    enabled: c.enabled(),
                    interval_ms: c.interval_ms(),
                    keep: c.keep(),
                })
                .unwrap_or_default(),
            resume: root.resume().map(str::to_string),
            error_policy: match root.error_policy() {
                None => ErrorPolicyDoc::default(),
                Some(p) => match p.kind() {
                    fb::ErrorPolicyKind::Terminate => ErrorPolicyDoc::Terminate,
                    fb::ErrorPolicyKind::Skip => ErrorPolicyDoc::Skip,
                    fb::ErrorPolicyKind::Budget => ErrorPolicyDoc::Budget(p.budget()),
                    other => {
                        return Err(JobError::field(
                            "error_policy.kind",
                            format!("{} is not an error policy", other.0),
                        ));
                    }
                },
            },
            ordered: root.ordered(),
            sizer: match root.sizer() {
                fb::Sizer::Rule => SizerDoc::Rule,
                fb::Sizer::Learned => SizerDoc::Learned,
                other => {
                    return Err(JobError::field(
                        "sizer",
                        format!("{} is not a sizer", other.0),
                    ));
                }
            },
            trace: root.trace().map(str::to_string),
            profiles_dir: root.profiles_dir().map(str::to_string),
            host_profile: match root.host_profile() {
                None => None,
                Some(p) => Some(read_host_profile(p)?),
            },
            allow_gil: root.allow_gil(),
            report: root
                .report()
                .map(|r| Reporting {
                    socket: r.socket().map(str::to_string),
                    file: r.file().map(str::to_string),
                    attempt: r.attempt(),
                    logs: r.logs().map(str::to_string),
                })
                .unwrap_or_default(),
            contracts,
        })
    }

    /// The job as FlatBuffers bytes, with the document's file identifier. Deterministic: the
    /// same job is always the same bytes.
    pub fn to_flatbuffer(&self) -> Result<Vec<u8>, JobError> {
        let mut b = flatbuffers::FlatBufferBuilder::new();
        let mut tasks = Vec::with_capacity(self.tasks.len());
        for (i, task) in self.tasks.iter().enumerate() {
            tasks.push(write_task(&mut b, i, task)?);
        }
        let tasks = b.create_vector(&tasks);
        let edges: Vec<_> = self.edges.iter().map(|e| write_edge(&mut b, e)).collect();
        let edges = b.create_vector(&edges);
        let mut contracts = Vec::with_capacity(self.contracts.len());
        for (i, contract) in self.contracts.iter().enumerate() {
            contracts.push(write_contract(&mut b, i, contract)?);
        }
        let contracts = b.create_vector(&contracts);
        let budget = write_budget(&mut b, &self.budget);
        let staging = write_staging(&mut b, &self.staging);
        let object_store = write_object_store(&mut b, &self.object_store);
        let checkpoint = fb::Checkpoint::create(
            &mut b,
            &fb::CheckpointArgs {
                enabled: self.checkpoint.enabled,
                interval_ms: self.checkpoint.interval_ms,
                keep: self.checkpoint.keep,
            },
        );
        let (kind, budget_n) = match self.error_policy {
            ErrorPolicyDoc::Terminate => (fb::ErrorPolicyKind::Terminate, 0),
            ErrorPolicyDoc::Skip => (fb::ErrorPolicyKind::Skip, 0),
            ErrorPolicyDoc::Budget(n) => (fb::ErrorPolicyKind::Budget, n),
        };
        let error_policy = fb::ErrorPolicy::create(
            &mut b,
            &fb::ErrorPolicyArgs {
                kind,
                budget: budget_n,
            },
        );
        let host_profile = self
            .host_profile
            .as_ref()
            .map(|p| write_host_profile(&mut b, p));
        let report = {
            let socket = opt_str(&mut b, &self.report.socket);
            let file = opt_str(&mut b, &self.report.file);
            let logs = opt_str(&mut b, &self.report.logs);
            fb::Report::create(
                &mut b,
                &fb::ReportArgs {
                    socket,
                    file,
                    attempt: self.report.attempt,
                    logs,
                },
            )
        };
        let id = opt_str(&mut b, &self.id);
        let resume = opt_str(&mut b, &self.resume);
        let trace = opt_str(&mut b, &self.trace);
        let profiles_dir = opt_str(&mut b, &self.profiles_dir);
        let root = fb::Job::create(
            &mut b,
            &fb::JobArgs {
                version: JOB_VERSION,
                id,
                tasks: Some(tasks),
                edges: Some(edges),
                budget: Some(budget),
                staging: Some(staging),
                object_store: Some(object_store),
                checkpoint: Some(checkpoint),
                resume,
                error_policy: Some(error_policy),
                ordered: self.ordered,
                sizer: match self.sizer {
                    SizerDoc::Rule => fb::Sizer::Rule,
                    SizerDoc::Learned => fb::Sizer::Learned,
                },
                trace,
                profiles_dir,
                host_profile,
                allow_gil: self.allow_gil,
                report: Some(report),
                contracts: Some(contracts),
            },
        );
        fb::finish_job_buffer(&mut b, root);
        Ok(b.finished_data().to_vec())
    }
}

/// Whether `bytes` carry the job document's file identifier: what a reader that takes either
/// version tells a FlatBuffers document from a JSON one by.
pub fn is_job_flatbuffer(bytes: &[u8]) -> bool {
    // The generated check asserts on a buffer shorter than an offset and an identifier.
    bytes.len() >= 8 && fb::job_buffer_has_identifier(bytes)
}

fn read_task(i: usize, task: fb::Task<'_>) -> Result<Task, JobError> {
    let role = match task.role() {
        fb::Role::Source => Role::Source,
        fb::Role::Kernel => Role::Kernel,
        fb::Role::Sink => Role::Sink,
        other => {
            return Err(JobError::field(
                format!("tasks[{i}].role"),
                format!("{} is not a role", other.0),
            ));
        }
    };
    let settings = match task.settings() {
        None => serde_json::Map::new(),
        Some(bytes) => flex::decode_map(bytes.bytes())
            .map_err(|reason| JobError::field(format!("tasks[{i}].settings"), reason))?,
    };
    Ok(Task {
        id: task.id().to_string(),
        role,
        kind: task.kind().to_string(),
        settings,
        inputs: strings(task.inputs()),
        outputs: strings(task.outputs()),
        barrier: task.barrier(),
        members: strings(task.members()),
        at: None,
    })
}

fn write_task<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    i: usize,
    task: &Task,
) -> Result<flatbuffers::WIPOffset<fb::Task<'b>>, JobError> {
    let bytes = flex::encode(&Value::Object(task.settings.clone()))
        .map_err(|reason| JobError::field(format!("tasks[{i}].settings"), reason))?;
    let id = b.create_string(&task.id);
    let kind = b.create_string(&task.kind);
    let settings = b.create_vector(&bytes);
    let inputs = string_vector(b, &task.inputs);
    let outputs = string_vector(b, &task.outputs);
    let members = string_vector(b, &task.members);
    Ok(fb::Task::create(
        b,
        &fb::TaskArgs {
            id: Some(id),
            role: match task.role {
                Role::Source => fb::Role::Source,
                Role::Kernel => fb::Role::Kernel,
                Role::Sink => fb::Role::Sink,
            },
            kind: Some(kind),
            settings: Some(settings),
            inputs: Some(inputs),
            outputs: Some(outputs),
            barrier: task.barrier,
            members: Some(members),
        },
    ))
}

fn endpoint(end: fb::Endpoint<'_>) -> PortRef {
    PortRef::new(end.task(), end.port())
}

fn flow(flow: fb::Flow) -> Option<Flow> {
    match flow {
        fb::Flow::Batches => Some(Flow::Batches),
        fb::Flow::Files => Some(Flow::Files),
        fb::Flow::Nothing => Some(Flow::Nothing),
        _ => None,
    }
}

fn write_edge<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    edge: &Edge,
) -> flatbuffers::WIPOffset<fb::Edge<'b>> {
    let from = write_endpoint(b, &edge.from);
    let to = write_endpoint(b, &edge.to);
    fb::Edge::create(
        b,
        &fb::EdgeArgs {
            from: Some(from),
            to: Some(to),
            flow: match edge.flow {
                Flow::Batches => fb::Flow::Batches,
                Flow::Files => fb::Flow::Files,
                Flow::Nothing => fb::Flow::Nothing,
            },
            barrier: edge.barrier,
        },
    )
}

fn write_endpoint<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    end: &PortRef,
) -> flatbuffers::WIPOffset<fb::Endpoint<'b>> {
    let task = b.create_string(&end.task);
    let port = b.create_string(&end.port);
    fb::Endpoint::create(
        b,
        &fb::EndpointArgs {
            task: Some(task),
            port: Some(port),
        },
    )
}

fn read_contract(i: usize, c: fb::Contract<'_>) -> Result<ContractEntry, JobError> {
    let document = flex::decode(c.document().bytes())
        .map_err(|reason| JobError::field(format!("contracts[{i}].document"), reason))?;
    let functions = match c.functions() {
        None => Vec::new(),
        Some(bytes) => match flex::decode(bytes.bytes()) {
            Ok(Value::Array(items)) => items,
            Ok(_) => {
                return Err(JobError::field(
                    format!("contracts[{i}].functions"),
                    "a list of function modules",
                ));
            }
            Err(reason) => {
                return Err(JobError::field(format!("contracts[{i}].functions"), reason));
            }
        },
    };
    Ok(ContractEntry {
        document,
        compiled: c.compiled().to_string(),
        functions,
        audiences: strings(c.audiences()),
    })
}

fn write_contract<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    i: usize,
    c: &ContractEntry,
) -> Result<flatbuffers::WIPOffset<fb::Contract<'b>>, JobError> {
    let document = flex::encode(&c.document)
        .map_err(|reason| JobError::field(format!("contracts[{i}].document"), reason))?;
    let functions = flex::encode(&Value::Array(c.functions.clone()))
        .map_err(|reason| JobError::field(format!("contracts[{i}].functions"), reason))?;
    let document = b.create_vector(&document);
    let compiled = b.create_string(&c.compiled);
    let functions = b.create_vector(&functions);
    let audiences = string_vector(b, &c.audiences);
    Ok(fb::Contract::create(
        b,
        &fb::ContractArgs {
            document: Some(document),
            compiled: Some(compiled),
            functions: Some(functions),
            audiences: Some(audiences),
        },
    ))
}

fn read_budget(b: fb::Budget<'_>) -> BudgetDoc {
    BudgetDoc {
        memory_bytes: b.memory_bytes(),
        cpu: b.cpu(),
        elastic: b.elastic().map(|e| ElasticDoc {
            memory_max_bytes: e.memory_max_bytes(),
            cpu_max: e.cpu_max(),
        }),
    }
}

fn write_budget<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    d: &BudgetDoc,
) -> flatbuffers::WIPOffset<fb::Budget<'b>> {
    let elastic = d.elastic.as_ref().map(|e| {
        fb::Elastic::create(
            b,
            &fb::ElasticArgs {
                memory_max_bytes: e.memory_max_bytes,
                cpu_max: e.cpu_max,
            },
        )
    });
    fb::Budget::create(
        b,
        &fb::BudgetArgs {
            memory_bytes: d.memory_bytes,
            cpu: d.cpu,
            elastic,
        },
    )
}

fn read_staging(s: fb::Staging<'_>) -> StagingDoc {
    StagingDoc {
        dir: s.dir().map(str::to_string),
        url: s.url().map(str::to_string),
        limit_bytes: s.limit_bytes(),
        durable: s.durable(),
    }
}

fn write_staging<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    d: &StagingDoc,
) -> flatbuffers::WIPOffset<fb::Staging<'b>> {
    let dir = opt_str(b, &d.dir);
    let url = opt_str(b, &d.url);
    fb::Staging::create(
        b,
        &fb::StagingArgs {
            dir,
            url,
            limit_bytes: d.limit_bytes,
            durable: d.durable,
        },
    )
}

fn read_object_store(o: fb::ObjectStore<'_>) -> ObjectStoreDoc {
    ObjectStoreDoc {
        s3: o.s3().map(|s| S3Doc {
            endpoint: s.endpoint().map(str::to_string),
            region: s.region().map(str::to_string),
            access_key_id: s.access_key_id().map(str::to_string),
            secret_access_key: s.secret_access_key().map(str::to_string),
            session_token: s.session_token().map(str::to_string),
            bucket: s.bucket().map(str::to_string),
        }),
        gcs: o.gcs().map(|g| GcsDoc {
            service_account_path: g.service_account_path().map(str::to_string),
            service_account_json: g.service_account_json().map(str::to_string),
            bucket: g.bucket().map(str::to_string),
        }),
        azure: o.azure().map(|a| AzureDoc {
            account: a.account().map(str::to_string),
            access_key: a.access_key().map(str::to_string),
            container: a.container().map(str::to_string),
        }),
        local_root: o.local_root().map(str::to_string),
        allow_http: o.allow_http(),
    }
}

fn write_object_store<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    d: &ObjectStoreDoc,
) -> flatbuffers::WIPOffset<fb::ObjectStore<'b>> {
    let s3 = d.s3.as_ref().map(|s| {
        let endpoint = opt_str(b, &s.endpoint);
        let region = opt_str(b, &s.region);
        let access_key_id = opt_str(b, &s.access_key_id);
        let secret_access_key = opt_str(b, &s.secret_access_key);
        let session_token = opt_str(b, &s.session_token);
        let bucket = opt_str(b, &s.bucket);
        fb::S3::create(
            b,
            &fb::S3Args {
                endpoint,
                region,
                access_key_id,
                secret_access_key,
                session_token,
                bucket,
            },
        )
    });
    let gcs = d.gcs.as_ref().map(|g| {
        let service_account_path = opt_str(b, &g.service_account_path);
        let service_account_json = opt_str(b, &g.service_account_json);
        let bucket = opt_str(b, &g.bucket);
        fb::Gcs::create(
            b,
            &fb::GcsArgs {
                service_account_path,
                service_account_json,
                bucket,
            },
        )
    });
    let azure = d.azure.as_ref().map(|a| {
        let account = opt_str(b, &a.account);
        let access_key = opt_str(b, &a.access_key);
        let container = opt_str(b, &a.container);
        fb::Azure::create(
            b,
            &fb::AzureArgs {
                account,
                access_key,
                container,
            },
        )
    });
    let local_root = opt_str(b, &d.local_root);
    fb::ObjectStore::create(
        b,
        &fb::ObjectStoreArgs {
            s3,
            gcs,
            azure,
            local_root,
            allow_http: d.allow_http,
        },
    )
}

fn guarantee(field: &str, g: fb::Guarantee) -> Result<Option<GuaranteeDoc>, JobError> {
    match g {
        fb::Guarantee::Unset => Ok(None),
        fb::Guarantee::Present => Ok(Some(GuaranteeDoc::Present)),
        fb::Guarantee::Absent => Ok(Some(GuaranteeDoc::Absent)),
        other => Err(JobError::field(
            format!("host_profile.{field}"),
            format!("{} is not a guarantee", other.0),
        )),
    }
}

fn read_host_profile(p: fb::HostProfile<'_>) -> Result<HostProfileDoc, JobError> {
    Ok(HostProfileDoc {
        huge_pages: guarantee("huge_pages", p.huge_pages())?,
        memlock: guarantee("memlock", p.memlock())?,
        io_uring: guarantee("io_uring", p.io_uring())?,
        direct_io: guarantee("direct_io", p.direct_io())?,
        gds: guarantee("gds", p.gds())?,
        rdma: guarantee("rdma", p.rdma())?,
        durable_staging: guarantee("durable_staging", p.durable_staging())?,
        staging_dir: p.staging_dir().map(str::to_string),
    })
}

fn write_host_profile<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    p: &HostProfileDoc,
) -> flatbuffers::WIPOffset<fb::HostProfile<'b>> {
    let g = |g: Option<GuaranteeDoc>| match g {
        None => fb::Guarantee::Unset,
        Some(GuaranteeDoc::Present) => fb::Guarantee::Present,
        Some(GuaranteeDoc::Absent) => fb::Guarantee::Absent,
    };
    let staging_dir = opt_str(b, &p.staging_dir);
    fb::HostProfile::create(
        b,
        &fb::HostProfileArgs {
            huge_pages: g(p.huge_pages),
            memlock: g(p.memlock),
            io_uring: g(p.io_uring),
            direct_io: g(p.direct_io),
            gds: g(p.gds),
            rdma: g(p.rdma),
            durable_staging: g(p.durable_staging),
            staging_dir,
        },
    )
}

fn opt_str<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    s: &Option<String>,
) -> Option<flatbuffers::WIPOffset<&'b str>> {
    s.as_deref().map(|s| b.create_string(s))
}

fn string_vector<'b>(
    b: &mut flatbuffers::FlatBufferBuilder<'b>,
    items: &[String],
) -> flatbuffers::WIPOffset<flatbuffers::Vector<'b, flatbuffers::ForwardsUOffset<&'b str>>> {
    let offsets: Vec<_> = items.iter().map(|s| b.create_string(s)).collect();
    b.create_vector(&offsets)
}

fn strings(v: Option<flatbuffers::Vector<'_, flatbuffers::ForwardsUOffset<&str>>>) -> Vec<String> {
    v.map(|v| v.iter().map(str::to_string).collect())
        .unwrap_or_default()
}
