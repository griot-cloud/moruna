//! MCP end to end, over TLS (item 2.4): an MCP client initializes, lists the tools and calls
//! each one; a call without the scope is a tool error; an unknown method gets `-32601`.
mod common;

use common::{Harness, INSTANCE, Response, WALL};
use moruna_api_server::{PROTOCOL_VERSION, TokenKey};
use serde_json::{Value, json};

/// Posts one JSON-RPC message to `/mcp` on client `index` with `token`.
fn post(harness: &mut Harness, index: usize, token: &str, message: &Value) -> Response {
    let body = serde_json::to_vec(message).unwrap();
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: node\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{}Content-Length: {}\r\n\r\n",
        Harness::bearer(token),
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    harness.exchange(index, &request)
}

/// A JSON-RPC request with this ID, method and params.
fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// The JSON-RPC answer in `response`: HTTP 200, JSON, for request `id`.
fn answer(response: &Response, id: u64) -> Value {
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert!(response.header("mcp-session-id").is_none(), "stateless");
    let body = response.json();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], id);
    body
}

/// The text of a tool result's one content item, parsed as JSON.
fn text(result: &Value) -> Value {
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn a_client_initializes_lists_and_calls_each_tool() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let admin = harness.token(&["admin"]);

    let initialized = answer(
        &post(
            &mut harness,
            client,
            &admin,
            &request(
                1,
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1" }
                }),
            ),
        ),
        1,
    );
    assert_eq!(
        initialized["result"],
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "moruna", "version": "0.7.0" }
        })
    );
    let notified = post(
        &mut harness,
        client,
        &admin,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );
    assert_eq!(notified.status, 202);
    assert!(notified.body.is_empty());

    // The tool list served equals mcp-tools.json, after parsing.
    let listed = answer(
        &post(
            &mut harness,
            client,
            &admin,
            &request(2, "tools/list", json!({})),
        ),
        2,
    );
    let checked_in: Value = serde_json::from_str(include_str!("../../mcp-tools.json")).unwrap();
    assert_eq!(listed["result"], checked_in);
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "moruna_health",
            "moruna_node",
            "moruna_node_audit",
            "moruna_jobs_validate",
            "moruna_jobs_submit",
            "moruna_jobs_list",
            "moruna_jobs_get",
            "moruna_jobs_cancel",
            "moruna_jobs_events",
            "moruna_jobs_report",
            "moruna_functions_register",
            "moruna_functions_list"
        ]
    );

    let call = |name: &str| request(3, "tools/call", json!({ "name": name, "arguments": {} }));
    let health = answer(
        &post(&mut harness, client, &admin, &call("moruna_health")),
        3,
    );
    assert_eq!(
        health["result"]["structuredContent"],
        json!({ "status": "ok" })
    );
    assert_eq!(text(&health["result"]), json!({ "status": "ok" }));

    harness.clock.advance(2_000);
    let node = answer(&post(&mut harness, client, &admin, &call("moruna_node")), 3);
    let document = json!({
        "version": "0.7.0", "instance": INSTANCE, "cloud": "openstack",
        "cores": 4, "memory_bytes": 8_589_934_592u64, "job_memory_bytes": 6_849_298_432u64,
        "executors": [{ "kind": "cpu", "slots": 4 }],
        "boot_ms": 2140, "time": WALL + 2, "health": "ok"
    });
    assert_eq!(node["result"]["structuredContent"], document);
    assert_eq!(text(&node["result"]), document);

    let audit = answer(
        &post(&mut harness, client, &admin, &call("moruna_node_audit")),
        3,
    );
    let records = audit["result"]["structuredContent"]["records"]
        .as_array()
        .unwrap();
    let key = TokenKey::new(harness.signer.public()).id().to_string();
    // Each tools/call is one record with its operation's ID, health's excepted; nothing else
    // MCP does is.
    assert_eq!(
        records,
        &[
            json!({ "time": WALL + 2, "key": key, "scope": ["admin"], "operation": "node", "status": 200 })
        ]
    );
    assert_eq!(text(&audit["result"]), audit["result"]["structuredContent"]);
}

/// 3E through MCP: a function is registered with the tool's arguments as its registration, and
/// listed; a registration that does not verify is a tool error.
#[test]
fn a_function_is_registered_and_listed_through_mcp() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let admin = harness.token(&["admin"]);
    let image = b"\x7fELF gate-score";
    let arguments: Value = serde_json::from_str(&common::signer::registration(
        "gate-score",
        "https://images.example.com/gate-score",
        image,
        &harness.signer,
    ))
    .unwrap();
    let call = |id, name: &str, arguments: Value| {
        request(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    };
    let registered = answer(
        &post(
            &mut harness,
            client,
            &admin,
            &call(5, "moruna_functions_register", arguments.clone()),
        ),
        5,
    );
    let function = &registered["result"]["structuredContent"];
    assert_eq!(function["name"], "gate-score");
    assert_eq!(function["sha256"], common::signer::sha256_hex(image));
    let listed = answer(
        &post(
            &mut harness,
            client,
            &admin,
            &call(6, "moruna_functions_list", json!({})),
        ),
        6,
    );
    assert_eq!(
        listed["result"]["structuredContent"]["functions"][0]["name"],
        "gate-score"
    );
    let mut forged = arguments;
    forged["sha256"] = common::signer::sha256_hex(b"another image").into();
    let refused = answer(
        &post(
            &mut harness,
            client,
            &admin,
            &call(7, "moruna_functions_register", forged),
        ),
        7,
    );
    assert_eq!(refused["result"]["isError"], true);
    assert!(
        text(&refused["result"])["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not verify")
    );
}

#[test]
fn a_call_without_the_scope_is_a_tool_error() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let observe = harness.token(&["observe"]);
    let called = answer(
        &post(
            &mut harness,
            client,
            &observe,
            &request(4, "tools/call", json!({ "name": "moruna_node_audit" })),
        ),
        4,
    );
    let result = &called["result"];
    assert_eq!(result["isError"], true);
    let error = text(result);
    assert_eq!(error["error"]["code"], "forbidden");
    assert!(error["error"]["message"].is_string());
    // It was recorded, refused.
    let admin = harness.token(&["admin"]);
    let audit = harness.get(client, "/v1/node/audit", &Harness::bearer(&admin));
    let records = audit.json()["records"].as_array().unwrap().clone();
    assert_eq!(records[0]["operation"], "node_audit");
    assert_eq!(records[0]["status"], 403);
    assert_eq!(records[0]["scope"], json!(["observe"]));
}

#[test]
fn an_unknown_method_gets_32601_and_malformed_json_32700() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let observe = harness.token(&["observe"]);
    let unknown = answer(
        &post(
            &mut harness,
            client,
            &observe,
            &request(5, "resources/list", json!({})),
        ),
        5,
    );
    assert_eq!(unknown["error"]["code"], -32_601);
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: node\r\n{}Content-Length: 1\r\n\r\n{{",
        Harness::bearer(&observe)
    );
    let malformed = harness.exchange(client, request.as_bytes());
    assert_eq!(malformed.status, 200);
    assert_eq!(malformed.json()["error"]["code"], -32_700);
    assert_eq!(malformed.json()["id"], Value::Null);
}

#[test]
fn mcp_needs_the_same_tokens_and_only_post() {
    let mut harness = Harness::new();
    let ping = request(6, "ping", json!({}));
    // Refused on its head, a message is never read, so its connection closes.
    let expired = harness.signer.token(INSTANCE, WALL - 1, &["observe"]);
    let nothing = harness.token(&[]);
    for (token, status, code) in [
        ("not.a.token", 401, "unauthorized"),
        (expired.as_str(), 401, "unauthorized"),
        (nothing.as_str(), 403, "forbidden"),
    ] {
        let client = harness.connect();
        let refused = post(&mut harness, client, token, &ping);
        assert_eq!(refused.status, status);
        assert_eq!(refused.error_code(), code);
        assert_eq!(refused.header("connection"), Some("close"));
        assert!(harness.clients[client].server_closed());
    }
    let client = harness.connect();
    let observe = harness.token(&["observe"]);
    assert_eq!(
        answer(&post(&mut harness, client, &observe, &ping), 6)["result"],
        json!({})
    );
    let get = harness.get(client, "/mcp", &Harness::bearer(&observe));
    assert_eq!(get.status, 405);
    assert_eq!(get.header("allow"), Some("POST"));
    assert!(get.body.is_empty());
    assert!(!harness.clients[client].server_closed());
}
