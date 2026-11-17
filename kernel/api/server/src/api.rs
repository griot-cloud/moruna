//! The operations (system design, section 2A): routing from `moruna-api`'s operations table, the
//! token each call needs and the record it leaves (2C), each operation's answer, and the error
//! body every refusal carries.

use crate::{
    audit::{AUDIT_ANSWER_LIMIT, AuditLog},
    http::{Authorization, Request},
    mcp::MCP_PATH,
    token::{Grant, TokenKey, Verifier},
};
use alloc::{string::String, vec::Vec};
use moruna_api::{
    Cloud, ErrorBody, ErrorCode, Executor, Health, HealthStatus, Node, NodeHealth, OPERATIONS,
    Operation, Response,
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

/// What the head of a request decides.
#[derive(Debug)]
pub(crate) enum Admission {
    /// The answer, from the head alone: no body is read.
    Answer(Answer),
    /// The call is allowed and takes a body: read it, then [complete](Api::complete) the call.
    Body(Call),
}

/// A call admitted on its head, waiting for its body.
#[derive(Debug)]
pub(crate) enum Call {
    /// One MCP message, from a caller whose token granted this.
    Mcp(Grant),
}

/// The operations' state: what the node says about itself, the keys its tokens are checked
/// against, and the audit record.
#[derive(Debug)]
pub(crate) struct Api {
    pub facts: NodeFacts,
    verifier: Verifier,
    audit: AuditLog,
}

impl Api {
    pub(crate) fn new(facts: NodeFacts, keys: Vec<TokenKey>) -> Self {
        Self {
            facts,
            verifier: Verifier::new(keys),
            audit: AuditLog::default(),
        }
    }

    /// What the head of `request`, whose body is `body` bytes, decides at `now`, Unix seconds,
    /// before any of the body is read. `/mcp` is MCP's (2D); every other route comes from the
    /// operations table only: a path in no operation is `not_found`, and a known path with
    /// another method is `invalid`. An operation that needs a scope needs a valid token (2C),
    /// checked first; `health` never reads the header. Step 2's operations take no query and no
    /// body. Each call of an operation but `health` is recorded.
    pub(crate) fn admit(&mut self, request: &Request, body: usize, now: u64) -> Admission {
        if request.path == MCP_PATH {
            return self.admit_mcp(request, now);
        }
        let Some(operation) = OPERATIONS.iter().find(|o| o.path == request.path) else {
            return Admission::Answer(Answer::error(
                ErrorCode::NotFound,
                "no operation has this path",
            ));
        };
        if request.method != operation.method.as_str() {
            return Admission::Answer(Answer::error(
                ErrorCode::Invalid,
                "this path takes another method",
            ));
        }
        let grant = match operation.scope {
            None => None,
            Some(_) => match self.verify(&request.authorization, now) {
                Ok(grant) => Some(grant),
                Err(why) => {
                    let answer = self.refuse(operation, None, ErrorCode::Unauthorized, why, now);
                    return Admission::Answer(answer);
                }
            },
        };
        let answer = if request.query {
            let why = "this operation takes no query";
            self.refuse(operation, grant.as_ref(), ErrorCode::Invalid, why, now)
        } else if body > 0 {
            let why = "this operation takes no body";
            self.refuse(operation, grant.as_ref(), ErrorCode::Invalid, why, now)
        } else {
            self.call(operation, grant.as_ref(), now)
        };
        Admission::Answer(answer)
    }

    /// Completes `call`, admitted on its head, with its `body`.
    pub(crate) fn complete(&mut self, call: Call, body: &[u8], now: u64) -> Answer {
        match call {
            Call::Mcp(grant) => self.mcp_message(&grant, body, now),
        }
    }

    /// The answer to `request` with `body`: admitted, then completed when it takes a body.
    #[cfg(test)]
    pub(crate) fn answer(&mut self, request: &Request, body: &[u8], now: u64) -> Answer {
        match self.admit(request, body.len(), now) {
            Admission::Answer(answer) => answer,
            Admission::Body(call) => self.complete(call, body, now),
        }
    }

    /// What the token in `authorization` grants at `now`, or why it is refused.
    pub(crate) fn verify(
        &self,
        authorization: &Authorization,
        now: u64,
    ) -> Result<Grant, &'static str> {
        self.verifier
            .verify(authorization, &self.facts.instance, now)
    }

    /// Calls `operation`, whose request has been checked, for a caller whose token granted
    /// `grant`: `forbidden` when its scope does not cover the operation. The call is recorded.
    pub(crate) fn call(
        &mut self,
        operation: &'static Operation,
        grant: Option<&Grant>,
        now: u64,
    ) -> Answer {
        if let Some(needed) = operation.scope {
            match grant {
                None => {
                    let why = "the call carries no token";
                    return self.refuse(operation, grant, ErrorCode::Unauthorized, why, now);
                }
                Some(grant) if !grant.covers(needed) => {
                    let why = "the token's scope does not cover this operation";
                    return self.refuse(operation, Some(grant), ErrorCode::Forbidden, why, now);
                }
                Some(_) => {}
            }
        }
        let answer = self.run(operation, now);
        self.record(now, grant, operation, answer.status);
        answer
    }

    /// Refuses a call of `operation` with `code`, and records it.
    pub(crate) fn refuse(
        &mut self,
        operation: &'static Operation,
        grant: Option<&Grant>,
        code: ErrorCode,
        why: &str,
        now: u64,
    ) -> Answer {
        let answer = Answer::error(code, why);
        self.record(now, grant, operation, answer.status);
        answer
    }

    /// Records a call of `operation`; `health` leaves no record.
    fn record(
        &mut self,
        now: u64,
        grant: Option<&Grant>,
        operation: &'static Operation,
        status: u16,
    ) {
        if operation.response != Response::Health {
            self.audit.record(now, grant, operation.id, status);
        }
    }

    /// Runs `operation`, whose call is allowed.
    fn run(&mut self, operation: &Operation, now: u64) -> Answer {
        match operation.response {
            Response::Health => Answer::ok(serde_json::to_vec(&Health {
                status: HealthStatus::Ok,
            })),
            Response::Node => Answer::ok(serde_json::to_vec(&self.facts.document(now))),
            Response::Audit => {
                Answer::ok(serde_json::to_vec(&self.audit.audit(AUDIT_ANSWER_LIMIT)))
            }
        }
    }

    /// How many calls the audit record holds.
    #[cfg(test)]
    pub(crate) fn recorded(&self) -> usize {
        self.audit.len()
    }

    /// The audit record's records, newest first.
    #[cfg(test)]
    pub(crate) fn records(&self) -> Vec<moruna_api::AuditRecord> {
        self.audit.audit(usize::MAX).records
    }
}

#[cfg(test)]
mod tests;
