//! The OpenAPI document and the MCP tool list, generated from the types and [`OPERATIONS`].
//!
//! Both are checked in under `kernel/api/`; `cargo run -p moruna-api --features schema`
//! rewrites them. The output is byte for byte stable: keys are sorted at every level, whatever
//! order a JSON map keeps, and each file ends in one newline.

use crate::{
    Audit, ErrorBody, FROM, Function, FunctionRegistration, Functions, Health, Job, JobEvents,
    JobStatus, Jobs, Node, OPERATIONS, Operation, Request, Response, Submitted, Validation,
};
use alloc::{string::String, vec::Vec};
use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings};
use serde_json::{Map, Value, json};

/// The OpenAPI document's file name, under `kernel/api/`.
pub const OPENAPI_FILE: &str = "openapi.json";

/// The MCP tool list's file name, under `kernel/api/`.
pub const MCP_TOOLS_FILE: &str = "mcp-tools.json";

/// The name of the bearer-token security scheme in the OpenAPI document.
const BEARER: &str = "bearer";

/// Both files, each with its name and its contents exactly as checked in.
pub fn files() -> [(&'static str, String); 2] {
    [
        (OPENAPI_FILE, render(&openapi(OPERATIONS))),
        (MCP_TOOLS_FILE, render(&mcp_tools(OPERATIONS))),
    ]
}

/// The schema of the body `request` takes, when it takes one: a job as a client writes it (its
/// version 1 document any JSON object), or a function's registration.
fn body(generator: &mut SchemaGenerator, request: Request) -> Option<Schema> {
    match request {
        Request::None => None,
        Request::Job => Some(generator.subschema_for::<Job<Map<String, Value>>>()),
        Request::Function => Some(generator.subschema_for::<FunctionRegistration>()),
    }
}

/// The schema of what `response` answers with; a report is any JSON object, and a stream of
/// events is text.
fn answer(generator: &mut SchemaGenerator, response: Response) -> Value {
    fn of<T: JsonSchema>(generator: &mut SchemaGenerator) -> Value {
        generator.subschema_for::<T>().to_value()
    }
    match response {
        Response::Health => of::<Health>(generator),
        Response::Node => of::<Node>(generator),
        Response::Audit => of::<Audit>(generator),
        Response::Validation => of::<Validation>(generator),
        Response::Submitted => of::<Submitted>(generator),
        Response::Jobs => of::<Jobs>(generator),
        Response::JobStatus => of::<JobStatus>(generator),
        Response::Events => json!({ "type": "string" }),
        Response::Report => json!({ "type": "object" }),
        Response::Function => of::<Function>(generator),
        Response::Functions => of::<Functions>(generator),
    }
}

/// The OpenAPI 3.1 document for `operations`: bearer-token security on every operation that
/// needs a scope, naming that scope, the body and the parameters each takes, and the error body
/// on every operation.
pub fn openapi(operations: &[Operation]) -> Value {
    let mut generator = SchemaSettings::draft2020_12()
        .with(|settings| {
            settings.definitions_path = "/components/schemas".into();
            settings.meta_schema = None;
        })
        .for_serialize()
        .into_generator();
    let error = generator.subschema_for::<ErrorBody>();

    let mut paths = Map::new();
    for operation in operations {
        let response = answer(&mut generator, operation.response);
        let content = match operation.response {
            Response::Events => "text/event-stream",
            _ => "application/json",
        };
        let security = match operation.scope {
            Some(scope) => json!([{ BEARER: [scope.as_str()] }]),
            None => json!([]),
        };
        let mut parameters = Vec::new();
        if operation.takes_id() {
            parameters.push(json!({
                "name": "id",
                "in": "path",
                "required": true,
                "description": "The run's ID.",
                "schema": { "type": "string" }
            }));
        }
        for name in operation.query {
            parameters.push(json!({
                "name": name,
                "in": "query",
                "required": false,
                "description": FROM,
                "schema": { "type": "integer", "minimum": 0 }
            }));
        }
        let method = operation.method.as_str().to_ascii_lowercase();
        let mut responses = Map::new();
        responses.insert(
            alloc::format!("{}", operation.status),
            json!({
                "description": "OK",
                "content": { content: { "schema": response } }
            }),
        );
        responses.insert(
            "default".into(),
            json!({
                "description": "An error. Its code matches the HTTP status.",
                "content": { "application/json": { "schema": error } }
            }),
        );
        // A path not seen yet starts as null, which indexing by a method makes an object.
        let item = paths.entry(operation.path).or_insert(Value::Null);
        item[method.as_str()] = json!({
            "operationId": operation.id,
            "summary": operation.summary,
            "security": security,
            "responses": responses
        });
        if !parameters.is_empty() {
            item[method.as_str()]["parameters"] = Value::Array(parameters);
        }
        if let Some(schema) = body(&mut generator, operation.request) {
            item[method.as_str()]["requestBody"] = json!({
                "required": true,
                "content": { "application/json": { "schema": schema } }
            });
        }
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Moruna node API",
            "version": "1",
            "description": "A Moruna node is driven only through this API. Paths start /v1, bodies are JSON, and unknown request fields are refused."
        },
        "paths": paths,
        "components": {
            "schemas": generator.take_definitions(true),
            "securitySchemes": {
                BEARER: {
                    "type": "http",
                    "scheme": "bearer",
                    "bearerFormat": "JWT",
                    "description": "A JWT signed with Ed25519 under one of the node's token keys. Its claims are aud (the node's instance ID), exp (Unix seconds, at most one hour ahead) and scope. admin covers submit, and submit covers observe."
                }
            }
        }
    })
}

/// The MCP tool list for `operations`, as MCP's `tools/list` result: one tool per operation,
/// named as [`Operation::tool_name`] says and described as [`Operation::tool_summary`] says. A
/// tool's input is the operation's request: its body's fields, the run's `id` when its path
/// carries one, its query's parameters, each optional, and nothing else, so an operation that
/// takes none of them has an empty object that refuses unknown fields. Its output schema is the
/// operation's response, with every subschema inlined; for the stream of events, the page of it
/// the tool answers ([`JobEvents`]).
pub fn mcp_tools(operations: &[Operation]) -> Value {
    let mut generator = SchemaSettings::draft2020_12()
        .with(|settings| {
            settings.meta_schema = None;
            settings.inline_subschemas = true;
        })
        .for_serialize()
        .into_generator();
    let tools: Vec<Value> = operations
        .iter()
        .map(|operation| {
            let output = match operation.response {
                Response::Health => generator.root_schema_for::<Health>().to_value(),
                Response::Node => generator.root_schema_for::<Node>().to_value(),
                Response::Audit => generator.root_schema_for::<Audit>().to_value(),
                Response::Validation => generator.root_schema_for::<Validation>().to_value(),
                Response::Submitted => generator.root_schema_for::<Submitted>().to_value(),
                Response::Jobs => generator.root_schema_for::<Jobs>().to_value(),
                Response::JobStatus => generator.root_schema_for::<JobStatus>().to_value(),
                Response::Events => generator
                    .root_schema_for::<JobEvents<Map<String, Value>>>()
                    .to_value(),
                Response::Report => json!({ "type": "object" }),
                Response::Function => generator.root_schema_for::<Function>().to_value(),
                Response::Functions => generator.root_schema_for::<Functions>().to_value(),
            };
            let mut input = match operation.request {
                Request::Job => generator
                    .root_schema_for::<Job<Map<String, Value>>>()
                    .to_value(),
                Request::Function => generator
                    .root_schema_for::<FunctionRegistration>()
                    .to_value(),
                Request::None => json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            };
            if operation.takes_id() {
                input["properties"]["id"] =
                    json!({ "type": "string", "description": "The run's ID." });
                input["required"] = json!(["id"]);
            }
            for name in operation.query {
                input["properties"][*name] = json!({
                    "type": "integer",
                    "minimum": 0,
                    "description": FROM
                });
            }
            json!({
                "name": operation.tool_name(),
                "description": operation.tool_summary(),
                "inputSchema": input,
                "outputSchema": output
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// `value` as pretty JSON with its keys sorted at every level, and a final newline.
pub fn render(value: &Value) -> String {
    let mut text =
        serde_json::to_string_pretty(&sorted(value)).expect("a JSON value always serializes");
    text.push('\n');
    text
}

/// `value` with the keys of every object, at every level, in sorted order.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<(&String, &Value)> = object.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), sorted(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests;
