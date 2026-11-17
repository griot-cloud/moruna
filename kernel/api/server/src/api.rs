//! The operations (system design, section 2A): routing from `moruna-api`'s operations table, each
//! operation's answer, and the error body every refusal carries.

use crate::http::Request;
use alloc::{string::String, vec::Vec};
use moruna_api::{
    Audit, Cloud, ErrorBody, ErrorCode, Executor, Health, HealthStatus, Node, NodeHealth,
    OPERATIONS, Operation, Response,
};

/// What the node says about itself in the node document, supplied by the caller. The server
/// adds only the wall clock, `time`, when it answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeFacts {
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
    /// Whether the node is healthy.
    pub health: NodeHealth,
}

impl NodeFacts {
    /// The node document at `time`, Unix seconds.
    pub fn document(&self, time: u64) -> Node {
        Node {
            version: self.version.clone(),
            instance: self.instance.clone(),
            cloud: self.cloud,
            cores: self.cores,
            memory_bytes: self.memory_bytes,
            executors: self.executors.clone(),
            boot_ms: self.boot_ms,
            time,
            health: self.health,
        }
    }
}

/// An answer: its status, its JSON body (empty for none), and whether the connection closes
/// after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
    pub close: bool,
    /// The methods a `405` names in its `Allow` header.
    pub allow: Option<&'static str>,
}

impl Answer {
    /// A `200` with `written`, a value written as JSON, as its body.
    pub(crate) fn ok(written: serde_json::Result<Vec<u8>>) -> Self {
        match written {
            Ok(body) => Self {
                status: 200,
                body,
                close: false,
                allow: None,
            },
            Err(_) => Self::error(ErrorCode::Internal, "the answer could not be written"),
        }
    }

    /// The error `code`, with its status and 2A's error body.
    pub(crate) fn error(code: ErrorCode, message: &str) -> Self {
        Self {
            status: code.status(),
            body: error_body(code, message),
            close: false,
            allow: None,
        }
    }
}

/// 2A's error body: `{"error":{"code":"<code>","message":"<text>"}}`.
pub(crate) fn error_body(code: ErrorCode, message: &str) -> Vec<u8> {
    serde_json::to_vec(&ErrorBody::new(code, message)).unwrap_or_default()
}

/// The operations' state: what the node says about itself.
#[derive(Debug)]
pub(crate) struct Api {
    pub facts: NodeFacts,
}

impl Api {
    pub(crate) fn new(facts: NodeFacts) -> Self {
        Self { facts }
    }

    /// The answer to `request` at `now`, Unix seconds. The route comes from the operations
    /// table only: a path in no operation is `not_found`, and a known path with another method
    /// is `invalid`.
    pub(crate) fn answer(&mut self, request: &Request, now: u64) -> Answer {
        let Some(operation) = OPERATIONS.iter().find(|o| o.path == request.path) else {
            return Answer::error(ErrorCode::NotFound, "no operation has this path");
        };
        if request.method != operation.method.as_str() {
            return Answer::error(ErrorCode::Invalid, "this path takes another method");
        }
        if request.query {
            return Answer::error(ErrorCode::Invalid, "this operation takes no query");
        }
        if !request.body.is_empty() {
            return Answer::error(ErrorCode::Invalid, "this operation takes no body");
        }
        self.run(operation, now)
    }

    /// Runs `operation`, whose request has been checked.
    fn run(&mut self, operation: &Operation, now: u64) -> Answer {
        match operation.response {
            Response::Health => Answer::ok(serde_json::to_vec(&Health {
                status: HealthStatus::Ok,
            })),
            Response::Node => Answer::ok(serde_json::to_vec(&self.facts.document(now))),
            Response::Audit => Answer::ok(serde_json::to_vec(&Audit {
                records: Vec::new(),
            })),
        }
    }
}

#[cfg(test)]
mod tests;
