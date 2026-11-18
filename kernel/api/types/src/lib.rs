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
//! operations (3B) take a [`Job`], a run's ID in the path, or both, and registering a function
//! (3E) takes a [`FunctionRegistration`]. The crate is `no_std` with
//! `alloc`, because the node's `api` process uses it; the `schema` feature, for the host only,
//! adds the generator.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use core::fmt;
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
    /// The most memory a job may have, in bytes: `node-admission`'s account less its own and
    /// the check's (system design 3A). A job whose budget is larger is refused; 0 on a node
    /// that runs no jobs.
    pub job_memory_bytes: u64,
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
    /// When the node took the job, in Unix seconds: `submitted_ms` divided by 1,000.
    pub submitted: u64,
    /// When the node took the job, in Unix milliseconds on the node's wall clock.
    pub submitted_ms: u64,
    /// When it began to run, in Unix seconds: `started_ms` divided by 1,000; absent until it
    /// has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    /// When it began to run, in Unix milliseconds on the node's wall clock; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    /// When it ended, in Unix seconds: `ended_ms` divided by 1,000; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    /// When it ended, in Unix milliseconds on the node's wall clock; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
}

/// The answer to `GET /v1/jobs/{id}` and to `POST /v1/jobs/{id}/cancel`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct JobStatus {
    /// The run's ID.
    pub id: String,
    /// Where the job is in its life.
    pub state: JobState,
    /// When the node took the job, in Unix seconds: `submitted_ms` divided by 1,000.
    pub submitted: u64,
    /// When the node took the job, in Unix milliseconds on the node's wall clock.
    pub submitted_ms: u64,
    /// When it began to run, in Unix seconds: `started_ms` divided by 1,000; absent until it
    /// has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    /// When it began to run, in Unix milliseconds on the node's wall clock; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    /// When it ended, in Unix seconds: `ended_ms` divided by 1,000; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    /// When it ended, in Unix milliseconds on the node's wall clock; absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
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

/// What the node shows in place of an address's query (system design 3E).
pub const QUERY_HIDDEN: &str = "?…";

/// A function's address as the node shows it, everywhere but where `node-admission` fetches
/// (system design 3E: `GET /v1/functions`, events, diagnostics, console and log lines): its
/// scheme, host and path, with any query replaced by [`QUERY_HIDDEN`]. A query is a credential,
/// a SAS or a pre-signed URL's signature, kept only to fetch. Being neither scheme, host nor
/// path, credentials before the host's `@` are not shown either, nor a fragment, which is never
/// sent. An address with none of them is shown as it is.
pub fn address_shown(address: &str) -> String {
    let (scheme, rest) = match address.find("://") {
        Some(at) => address.split_at(at + "://".len()),
        None => ("", address),
    };
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (path, after) = tail.split_at(tail.find(['?', '#']).unwrap_or(tail.len()));
    let query = if after.starts_with('?') {
        QUERY_HIDDEN
    } else {
        ""
    };
    format!("{scheme}{host}{path}{query}")
}

/// A function's own memory as it was measured (system design 3A): what one instance holds
/// outside its batch, as a line over the batch's bytes, and what it gives per byte it takes,
/// with the units and the range of input the line was fitted from. `moruna check`'s warm-up
/// fits it, a registration may carry it, and the node's own readings replace it. A job that
/// uses the function is sized from it; the function's account still bounds what it may hold.
/// Unknown fields are refused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Measured {
    /// Bytes held whatever the batch: the line's value at no input.
    pub fixed_bytes: f64,
    /// Bytes held beside each byte of the input batch: the line's slope.
    pub per_input_byte: f64,
    /// Bytes out per byte in, the median over the units; absent when no unit said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_input_byte: Option<f64>,
    /// The spread of the units about the line: the root mean square of each unit's departure,
    /// its measured bytes over the line's value there, less one. The margin a job is sized
    /// with is 1.645 times it.
    pub spread: f64,
    /// Units the line was fitted from.
    pub units: u64,
    /// The smallest input among them, in bytes.
    pub input_bytes_min: u64,
    /// The largest.
    pub input_bytes_max: u64,
}

impl Measured {
    /// Why this is not a measurement of memory, when it is not one: an amount or a rate that
    /// is negative or not finite, no units, or a range of input that is not one.
    pub fn refusal(&self) -> Option<&'static str> {
        let amounts = [
            self.fixed_bytes,
            self.per_input_byte,
            self.spread,
            self.output_per_input_byte.unwrap_or(0.0),
        ];
        if amounts.iter().any(|a| !a.is_finite() || *a < 0.0) {
            return Some(
                "fixed_bytes, per_input_byte, output_per_input_byte and spread are finite \
                 amounts of zero or more",
            );
        }
        if self.units == 0 {
            return Some("units is how many measured units the line was fitted from, at least 1");
        }
        if self.input_bytes_min == 0 || self.input_bytes_min > self.input_bytes_max {
            return Some(
                "input_bytes_min and input_bytes_max are the range of input the units covered, \
                 from 1 byte up",
            );
        }
        None
    }
}

/// Whose a function's measured memory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MeasuredBy {
    /// The registration's: what its client measured before registering.
    Registration,
    /// The node's own reading, from jobs that ran the function here. It replaces a
    /// registration's.
    Node,
}

/// A function image to register, the body of `POST /v1/functions` (system design 3E). Unknown
/// fields are refused.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FunctionRegistration {
    /// The name a job names the function by (a version 1 `kernels` entry
    /// `{"kind": "function", "function": "<name>"}`, a version 2 task of kind `function`): 1 to 64
    /// characters, a lowercase letter first, then lowercase letters, digits, `_`, `-` and `.`.
    pub name: String,
    /// Where the image file is fetched from at job start: an `https://` URL. An object store's
    /// private object is given as a pre-signed or SAS URL.
    pub address: String,
    /// The SHA-256 of the image file, as 64 lowercase hexadecimal digits.
    pub sha256: String,
    /// An Ed25519 signature over the 32 bytes of that SHA-256, by one of node config's
    /// `functions.keys`, in standard base64.
    pub signature: String,
    /// The function's memory as its client measured it (`moruna check`'s warm-up), when it did
    /// (system design 3A). It is outside the signature: it says how the function behaves, not
    /// what it is, and the node's own readings replace it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<Measured>,
}

impl fmt::Debug for FunctionRegistration {
    /// The registration, its address as the node shows it: never its query.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionRegistration")
            .field("name", &self.name)
            .field("address", &address_shown(&self.address))
            .field("sha256", &self.sha256)
            .field("signature", &self.signature)
            .field("measured", &self.measured)
            .finish()
    }
}

/// A registered function, as `GET /v1/functions` lists it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Function {
    /// The name a job names it by.
    pub name: String,
    /// Where its image is fetched from, as the node shows it: its scheme, host and path, with
    /// any query (a SAS or a pre-signed URL's signature) replaced by `?…`.
    pub address: String,
    /// The SHA-256 of its image, as 64 lowercase hexadecimal digits.
    pub sha256: String,
    /// The first 16 hex characters of the SHA-256 of the public bytes of the key whose signature
    /// verified, as the audit record names a token key.
    pub key: String,
    /// When it was registered, in Unix seconds.
    pub registered: u64,
    /// Its memory as it was measured, when it has been (system design 3A): what a job that
    /// uses it is sized from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<Measured>,
    /// Whose that measurement is; absent with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_by: Option<MeasuredBy>,
}

impl Function {
    /// The function as the node shows it: its address as [`address_shown`] gives it.
    pub fn shown(&self) -> Function {
        Function {
            address: address_shown(&self.address),
            ..self.clone()
        }
    }
}

impl fmt::Debug for Function {
    /// The function, its address as the node shows it: never its query.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Function")
            .field("name", &self.name)
            .field("address", &address_shown(&self.address))
            .field("sha256", &self.sha256)
            .field("key", &self.key)
            .field("registered", &self.registered)
            .field("measured", &self.measured)
            .field("measured_by", &self.measured_by)
            .finish()
    }
}

/// The answer to `GET /v1/functions`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
pub struct Functions {
    /// Every registered function, by name.
    pub functions: Vec<Function>,
}

/// The most events one answer of the MCP tool `moruna_jobs_events` holds (item 3.7): MCP stays
/// stateless, so the tool answers a page of what `GET /v1/jobs/{id}/events` streams, and an
/// agent polls.
pub const EVENTS_PAGE: usize = 1_000;

/// The most bytes of events' JSON (their `data`) one page of `moruna_jobs_events` holds; a page
/// always holds one event, however large.
pub const EVENTS_PAGE_BYTES: usize = 1024 * 1024;

/// What the MCP tool `moruna_jobs_events` does, for the MCP tool list.
pub const EVENTS_TOOL_SUMMARY: &str = "A job's events, a page at a time: up to 1,000, and 1 MiB of them, after the record `from` (0, or absent, for the first), each event and log record with its number, then, once every record has been given and the job has ended, its report and its exit. `next` is the `from` of the next call: poll with it until the exit.";

/// What `from` is, to the stream of a job's events and to its MCP tool: the number of the last
/// record already read, as Server-Sent Events' `Last-Event-ID` is.
pub const FROM: &str = "The number of the last record already read, as Server-Sent Events' Last-Event-ID: the records after it are sent. 0, or absent, for the first.";

/// The answer of the MCP tool `moruna_jobs_events`: a page of a job's events, as a stream of
/// `GET /v1/jobs/{id}/events` after the record `from` would send them at once, and where the
/// next page begins.
// `D` is an event's JSON: any JSON object, in whatever form the program holds JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "schema"), derive(schemars::JsonSchema))]
#[cfg_attr(any(test, feature = "schema"), schemars(rename = "JobEvents"))]
pub struct JobEvents<D> {
    /// At most 1,000, and at most 1 MiB of JSON unless the first alone is larger, in the order
    /// the stream sends them: the job's event and log records after `from`, then, once they
    /// have all been given, a note of the engine's skipped lines if it had any, and, once the
    /// job has ended, its report and its exit, which come together.
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
    /// A [`FunctionRegistration`].
    Function,
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
    /// [`Function`].
    Function,
    /// [`Functions`].
    Functions,
}

/// The name of a run's ID in a path: `{id}`.
pub const ID: &str = "{id}";

/// One operation of the API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation {
    /// The operation's ID; its MCP tool is `moruna_<id>`.
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
    /// The name of the operation's MCP tool: `moruna_<id>`.
    pub fn tool_name(&self) -> String {
        format!("moruna_{}", self.id)
    }

    /// What the operation's MCP tool does: the operation's summary, and
    /// [`EVENTS_TOOL_SUMMARY`] for the stream of events, whose tool answers a page of it.
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
        summary: "A job's events as Server-Sent Events: each event and log record as its JSON line, heartbeats, then the report and the exit. `from` is the last record already read, as Server-Sent Events' Last-Event-ID: the node's buffer is replayed after it.",
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
    Operation {
        id: "functions_register",
        method: Method::Post,
        path: "/v1/functions",
        scope: Some(Scope::Admin),
        request: Request::Function,
        query: &[],
        response: Response::Function,
        status: 201,
        summary: "Register a function image by name: its https:// address, its SHA-256, and an Ed25519 signature over that SHA-256 by one of node config's functions.keys. Nothing is fetched until a job names it; registering a name again replaces it.",
    },
    Operation {
        id: "functions_list",
        method: Method::Get,
        path: "/v1/functions",
        scope: Some(Scope::Observe),
        request: Request::None,
        query: &[],
        response: Response::Functions,
        status: 200,
        summary: "Every registered function: its name, its address with any query shown as ?…, SHA-256, the key that signed it and when it was registered.",
    },
];

#[cfg(test)]
mod tests;
