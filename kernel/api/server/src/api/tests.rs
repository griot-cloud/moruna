//! Routing from the operations table, the token each operation needs, each operation's answer,
//! and the record each call leaves.

use super::*;
use crate::audit::AUDIT_LIMIT;
use crate::{http::Request, signer::Signer};
use alloc::{format, string::ToString, vec};
use moruna_api::{ExecutorKind, Node, Scope};
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

fn get(path: &str, token: Option<&str>) -> Request {
    Request {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        authorization: token.map_or(Authorization::Missing, |token| {
            Authorization::One(format!("Bearer {token}").into_bytes())
        }),
        close: false,
    }
}

fn body(answer: &Answer) -> Value {
    serde_json::from_slice(&answer.body.bytes()).unwrap()
}

/// An API whose one token key is `signer`'s.
fn api(signer: &Signer) -> Api {
    Api::new(
        facts(),
        vec![TokenKey::new(signer.public())],
        vec![crate::FunctionKey::new(signer.public())],
        Box::new(crate::NoEngine),
    )
}

#[test]
fn each_operation_answers_a_token_whose_scope_covers_it() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let admin = signer.token("i-1", NOW + 60, &["admin"]);
    let health = api.answer(&get("/v1/health", None), &[], NOW);
    assert_eq!(health.status, 200);
    assert_eq!(health.body.bytes(), br#"{"status":"ok"}"#);
    let node = api.answer(&get("/v1/node", Some(&admin)), &[], NOW);
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
    let node: Node = serde_json::from_slice(&node.body.bytes()).unwrap();
    assert_eq!(node, facts().document(NOW));
    let audit = api.answer(&get("/v1/node/audit", Some(&admin)), &[], NOW);
    assert_eq!(audit.status, 200);
    assert_eq!(
        body(&audit)["records"].as_array().unwrap().len(),
        1,
        "the node call; health leaves no record"
    );
}

#[test]
fn health_never_reads_the_header() {
    let signer = Signer::new();
    let mut api = api(&signer);
    for token in [None, Some("garbage")] {
        assert_eq!(api.answer(&get("/v1/health", token), &[], NOW).status, 200);
    }
    assert_eq!(api.recorded(), 0, "health leaves no record");
    // Not even refused.
    let mut query = get("/v1/health", None);
    query.query = Some(String::new());
    assert_eq!(api.answer(&query, &[], NOW).status, 400);
    assert_eq!(api.recorded(), 0);
}

#[test]
fn what_no_operation_takes_is_refused() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let admin = signer.token("i-1", NOW + 60, &["admin"]);
    let refused = |answer: Answer, status: u16, code: &str| {
        assert_eq!(answer.status, status);
        assert_eq!(body(&answer)["error"]["code"], code);
        assert!(!answer.close);
    };
    refused(
        api.answer(&get("/v1/nodes", None), &[], NOW),
        404,
        "not_found",
    );
    refused(
        api.answer(&get("/v1/node/", None), &[], NOW),
        404,
        "not_found",
    );
    refused(api.answer(&get("/", None), &[], NOW), 404, "not_found");
    let mut post = get("/v1/node", Some(&admin));
    post.method = "POST".to_string();
    refused(api.answer(&post, &[], NOW), 400, "invalid");
    let mut head = get("/v1/health", None);
    head.method = "HEAD".to_string();
    refused(api.answer(&head, &[], NOW), 400, "invalid");
    assert_eq!(api.recorded(), 0, "no operation was called");
    let mut query = get("/v1/node", Some(&admin));
    query.query = Some(String::new());
    refused(api.answer(&query, &[], NOW), 400, "invalid");
    let with_body = get("/v1/node", Some(&admin));
    refused(api.answer(&with_body, b"{}", NOW), 400, "invalid");
    // A token is checked before the request is.
    let mut no_token = get("/v1/node", None);
    no_token.query = Some("x=1".to_string());
    refused(api.answer(&no_token, &[], NOW), 401, "unauthorized");
    assert_eq!(api.recorded(), 3);
}

#[test]
fn a_token_is_needed_and_its_scope_must_cover_the_operation() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let observe = signer.token("i-1", NOW + 60, &["observe"]);
    let submit = signer.token("i-1", NOW + 60, &["submit"]);
    let none = signer.token("i-1", NOW + 60, &[]);
    let status = |api: &mut Api, path: &str, token: Option<&str>| {
        let answer = api.answer(&get(path, token), &[], NOW);
        if answer.status != 200 {
            let code = body(&answer)["error"]["code"].as_str().unwrap().to_string();
            assert_eq!(
                ErrorCode::status(match code.as_str() {
                    "unauthorized" => ErrorCode::Unauthorized,
                    "forbidden" => ErrorCode::Forbidden,
                    other => panic!("{other}"),
                }),
                answer.status
            );
        }
        answer.status
    };
    assert_eq!(status(&mut api, "/v1/node", None), 401);
    assert_eq!(status(&mut api, "/v1/node", Some(&observe)), 200);
    assert_eq!(status(&mut api, "/v1/node", Some(&submit)), 200);
    assert_eq!(status(&mut api, "/v1/node", Some(&none)), 403);
    assert_eq!(status(&mut api, "/v1/node/audit", Some(&observe)), 403);
    assert_eq!(status(&mut api, "/v1/node/audit", Some(&submit)), 403);
    // A scope is checked before the request is: a query does not hide a forbidden call.
    let mut query = get("/v1/node/audit", Some(&observe));
    query.query = Some("x=1".to_string());
    assert_eq!(api.answer(&query, &[], NOW).status, 403);
    let audit = api.records();
    assert_eq!(audit[0].operation, "node_audit");
    assert_eq!(audit[0].status, 403);
    assert_eq!(audit[1].scope, [Scope::Submit]);
    assert_eq!(audit[1].key, TokenKey::new(signer.public()).id());
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
        let parsed: ErrorBody = serde_json::from_slice(&answer.body.bytes()).unwrap();
        assert_eq!(parsed, ErrorBody::new(code, "why"));
    }
    let failed = Answer::ok(Err(serde_json::from_slice::<Value>(b"{").unwrap_err()));
    assert_eq!(failed.status, 500);
}

#[test]
fn audit_answers_the_newest_1000_of_the_10000_it_keeps() {
    let signer = Signer::new();
    let mut api = api(&signer);
    let grant = crate::token::Grant {
        key: TokenKey::new(signer.public()).id().to_string(),
        scope: vec![Scope::Admin],
    };
    let node = &OPERATIONS[1];
    for second in 0..=AUDIT_LIMIT as u64 {
        let outcome = api.call_tool(node, &grant, Inputs::default(), &Value::Null, NOW + second);
        assert!(matches!(outcome, Outcome::Answer(answer) if answer.status == 200));
    }
    assert_eq!(
        api.recorded(),
        AUDIT_LIMIT,
        "after 10,001 calls, exactly 10,000"
    );
    let admin = signer.token("i-1", NOW + 10_600, &["admin"]);
    let audit = api.answer(&get("/v1/node/audit", Some(&admin)), &[], NOW + 10_001);
    let records = body(&audit)["records"].as_array().unwrap().clone();
    assert_eq!(records.len(), AUDIT_ANSWER_LIMIT);
    assert_eq!(records[0]["time"], NOW + AUDIT_LIMIT as u64, "newest first");
    assert_eq!(
        records[AUDIT_ANSWER_LIMIT - 1]["time"],
        NOW + (AUDIT_LIMIT - AUDIT_ANSWER_LIMIT + 1) as u64
    );
}
