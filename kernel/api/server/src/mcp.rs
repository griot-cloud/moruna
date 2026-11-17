//! MCP on the API's port (system design, section 2D): `POST /mcp`, MCP's Streamable HTTP,
//! stateless. Each request carries one JSON-RPC 2.0 message and is answered with one JSON
//! response; a notification or a response is answered `202` with no body. No server-sent stream
//! and no session ID. The tools are the operations, one each, listed exactly as
//! `kernel/api/mcp-tools.json` (generated from `moruna-api`) lists them.

use crate::{
    api::{Admission, Answer, Api, Call},
    http::Request,
    token::Grant,
};
use alloc::{string::String, vec::Vec};
use moruna_api::{ErrorCode, OPERATIONS, Scope};
use serde_json::{Map, Value, json};

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

/// A tool result holding an operation's answer: on success, its JSON as `structuredContent` and
/// as one text content item; on an error, 2A's error body as the text and `isError`.
fn tool_result(answer: &Answer) -> Value {
    let text = String::from_utf8_lossy(&answer.body);
    if answer.status == 200 {
        let structured: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
        json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": structured
        })
    } else {
        json!({
            "content": [{ "type": "text", "text": text }],
            "isError": true
        })
    }
}

impl Api {
    /// What the head of a request for `/mcp` decides at `now`, Unix seconds, before its body is
    /// read: only `POST`, and every MCP request needs a token covering `observe` at least.
    pub(crate) fn admit_mcp(&mut self, request: &Request, now: u64) -> Admission {
        match request.method.as_str() {
            "POST" => {}
            "GET" => {
                return Admission::Answer(Answer {
                    status: 405,
                    body: Vec::new(),
                    close: false,
                    allow: Some("POST"),
                });
            }
            _ => {
                return Admission::Answer(Answer::error(
                    ErrorCode::Invalid,
                    "this path takes another method",
                ));
            }
        }
        let grant = match self.verify(&request.authorization, now) {
            Ok(grant) => grant,
            Err(why) => return Admission::Answer(Answer::error(ErrorCode::Unauthorized, why)),
        };
        if !grant.covers(Scope::Observe) {
            return Admission::Answer(Answer::error(
                ErrorCode::Forbidden,
                "the token's scope does not cover observe",
            ));
        }
        if request.query {
            return Admission::Answer(Answer::error(ErrorCode::Invalid, "MCP takes no query"));
        }
        Admission::Body(Call::Mcp(grant))
    }

    /// The answer to one MCP message, `body`, from a caller whose token granted `grant`.
    pub(crate) fn mcp_message(&mut self, grant: &Grant, body: &[u8], now: u64) -> Answer {
        let Ok(message) = serde_json::from_slice::<Value>(body) else {
            return reply(&Value::Null, Err((PARSE_ERROR, "Parse error")));
        };
        let Some(message) = message.as_object() else {
            return reply(&Value::Null, Err((INVALID_REQUEST, "Invalid Request")));
        };
        let Some(method) = message.get("method") else {
            // A response to the server, which sends no requests: accepted, as the transport says.
            return if message.contains_key("result") || message.contains_key("error") {
                accepted()
            } else {
                reply(&Value::Null, Err((INVALID_REQUEST, "Invalid Request")))
            };
        };
        let Some(id) = message.get("id") else {
            // A notification, `notifications/initialized` among them.
            return accepted();
        };
        let id = match id {
            Value::String(_) | Value::Number(_) => id,
            _ => return reply(&Value::Null, Err((INVALID_REQUEST, "Invalid Request"))),
        };
        let (Some(method), Some("2.0")) = (
            method.as_str(),
            message.get("jsonrpc").and_then(Value::as_str),
        ) else {
            return reply(id, Err((INVALID_REQUEST, "Invalid Request")));
        };
        let outcome = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "moruna", "version": self.facts.version }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools()),
            "tools/call" => self.call_tool(message.get("params"), grant, now),
            _ => Err((METHOD_NOT_FOUND, "Method not found")),
        };
        reply(id, outcome)
    }

    /// Runs the operation of the tool `params` names, with the same scope rule as HTTP, and
    /// records the call.
    fn call_tool(
        &mut self,
        params: Option<&Value>,
        grant: &Grant,
        now: u64,
    ) -> Result<Value, (i64, &'static str)> {
        let params = params
            .and_then(Value::as_object)
            .ok_or((INVALID_PARAMS, "Invalid params: tools/call takes an object"))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "Invalid params: the tool's name is missing"))?;
        let operation = OPERATIONS
            .iter()
            .find(|operation| name.strip_prefix("moruna_") == Some(operation.id))
            .ok_or((INVALID_PARAMS, "Invalid params: no tool has this name"))?;
        let empty = Map::new();
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(arguments)) => arguments,
            Some(_) => return Err((INVALID_PARAMS, "Invalid params: arguments is an object")),
        };
        // Step 2's operations take no request, so any argument is an unknown field.
        let answer = if arguments.is_empty() {
            self.call(operation, Some(grant), now)
        } else {
            self.refuse(
                operation,
                Some(grant),
                ErrorCode::Invalid,
                "this operation takes no arguments",
                now,
            )
        };
        Ok(tool_result(&answer))
    }
}

#[cfg(test)]
mod tests;
