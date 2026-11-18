//! The types against the JSON the system design writes (sections 2A and 2C).

use super::*;
use alloc::{string::ToString, vec};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Parses `text` as `T`, writes it back, and checks the result equals `text` field for field.
fn round_trip<T: Serialize + DeserializeOwned>(text: &str) -> T {
    let parsed: T = serde_json::from_str(text).expect("the design's JSON parses");
    let written = serde_json::to_value(&parsed).expect("the type serializes");
    let original: Value = serde_json::from_str(text).expect("the design's JSON is JSON");
    assert_eq!(written, original, "{text} does not round-trip");
    parsed
}

fn refused<T: DeserializeOwned>(text: &str) -> String {
    match serde_json::from_str::<T>(text) {
        Ok(_) => panic!("{text} was accepted"),
        Err(error) => error.to_string(),
    }
}

const SCOPES: [Scope; 3] = [Scope::Observe, Scope::Submit, Scope::Admin];

#[test]
fn health_round_trips() {
    let health: Health = round_trip(r#"{"status":"ok"}"#);
    assert_eq!(
        health,
        Health {
            status: HealthStatus::Ok
        }
    );
    refused::<Health>(r#"{"status":"degraded"}"#);
    refused::<Health>(r#"{}"#);
}

#[test]
fn node_round_trips_the_design_document() {
    let node: Node = round_trip(
        r#"{
            "version": "0.7.0", "instance": "<id>", "cloud": "openstack",
            "cores": 4, "memory_bytes": 8589934592, "job_memory_bytes": 6849298432,
            "executors": [{ "kind": "cpu", "slots": 4 }],
            "boot_ms": 2140, "time": 1794931200, "health": "ok"
        }"#,
    );
    assert_eq!(
        node,
        Node {
            version: "0.7.0".into(),
            instance: "<id>".into(),
            cloud: Cloud::OpenStack,
            cores: 4,
            memory_bytes: 8_589_934_592,
            job_memory_bytes: 6_849_298_432,
            executors: vec![Executor {
                kind: ExecutorKind::Cpu,
                slots: 4,
            }],
            boot_ms: 2140,
            time: 1_794_931_200,
            health: NodeHealth::Ok,
        }
    );
}

/// A node document with this cloud and health, each a JSON value.
fn node_json(cloud: &str, health: &str) -> String {
    format!(
        r#"{{"version":"0.7.0","instance":"i-1","cloud":{cloud},"cores":1,"memory_bytes":1,"job_memory_bytes":1,"executors":[],"boot_ms":1,"time":1,"health":{health}}}"#
    )
}

#[test]
fn node_takes_each_cloud_and_health() {
    for (name, cloud) in [
        ("openstack", Cloud::OpenStack),
        ("azure", Cloud::Azure),
        ("none", Cloud::None),
    ] {
        for (health_name, health) in [("ok", NodeHealth::Ok), ("degraded", NodeHealth::Degraded)] {
            let node: Node = round_trip(&node_json(
                &format!(r#""{name}""#),
                &format!(r#""{health_name}""#),
            ));
            assert_eq!((node.cloud, node.health), (cloud, health));
        }
    }
}

#[test]
fn node_refuses_values_outside_the_design() {
    refused::<Node>(&node_json(r#""aws""#, r#""ok""#));
    refused::<Node>(&node_json(r#""openstack""#, r#""down""#));
    refused::<Node>(r#"{"version":"0.7.0"}"#);
    refused::<Executor>(r#"{"kind":"gpu","slots":1}"#);
    refused::<Executor>(r#"{"kind":"cpu","slots":-1}"#);
}

#[test]
fn audit_round_trips_newest_first() {
    let audit: Audit = round_trip(
        r#"{"records":[
            {"time":1794931260,"key":"0123456789abcdef","scope":["admin"],"operation":"node_audit","status":200},
            {"time":1794931200,"key":"","scope":[],"operation":"node","status":401}
        ]}"#,
    );
    assert_eq!(
        audit.records,
        vec![
            AuditRecord {
                time: 1_794_931_260,
                key: "0123456789abcdef".into(),
                scope: vec![Scope::Admin],
                operation: "node_audit".into(),
                status: 200,
            },
            AuditRecord {
                time: 1_794_931_200,
                key: String::new(),
                scope: vec![],
                operation: "node".into(),
                status: 401,
            },
        ]
    );
    refused::<AuditRecord>(
        r#"{"time":1,"key":"","scope":["root"],"operation":"node","status":200}"#,
    );
}

#[test]
fn every_error_code_round_trips_with_its_status() {
    let expected = [
        ("invalid", ErrorCode::Invalid, 400),
        ("unauthorized", ErrorCode::Unauthorized, 401),
        ("forbidden", ErrorCode::Forbidden, 403),
        ("not_found", ErrorCode::NotFound, 404),
        ("conflict", ErrorCode::Conflict, 409),
        ("busy", ErrorCode::Busy, 503),
        ("internal", ErrorCode::Internal, 500),
    ];
    for (name, code, status) in expected {
        let text = format!(r#"{{"error":{{"code":"{name}","message":"<text>"}}}}"#);
        let body: ErrorBody = round_trip(&text);
        assert_eq!(body, ErrorBody::new(code, "<text>"));
        assert_eq!(code.status(), status, "{name}");
    }
    refused::<ErrorBody>(r#"{"error":{"code":"teapot","message":""}}"#);
    refused::<ErrorBody>(r#"{"code":"invalid","message":""}"#);
}

#[test]
fn claims_round_trip_the_design_example() {
    let claims: Claims =
        round_trip(r#"{ "aud": "<instance id>", "exp": 1794934800, "scope": ["observe"] }"#);
    assert_eq!(
        claims,
        Claims {
            aud: "<instance id>".into(),
            exp: 1_794_934_800,
            scope: vec![Scope::Observe],
        }
    );
    let all: Claims = round_trip(r#"{"aud":"i-1","exp":1,"scope":["observe","submit","admin"]}"#);
    assert_eq!(all.scope, SCOPES);
}

#[test]
fn claims_refuse_an_unknown_field() {
    let error = refused::<Claims>(r#"{"aud":"i-1","exp":1,"scope":["observe"],"iat":0}"#);
    assert!(error.contains("unknown field `iat`"), "{error}");
    refused::<Claims>(r#"{"aud":"i-1","exp":1}"#);
    refused::<Claims>(r#"{"aud":"i-1","exp":1,"scope":["root"]}"#);
}

#[test]
fn covers_is_right_for_all_nine_pairs() {
    // (held, needed, covers)
    let table = [
        (Scope::Observe, Scope::Observe, true),
        (Scope::Observe, Scope::Submit, false),
        (Scope::Observe, Scope::Admin, false),
        (Scope::Submit, Scope::Observe, true),
        (Scope::Submit, Scope::Submit, true),
        (Scope::Submit, Scope::Admin, false),
        (Scope::Admin, Scope::Observe, true),
        (Scope::Admin, Scope::Submit, true),
        (Scope::Admin, Scope::Admin, true),
    ];
    for (held, needed, covers) in table {
        assert_eq!(held.covers(needed), covers, "{held:?} covers {needed:?}");
    }
}

#[test]
fn scope_names_match_their_json() {
    for scope in SCOPES {
        assert_eq!(
            serde_json::to_value(scope).expect("a scope serializes"),
            Value::String(scope.as_str().into())
        );
    }
}

#[test]
fn the_operations_table_is_the_design_table() {
    let rows: Vec<_> = OPERATIONS
        .iter()
        .map(|op| {
            (
                op.id,
                op.method.as_str(),
                op.path,
                op.scope,
                op.request,
                op.response,
                op.status,
            )
        })
        .collect();
    let observe = Some(Scope::Observe);
    let submit = Some(Scope::Submit);
    let (none, job) = (Request::None, Request::Job);
    assert_eq!(
        rows,
        vec![
            (
                "health",
                "GET",
                "/v1/health",
                None,
                none,
                Response::Health,
                200
            ),
            (
                "node",
                "GET",
                "/v1/node",
                observe,
                none,
                Response::Node,
                200
            ),
            (
                "node_audit",
                "GET",
                "/v1/node/audit",
                Some(Scope::Admin),
                none,
                Response::Audit,
                200
            ),
            // 3B's table, row for row.
            (
                "jobs_validate",
                "POST",
                "/v1/jobs/validate",
                submit,
                job,
                Response::Validation,
                200
            ),
            (
                "jobs_submit",
                "POST",
                "/v1/jobs",
                submit,
                job,
                Response::Submitted,
                201
            ),
            (
                "jobs_list",
                "GET",
                "/v1/jobs",
                observe,
                none,
                Response::Jobs,
                200
            ),
            (
                "jobs_get",
                "GET",
                "/v1/jobs/{id}",
                observe,
                none,
                Response::JobStatus,
                200
            ),
            (
                "jobs_cancel",
                "POST",
                "/v1/jobs/{id}/cancel",
                submit,
                none,
                Response::JobStatus,
                200
            ),
            (
                "jobs_events",
                "GET",
                "/v1/jobs/{id}/events",
                observe,
                none,
                Response::Events,
                200
            ),
            (
                "jobs_report",
                "GET",
                "/v1/jobs/{id}/report",
                observe,
                none,
                Response::Report,
                200
            ),
            // 3E's two.
            (
                "functions_register",
                "POST",
                "/v1/functions",
                Some(Scope::Admin),
                Request::Function,
                Response::Function,
                201
            ),
            (
                "functions_list",
                "GET",
                "/v1/functions",
                observe,
                none,
                Response::Functions,
                200
            ),
        ]
    );
    assert!(OPERATIONS.iter().all(|op| !op.summary.is_empty()));
    for op in OPERATIONS {
        let events = op.id == "jobs_events";
        // Every operation has a tool, the stream of events one that answers a page (item 3.7).
        assert_eq!(op.tool_name(), format!("moruna_{}", op.id));
        if events {
            assert_eq!(op.tool_summary(), EVENTS_TOOL_SUMMARY);
        } else {
            assert_eq!(op.tool_summary(), op.summary);
        }
        assert_eq!(op.query.is_empty(), !events, "{}", op.id);
    }
    assert_eq!(OPERATIONS[8].query, ["from"]);
}

#[test]
fn a_page_of_events_round_trips() {
    let page: JobEvents<Value> = round_trip(
        r#"{"records":[
            {"id":4,"event":"event","data":{"type":"event","kind":"log","text":"one"}},
            {"event":"note","data":{"type":"note","skipped_lines":1}},
            {"event":"report","data":{"type":"report","report":{"rows_out":3},"plan":null}},
            {"event":"exit","data":{"type":"exit","code":0,"diagnostic":null}}
        ],"next":4}"#,
    );
    assert_eq!(page.next, 4);
    let names: Vec<JobEventName> = page.records.iter().map(|record| record.event).collect();
    assert_eq!(
        names,
        [
            JobEventName::Event,
            JobEventName::Note,
            JobEventName::Report,
            JobEventName::Exit
        ]
    );
    assert_eq!(page.records[0].id, Some(4));
    assert_eq!(page.records[1].id, None);
    assert_eq!(page.records[3].data["code"], 0);
    let empty: JobEvents<Value> = round_trip(r#"{"records":[],"next":0}"#);
    assert!(empty.records.is_empty());
    refused::<JobEventName>(r#""heartbeat""#);
    assert_eq!((EVENTS_PAGE, EVENTS_PAGE_BYTES), (1_000, 1 << 20));
    assert!(FROM.contains("Last-Event-ID") && FROM.contains("after it"));
}

#[test]
fn a_path_matches_its_operation_and_gives_the_run_id() {
    let by_id = |id: &str| OPERATIONS.iter().find(|op| op.id == id).unwrap();
    let get = by_id("jobs_get");
    assert!(get.takes_id() && !by_id("jobs_list").takes_id());
    assert_eq!(get.matches("/v1/jobs/abc"), Some(Some("abc")));
    assert_eq!(get.matches("/v1/jobs/"), None, "an empty ID is no ID");
    assert_eq!(get.matches("/v1/jobs"), None);
    assert_eq!(get.matches("/v1/jobs/abc/"), None);
    assert_eq!(get.matches("/v1/jobs/abc/report"), None);
    assert_eq!(get.matches("/v2/jobs/abc"), None);
    assert_eq!(by_id("jobs_list").matches("/v1/jobs"), Some(None));
    assert_eq!(by_id("jobs_list").matches("/v1/jobs/"), None);
    let cancel = by_id("jobs_cancel");
    assert_eq!(cancel.matches("/v1/jobs/abc/cancel"), Some(Some("abc")));
    assert_eq!(cancel.matches("/v1/jobs/abc/events"), None);
    assert_eq!(cancel.path_for("abc"), "/v1/jobs/abc/cancel");
    assert_eq!(by_id("node").path_for("abc"), "/v1/node");
    // The path of `validate` is also a job path whose ID would be `validate`: the method tells
    // them apart.
    assert_eq!(get.matches("/v1/jobs/validate"), Some(Some("validate")));
    assert_eq!(
        by_id("jobs_validate").matches("/v1/jobs/validate"),
        Some(None)
    );
}

#[test]
fn a_job_is_its_document_in_one_version() {
    let v1: Job<Value> = round_trip(r#"{"v1":{"moruna_spec":1,"source":{"kind":"rows"}}}"#);
    assert_eq!(
        v1,
        Job::v1(serde_json::json!({"moruna_spec":1,"source":{"kind":"rows"}}))
    );
    let v2: Job<Value> = round_trip(r#"{"v2":"TUpCMg==","secrets":{"s3-key":"s3cr3t"}}"#);
    assert_eq!(v2.v2.as_deref(), Some("TUpCMg=="));
    assert_eq!(v2, Job::v2("TUpCMg==").with_secret("s3-key", "s3cr3t"));
    assert!(Job::<Value>::v2("TUpCMg==").secrets.is_empty());
    assert!(refused::<Job<Value>>(r#"{"v1":{},"v3":{}}"#).contains("v3"));
    assert!(refused::<Job<Value>>(r#"{"v2":7}"#).contains("string"));
    refused::<Job<Value>>(r#"{"v2":"TUpCMg==","secrets":["s3cr3t"]}"#);
    refused::<Job<Value>>(r#"{"v2":"TUpCMg==","secrets":{"k":7}}"#);
}

#[test]
fn the_job_answers_round_trip_3b() {
    let ok: Validation = round_trip(r#"{"ok":true}"#);
    assert!(ok.ok && ok.refusals.is_empty());
    let not: Validation = round_trip(
        r#"{"ok":false,"refusals":[{"field":"tasks[0].kind","message":"no such kind"}]}"#,
    );
    assert_eq!(not.refusals[0].field, "tasks[0].kind");
    let submitted: Submitted = round_trip(r#"{"id":"0123456789abcdef0123456789abcdef"}"#);
    assert_eq!(submitted.id.len(), 32);
    let jobs: Jobs = round_trip(
        r#"{"jobs":[
            {"id":"b","state":"running","submitted":1794931260,"started":1794931261},
            {"id":"a","state":"completed","submitted":1794931200,"started":1794931201,"ended":1794931230}
        ]}"#,
    );
    assert_eq!(jobs.jobs[0].state, JobState::Running);
    assert_eq!(jobs.jobs[0].ended, None);
    let status: JobStatus = round_trip(
        r#"{"id":"a","state":"cancelled","submitted":1,"started":2,"ended":3,
            "heartbeat":{"t_ms":1500,"committed_seq":4,"rows_out":10,"bytes_out":80,
                "active_workers":2,"ceiling_bytes":1024,"anon_bytes":512,"bottleneck":"sink",
                "staging_bytes":0},
            "exit_code":130}"#,
    );
    assert_eq!(status.exit_code, Some(130));
    assert_eq!(
        status.heartbeat.as_ref().unwrap().bottleneck.as_deref(),
        Some("sink")
    );
    let starting: JobStatus = round_trip(r#"{"id":"a","state":"starting","submitted":1}"#);
    assert!(starting.heartbeat.is_none() && starting.exit_code.is_none());
    // The engine's heartbeat reads whatever fields it leaves out or nulls, and others it adds.
    let heartbeat: Heartbeat = serde_json::from_str(
        r#"{"type":"heartbeat","t_ms":7,"committed_seq":null,"rows_out":1,"later":true}"#,
    )
    .unwrap();
    assert_eq!(
        (heartbeat.t_ms, heartbeat.committed_seq, heartbeat.rows_out),
        (7, None, 1)
    );
    for (name, state, ended) in [
        ("starting", JobState::Starting, false),
        ("running", JobState::Running, false),
        ("completed", JobState::Completed, true),
        ("failed", JobState::Failed, true),
        ("cancelled", JobState::Cancelled, true),
    ] {
        assert_eq!(
            serde_json::to_value(state).unwrap(),
            Value::String(name.into())
        );
        assert_eq!(state.ended(), ended);
    }
    refused::<JobState>(r#""paused""#);
    assert_eq!(
        (Method::Get.as_str(), Method::Post.as_str()),
        ("GET", "POST")
    );
}

#[test]
fn a_function_and_its_registration_round_trip_3e() {
    let registration: FunctionRegistration = round_trip(
        r#"{"name":"gate-score","address":"https://example.com/gate-score","sha256":"ab","signature":"c2ln"}"#,
    );
    assert_eq!(registration.name, "gate-score");
    assert!(
        refused::<FunctionRegistration>(r#"{"name":"a","address":"b","sha256":"c"}"#)
            .contains("signature")
    );
    assert!(
        refused::<FunctionRegistration>(
            r#"{"name":"a","address":"b","sha256":"c","signature":"d","key":"e"}"#
        )
        .contains("key")
    );
    let functions: Functions = round_trip(
        r#"{"functions":[{"name":"gate-score","address":"https://images.example.com/gate-score","sha256":"ab","key":"0123456789abcdef","registered":1794931200}]}"#,
    );
    assert_eq!(functions.functions[0].registered, 1794931200);
    let none: Functions = round_trip(r#"{"functions":[]}"#);
    assert!(none.functions.is_empty());
}
