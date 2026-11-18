//! Tokens and the audit record end to end, over TLS (item 2.3): each refusal with its answer,
//! the scopes, and what `node/audit` shows after the calls.
mod common;

use common::{Harness, INSTANCE, Response, WALL, signer::Signer};
use moruna_api_server::{AUDIT_ANSWER_LIMIT, Identity, TokenKey};
use serde_json::{Value, json};

/// A GET of `path` on a new connection carrying `token` (none when `None`).
fn call(harness: &mut Harness, path: &str, token: Option<&str>) -> Response {
    let client = harness.connect();
    let header = token.map(Harness::bearer).unwrap_or_default();
    harness.get(client, path, &header)
}

fn unauthorized(response: &Response) {
    assert_eq!(response.status, 401);
    assert_eq!(response.error_code(), "unauthorized");
    assert_eq!(response.header("www-authenticate"), Some("Bearer"));
}

#[test]
fn each_bad_token_is_refused_unauthorized() {
    let mut harness = Harness::new();
    let now = WALL;
    let good = harness.signer.token(INSTANCE, now + 600, &["observe"]);
    assert_eq!(call(&mut harness, "/v1/node", Some(&good)).status, 200);

    // A bad signature.
    let (input, signature) = good.rsplit_once('.').unwrap();
    let mut signature = signature.to_string().into_bytes();
    signature[10] = if signature[10] == b'A' { b'B' } else { b'A' };
    let forged = format!("{input}.{}", String::from_utf8(signature).unwrap());
    unauthorized(&call(&mut harness, "/v1/node", Some(&forged)));
    // A token signed by a key not in the list.
    let stranger = Signer::new().token(INSTANCE, now + 600, &["observe"]);
    unauthorized(&call(&mut harness, "/v1/node", Some(&stranger)));
    // A wrong aud.
    let elsewhere = harness.signer.token("i-other", now + 600, &["observe"]);
    unauthorized(&call(&mut harness, "/v1/node", Some(&elsewhere)));
    // An expired exp, and one that is now.
    for exp in [now - 1, now] {
        let expired = harness.signer.token(INSTANCE, exp, &["observe"]);
        unauthorized(&call(&mut harness, "/v1/node", Some(&expired)));
    }
    // An exp more than an hour ahead; exactly an hour is allowed.
    let long = harness.signer.token(INSTANCE, now + 3_601, &["observe"]);
    unauthorized(&call(&mut harness, "/v1/node", Some(&long)));
    let hour = harness.signer.token(INSTANCE, now + 3_600, &["observe"]);
    assert_eq!(call(&mut harness, "/v1/node", Some(&hour)).status, 200);
    // An alg other than EdDSA.
    let claims = format!(
        r#"{{"aud":"{INSTANCE}","exp":{},"scope":["observe"]}}"#,
        now + 600
    );
    for header in [
        r#"{"alg":"HS256","typ":"JWT"}"#,
        r#"{"alg":"none","typ":"JWT"}"#,
    ] {
        let token = harness.signer.sign(header, &claims);
        unauthorized(&call(&mut harness, "/v1/node", Some(&token)));
    }
    // A malformed token.
    for token in ["", "abc", "a.b.c", &good[1..], &format!("{good}.x")] {
        unauthorized(&call(&mut harness, "/v1/node", Some(token)));
    }
    // A missing header, and two of them.
    unauthorized(&call(&mut harness, "/v1/node", None));
    let client = harness.connect();
    let twice = format!("{}{}", Harness::bearer(&good), Harness::bearer(&good));
    unauthorized(&harness.get(client, "/v1/node", &twice));
}

#[test]
fn the_clock_moves_a_token_to_expiry() {
    let mut harness = Harness::new();
    let token = harness.signer.token(INSTANCE, WALL + 10, &["observe"]);
    let client = harness.connect();
    let header = Harness::bearer(&token);
    assert_eq!(harness.get(client, "/v1/node", &header).status, 200);
    harness.clock.advance(10_000);
    unauthorized(&harness.get(client, "/v1/node", &header));
}

/// When `platform` sets the node's wall clock again (system design 3D), the next call is judged
/// by it: each call is checked against the clock as it reads then, and nothing of a token is
/// kept between calls, so a step moves a token's expiry by the step and no more.
#[test]
fn a_token_is_judged_by_the_wall_clock_as_it_reads_at_each_call() {
    let mut harness = Harness::new();
    let token = harness.signer.token(INSTANCE, WALL + 10, &["observe"]);
    let header = Harness::bearer(&token);
    harness.clock.advance(10_000);
    // A refusal closes its connection: each call has its own.
    let client = harness.connect();
    unauthorized(&harness.get(client, "/v1/node", &header));
    // Set 400 ms back: the token is in its last second again.
    let wall = &harness.clock.unix_ms;
    wall.set(wall.get() - 400);
    let client = harness.connect();
    assert_eq!(harness.get(client, "/v1/node", &header).status, 200);
    // Set 400 ms on again: expired, as before the step.
    let wall = &harness.clock.unix_ms;
    wall.set(wall.get() + 400);
    let client = harness.connect();
    unauthorized(&harness.get(client, "/v1/node", &header));
}

#[test]
fn observe_reads_node_but_not_audit_and_admin_reads_both() {
    let mut harness = Harness::new();
    let client = harness.connect();
    assert_eq!(harness.get_as(client, "/v1/node", &["observe"]).status, 200);
    let forbidden = harness.get_as(client, "/v1/node/audit", &["observe"]);
    assert_eq!(forbidden.status, 403);
    assert_eq!(forbidden.error_code(), "forbidden");
    let forbidden = harness.get_as(client, "/v1/node/audit", &["submit"]);
    assert_eq!(forbidden.status, 403);
    assert_eq!(harness.get_as(client, "/v1/node", &["submit"]).status, 200);
    assert_eq!(harness.get_as(client, "/v1/node", &["admin"]).status, 200);
    assert_eq!(
        harness.get_as(client, "/v1/node/audit", &["admin"]).status,
        200
    );
    let nothing = harness.get_as(client, "/v1/node", &[]);
    assert_eq!(nothing.status, 403);
    // A refusal keeps the connection.
    assert!(!harness.clients[client].server_closed());
}

#[test]
fn health_needs_no_token_and_never_reads_one() {
    let mut harness = Harness::new();
    assert_eq!(call(&mut harness, "/v1/health", None).status, 200);
    assert_eq!(
        call(&mut harness, "/v1/health", Some("not.a.token")).status,
        200
    );
}

#[test]
fn any_of_the_node_keys_verifies_and_the_record_names_which() {
    let first = Signer::new();
    let second = Signer::new();
    let keys = vec![
        TokenKey::new(first.public()),
        TokenKey::new(second.public()),
    ];
    let identity = Identity::generate(INSTANCE, WALL).unwrap();
    let mut harness = Harness::with(identity, keys.clone(), first);
    let by_second = second.token(INSTANCE, WALL + 600, &["observe"]);
    assert_eq!(call(&mut harness, "/v1/node", Some(&by_second)).status, 200);
    let records = audit(&mut harness);
    assert_eq!(records[0]["key"], keys[1].id());
    assert_eq!(records[0]["operation"], "node");
}

/// The audit record, read with an admin token on a new connection.
fn audit(harness: &mut Harness) -> Vec<Value> {
    let token = harness.token(&["admin"]);
    let response = call(harness, "/v1/node/audit", Some(&token));
    assert_eq!(response.status, 200);
    response.json()["records"].as_array().unwrap().clone()
}

#[test]
fn each_call_appears_in_audit_newest_first() {
    let mut harness = Harness::new();
    let key = TokenKey::new(harness.signer.public());
    let key = key.id().to_string();
    harness.clock.advance(1_000);
    assert_eq!(call(&mut harness, "/v1/health", None).status, 200);
    harness.clock.advance(1_000);
    let observe = harness.token(&["observe"]);
    assert_eq!(call(&mut harness, "/v1/node", Some(&observe)).status, 200);
    harness.clock.advance(1_000);
    assert_eq!(call(&mut harness, "/v1/node", None).status, 401);
    harness.clock.advance(1_000);
    assert_eq!(
        call(&mut harness, "/v1/node/audit", Some(&observe)).status,
        403
    );
    // Not calls of an operation: no record.
    assert_eq!(call(&mut harness, "/v1/nothing", None).status, 404);
    harness.clock.advance(1_000);
    let records = audit(&mut harness);
    assert_eq!(
        records,
        [
            json!({ "time": WALL + 4, "key": key, "scope": ["observe"], "operation": "node_audit", "status": 403 }),
            json!({ "time": WALL + 3, "key": "", "scope": [], "operation": "node", "status": 401 }),
            json!({ "time": WALL + 2, "key": key, "scope": ["observe"], "operation": "node", "status": 200 }),
        ],
        "health leaves no record"
    );
    // The audit call itself is recorded once it has answered.
    let records = audit(&mut harness);
    assert_eq!(records.len(), 4);
    assert_eq!(
        records[0],
        json!({ "time": WALL + 5, "key": key, "scope": ["admin"], "operation": "node_audit", "status": 200 })
    );
}

#[test]
fn audit_answers_the_newest_1_000_calls() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let calls = AUDIT_ANSWER_LIMIT + 1;
    // Pipelined in batches, so that the test stays quick.
    let request = format!(
        "GET /v1/node HTTP/1.1\r\nHost: node\r\n{}\r\n",
        Harness::bearer(&harness.token(&["observe"]))
    );
    let mut sent = 0;
    while sent < calls {
        let batch = (calls - sent).min(250);
        harness.clients[client].send(request.repeat(batch).as_bytes());
        harness.settle();
        for _ in 0..batch {
            assert_eq!(harness.clients[client].response().unwrap().status, 200);
        }
        sent += batch;
    }
    let records = audit(&mut harness);
    assert_eq!(records.len(), AUDIT_ANSWER_LIMIT);
    assert!(records.iter().all(|r| r["operation"] == "node"));
}
