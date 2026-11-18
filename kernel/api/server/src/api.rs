//! The operations (system design, sections 2A and 3B): routing from `moruna-api`'s operations
//! table, the token each call needs and the record it leaves (2C), each operation's answer, and
//! the error body every refusal carries.

use crate::{
    audit::{AUDIT_ANSWER_LIMIT, AuditLog},
    engine::EngineHost,
    http::{Authorization, Request},
    jobs::{Jobs, Stream},
    mcp::{self, MCP_PATH},
    token::{Grant, TokenKey, Verifier},
};
use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
use moruna_api::{
    Cloud, ErrorBody, ErrorCode, Executor, Health, HealthStatus, Method, Node, NodeHealth,
    OPERATIONS, Operation, Request as Body, Response,
};
use serde_json::Value;

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
    /// What becomes of the call, from the head alone: no body is read.
    Now(Outcome),
    /// The call is allowed and takes a body: read it, then [complete](Api::complete) the call.
    Body(Call),
}

/// What becomes of a call.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Its answer.
    Answer(Answer),
    /// Its answer comes later, under this ticket: the engine's verdict on a document.
    Wait(u64),
    /// A stream of a job's events, as Server-Sent Events.
    Stream(Stream),
}

/// A call admitted on its head, waiting for its body.
#[derive(Debug)]
pub(crate) enum Call {
    /// One MCP message, from a caller whose token granted this.
    Mcp(Grant),
    /// An operation that takes a job.
    Operation(&'static Operation, Option<Grant>),
}

/// What an operation is called with, beside its token.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Inputs<'a> {
    /// The run's ID in the path.
    pub id: Option<&'a str>,
    /// The record a stream of events, or the events' tool's page, comes after: the last one
    /// the reader has.
    pub from: Option<u64>,
    /// The body.
    pub body: &'a [u8],
}

/// How an answer that comes later is written when it comes.
#[derive(Debug)]
enum Shape {
    /// As it is.
    Plain,
    /// As the result of the MCP tool call with this JSON-RPC ID.
    Tool(Value),
}

/// A call whose answer comes later.
#[derive(Debug)]
struct Pending {
    operation: &'static Operation,
    grant: Option<Grant>,
    shape: Shape,
}

/// The operations' state: what the node says about itself, the keys its tokens are checked
/// against, the audit record, the jobs, and the engine's host.
pub(crate) struct Api {
    pub facts: NodeFacts,
    verifier: Verifier,
    audit: AuditLog,
    jobs: Jobs,
    host: Box<dyn EngineHost>,
    /// The calls whose answers come later, by ticket, and the answers that have come.
    pending: BTreeMap<u64, Pending>,
    ready: BTreeMap<u64, Answer>,
}

impl core::fmt::Debug for Api {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Api").field("facts", &self.facts).finish()
    }
}

/// The value of the query `from=<n>`, the only query an operation takes.
fn from_of(query: &str) -> Option<u64> {
    let digits = query.strip_prefix("from=")?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

impl Api {
    pub(crate) fn new(facts: NodeFacts, keys: Vec<TokenKey>, host: Box<dyn EngineHost>) -> Self {
        Self {
            facts,
            verifier: Verifier::new(keys),
            audit: AuditLog::default(),
            jobs: Jobs::default(),
            host,
            pending: BTreeMap::new(),
            ready: BTreeMap::new(),
        }
    }

    /// What the head of `request`, whose body is `body` bytes, decides at `now`, Unix seconds,
    /// before any of the body is read. `/mcp` is MCP's (2D); every other route comes from the
    /// operations table only: a path in no operation is `not_found`, and a known path with
    /// another method is `invalid`. An operation that needs a scope needs a valid token whose
    /// scope covers it (2C), checked first; `health` never reads the header. A body is read
    /// only for an operation that takes one, and a submit is `busy` on its head while a job
    /// runs. Each call of an operation but `health` is recorded.
    pub(crate) fn admit(&mut self, request: &Request, body: usize, now: u64) -> Admission {
        if request.path == MCP_PATH {
            return self.admit_mcp(request, now);
        }
        let mut known = false;
        let mut found = None;
        for operation in OPERATIONS {
            if let Some(id) = operation.matches(&request.path) {
                known = true;
                if operation.method.as_str() == request.method {
                    found = Some((operation, id));
                    break;
                }
            }
        }
        let Some((operation, id)) = found else {
            return Admission::Now(Outcome::Answer(if known {
                Answer::error(ErrorCode::Invalid, "this path takes another method")
            } else {
                Answer::error(ErrorCode::NotFound, "no operation has this path")
            }));
        };
        let refuse = |api: &mut Self, grant: Option<&Grant>, code, why| {
            Admission::Now(Outcome::Answer(
                api.refuse(operation, grant, code, why, now),
            ))
        };
        let grant = match operation.scope {
            None => None,
            Some(needed) => match self.verify(&request.authorization, now) {
                Err(why) => return refuse(self, None, ErrorCode::Unauthorized, why),
                Ok(grant) if !grant.covers(needed) => {
                    let why = "the token's scope does not cover this operation";
                    return refuse(self, Some(&grant), ErrorCode::Forbidden, why);
                }
                Ok(grant) => Some(grant),
            },
        };
        let from = match request.query.as_deref() {
            None => None,
            Some(query) if operation.query == ["from"] => match from_of(query) {
                Some(from) => Some(from),
                None => {
                    let why = "this operation's query is from=<a record's number>";
                    return refuse(self, grant.as_ref(), ErrorCode::Invalid, why);
                }
            },
            Some(_) => {
                let why = "this operation takes no query";
                return refuse(self, grant.as_ref(), ErrorCode::Invalid, why);
            }
        };
        match operation.request {
            Body::None if body > 0 => {
                let why = "this operation takes no body";
                refuse(self, grant.as_ref(), ErrorCode::Invalid, why)
            }
            Body::None => {
                let inputs = Inputs {
                    id,
                    from,
                    body: &[],
                };
                Admission::Now(self.invoke(operation, grant, inputs, Shape::Plain, now))
            }
            Body::Job if operation.response == Response::Submitted && self.jobs.busy() => {
                refuse(self, grant.as_ref(), ErrorCode::Busy, "a job is running")
            }
            Body::Job => Admission::Body(Call::Operation(operation, grant)),
        }
    }

    /// Completes `call`, admitted on its head, with its `body`.
    pub(crate) fn complete(&mut self, call: Call, body: &[u8], now: u64) -> Outcome {
        match call {
            Call::Mcp(grant) => self.mcp_message(&grant, body, now),
            Call::Operation(operation, grant) => {
                let inputs = Inputs {
                    id: None,
                    from: None,
                    body,
                };
                self.invoke(operation, grant, inputs, Shape::Plain, now)
            }
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

    /// Calls `operation` as an MCP tool whose JSON-RPC ID is `id`, for a caller whose token
    /// granted `grant`: `forbidden` when its scope does not cover the operation.
    pub(crate) fn call_tool(
        &mut self,
        operation: &'static Operation,
        grant: &Grant,
        inputs: Inputs<'_>,
        id: &Value,
        now: u64,
    ) -> Outcome {
        if let Some(needed) = operation.scope
            && !grant.covers(needed)
        {
            let why = "the token's scope does not cover this operation";
            let answer = self.refuse(operation, Some(grant), ErrorCode::Forbidden, why, now);
            return Outcome::Answer(answer);
        }
        self.invoke(
            operation,
            Some(grant.clone()),
            inputs,
            Shape::Tool(id.clone()),
            now,
        )
    }

    /// Runs `operation`, whose call is allowed, and records it: at once when its answer is
    /// known, and when the answer comes when it comes later.
    fn invoke(
        &mut self,
        operation: &'static Operation,
        grant: Option<Grant>,
        inputs: Inputs<'_>,
        shape: Shape,
        now: u64,
    ) -> Outcome {
        let id = inputs.id.unwrap_or_default();
        let answer = match operation.response {
            Response::Health => Answer::ok(serde_json::to_vec(&Health {
                status: HealthStatus::Ok,
            })),
            Response::Node => Answer::ok(serde_json::to_vec(&self.facts.document(now))),
            Response::Audit => {
                Answer::ok(serde_json::to_vec(&self.audit.audit(AUDIT_ANSWER_LIMIT)))
            }
            Response::Jobs => Answer::ok(serde_json::to_vec(&self.jobs.list())),
            Response::JobStatus if operation.method == Method::Post => {
                self.jobs.cancel(self.host.as_mut(), id)
            }
            Response::JobStatus => match self.jobs.status(id) {
                Some(status) => Answer::ok(serde_json::to_vec(&status)),
                None => Answer::error(ErrorCode::NotFound, "no job has this ID"),
            },
            Response::Report => self.jobs.report(id),
            // The tool answers a page of the stream, after its record `from` (item 3.7).
            Response::Events if matches!(shape, Shape::Tool(_)) => {
                match self.jobs.page(id, inputs.from.unwrap_or(0)) {
                    Some(page) => Answer::ok(Ok(page.into_bytes())),
                    None => Answer::error(ErrorCode::NotFound, "no job has this ID"),
                }
            }
            Response::Events => match self.jobs.stream(id, inputs.from) {
                Some(stream) => {
                    self.record(now, grant.as_ref(), operation, operation.status);
                    return Outcome::Stream(stream);
                }
                None => Answer::error(ErrorCode::NotFound, "no job has this ID"),
            },
            Response::Validation | Response::Submitted => {
                let waiting = if operation.response == Response::Validation {
                    self.jobs.validate(inputs.body)
                } else {
                    self.jobs.submit(self.host.as_mut(), inputs.body, now)
                };
                match waiting {
                    Ok(ticket) => {
                        self.pending.insert(
                            ticket,
                            Pending {
                                operation,
                                grant,
                                shape,
                            },
                        );
                        return Outcome::Wait(ticket);
                    }
                    Err(answer) => answer,
                }
            }
        };
        self.record(now, grant.as_ref(), operation, answer.status);
        Outcome::Answer(answer)
    }

    /// Moves the jobs as far as they go now: the engine's messages and the checks. An answer
    /// that has come is recorded, and kept for the call that waits for it.
    pub(crate) fn pump(&mut self, now: u64) {
        self.jobs.pump(self.host.as_mut(), now);
        for (ticket, answer) in self.jobs.resolved() {
            let Some(pending) = self.pending.remove(&ticket) else {
                continue;
            };
            self.record(
                now,
                pending.grant.as_ref(),
                pending.operation,
                answer.status,
            );
            let answer = match pending.shape {
                Shape::Plain => answer,
                Shape::Tool(id) => mcp::tool_answer(&id, &answer),
            };
            self.ready.insert(ticket, answer);
        }
    }

    /// The answer that came under `ticket`, once it has.
    pub(crate) fn ready(&mut self, ticket: u64) -> Option<Answer> {
        self.ready.remove(&ticket)
    }

    /// The call that waited under `ticket` has gone.
    pub(crate) fn abandon(&mut self, ticket: u64) {
        self.pending.remove(&ticket);
        self.ready.remove(&ticket);
        self.jobs.abandon(ticket);
    }

    /// Writes to `out` what `stream` can be sent now, and says whether it has ended.
    pub(crate) fn fill(&self, stream: &mut Stream, out: &mut Vec<u8>) -> bool {
        self.jobs.fill(stream, out)
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

    /// The answer to `request` with `body`, for a call whose answer does not come later.
    #[cfg(test)]
    pub(crate) fn answer(&mut self, request: &Request, body: &[u8], now: u64) -> Answer {
        let outcome = match self.admit(request, body.len(), now) {
            Admission::Now(outcome) => outcome,
            Admission::Body(call) => self.complete(call, body, now),
        };
        match outcome {
            Outcome::Answer(answer) => answer,
            other => panic!("not an answer: {other:?}"),
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
