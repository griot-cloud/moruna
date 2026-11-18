//! MCP's messages, each answered as the transport and JSON-RPC 2.0 say.

use super::*;
use crate::{
    api::NodeFacts,
    http::{Authorization, Request},
    signer::Signer,
    token::TokenKey,
};
use alloc::{format, string::ToString, sync::Arc, vec, vec::Vec};
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
    serde_json::from_slice(&answer.body.bytes()).unwrap()
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
        OPERATIONS.len()
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
        assert!(answer.body.bytes().is_empty());
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
    assert!(answer.body.bytes().is_empty());
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
    let result = |answer: Answer| -> Value {
        let reply = json_of(&tool_answer(&json!("x"), answer));
        assert_eq!(
            (&reply["jsonrpc"], &reply["id"]),
            (&json!("2.0"), &json!("x"))
        );
        reply["result"].clone()
    };
    let written = result(Answer::error(ErrorCode::Internal, "x"));
    assert_eq!(written["isError"], true);
    assert!(written.get("structuredContent").is_none());
    let error: Value =
        serde_json::from_str(written["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(error["error"]["code"], "internal");
    let written = result(Answer::with(200, Body::of(Text::Static("not \"json\"\n"))));
    assert!(written.get("structuredContent").is_none());
    assert!(written.get("isError").is_none());
    assert_eq!(written["content"][0]["text"], "not \"json\"\n");
    // The answer's JSON is written as it came, its spacing and order kept, and shared, not
    // copied: the reply holds the one text, twice.
    let spaced: Arc<str> = Arc::from(r#"{ "b": 1,  "a": [2] }"#);
    let reply = tool_answer(
        &json!(9),
        Answer::with(201, Body::of(Text::Shared(spaced.clone()))),
    );
    assert_eq!(Arc::strong_count(&spaced), 2);
    let text = String::from_utf8(reply.body.bytes()).unwrap();
    assert!(
        text.contains(r#""structuredContent":{ "b": 1,  "a": [2] }"#),
        "{text}"
    );
    assert!(
        text.contains(r#""text":"{ \"b\": 1,  \"a\": [2] }""#),
        "{text}"
    );
    assert_eq!(reply.status, 200);
    // The tool list is the checked-in file, as it is.
    let call = r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#;
    let signer = Signer::new();
    let token = signer.token("i-1", NOW + 60, &["observe"]);
    let listed = send(&mut api(&signer), Some(&token), call).body.bytes();
    let listed = String::from_utf8(listed).unwrap();
    assert!(
        listed.ends_with(&format!(r#""result":{TOOLS}}}"#)),
        "{listed}"
    );
}

/// A job whose engine has told `records` records, its first then log records, and has ended
/// when `ended`, on an API whose engine is the tests' own; and an observer's token.
fn api_with_job(signer: &Signer, records: usize, ended: bool) -> (Api, String) {
    let engine = crate::fake::Engine::new();
    let mut api = Api::new(
        api(signer).facts.clone(),
        vec![TokenKey::new(signer.public())],
        Vec::new(),
        alloc::boxed::Box::new(engine.clone()),
    );
    let submit = signer.token("i-1", NOW + 60, &["submit"]);
    let job = r#"{"v1":{"moruna_spec":1}}"#;
    let mut request = post(Some(&submit));
    request.path = "/v1/jobs".to_string();
    let outcome = match api.admit(&request, job.len(), NOW) {
        crate::api::Admission::Body(call) => api.complete(call, job.as_bytes(), NOW),
        other => panic!("{other:?}"),
    };
    let crate::api::Outcome::Wait(ticket) = outcome else {
        panic!("the submit waits for the engine");
    };
    engine.attempt_started();
    for n in 1..records {
        engine.log("source", &format!("line {n}"));
    }
    if ended {
        engine.finish(r#"{"rows_out":3}"#, 0, None);
    }
    api.pump(NOW);
    assert_eq!(api.ready(ticket).unwrap().status, 201);
    (api, signer.token("i-1", NOW + 60, &["observe"]))
}

/// The events' tool called with `arguments`: its result.
fn events(api: &mut Api, token: &str, arguments: &str) -> Value {
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"moruna_jobs_events","arguments":{arguments}}}}}"#
    );
    json_of(&send(api, Some(token), &call))["result"].clone()
}

#[test]
fn the_events_tool_answers_a_page_after_from() {
    let signer = Signer::new();
    let (mut api, observe) = api_with_job(&signer, 3, true);
    let run = crate::fake::RUN;
    let whole = events(&mut api, &observe, &format!(r#"{{"id":"{run}"}}"#));
    let page = &whole["structuredContent"];
    let names: Vec<&str> = page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["event"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["event", "event", "event", "report", "exit"]);
    assert_eq!(page["next"], 3);
    let text: Value = serde_json::from_str(whole["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&text, page);
    // After the second record: the third, then the end.
    let rest = events(&mut api, &observe, &format!(r#"{{"id":"{run}","from":2}}"#));
    let records = rest["structuredContent"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["id"], 3);
    assert_eq!(records[0]["data"]["text"], "line 2");
    // No such job: the operation's not_found.
    let missing = events(&mut api, &observe, r#"{"id":"ffff","from":0}"#);
    assert_eq!(missing["isError"], true);
    assert!(
        missing["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(r#""code":"not_found""#)
    );
    // Each call is one record of jobs_events.
    let seen: Vec<(String, u16)> = api
        .records()
        .iter()
        .map(|r| (r.operation.clone(), r.status))
        .collect();
    assert_eq!(
        seen,
        [
            ("jobs_events".to_string(), 404),
            ("jobs_events".to_string(), 200),
            ("jobs_events".to_string(), 200),
            ("jobs_submit".to_string(), 201)
        ]
    );
}

#[test]
fn the_events_tool_takes_the_run_id_and_from_and_nothing_else() {
    let signer = Signer::new();
    let (mut api, observe) = api_with_job(&signer, 1, false);
    let run = crate::fake::RUN;
    for arguments in [
        "{}".to_string(),
        r#"{"from":0}"#.to_string(),
        r#"{"id":7}"#.to_string(),
        format!(r#"{{"id":"{run}","from":-1}}"#),
        format!(r#"{{"id":"{run}","from":1.5}}"#),
        format!(r#"{{"id":"{run}","from":"1"}}"#),
        format!(r#"{{"id":"{run}","from":null}}"#),
        format!(r#"{{"id":"{run}","to":3}}"#),
    ] {
        let refused = events(&mut api, &observe, &arguments);
        assert_eq!(refused["isError"], true, "{arguments}");
        let error: Value =
            serde_json::from_str(refused["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(error["error"]["code"], "invalid", "{arguments}");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("from, a record's number"),
            "{arguments}"
        );
    }
    // The largest from is taken, and gives what a job still running has after it: nothing.
    let far = events(
        &mut api,
        &observe,
        &format!(r#"{{"id":"{run}","from":{}}}"#, u64::MAX),
    );
    assert_eq!(
        far["structuredContent"],
        json!({ "records": [], "next": u64::MAX })
    );
    // Only the events' tool takes from.
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"moruna_jobs_get","arguments":{{"id":"{run}","from":1}}}}}}"#
    );
    let refused = &json_of(&send(&mut api, Some(&observe), &call))["result"];
    assert!(
        refused["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("the run's id, and nothing else")
    );
}
