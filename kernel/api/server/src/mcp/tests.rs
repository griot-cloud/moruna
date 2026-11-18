//! MCP's messages, each answered as the transport and JSON-RPC 2.0 say.

use super::*;
use crate::{
    api::NodeFacts,
    http::{Authorization, Request},
    signer::Signer,
    token::TokenKey,
};
use alloc::{format, string::ToString, vec};
use moruna_api::{Cloud, Executor, ExecutorKind, NodeHealth};

const NOW: u64 = 1_794_931_200;

fn api(signer: &Signer) -> Api {
    Api::new(
        NodeFacts {
            version: "0.7.0".to_string(),
            instance: "i-1".to_string(),
            cloud: Cloud::OpenStack,
            cores: 4,
            memory_bytes: 1 << 33,
            executors: vec![Executor {
                kind: ExecutorKind::Cpu,
                slots: 4,
            }],
            boot_ms: 2140,
            health: NodeHealth::Ok,
        },
        vec![TokenKey::new(signer.public())],
        vec![crate::FunctionKey::new(signer.public())],
        alloc::boxed::Box::new(crate::NoEngine),
    )
}

fn post(token: Option<&str>) -> Request {
    Request {
        method: "POST".to_string(),
        path: MCP_PATH.to_string(),
        query: None,
        authorization: token.map_or(Authorization::Missing, |token| {
            Authorization::One(format!("Bearer {token}").into_bytes())
        }),
        close: false,
    }
}

/// The answer to an MCP message, `body`, posted with `token`.
fn send(api: &mut Api, token: Option<&str>, body: &str) -> Answer {
    api.answer(&post(token), body.as_bytes(), NOW)
}

fn json_of(answer: &Answer) -> Value {
    assert_eq!(answer.status, 200);
    serde_json::from_slice(&answer.body).unwrap()
}

#[test]
fn initialize_ping_and_list_answer() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let token = signer.token("i-1", NOW + 60, &["observe"]);
    let answer = send(
        &mut api,
        Some(&token),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    assert_eq!(
        json_of(&answer),
        json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "moruna", "version": "0.7.0" }
            }
        })
    );
    let answer = send(
        &mut api,
        Some(&token),
        r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#,
    );
    assert_eq!(
        json_of(&answer),
        json!({ "jsonrpc": "2.0", "id": "p", "result": {} })
    );
    let answer = send(
        &mut api,
        Some(&token),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let expected: Value = serde_json::from_str(TOOLS).unwrap();
    assert_eq!(json_of(&answer)["result"], expected);
    assert_eq!(
        expected["tools"].as_array().unwrap().len(),
        OPERATIONS.iter().filter(|op| op.has_tool()).count()
    );
    assert_eq!(
        api.recorded(),
        0,
        "only tools/call is a call of an operation"
    );
}

#[test]
fn notifications_and_responses_are_accepted_without_a_body() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let token = signer.token("i-1", NOW + 60, &["observe"]);
    for body in [
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":7,"result":{}}"#,
        r#"{"jsonrpc":"2.0","id":7,"error":{"code":1,"message":"no"}}"#,
    ] {
        let answer = send(&mut api, Some(&token), body);
        assert_eq!(answer.status, 202, "{body}");
        assert!(answer.body.is_empty());
    }
}

#[test]
fn what_json_rpc_refuses_gets_its_code() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let token = signer.token("i-1", NOW + 60, &["observe"]);
    for (body, code, id) in [
        ("{", -32_700, Value::Null),
        ("", -32_700, Value::Null),
        ("[]", -32_600, Value::Null),
        (
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
            -32_600,
            Value::Null,
        ),
        (r#"{"jsonrpc":"2.0","id":1}"#, -32_600, Value::Null),
        (
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            -32_600,
            Value::Null,
        ),
        (
            r#"{"jsonrpc":"2.0","id":[1],"method":"ping"}"#,
            -32_600,
            Value::Null,
        ),
        (
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            -32_600,
            json!(1),
        ),
        (r#"{"id":1,"method":"ping"}"#, -32_600, json!(1)),
        (r#"{"jsonrpc":"2.0","id":1,"method":5}"#, -32_600, json!(1)),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
            -32_601,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"notifications/initialized"}"#,
            -32_601,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#,
            -32_602,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":[]}"#,
            -32_602,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
            -32_602,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"moruna_jobs"}}"#,
            -32_602,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"node"}}"#,
            -32_602,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"moruna_node","arguments":[]}}"#,
            -32_602,
            json!(1),
        ),
    ] {
        let answer = json_of(&send(&mut api, Some(&token), body));
        assert_eq!(answer["error"]["code"], code, "{body}");
        assert_eq!(answer["id"], id, "{body}");
        assert_eq!(answer["jsonrpc"], "2.0");
        assert!(answer["error"]["message"].is_string());
    }
    assert_eq!(api.recorded(), 0);
}

#[test]
fn a_tool_call_runs_its_operation_and_is_recorded() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let observe = signer.token("i-1", NOW + 60, &["observe"]);
    let call = |name: &str, arguments: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"{name}"{arguments}}}}}"#
        )
    };
    let answer = json_of(&send(&mut api, Some(&observe), &call("moruna_node", "")));
    let result = &answer["result"];
    assert_eq!(result["structuredContent"], json!(api.facts.document(NOW)));
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    assert_eq!(result["content"][0]["type"], "text");
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    assert!(result.get("isError").is_none());

    let answer = json_of(&send(
        &mut api,
        Some(&observe),
        &call("moruna_health", r#","arguments":null"#),
    ));
    assert_eq!(
        answer["result"]["structuredContent"],
        json!({ "status": "ok" })
    );

    // Without the scope: a tool error with 2A's forbidden body.
    let answer = json_of(&send(
        &mut api,
        Some(&observe),
        &call("moruna_node_audit", r#","arguments":{}"#),
    ));
    let result = &answer["result"];
    assert_eq!(result["isError"], true);
    assert!(result.get("structuredContent").is_none());
    let error: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(error["error"]["code"], "forbidden");

    // An argument the operation does not take: a tool error with 2A's invalid body.
    let answer = json_of(&send(
        &mut api,
        Some(&observe),
        &call("moruna_node", r#","arguments":{"x":1}"#),
    ));
    let error: Value =
        serde_json::from_str(answer["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(error["error"]["code"], "invalid");

    let records = api.records();
    let key = TokenKey::new(signer.public());
    let seen: Vec<(&str, u16)> = records
        .iter()
        .map(|r| (r.operation.as_str(), r.status))
        .collect();
    assert_eq!(seen, [("node", 400), ("node_audit", 403), ("node", 200)]);
    assert!(
        records
            .iter()
            .all(|r| r.key == key.id() && r.scope == [Scope::Observe])
    );
}

#[test]
fn every_mcp_request_needs_a_token_for_observe_at_least() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let answer = send(&mut api, None, ping);
    assert_eq!(answer.status, 401);
    let stranger = Signer::new().token("i-1", NOW + 60, &["admin"]);
    assert_eq!(send(&mut api, Some(&stranger), ping).status, 401);
    let nothing = signer.token("i-1", NOW + 60, &[]);
    assert_eq!(send(&mut api, Some(&nothing), ping).status, 403);
    let admin = signer.token("i-1", NOW + 60, &["admin"]);
    assert_eq!(send(&mut api, Some(&admin), ping).status, 200);
    assert_eq!(api.recorded(), 0);
}

#[test]
fn only_post_is_served_on_mcp() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let mut get = post(None);
    get.method = "GET".to_string();
    let answer = api.answer(&get, &[], NOW);
    assert_eq!(answer.status, 405);
    assert_eq!(answer.allow, Some("POST"));
    assert!(answer.body.is_empty());
    let mut delete = post(None);
    delete.method = "DELETE".to_string();
    assert_eq!(api.answer(&delete, &[], NOW).status, 400);
    let token = signer.token("i-1", NOW + 60, &["observe"]);
    let mut query = post(Some(&token));
    query.query = Some(String::new());
    let ping = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    assert_eq!(api.answer(&query, ping, NOW).status, 400);
}

#[test]
fn the_tool_list_parses_and_a_bad_body_still_answers() {
    assert!(tools()["tools"].is_array());
    let failed = Answer::error(ErrorCode::Internal, "x");
    assert_eq!(tool_result(&failed)["isError"], true);
    let odd = Answer {
        status: 200,
        body: b"not json".to_vec(),
        close: false,
        allow: None,
    };
    assert_eq!(tool_result(&odd)["structuredContent"], Value::Null);
}
