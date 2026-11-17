//! The node API's requests, responses, errors and token claims, as Rust types (system design,
//! section 2A, and the claims of 2C).
//!
//! Every operation is listed once, in [`OPERATIONS`]. The OpenAPI document
//! (`kernel/api/openapi.json`) and the MCP tool list (`kernel/api/mcp-tools.json`) are generated
//! from these types and that table, and checked in. From `kernel/`, one command regenerates both:
//!
//! ```text
//! cargo run -p moruna-api --features schema
//! ```
//!
//! A test fails when either file differs from what the types and the table generate.
//!
//! Paths start `/v1` and bodies are JSON. Step 2's operations take no request body; step 3's job
//! operations (3B) take a [`Job`], a run's ID in the path, or both. The crate is `no_std` with
//! `alloc`, because the node's `api` process uses it; the `schema` feature, for the host only,
//! adds the generator.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use serde::{Deserialize, Serialize};

#[cfg(any(test, feature = "schema"))]
pub mod generate;

/// The answer to `GET /v1/health`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Health {
    /// Always `ok`: a node that answers is up.
    pub status: HealthStatus,
}

/// What `GET /v1/health` reports: only that the node is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// The node is up.
    Ok,
}

/// The node document, the answer to `GET /v1/node`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Node {
    /// The version of Moruna the node runs.
    pub version: String,
    /// The cloud's ID for this machine.
    pub instance: String,
    /// The cloud the node found at boot.
    pub cloud: Cloud,
    /// The node's CPU cores.
    pub cores: u32,
    /// The node's memory, in bytes.
    pub memory_bytes: u64,
    /// What the node can run work on.
    pub executors: Vec<Executor>,
    /// Milliseconds from the core's first instruction to ready.
    pub boot_ms: u64,
    /// The node's wall clock, in Unix seconds.
    pub time: u64,
    /// Whether the node is healthy.
    pub health: NodeHealth,
}

/// The cloud a node runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Cloud {
    /// OpenStack, found by its instance data.
    OpenStack,
    /// Azure, found by its instance data.
    Azure,
    /// No cloud: a development node, configured from a file in its image.
    None,
}

/// One kind of executor and how many units of work it runs at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Executor {
    /// The kind of executor.
    pub kind: ExecutorKind,
    /// How many units of work it runs at once.
    pub slots: u32,
}

/// A kind of executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    /// The node's CPU cores.
    Cpu,
}

/// A node's health.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NodeHealth {
    /// Healthy.
    Ok,
    /// Running, with a fault.
    Degraded,
}

/// The answer to `GET /v1/node/audit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Audit {
    /// The most recent calls, newest first.
    pub records: Vec<AuditRecord>,
}

/// The record one call leaves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct AuditRecord {
    /// When the call was made, in Unix seconds.
    pub time: u64,
    /// The first 16 hex characters of the SHA-256 of the token key's public bytes, or empty when
    /// the call carried no valid token.
    pub key: String,
    /// The token's scope.
    pub scope: Vec<Scope>,
    /// The operation called.
    pub operation: String,
    /// The HTTP status of the answer.
    pub status: u16,
}

/// What a token allows. `admin` covers `submit`, and `submit` covers `observe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Read the node.
    Observe,
    /// Submit work, and everything `observe` allows.
    Submit,
    /// Administer the node, and everything `submit` allows.
    Admin,
}

impl Scope {
    /// Whether a token with this scope may call an operation that needs `needed`.
    pub const fn covers(self, needed: Scope) -> bool {
        matches!(
            (self, needed),
            (Scope::Admin, _)
                | (Scope::Submit, Scope::Submit | Scope::Observe)
                | (Scope::Observe, Scope::Observe)
        )
    }

    /// The scope's JSON name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Scope::Observe => "observe",
            Scope::Submit => "submit",
            Scope::Admin => "admin",
        }
    }
}

/// The body of every error answer: `{"error":{"code":"<code>","message":"<text>"}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct ErrorBody {
    /// What went wrong.
    pub error: ErrorDetail,
}

impl ErrorBody {
    /// An error body with this code and message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                code,
                message: message.into(),
            },
        }
    }
}

/// What went wrong, inside an error body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct ErrorDetail {
    /// The kind of error; the answer's HTTP status matches it.
    pub code: ErrorCode,
    /// A sentence for a person to read.
    pub message: String,
}

/// The kinds of error, each with its HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 400: the request is malformed or over a limit.
    Invalid,
    /// 401: the call carries no valid token.
    Unauthorized,
    /// 403: the token's scope does not cover the operation.
    Forbidden,
    /// 404: no such path or object.
    NotFound,
    /// 409: the request conflicts with the node's state.
    Conflict,
    /// 503: the node cannot take the request now.
    Busy,
    /// 500: the node failed.
    Internal,
}

impl ErrorCode {
    /// The HTTP status this code is answered with.
    pub const fn status(self) -> u16 {
        match self {
            ErrorCode::Invalid => 400,
            ErrorCode::Unauthorized => 401,
            ErrorCode::Forbidden => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict => 409,
            ErrorCode::Busy => 503,
            ErrorCode::Internal => 500,
        }
    }
}

/// A token's claims (system design, section 2C). Unknown fields are refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    /// The instance ID of the node the token is for.
    pub aud: String,
    /// When the token expires, in Unix seconds.
    pub exp: u64,
    /// What the token allows.
    pub scope: Vec<Scope>,
}

/// A job as a client hands it to the node: the job document, as version 1 JSON (`v1`) or as
/// version 2's FlatBuffers in base64 (`v2`), exactly one of the two, and the secrets its
/// object-store settings refer to by name (`{"secret": "<name>"}`).
// `D` is the version 1 document's type: any JSON object, in whatever form the program holds JSON
// (system design, section 3B).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[cfg_attr(any(test, feature = "schema"), schemars(rename = "Job"))]
#[serde(deny_unknown_fields)]
pub struct Job<D> {
    /// The job document, version 1, as JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v1: Option<D>,
    /// The job document, version 2: its `MJB2` FlatBuffer in standard base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v2: Option<String>,
    /// The job's secrets (object-store credentials), by name: at most 32. They reach the engine
    /// beside the document, never in it, and the node shows none of them in any answer, event,
    /// log or report of this job.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
}

impl<D> Job<D> {
    /// A job whose document is version 1 JSON.
    pub fn v1(document: D) -> Self {
        Self {
            v1: Some(document),
            v2: None,
            secrets: BTreeMap::new(),
        }
    }

    /// A job whose document is version 2, in standard base64.
    pub fn v2(base64: impl Into<String>) -> Self {
        Self {
            v1: None,
            v2: Some(base64.into()),
            secrets: BTreeMap::new(),
        }
    }

    /// The job with the secret `value` under `name`.
    pub fn with_secret(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets.insert(name.into(), value.into());
        self
    }
}

/// The answer to `POST /v1/jobs/validate`: whether the engine takes the document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Validation {
    /// Whether the document is sound.
    pub ok: bool,
    /// Why not, when it is not: `moruna-job`'s refusals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<Refusal>,
}

/// One refusal of a job document, with the field it is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Refusal {
    /// Where the problem is: `tasks[0].kind`, `budget.memory_bytes`, or `spec` for the document
    /// as a whole.
    pub field: String,
    /// One sentence that says what to change.
    pub message: String,
}

/// The answer to `POST /v1/jobs`: the job the node has taken.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Submitted {
    /// The run's ID.
    pub id: String,
}

/// Where a job is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    /// The engine has taken the document and has not yet said how the run goes.
    Starting,
    /// The engine runs it.
    Running,
    /// It ended well: exit code 0.
    Completed,
    /// It ended with an error.
    Failed,
    /// It was cancelled: exit code 130.
    Cancelled,
}

impl JobState {
    /// Whether the job has ended.
    pub const fn ended(self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Failed | JobState::Cancelled
        )
    }
}

/// The answer to `GET /v1/jobs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Jobs {
    /// The last 100 jobs, newest first.
    pub jobs: Vec<JobSummary>,
}

/// One job in the list: its ID, its state and its times.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct JobSummary {
    /// The run's ID.
    pub id: String,
    /// Where the job is in its life.
    pub state: JobState,
    /// When the node took the job, in Unix seconds.
    pub submitted: u64,
    /// When it began to run, in Unix seconds; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    /// When it ended, in Unix seconds; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
}

/// The answer to `GET /v1/jobs/{id}` and to `POST /v1/jobs/{id}/cancel`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct JobStatus {
    /// The run's ID.
    pub id: String,
    /// Where the job is in its life.
    pub state: JobState,
    /// When the node took the job, in Unix seconds.
    pub submitted: u64,
    /// When it began to run, in Unix seconds; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    /// When it ended, in Unix seconds; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    /// The engine's latest heartbeat; absent until it has sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<Heartbeat>,
    /// The engine's exit code; absent until the job has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// The fields of the engine's heartbeat (`moruna-hosted-engine.md`, section 4.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Heartbeat {
    /// Milliseconds since the engine began the run.
    pub t_ms: u64,
    /// The sink's commit watermark.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_seq: Option<u64>,
    /// Rows the last stage has produced so far.
    pub rows_out: u64,
    /// Bytes the last stage has produced so far.
    pub bytes_out: u64,
    /// Workers allowed to take tasks.
    pub active_workers: u16,
    /// The memory ceiling in force, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ceiling_bytes: Option<u64>,
    /// The engine's anonymous memory, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anon_bytes: Option<u64>,
    /// What holds the run back, as the engine's controller last judged it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottleneck: Option<String>,
    /// Bytes staged on disk.
    pub staging_bytes: u64,
}

/// The name of the MCP tool that follows a job's events (item 3.7): MCP stays stateless, so the
/// tool answers a page of what `GET /v1/jobs/{id}/events` streams, and an agent polls.
pub const EVENTS_TOOL: &str = "moruna_job_events";

/// The most records one answer of [`EVENTS_TOOL`] holds.
pub const EVENTS_PAGE: usize = 1_000;

/// What [`EVENTS_TOOL`] does, for the MCP tool list.
pub const EVENTS_TOOL_SUMMARY: &str = "A job's events, a page at a time: up to 1,000 after the record `from` (0, or absent, for the first), each event and log record with its number, then, once every record has been given and the job has ended, its report and its exit. `next` is the `from` of the next call: poll with it until the exit.";

/// The answer of the MCP tool `moruna_job_events`: a page of a job's events, as a stream of
/// `GET /v1/jobs/{id}/events` from the record after `from` would send them at once, and where
/// the next page begins.
// `D` is an event's JSON: any JSON object, in whatever form the program holds JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[cfg_attr(any(test, feature = "schema"), schemars(rename = "JobEvents"))]
pub struct JobEvents<D> {
    /// At most 1,000, in the order the stream sends them: the job's event and log records after
    /// `from`, then, once they have all been given, a note of the engine's skipped lines if it
    /// had any, and, once the job has ended, its report and its exit.
    pub records: Vec<JobEvent<D>>,
    /// The `from` of the next call: the number of the last record in this page, or this call's
    /// `from` when the page holds none.
    pub next: u64,
}

/// One event of a job's stream, as `GET /v1/jobs/{id}/events` sends it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[cfg_attr(any(test, feature = "schema"), schemars(rename = "JobEvent"))]
pub struct JobEvent<D> {
    /// The record's number in the job's stream, from 1, for an `event`; absent for the others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// What it is.
    pub event: JobEventName,
    /// Its JSON: the record's line as `moruna_event` renders it, or the engine's message.
    pub data: D,
}

/// The name of an event in a job's stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum JobEventName {
    /// An event or log record of the engine's.
    Event,
    /// How many of the engine's lines were too long to read, and were skipped.
    Note,
    /// The engine's `report` message: the run's report and its plan.
    Report,
    /// The engine's `exit` message: its exit code and diagnostic. Nothing follows it.
    Exit,
}

/// An HTTP method the API uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
}

impl Method {
    /// The method as HTTP writes it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// The body an operation takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// None.
    None,
    /// A [`Job`].
    Job,
}

/// The type an operation answers with on success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    /// [`Health`].
    Health,
    /// [`Node`].
    Node,
    /// [`Audit`].
    Audit,
    /// [`Validation`].
    Validation,
    /// [`Submitted`].
    Submitted,
    /// [`Jobs`].
    Jobs,
    /// [`JobStatus`].
    JobStatus,
    /// Server-Sent Events (`text/event-stream`): the job's event and log records, heartbeats,
    /// then its report and exit, each event's data one line of JSON.
    Events,
    /// The engine's `RunReport`: a JSON object, as the engine wrote it.
    Report,
}

/// The name of a run's ID in a path: `{id}`.
pub const ID: &str = "{id}";

/// One operation of the API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation {
    /// The operation's ID. Its MCP tool is `moruna_<id>`, but for the stream of events
    /// ([`Operation::tool_name`]).
    pub id: &'static str,
    /// Its HTTP method.
    pub method: Method,
    /// Its path. A segment that is [`ID`] stands for a run's ID.
    pub path: &'static str,
    /// The scope it needs, or `None` when it needs no token.
    pub scope: Option<Scope>,
    /// The body it takes.
    pub request: Request,
    /// The query parameters it takes, each an optional unsigned integer.
    pub query: &'static [&'static str],
    /// What it answers with on success.
    pub response: Response,
    /// The HTTP status of its answer on success.
    pub status: u16,
    /// One sentence on what it does, for the OpenAPI document and the MCP tool list.
    pub summary: &'static str,
}

impl Operation {
    /// The name of the operation's MCP tool: `moruna_<id>`, and [`EVENTS_TOOL`] for the stream
    /// of events, whose tool answers a page of it.
    pub fn tool_name(&self) -> String {
        match self.response {
            Response::Events => String::from(EVENTS_TOOL),
            _ => format!("moruna_{}", self.id),
        }
    }

    /// What the operation's MCP tool does: the operation's summary, and
    /// [`EVENTS_TOOL_SUMMARY`] for the stream of events.
    pub const fn tool_summary(&self) -> &'static str {
        match self.response {
            Response::Events => EVENTS_TOOL_SUMMARY,
            _ => self.summary,
        }
    }

    /// Whether the operation's path carries a run's ID.
    pub fn takes_id(&self) -> bool {
        self.path.split('/').any(|segment| segment == ID)
    }

    /// Whether `path` is this operation's: `Some(None)` when it is, `Some(Some(id))` when it is
    /// and carries the run's ID, `None` when it is another path.
    pub fn matches<'p>(&self, path: &'p str) -> Option<Option<&'p str>> {
        let mut id = None;
        let mut given = path.split('/');
        for segment in self.path.split('/') {
            let found = given.next()?;
            if segment == ID {
                if found.is_empty() {
                    return None;
                }
                id = Some(found);
            } else if segment != found {
                return None;
            }
        }
        given.next().is_none().then_some(id)
    }

    /// The operation's path for the run `id`; its path as it is when it carries no ID.
    pub fn path_for(&self, id: &str) -> String {
        self.path.replace(ID, id)
    }
}

/// Every operation of the API. The generators and the server read this list.
pub const OPERATIONS: &[Operation] = &[
    Operation {
        id: "health",
        method: Method::Get,
        path: "/v1/health",
        scope: None,
        request: Request::None,
        query: &[],
        response: Response::Health,
        status: 200,
        summary: "Whether the node is up. The only call that needs no token.",
    },
    Operation {
        id: "node",
        method: Method::Get,
        path: "/v1/node",
        scope: Some(Scope::Observe),
        request: Request::None,
        query: &[],
        response: Response::Node,
        status: 200,
        summary: "The node document: version, instance, cloud, cores, memory, executors, boot time, clock and health.",
    },
    Operation {
        id: "node_audit",
        method: Method::Get,
        path: "/v1/node/audit",
        scope: Some(Scope::Admin),
        request: Request::None,
        query: &[],
        response: Response::Audit,
        status: 200,
        summary: "The most recent calls to the node, newest first.",
    },
    Operation {
        id: "jobs_validate",
        method: Method::Post,
        path: "/v1/jobs/validate",
        scope: Some(Scope::Submit),
        request: Request::Job,
        query: &[],
        response: Response::Validation,
        status: 200,
        summary: "Whether the engine takes a job document, and its refusals when it does not. Nothing runs.",
    },
    Operation {
        id: "jobs_submit",
        method: Method::Post,
        path: "/v1/jobs",
        scope: Some(Scope::Submit),
        request: Request::Job,
        query: &[],
        response: Response::Submitted,
        status: 201,
        summary: "Run a job in a fresh engine and answer its run ID. Busy while a job runs; invalid with the engine's refusals.",
    },
    Operation {
        id: "jobs_list",
        method: Method::Get,
        path: "/v1/jobs",
        scope: Some(Scope::Observe),
        request: Request::None,
        query: &[],
        response: Response::Jobs,
        status: 200,
        summary: "The last 100 jobs, newest first: each one's ID, state and times.",
    },
    Operation {
        id: "jobs_get",
        method: Method::Get,
        path: "/v1/jobs/{id}",
        scope: Some(Scope::Observe),
        request: Request::None,
        query: &[],
        response: Response::JobStatus,
        status: 200,
        summary: "One job: its state, its times, the engine's latest heartbeat and its exit code.",
    },
    Operation {
        id: "jobs_cancel",
        method: Method::Post,
        path: "/v1/jobs/{id}/cancel",
        scope: Some(Scope::Submit),
        request: Request::None,
        query: &[],
        response: Response::JobStatus,
        status: 200,
        summary: "Tell the engine to cancel a job, and answer the job as it stands. Conflict if the job has ended.",
    },
    Operation {
        id: "jobs_events",
        method: Method::Get,
        path: "/v1/jobs/{id}/events",
        scope: Some(Scope::Observe),
        request: Request::None,
        query: &["from"],
        response: Response::Events,
        status: 200,
        summary: "A job's events as Server-Sent Events: each event and log record as its JSON line, heartbeats, then the report and the exit. `from` replays from that record of the node's buffer.",
    },
    Operation {
        id: "jobs_report",
        method: Method::Get,
        path: "/v1/jobs/{id}/report",
        scope: Some(Scope::Observe),
        request: Request::None,
        query: &[],
        response: Response::Report,
        status: 200,
        summary: "A job's report, the engine's RunReport, once the job has ended.",
    },
];

#[cfg(test)]
mod tests;
