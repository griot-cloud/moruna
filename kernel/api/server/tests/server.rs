//! The server end to end, through real rustls clients over an in-memory transport (item 2.2):
//! the handshake, each operation's answer field for field as 2A, and each limit.
mod common;

use common::{Harness, INSTANCE, WALL, pinned};
use moruna_api_server::{
    BODY_LIMIT, CONNECTION_LIMIT, HEADER_LIMIT, IDLE_LIMIT_MS, Identity, NodeFacts,
};
use rustls::ClientConfig;
use serde_json::json;
use std::sync::Arc;

#[test]
fn a_pinned_client_completes_the_handshake_on_tls_1_3_and_http_1_1() {
    let mut harness = Harness::new();
    let client = harness.connect();
    harness.settle();
    let tls = &harness.clients[client].tls;
    assert!(!tls.is_handshaking());
    assert_eq!(
        tls.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_3)
    );
    assert_eq!(tls.alpn_protocol(), Some(&b"http/1.1"[..]));
    let presented = tls.peer_certificates().unwrap();
    assert_eq!(presented.len(), 1);
}

#[test]
fn a_client_pinned_to_another_fingerprint_is_refused_by_itself() {
    let mut harness = Harness::new();
    let wrong = pinned(&"0".repeat(64));
    let client = harness.connect_with(wrong);
    harness.settle();
    assert!(harness.clients[client].error.is_some());
}

#[test]
fn health_answers_as_2a() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let response = harness.get(client, "/v1/health", "");
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(response.body, br#"{"status":"ok"}"#);
    assert_eq!(response.header("connection"), None, "kept alive");
}

#[test]
fn node_answers_the_node_document_field_for_field() {
    let mut harness = Harness::new();
    let client = harness.connect();
    harness.clock.advance(5_000);
    let response = harness.get_as(client, "/v1/node", &["observe"]);
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(
        response.json(),
        json!({
            "version": "0.7.0", "instance": INSTANCE, "cloud": "openstack",
            "cores": 4, "memory_bytes": 8_589_934_592u64,
            "executors": [{ "kind": "cpu", "slots": 4 }],
            "boot_ms": 2140, "time": WALL + 5, "health": "ok"
        })
    );
}

#[test]
fn the_node_document_follows_the_facts_the_caller_changes() {
    let mut harness = Harness::new();
    let client = harness.connect();
    *harness.server.facts_mut() = NodeFacts {
        health: moruna_api::NodeHealth::Degraded,
        cloud: moruna_api::Cloud::None,
        ..common::facts()
    };
    let body = harness.get_as(client, "/v1/node", &["observe"]).json();
    assert_eq!(body["health"], "degraded");
    assert_eq!(body["cloud"], "none");
}

#[test]
fn audit_answers_its_list() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let response = harness.get_as(client, "/v1/node/audit", &["admin"]);
    assert_eq!(response.status, 200);
    assert!(response.json()["records"].is_array());
}

#[test]
fn routes_come_from_the_operations_table_only() {
    let mut harness = Harness::new();
    let client = harness.connect();
    for path in ["/", "/v1", "/v1/node/", "/v2/node", "/v1/jobs", "/V1/NODE"] {
        let response = harness.get(client, path, "");
        assert_eq!(response.status, 404, "{path}");
        assert_eq!(response.error_code(), "not_found");
    }
    for method in ["POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH"] {
        let request =
            format!("{method} /v1/node HTTP/1.1\r\nHost: node\r\nContent-Length: 0\r\n\r\n");
        let response = harness.exchange(client, request.as_bytes());
        assert_eq!(response.status, 400, "{method}");
        assert_eq!(response.error_code(), "invalid");
    }
    // Every refusal above kept the connection.
    assert!(!harness.clients[client].server_closed());
    assert_eq!(harness.get(client, "/v1/health", "").status, 200);
}

#[test]
fn keep_alive_serves_many_requests_and_close_is_honoured() {
    let mut harness = Harness::new();
    let client = harness.connect();
    for _ in 0..5 {
        assert_eq!(harness.get(client, "/v1/health", "").status, 200);
    }
    // Pipelined: three requests in one write, three answers in order.
    let bearer = Harness::bearer(&harness.token(&["observe"]));
    harness.clients[client].send(
        format!(
            "GET /v1/health HTTP/1.1\r\nHost: node\r\n\r\nGET /v1/node HTTP/1.1\r\nHost: node\r\n{bearer}\r\nGET /nope HTTP/1.1\r\nHost: node\r\n\r\n"
        )
        .as_bytes(),
    );
    harness.settle();
    let statuses: Vec<u16> = (0..3)
        .map(|_| harness.clients[client].response().unwrap().status)
        .collect();
    assert_eq!(statuses, [200, 200, 404]);
    let response = harness.get(client, "/v1/health", "Connection: close\r\n");
    assert_eq!(response.status, 200);
    assert_eq!(response.header("connection"), Some("close"));
    assert!(
        harness.clients[client].closed_by_server,
        "close_notify sent"
    );
    assert!(harness.clients[client].server_closed());
    assert_eq!(harness.server.connections(), 0);
}

#[test]
fn a_client_that_hangs_up_after_its_request_gets_its_answer_then_a_close() {
    let mut harness = Harness::new();
    let client = harness.connect();
    harness.settle();
    harness.clients[client].send(b"GET /v1/health HTTP/1.1\r\nHost: node\r\n\r\n");
    harness.clients[client].pump();
    harness.clients[client].hang_up();
    harness.settle();
    assert_eq!(harness.clients[client].response().unwrap().status, 200);
    assert!(harness.clients[client].server_closed());
    assert_eq!(harness.server.connections(), 0);
}

#[test]
fn a_client_close_notify_after_pipelined_requests_gets_every_answer_then_a_close() {
    let mut harness = Harness::new();
    let client = harness.connect();
    harness.settle();
    let bearer = Harness::bearer(&harness.token(&["observe"]));
    harness.clients[client].send(
        format!(
            "GET /v1/health HTTP/1.1\r\nHost: node\r\n\r\nGET /v1/node HTTP/1.1\r\nHost: node\r\n{bearer}\r\n"
        )
        .as_bytes(),
    );
    harness.clients[client].tls.send_close_notify();
    harness.settle();
    assert_eq!(harness.clients[client].response().unwrap().status, 200);
    assert_eq!(harness.clients[client].response().unwrap().status, 200);
    assert!(harness.clients[client].closed_by_server);
    assert!(harness.clients[client].server_closed());
}

#[test]
fn headers_over_16_kib_are_invalid_then_close() {
    let mut harness = Harness::new();
    let client = harness.connect();
    // At the limit: answered, and the connection stays.
    let start = "GET /v1/health HTTP/1.1\r\nHost: node\r\nX-Pad: ";
    let pad = "a".repeat(HEADER_LIMIT - start.len() - 4);
    let at_limit = format!("{start}{pad}\r\n\r\n");
    assert_eq!(at_limit.len(), HEADER_LIMIT);
    assert_eq!(harness.exchange(client, at_limit.as_bytes()).status, 200);
    assert!(!harness.clients[client].server_closed());
    // One byte over.
    let over = format!("{start}{pad}a\r\n\r\n");
    let response = harness.exchange(client, over.as_bytes());
    assert_eq!(response.status, 400);
    assert_eq!(response.error_code(), "invalid");
    assert_eq!(response.header("connection"), Some("close"));
    assert!(harness.clients[client].server_closed());
}

#[test]
fn a_body_over_1_mib_is_invalid_then_close() {
    let mut harness = Harness::new();
    let client = harness.connect();
    // At the limit the body is read whole and the request answered (GET takes no body).
    let bearer = Harness::bearer(&harness.token(&["observe"]));
    let mut at_limit = format!(
        "GET /v1/node HTTP/1.1\r\nHost: node\r\n{bearer}Content-Length: {BODY_LIMIT}\r\n\r\n"
    )
    .into_bytes();
    at_limit.resize(at_limit.len() + BODY_LIMIT, b'x');
    let response = harness.exchange(client, &at_limit);
    assert_eq!(response.status, 400);
    assert_eq!(response.header("connection"), None, "kept alive");
    assert_eq!(harness.get(client, "/v1/health", "").status, 200);
    // One byte over: refused on its head, before any of the body.
    let over = format!(
        "GET /v1/node HTTP/1.1\r\nHost: node\r\nContent-Length: {}\r\n\r\n",
        BODY_LIMIT + 1
    );
    let response = harness.exchange(client, over.as_bytes());
    assert_eq!(response.status, 400);
    assert_eq!(response.error_code(), "invalid");
    assert_eq!(response.header("connection"), Some("close"));
    assert!(harness.clients[client].server_closed());
}

#[test]
fn the_65th_connection_is_busy_then_closed() {
    let mut harness = Harness::new();
    let clients: Vec<usize> = (0..CONNECTION_LIMIT).map(|_| harness.connect()).collect();
    harness.settle();
    assert_eq!(harness.server.served(), CONNECTION_LIMIT);
    let extra = harness.connect();
    assert_eq!(harness.server.connections(), CONNECTION_LIMIT + 1);
    assert_eq!(harness.server.served(), CONNECTION_LIMIT);
    let response = harness.get(extra, "/v1/health", "");
    assert_eq!(response.status, 503);
    assert_eq!(response.error_code(), "busy");
    assert_eq!(response.header("connection"), Some("close"));
    assert!(harness.clients[extra].server_closed());
    assert_eq!(harness.server.connections(), CONNECTION_LIMIT);
    // The 64 are served throughout.
    for &client in &clients {
        assert_eq!(harness.get(client, "/v1/health", "").status, 200);
    }
    // Once one closes, a new connection is served.
    harness.get(clients[0], "/v1/health", "Connection: close\r\n");
    let next = harness.connect();
    assert_eq!(harness.get(next, "/v1/health", "").status, 200);
}

#[test]
fn a_connection_idle_for_60_s_is_closed() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let silent = harness.connect();
    harness.settle();
    assert_eq!(harness.server.deadline(), Some(1_000 + IDLE_LIMIT_MS));
    harness.clock.advance(IDLE_LIMIT_MS - 1);
    assert_eq!(harness.get(client, "/v1/health", "").status, 200);
    assert!(!harness.clients[silent].server_closed(), "not yet");
    harness.clock.advance(1);
    harness.settle();
    assert!(harness.clients[silent].server_closed());
    assert!(!harness.clients[client].server_closed());
    assert_eq!(harness.server.connections(), 1);
    // A request half sent does not keep a connection either.
    harness.clients[client].send(b"GET /v1/health HTTP/1.1\r\nHo");
    harness.settle();
    harness.clock.advance(IDLE_LIMIT_MS);
    harness.settle();
    assert!(harness.clients[client].server_closed());
    assert!(harness.clients[client].response().is_none());
    assert_eq!(harness.server.deadline(), None);
}

#[test]
fn a_client_that_never_finishes_its_handshake_is_closed_when_idle() {
    let mut harness = Harness::new();
    let client = harness.connect();
    // The client's hello never leaves it.
    harness.server.poll();
    harness.clock.advance(IDLE_LIMIT_MS);
    harness.server.poll();
    assert!(harness.clients[client].server_closed());
}

#[test]
fn a_client_that_hangs_up_during_the_handshake_is_closed() {
    let mut harness = Harness::new();
    let client = harness.connect();
    harness.clients[client].hang_up();
    harness.server.poll();
    assert!(harness.clients[client].server_closed());
    assert_eq!(harness.server.connections(), 0);
}

#[test]
fn bytes_that_are_not_tls_close_the_connection() {
    let mut harness = Harness::new();
    let client = harness.connect();
    harness.clients[client]
        .pipe
        .borrow_mut()
        .to_server
        .extend(b"GET /v1/health HTTP/1.1\r\nHost: node\r\n\r\n");
    harness.server.poll();
    assert!(harness.clients[client].server_closed());
}

#[test]
fn a_broken_transport_is_closed() {
    let mut harness = Harness::new();
    let client = harness.connect();
    harness.settle();
    harness.clients[client].pipe.borrow_mut().broken = true;
    harness.server.poll();
    assert!(harness.clients[client].server_closed());
    assert_eq!(harness.server.connections(), 0);
}

#[test]
fn a_client_that_reads_slowly_gets_every_byte() {
    let mut harness = Harness::new();
    let client = harness.connect();
    harness.clients[client].pipe.borrow_mut().capacity = Some(100);
    harness.settle();
    let body = harness.get_as(client, "/v1/node", &["observe"]).json();
    assert_eq!(body["instance"], INSTANCE);
}

#[test]
fn a_tls_1_2_client_is_refused() {
    let mut harness = Harness::new();
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(common::Pin::new(&harness.fingerprint)))
            .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let client = harness.connect_with(Arc::new(config));
    harness.settle();
    assert!(harness.clients[client].server_closed());
    assert!(harness.clients[client].tls.is_handshaking());
}

/// Test-only credentials made with OpenSSL, standing in for an operator's: a P-256 key in SEC 1
/// form and an RSA key in PKCS #1 form, each with a self-signed certificate.
const OPERATORS: [(&str, &str); 2] = [
    (
        include_str!("fixtures/operator-ec.pem"),
        include_str!("fixtures/operator-ec.key"),
    ),
    (
        include_str!("fixtures/operator-rsa.pem"),
        include_str!("fixtures/operator-rsa.key"),
    ),
];

#[test]
fn the_operator_identity_is_presented_when_given() {
    for (certificate, key) in OPERATORS {
        let operator = Identity::from_pem(certificate, key).unwrap();
        let mut harness = Harness::with_identity(operator.clone());
        assert_eq!(harness.fingerprint, operator.fingerprint());
        let client = harness.connect();
        harness.settle();
        let presented = harness.clients[client].tls.peer_certificates().unwrap();
        assert_eq!(presented[0].as_ref(), operator.certificate());
        assert_eq!(harness.get(client, "/v1/health", "").status, 200);
    }
}
