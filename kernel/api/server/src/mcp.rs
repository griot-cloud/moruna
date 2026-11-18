//! MCP on the API's port (system design, section 2D): `POST /mcp`, MCP's Streamable HTTP,
//! stateless. Each request carries one JSON-RPC 2.0 message and is answered with one JSON
//! response; a notification or a response is answered `202` with no body. No server-sent stream
//! and no session ID. The tools are the operations, one each, listed exactly as
//! `kernel/api/mcp-tools.json` (generated from `moruna-api`) lists them. The stream of a job's
//! events is the tool `moruna_jobs_events` (item 3.7), which answers a page of it at a time:
//! an agent polls, with the `next` each page gives.

use crate::{
    api::{Admission, Answer, Api, Call, Inputs, Outcome},
    http::Request,
    token::Grant,
};
use alloc::{borrow::Cow, string::String, vec::Vec};
use moruna_api::{ErrorCode, OPERATIONS, Request as Body, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json, value::RawValue};

/// The path MCP is served on.
pub(crate) const MCP_PATH: &str = "/mcp";
/// The MCP protocol version `initialize` answers.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// The tool list `tools/list` answers, as generated from `moruna-api` and checked in.
const TOOLS: &str = include_str!("../../mcp-tools.json");

/// JSON-RPC 2.0's error codes.
const PARSE_ERROR: i64 = -32_700;
const INVALID_REQUEST: i64 = -32_600;
const METHOD_NOT_FOUND: i64 = -32_601;
const INVALID_PARAMS: i64 = -32_602;

/// A JSON-RPC answer: `result` or an error with this code and message, for the request `id`.
fn reply(id: &Value, outcome: Result<Value, (i64, &str)>) -> Answer {
    let message = match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    };
    Answer::ok(serde_json::to_vec(&message))
}

/// The answer to a notification or a response: accepted, with no body.
fn accepted() -> Answer {
    Answer {
        status: 202,
        body: Vec::new(),
        close: false,
        allow: None,
    }
}

/// The tool list, parsed from the checked-in file.
pub(crate) fn tools() -> Value {
    serde_json::from_str(TOOLS).unwrap_or(Value::Null)
}

/// A JSON-RPC answer whose `result` is a tool's.
#[derive(Serialize)]
struct ToolReply<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    result: ToolResult<'a>,
}

/// A tool result: one text content item, and the answer's JSON or that it is an error.
#[derive(Serialize)]
struct ToolResult<'a> {
    content: [Content<'a>; 1],
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    structured: Option<&'a RawValue>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
}

/// A text content item.
#[derive(Serialize)]
struct Content<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: Cow<'a, str>,
}

/// A tool result holding an operation's answer: on success, its JSON as `structuredContent` and
/// as one text content item; on an error, 2A's error body as the text and `isError`. The
/// answer's JSON is written as the text it is: no tree is made of it, however long it is.
fn tool_result(answer: &Answer) -> ToolResult<'_> {
    let ok = (200..300).contains(&answer.status);
    ToolResult {
        content: [Content {
            kind: "text",
            text: String::from_utf8_lossy(&answer.body),
        }],
        structured: ok
            .then(|| serde_json::from_slice(&answer.body).ok())
            .flatten(),
        is_error: (!ok).then_some(true),
    }
}

/// The answer to the tool call whose JSON-RPC ID is `id`, from the operation's `answer`.
pub(crate) fn tool_answer(id: &Value, answer: &Answer) -> Answer {
    Answer::ok(serde_json::to_vec(&ToolReply {
        jsonrpc: "2.0",
        id,
        result: tool_result(answer),
    }))
}

/// A `tools/call` message, read for its arguments as they were written.
#[derive(Deserialize)]
struct RawCall<'a> {
    #[serde(borrow, default)]
    params: Option<RawParams<'a>>,
}

#[derive(Deserialize)]
struct RawParams<'a> {
    #[serde(borrow, default)]
    arguments: Option<&'a RawValue>,
}

impl Api {
    /// What the head of a request for `/mcp` decides at `now`, Unix seconds, before its body is
    /// read: only `POST`, and every MCP request needs a token covering `observe` at least.
    pub(crate) fn admit_mcp(&mut self, request: &Request, now: u64) -> Admission {
        let answer = |answer| Admission::Now(Outcome::Answer(answer));
        match request.method.as_str() {
            "POST" => {}
            "GET" => {
                return answer(Answer {
                    status: 405,
                    body: Vec::new(),
                    close: false,
                    allow: Some("POST"),
                });
            }
            _ => {
                return answer(Answer::error(
                    ErrorCode::Invalid,
                    "this path takes another method",
                ));
            }
        }
        let grant = match self.verify(&request.authorization, now) {
            Ok(grant) => grant,
            Err(why) => return answer(Answer::error(ErrorCode::Unauthorized, why)),
        };
        if !grant.covers(Scope::Observe) {
            return answer(Answer::error(
                ErrorCode::Forbidden,
                "the token's scope does not cover observe",
            ));
        }
        if request.query.is_some() {
            return answer(Answer::error(ErrorCode::Invalid, "MCP takes no query"));
        }
        Admission::Body(Call::Mcp(grant))
    }

    /// What becomes of one MCP message, `body`, from a caller whose token granted `grant`.
    pub(crate) fn mcp_message(&mut self, grant: &Grant, body: &[u8], now: u64) -> Outcome {
        let answer = |id: &Value, outcome| Outcome::Answer(reply(id, outcome));
        let Ok(message) = serde_json::from_slice::<Value>(body) else {
            return answer(&Value::Null, Err((PARSE_ERROR, "Parse error")));
        };
        let Some(message) = message.as_object() else {
            return answer(&Value::Null, Err((INVALID_REQUEST, "Invalid Request")));
        };
        let Some(method) = message.get("method") else {
            // A response to the server, which sends no requests: accepted, as the transport says.
            return if message.contains_key("result") || message.contains_key("error") {
                Outcome::Answer(accepted())
            } else {
                answer(&Value::Null, Err((INVALID_REQUEST, "Invalid Request")))
            };
        };
        let Some(id) = message.get("id") else {
            // A notification, `notifications/initialized` among them.
            return Outcome::Answer(accepted());
        };
        let id = match id {
            Value::String(_) | Value::Number(_) => id,
            _ => return answer(&Value::Null, Err((INVALID_REQUEST, "Invalid Request"))),
        };
        let (Some(method), Some("2.0")) = (
            method.as_str(),
            message.get("jsonrpc").and_then(Value::as_str),
        ) else {
            return answer(id, Err((INVALID_REQUEST, "Invalid Request")));
        };
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "moruna", "version": self.facts.version }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools()),
            "tools/call" => match self.tool(message.get("params"), body, grant, id, now) {
                Ok(Outcome::Answer(answer)) => return Outcome::Answer(tool_answer(id, &answer)),
                Ok(later) => return later,
                Err(refused) => Err(refused),
            },
            _ => Err((METHOD_NOT_FOUND, "Method not found")),
        };
        answer(id, result)
    }

    /// Runs the operation of the tool `params` names, with the same scope rule as HTTP, and
    /// records the call. A tool's arguments are its operation's request: a job's fields; or the
    /// run's `id`, and `from` for the events' tool, which may be left out; or nothing. `body`
    /// is the whole message, read for a job as it was written.
    fn tool(
        &mut self,
        params: Option<&Value>,
        body: &[u8],
        grant: &Grant,
        id: &Value,
        now: u64,
    ) -> Result<Outcome, (i64, &'static str)> {
        let params = params
            .and_then(Value::as_object)
            .ok_or((INVALID_PARAMS, "Invalid params: tools/call takes an object"))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "Invalid params: the tool's name is missing"))?;
        let operation = OPERATIONS
            .iter()
            .find(|operation| operation.tool_name() == name)
            .ok_or((INVALID_PARAMS, "Invalid params: no tool has this name"))?;
        let empty = Map::new();
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(arguments)) => arguments,
            Some(_) => return Err((INVALID_PARAMS, "Invalid params: arguments is an object")),
        };
        let refuse = |api: &mut Self, why| {
            let code = ErrorCode::Invalid;
            Ok(Outcome::Answer(api.refuse(
                operation,
                Some(grant),
                code,
                why,
                now,
            )))
        };
        if operation.request == Body::Job {
            let written = serde_json::from_slice::<RawCall<'_>>(body)
                .ok()
                .and_then(|call| call.params)
                .and_then(|params| params.arguments)
                .map_or("{}", RawValue::get);
            let inputs = Inputs {
                body: written.as_bytes(),
                ..Inputs::default()
            };
            return Ok(self.call_tool(operation, grant, inputs, id, now));
        }
        // No operation takes a query without a run's ID.
        let takes = match (operation.takes_id(), operation.query.is_empty()) {
            (false, _) => "this operation takes no arguments",
            (true, true) => "this operation takes the run's id, and nothing else",
            (true, false) => {
                "this operation takes the run's id, and from, a record's number, and nothing else"
            }
        };
        let mut inputs = Inputs::default();
        for (argument, value) in arguments {
            match argument.as_str() {
                "id" if operation.takes_id() && value.is_string() => inputs.id = value.as_str(),
                query if operation.query.contains(&query) && value.is_u64() => {
                    inputs.from = value.as_u64();
                }
                _ => return refuse(self, takes),
            }
        }
        if operation.takes_id() && inputs.id.is_none() {
            return refuse(self, takes);
        }
        Ok(self.call_tool(operation, grant, inputs, id, now))
    }
}

#[cfg(test)]
mod tests;
