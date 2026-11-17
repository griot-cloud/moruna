//! Routing from the operations table, and each operation's answer.

use super::*;
use crate::http::{Authorization, Request};
use alloc::{string::ToString, vec};
use moruna_api::{ExecutorKind, Node};
use serde_json::{Value, json};

const NOW: u64 = 1_794_931_200;

fn facts() -> NodeFacts {
    NodeFacts {
        version: "0.7.0".to_string(),
        instance: "i-1".to_string(),
        cloud: Cloud::Azure,
        cores: 4,
        memory_bytes: 8_589_934_592,
        executors: vec![Executor {
            kind: ExecutorKind::Cpu,
            slots: 4,
        }],
        boot_ms: 2140,
        health: NodeHealth::Degraded,
    }
}

fn get(path: &str) -> Request {
    Request {
        method: "GET".to_string(),
        path: path.to_string(),
        query: false,
        authorization: Authorization::Missing,
        close: false,
        body: Vec::new(),
    }
}

fn body(answer: &Answer) -> Value {
    serde_json::from_slice(&answer.body).unwrap()
}

#[test]
fn each_operation_answers() {
    let mut api = Api::new(facts());
    let health = api.answer(&get("/v1/health"), NOW);
    assert_eq!(health.status, 200);
    assert_eq!(health.body, br#"{"status":"ok"}"#);
    let node = api.answer(&get("/v1/node"), NOW);
    assert_eq!(node.status, 200);
    assert_eq!(
        body(&node),
        json!({
            "version": "0.7.0", "instance": "i-1", "cloud": "azure",
            "cores": 4, "memory_bytes": 8_589_934_592u64,
            "executors": [{ "kind": "cpu", "slots": 4 }],
            "boot_ms": 2140, "time": NOW, "health": "degraded"
        })
    );
    let node: Node = serde_json::from_slice(&node.body).unwrap();
    assert_eq!(node, facts().document(NOW));
    let audit = api.answer(&get("/v1/node/audit"), NOW);
    assert_eq!(audit.status, 200);
    assert_eq!(body(&audit), json!({ "records": [] }));
}

#[test]
fn what_no_operation_takes_is_refused() {
    let mut api = Api::new(facts());
    let refused = |answer: Answer, status: u16, code: &str| {
        assert_eq!(answer.status, status);
        assert_eq!(body(&answer)["error"]["code"], code);
        assert!(!answer.close);
    };
    refused(api.answer(&get("/v1/nodes"), NOW), 404, "not_found");
    refused(api.answer(&get("/v1/node/"), NOW), 404, "not_found");
    refused(api.answer(&get("/"), NOW), 404, "not_found");
    let mut post = get("/v1/node");
    post.method = "POST".to_string();
    refused(api.answer(&post, NOW), 400, "invalid");
    let mut head = get("/v1/health");
    head.method = "HEAD".to_string();
    refused(api.answer(&head, NOW), 400, "invalid");
    let mut query = get("/v1/node");
    query.query = true;
    refused(api.answer(&query, NOW), 400, "invalid");
    let mut with_body = get("/v1/node");
    with_body.body = b"{}".to_vec();
    refused(api.answer(&with_body, NOW), 400, "invalid");
}

#[test]
fn errors_carry_their_code_and_status() {
    for code in [
        ErrorCode::Invalid,
        ErrorCode::Unauthorized,
        ErrorCode::Forbidden,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::Busy,
        ErrorCode::Internal,
    ] {
        let answer = Answer::error(code, "why");
        assert_eq!(answer.status, code.status());
        let parsed: ErrorBody = serde_json::from_slice(&answer.body).unwrap();
        assert_eq!(parsed, ErrorBody::new(code, "why"));
    }
    let failed = Answer::ok(Err(serde_json::from_slice::<Value>(b"{").unwrap_err()));
    assert_eq!(failed.status, 500);
}
