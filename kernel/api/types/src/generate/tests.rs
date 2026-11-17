//! The generated files against the checked-in ones, and against the system design.

use super::*;
use alloc::{format, string::ToString};

const REGENERATE: &str = "from kernel/: cargo run -p moruna-api --features schema";

/// The first line where `checked_in` and `generated` differ, for the failure message.
fn first_difference(checked_in: &str, generated: &str) -> String {
    let mut checked_in_lines = checked_in.lines();
    let mut generated_lines = generated.lines();
    for line in 1.. {
        match (checked_in_lines.next(), generated_lines.next()) {
            (None, None) => break,
            (a, b) if a == b => {}
            (a, b) => {
                return format!(
                    "line {line}: checked in {:?}, generated {:?}",
                    a.unwrap_or("<end>"),
                    b.unwrap_or("<end>")
                );
            }
        }
    }
    "a difference in line endings".to_string()
}

#[test]
fn checked_in_files_match_the_types_and_the_table() {
    let checked_in = [
        include_str!("../../../openapi.json"),
        include_str!("../../../mcp-tools.json"),
    ];
    for ((name, generated), checked_in) in files().iter().zip(checked_in) {
        assert!(
            *generated == checked_in,
            "kernel/api/{name} is stale ({}); regenerate it, {REGENERATE}",
            first_difference(checked_in, generated)
        );
    }
}

#[test]
fn a_change_to_the_table_changes_both_files() {
    let fewer = &OPERATIONS[..OPERATIONS.len() - 1];
    assert_ne!(render(&openapi(fewer)), render(&openapi(OPERATIONS)));
    assert_ne!(render(&mcp_tools(fewer)), render(&mcp_tools(OPERATIONS)));
    assert_eq!(openapi(fewer)["paths"].as_object().map(Map::len), Some(2));
    assert_eq!(mcp_tools(fewer)["tools"].as_array().map(Vec::len), Some(2));
}

#[test]
fn rendering_is_stable_sorted_and_ends_in_one_newline() {
    let value = json!({ "b": [{ "z": 1, "y": 2 }], "a": { "d": 1, "c": 2 } });
    assert_eq!(
        render(&value),
        "{\n  \"a\": {\n    \"c\": 2,\n    \"d\": 1\n  },\n  \"b\": [\n    {\n      \"y\": 2,\n      \"z\": 1\n    }\n  ]\n}\n"
    );
    assert_eq!(files(), files());
    for (_, text) in files() {
        assert!(text.ends_with("}\n") && !text.ends_with("\n\n"));
    }
}

#[test]
fn openapi_has_every_operation_with_its_security_and_the_error_body() {
    let document = openapi(OPERATIONS);
    assert_eq!(document["openapi"], "3.1.0");
    let scheme = &document["components"]["securitySchemes"][BEARER];
    assert_eq!(
        (&scheme["type"], &scheme["scheme"], &scheme["bearerFormat"]),
        (&json!("http"), &json!("bearer"), &json!("JWT"))
    );
    let expected = [
        ("/v1/health", "health", json!([]), "Health"),
        (
            "/v1/node",
            "node",
            json!([{ "bearer": ["observe"] }]),
            "Node",
        ),
        (
            "/v1/node/audit",
            "node_audit",
            json!([{ "bearer": ["admin"] }]),
            "Audit",
        ),
    ];
    assert_eq!(
        document["paths"].as_object().map(Map::len),
        Some(expected.len())
    );
    for (path, id, security, response) in expected {
        let operation = &document["paths"][path]["get"];
        assert_eq!(operation["operationId"], id, "{path}");
        assert_eq!(operation["security"], security, "{path}");
        let responses = &operation["responses"];
        assert_eq!(
            responses["200"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{response}"),
            "{path}"
        );
        assert_eq!(
            responses["default"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ErrorBody",
            "{path}"
        );
    }
}

/// The property names of a component schema, sorted.
fn properties(document: &Value, schema: &str) -> Vec<String> {
    let mut names: Vec<String> = document["components"]["schemas"][schema]["properties"]
        .as_object()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// The strings a string-enum schema allows, sorted: one `const` per `oneOf` branch.
fn allowed(schema: &Value) -> Vec<String> {
    let mut values: Vec<String> = schema["oneOf"]
        .as_array()
        .map(|branches| {
            branches
                .iter()
                .filter_map(|branch| branch["const"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    values.sort();
    values
}

/// The values a component schema allows, sorted.
fn values(document: &Value, schema: &str) -> Vec<String> {
    allowed(&document["components"]["schemas"][schema])
}

#[test]
fn openapi_schemas_carry_the_design_names_and_no_others() {
    let document = openapi(OPERATIONS);
    let strings = |names: &[&str]| -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        names.sort();
        names
    };
    assert_eq!(properties(&document, "Health"), strings(&["status"]));
    assert_eq!(values(&document, "HealthStatus"), strings(&["ok"]));
    assert_eq!(
        properties(&document, "Node"),
        strings(&[
            "version",
            "instance",
            "cloud",
            "cores",
            "memory_bytes",
            "executors",
            "boot_ms",
            "time",
            "health"
        ])
    );
    assert_eq!(
        properties(&document, "Executor"),
        strings(&["kind", "slots"])
    );
    assert_eq!(values(&document, "ExecutorKind"), strings(&["cpu"]));
    assert_eq!(
        values(&document, "Cloud"),
        strings(&["openstack", "azure", "none"])
    );
    assert_eq!(
        values(&document, "NodeHealth"),
        strings(&["ok", "degraded"])
    );
    assert_eq!(properties(&document, "Audit"), strings(&["records"]));
    assert_eq!(
        properties(&document, "AuditRecord"),
        strings(&["time", "key", "scope", "operation", "status"])
    );
    assert_eq!(
        values(&document, "Scope"),
        strings(&["observe", "submit", "admin"])
    );
    assert_eq!(properties(&document, "ErrorBody"), strings(&["error"]));
    assert_eq!(
        properties(&document, "ErrorDetail"),
        strings(&["code", "message"])
    );
    assert_eq!(
        values(&document, "ErrorCode"),
        strings(&[
            "invalid",
            "unauthorized",
            "forbidden",
            "not_found",
            "conflict",
            "busy",
            "internal"
        ])
    );
}

#[test]
fn mcp_has_one_tool_per_operation_with_its_input_and_output() {
    let list = mcp_tools(OPERATIONS);
    let tools = list["tools"].as_array().expect("a tool list");
    assert_eq!(tools.len(), OPERATIONS.len());
    let expected = [
        ("moruna_health", "Health"),
        ("moruna_node", "Node"),
        ("moruna_node_audit", "Audit"),
    ];
    for ((tool, operation), (name, title)) in tools.iter().zip(OPERATIONS).zip(expected) {
        assert_eq!(tool["name"], name);
        assert_eq!(tool["description"], operation.summary);
        assert_eq!(
            tool["inputSchema"],
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        );
        assert_eq!(tool["outputSchema"]["type"], "object", "{name}");
        assert_eq!(tool["outputSchema"]["title"], title, "{name}");
    }
    // Each output schema stands alone: every subschema is inlined, so no reference remains.
    let text = render(&list);
    assert!(!text.contains("$ref") && !text.contains("$defs"));
    assert_eq!(
        allowed(&tools[1]["outputSchema"]["properties"]["cloud"]),
        ["azure", "none", "openstack"]
    );
}
