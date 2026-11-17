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
            "cores": 4, "memory_bytes": 8589934592,
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
        r#"{{"version":"0.7.0","instance":"i-1","cloud":{cloud},"cores":1,"memory_bytes":1,"executors":[],"boot_ms":1,"time":1,"health":{health}}}"#
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
                op.response,
                op.tool_name(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "health",
                "GET",
                "/v1/health",
                None,
                Response::Health,
                "moruna_health".to_string()
            ),
            (
                "node",
                "GET",
                "/v1/node",
                Some(Scope::Observe),
                Response::Node,
                "moruna_node".to_string()
            ),
            (
                "node_audit",
                "GET",
                "/v1/node/audit",
                Some(Scope::Admin),
                Response::Audit,
                "moruna_node_audit".to_string()
            ),
        ]
    );
    assert!(OPERATIONS.iter().all(|op| !op.summary.is_empty()));
}
