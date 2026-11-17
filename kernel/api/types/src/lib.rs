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
//! Paths start `/v1` and bodies are JSON. Step 2's operations take no request body. The crate is
//! `no_std` with `alloc`, because the node's `api` process uses it; the `schema` feature, for the
//! host only, adds the generator.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};
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

/// An HTTP method the API uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
}

impl Method {
    /// The method as HTTP writes it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
        }
    }
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
}

/// One operation of the API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation {
    /// The operation's ID; its MCP tool is `moruna_<id>`.
    pub id: &'static str,
    /// Its HTTP method.
    pub method: Method,
    /// Its path.
    pub path: &'static str,
    /// The scope it needs, or `None` when it needs no token.
    pub scope: Option<Scope>,
    /// What it answers with on success.
    pub response: Response,
    /// One sentence on what it does, for the OpenAPI document and the MCP tool list.
    pub summary: &'static str,
}

impl Operation {
    /// The name of the operation's MCP tool.
    pub fn tool_name(&self) -> String {
        format!("moruna_{}", self.id)
    }
}

/// Every operation of the API. The generators and the server read this list.
pub const OPERATIONS: &[Operation] = &[
    Operation {
        id: "health",
        method: Method::Get,
        path: "/v1/health",
        scope: None,
        response: Response::Health,
        summary: "Whether the node is up. The only call that needs no token.",
    },
    Operation {
        id: "node",
        method: Method::Get,
        path: "/v1/node",
        scope: Some(Scope::Observe),
        response: Response::Node,
        summary: "The node document: version, instance, cloud, cores, memory, executors, boot time, clock and health.",
    },
    Operation {
        id: "node_audit",
        method: Method::Get,
        path: "/v1/node/audit",
        scope: Some(Scope::Admin),
        response: Response::Audit,
        summary: "The most recent calls to the node, newest first.",
    },
];

#[cfg(test)]
mod tests;
