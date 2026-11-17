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
    let step_2 = &OPERATIONS[..3];
    assert_eq!(openapi(step_2)["paths"].as_object().map(Map::len), Some(3));
    assert_eq!(mcp_tools(step_2)["tools"].as_array().map(Vec::len), Some(3));
    // Step 3's seven operations are on six paths, and all but the stream of events are tools.
    assert_eq!(
        openapi(OPERATIONS)["paths"].as_object().map(Map::len),
        Some(9)
    );
    assert_eq!(
        mcp_tools(OPERATIONS)["tools"].as_array().map(Vec::len),
        Some(9)
    );
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
    let observe = json!([{ "bearer": ["observe"] }]);
    let submit = json!([{ "bearer": ["submit"] }]);
    let expected = [
        ("get", "/v1/health", "health", json!([]), "200", "Health"),
        ("get", "/v1/node", "node", observe.clone(), "200", "Node"),
        (
            "get",
            "/v1/node/audit",
            "node_audit",
            json!([{ "bearer": ["admin"] }]),
            "200",
            "Audit",
        ),
        (
            "post",
            "/v1/jobs/validate",
            "jobs_validate",
            submit.clone(),
            "200",
            "Validation",
        ),
        (
            "post",
            "/v1/jobs",
            "jobs_submit",
            submit.clone(),
            "201",
            "Submitted",
        ),
        (
            "get",
            "/v1/jobs",
            "jobs_list",
            observe.clone(),
            "200",
            "Jobs",
        ),
        (
            "get",
            "/v1/jobs/{id}",
            "jobs_get",
            observe.clone(),
            "200",
            "JobStatus",
        ),
        (
            "post",
            "/v1/jobs/{id}/cancel",
            "jobs_cancel",
            submit,
            "200",
            "JobStatus",
        ),
    ];
    for (method, path, id, security, status, response) in expected {
        let operation = &document["paths"][path][method];
        assert_eq!(operation["operationId"], id, "{path}");
        assert_eq!(operation["security"], security, "{path}");
        let responses = &operation["responses"];
        assert_eq!(responses.as_object().map(Map::len), Some(2), "{path}");
        assert_eq!(
            responses[status]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{response}"),
            "{path}"
        );
        assert_eq!(
            responses["default"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ErrorBody",
            "{path}"
        );
        let takes_id = path.contains("{id}");
        assert_eq!(
            operation["parameters"]
                == json!([{
                    "name": "id", "in": "path", "required": true,
                    "description": "The run's ID.", "schema": { "type": "string" }
                }]),
            takes_id,
            "{path}"
        );
        assert_eq!(
            operation["requestBody"]["content"]["application/json"]["schema"]["$ref"]
                == "#/components/schemas/Job",
            id == "jobs_validate" || id == "jobs_submit",
            "{path}"
        );
    }
    // The stream of events is text, replayed from `from`; the report is any JSON object.
    let events = &document["paths"]["/v1/jobs/{id}/events"]["get"];
    assert_eq!(events["operationId"], "jobs_events");
    assert_eq!(events["security"], observe);
    assert_eq!(
        events["responses"]["200"]["content"]["text/event-stream"]["schema"],
        json!({ "type": "string" })
    );
    assert_eq!(
        events["parameters"][1],
        json!({ "name": "from", "in": "query", "required": false,
                "schema": { "type": "integer", "minimum": 0 } })
    );
    let report = &document["paths"]["/v1/jobs/{id}/report"]["get"];
    assert_eq!(report["operationId"], "jobs_report");
    assert_eq!(
        report["responses"]["200"]["content"]["application/json"]["schema"],
        json!({ "type": "object" })
    );
    let total: usize = document["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|methods| methods.as_object().unwrap().len())
        .sum();
    assert_eq!(total, OPERATIONS.len());
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
    // 3B's names.
    assert_eq!(
        properties(&document, "Job"),
        strings(&["v1", "v2", "secrets"])
    );
    assert_eq!(
        document["components"]["schemas"]["Job"]["additionalProperties"],
        false
    );
    assert_eq!(
        properties(&document, "Validation"),
        strings(&["ok", "refusals"])
    );
    assert_eq!(
        properties(&document, "Refusal"),
        strings(&["field", "message"])
    );
    assert_eq!(properties(&document, "Submitted"), strings(&["id"]));
    assert_eq!(properties(&document, "Jobs"), strings(&["jobs"]));
    assert_eq!(
        properties(&document, "JobSummary"),
        strings(&["id", "state", "submitted", "started", "ended"])
    );
    assert_eq!(
        properties(&document, "JobStatus"),
        strings(&[
            "id",
            "state",
            "submitted",
            "started",
            "ended",
            "heartbeat",
            "exit_code"
        ])
    );
    assert_eq!(
        values(&document, "JobState"),
        strings(&["starting", "running", "completed", "failed", "cancelled"])
    );
    assert_eq!(
        properties(&document, "Heartbeat"),
        strings(&[
            "t_ms",
            "committed_seq",
            "rows_out",
            "bytes_out",
            "active_workers",
            "ceiling_bytes",
            "anon_bytes",
            "bottleneck",
            "staging_bytes"
        ])
    );
}

#[test]
fn mcp_has_one_tool_per_operation_with_its_input_and_output() {
    let list = mcp_tools(OPERATIONS);
    let tools = list["tools"].as_array().expect("a tool list");
    let with_tools: Vec<&Operation> = OPERATIONS.iter().filter(|op| op.has_tool()).collect();
    assert_eq!(tools.len(), with_tools.len());
    let nothing = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    let id = json!({
        "type": "object",
        "properties": { "id": { "type": "string", "description": "The run's ID." } },
        "required": ["id"],
        "additionalProperties": false
    });
    let expected = [
        ("moruna_health", Some("Health"), Some(&nothing)),
        ("moruna_node", Some("Node"), Some(&nothing)),
        ("moruna_node_audit", Some("Audit"), Some(&nothing)),
        ("moruna_jobs_validate", Some("Validation"), None),
        ("moruna_jobs_submit", Some("Submitted"), None),
        ("moruna_jobs_list", Some("Jobs"), Some(&nothing)),
        ("moruna_jobs_get", Some("JobStatus"), Some(&id)),
        ("moruna_jobs_cancel", Some("JobStatus"), Some(&id)),
        ("moruna_jobs_report", None, Some(&id)),
    ];
    for ((tool, operation), (name, title, input)) in tools.iter().zip(with_tools).zip(expected) {
        assert_eq!(tool["name"], name);
        assert_eq!(tool["description"], operation.summary);
        match input {
            Some(input) => assert_eq!(&tool["inputSchema"], input, "{name}"),
            // A job: its document in one version, and nothing else.
            None => {
                let input = &tool["inputSchema"];
                assert_eq!(input["title"], "Job", "{name}");
                assert_eq!(input["additionalProperties"], false, "{name}");
                assert_eq!(
                    input["properties"].as_object().map(Map::len),
                    Some(3),
                    "{name}"
                );
            }
        }
        assert_eq!(tool["outputSchema"]["type"], "object", "{name}");
        if let Some(title) = title {
            assert_eq!(tool["outputSchema"]["title"], title, "{name}");
        }
    }
    // Each output schema stands alone: every subschema is inlined, so no reference remains.
    let text = render(&list);
    assert!(!text.contains("$ref") && !text.contains("$defs"));
    assert_eq!(
        allowed(&tools[1]["outputSchema"]["properties"]["cloud"]),
        ["azure", "none", "openstack"]
    );
}
