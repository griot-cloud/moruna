//! The OpenAPI document and the MCP tool list, generated from the types and [`OPERATIONS`].
//!
//! Both are checked in under `kernel/api/`; `cargo run -p moruna-api --features schema`
//! rewrites them. The output is byte for byte stable: keys are sorted at every level, whatever
//! order a JSON map keeps, and each file ends in one newline.

use crate::{Audit, ErrorBody, Health, Node, OPERATIONS, Operation, Response};
use alloc::{string::String, vec::Vec};
use schemars::generate::SchemaSettings;
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

/// The OpenAPI 3.1 document for `operations`: bearer-token security on every operation that
/// needs a scope, naming that scope, and the error body on every operation.
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
        let response = match operation.response {
            Response::Health => generator.subschema_for::<Health>(),
            Response::Node => generator.subschema_for::<Node>(),
            Response::Audit => generator.subschema_for::<Audit>(),
        };
        let security = match operation.scope {
            Some(scope) => json!([{ BEARER: [scope.as_str()] }]),
            None => json!([]),
        };
        let method = operation.method.as_str().to_ascii_lowercase();
        // A path not seen yet starts as null, which indexing by a method makes an object.
        let item = paths.entry(operation.path).or_insert(Value::Null);
        item[method.as_str()] = json!({
            "operationId": operation.id,
            "summary": operation.summary,
            "security": security,
            "responses": {
                "200": {
                    "description": "OK",
                    "content": { "application/json": { "schema": response } }
                },
                "default": {
                    "description": "An error. Its code matches the HTTP status.",
                    "content": { "application/json": { "schema": error } }
                }
            }
        });
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
/// named `moruna_<id>`. A tool's input is the operation's request, and step 2's operations take
/// none, so its input schema is an empty object that refuses unknown fields. Its output schema is
/// the operation's response, with every subschema inlined.
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
                Response::Health => generator.root_schema_for::<Health>(),
                Response::Node => generator.root_schema_for::<Node>(),
                Response::Audit => generator.root_schema_for::<Audit>(),
            };
            json!({
                "name": operation.tool_name(),
                "description": operation.summary,
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                },
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
