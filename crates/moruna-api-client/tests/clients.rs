//! The three clients against a node API server on the host, over TLS, with a generated identity
//! (item 2.5, "Success"): the Rust client, the `moruna node` command and the Python client each
//! read `node` with an `observe` token, read `audit` with an `admin` token, and are refused with
//! the wrong pin.
//!
//! The server is [`support::TestNode`]; until item 2.2 is merged it is a stand-in, and swapping
//! in `moruna-api-server` changes `support` only.

mod support;

use std::path::Path;
use std::process::Command;

use moruna_api::{
    Audit, ErrorCode, Health, HealthStatus, Method, Node, OPERATIONS, Operation, Response, Scope,
};
use moruna_api_client::{Client, Error, cli};
use support::TestNode;

fn client(node: &TestNode, scope: Scope) -> Client {
    Client::new(&node.url, node.token(scope), Some(&node.fingerprint)).expect("a client")
}

#[test]
fn rust_client_reads_node_with_an_observe_token() {
    let node = TestNode::start();
    assert_eq!(client(&node, Scope::Observe).node().unwrap(), node.document);
}

#[test]
fn rust_client_reads_audit_with_an_admin_token() {
    let node = TestNode::start();
    client(&node, Scope::Observe).node().unwrap();
    let audit = client(&node, Scope::Admin).node_audit().unwrap();
    let newest = &audit.records[0];
    assert_eq!(newest.operation, "node");
    assert_eq!(newest.key, node.key_id());
    assert_eq!(newest.scope, vec![Scope::Observe]);
    assert_eq!(newest.status, 200);
}

#[test]
fn rust_client_is_refused_with_the_wrong_pin_and_sends_nothing() {
    let node = TestNode::start();
    let wrong = Client::new(&node.url, node.token(Scope::Admin), Some(&node.wrong_pin())).unwrap();
    match wrong.node() {
        Err(Error::PinMismatch { pin, found }) => {
            assert_eq!(pin.to_string(), node.wrong_pin());
            assert_eq!(found.to_string(), node.fingerprint);
        }
        other => panic!("expected the pin's refusal, got {other:?}"),
    }
    // The handshake was refused before the request was sent, so the node saw no call.
    let audit = client(&node, Scope::Admin).node_audit().unwrap();
    assert!(audit.records.is_empty(), "{audit:?}");
}

#[test]
fn rust_client_without_a_pin_verifies_the_certificate() {
    let node = TestNode::start();
    let unpinned = Client::new(&node.url, node.token(Scope::Observe), None).unwrap();
    match unpinned.node() {
        Err(Error::Transport {
            operation, reason, ..
        }) => {
            assert_eq!(operation, "node");
            assert!(reason.contains("UnknownIssuer"), "{reason}");
        }
        other => panic!("expected the certificate to be refused, got {other:?}"),
    }
}

#[test]
fn rust_client_health_needs_no_token() {
    let node = TestNode::start();
    let anyone = Client::new(&node.url, "not a token", Some(&node.fingerprint)).unwrap();
    assert_eq!(
        anyone.health().unwrap(),
        Health {
            status: HealthStatus::Ok
        }
    );
}

#[test]
fn rust_client_calls_every_operation_of_the_table() {
    let node = TestNode::start();
    let admin = client(&node, Scope::Admin);
    for operation in OPERATIONS {
        let answer: serde_json::Value = admin.call(operation).unwrap();
        assert!(answer.is_object(), "{}: {answer}", operation.id);
    }
    let audit = admin.node_audit().unwrap();
    let called: Vec<_> = audit.records.iter().map(|r| r.operation.as_str()).collect();
    assert_eq!(called, ["node_audit", "node", "health"]);
}

#[test]
fn rust_client_refusals_carry_the_node_error() {
    let node = TestNode::start();
    let forbidden = client(&node, Scope::Observe).node_audit().unwrap_err();
    assert!(
        matches!(&forbidden, Error::Api { operation: "node_audit", status: 403, error }
            if error.code == ErrorCode::Forbidden),
        "{forbidden:?}"
    );
    assert!(
        forbidden
            .to_string()
            .starts_with("the node refused node_audit: forbidden (403): "),
        "{forbidden}"
    );

    let elsewhere = node.key.token("another-instance", &[Scope::Admin], 600);
    let unauthorized = Client::new(&node.url, elsewhere, Some(&node.fingerprint))
        .unwrap()
        .node()
        .unwrap_err();
    assert!(
        matches!(&unauthorized, Error::Api { status: 401, error, .. }
            if error.code == ErrorCode::Unauthorized),
        "{unauthorized:?}"
    );

    let missing = Operation {
        id: "missing",
        method: Method::Get,
        path: "/v1/missing",
        scope: Some(Scope::Observe),
        response: Response::Node,
        summary: "",
    };
    let not_found = client(&node, Scope::Admin)
        .call::<Node>(&missing)
        .unwrap_err();
    assert!(
        matches!(&not_found, Error::Api { status: 404, error, .. }
            if error.code == ErrorCode::NotFound),
        "{not_found:?}"
    );
}

/// `moruna node` with these arguments: its exit code, stdout and stderr.
fn moruna_node(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::node(&args, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn cli_prints_the_node_document_with_an_observe_token() {
    let node = TestNode::start();
    let token = node.token(Scope::Observe);
    let (code, out, err) = moruna_node(&[
        "--url",
        &node.url,
        "--token",
        &token,
        "--pin",
        &node.fingerprint,
    ]);
    assert_eq!((code, err.as_str()), (0, ""));
    let printed: Node = serde_json::from_str(&out).expect("the node document as JSON");
    assert_eq!(printed, node.document);
}

#[test]
fn cli_prints_the_audit_with_an_admin_token() {
    let node = TestNode::start();
    client(&node, Scope::Observe).node().unwrap();
    let token = node.token(Scope::Admin);
    let (code, out, err) = moruna_node(&[
        "--audit",
        "--url",
        &node.url,
        "--pin",
        &node.fingerprint,
        "--token",
        &token,
    ]);
    assert_eq!((code, err.as_str()), (0, ""));
    let printed: Audit = serde_json::from_str(&out).expect("the audit as JSON");
    assert_eq!(printed.records[0].operation, "node");
    assert_eq!(printed.records[0].key, node.key_id());
}

#[test]
fn cli_is_refused_with_the_wrong_pin() {
    let node = TestNode::start();
    let token = node.token(Scope::Observe);
    let wrong = node.wrong_pin();
    let (code, out, err) = moruna_node(&["--url", &node.url, "--token", &token, "--pin", &wrong]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(
        err,
        format!(
            "moruna node: the server's certificate has SHA-256 {}, not the pinned {wrong}\n",
            node.fingerprint
        )
    );
}

#[test]
fn cli_reports_a_refusal_on_stderr() {
    let node = TestNode::start();
    let token = node.token(Scope::Observe);
    let (code, out, err) = moruna_node(&[
        "--url",
        &node.url,
        "--token",
        &token,
        "--pin",
        &node.fingerprint,
        "--audit",
    ]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(
        err.starts_with("moruna node: the node refused node_audit: forbidden (403): "),
        "{err}"
    );
}

#[test]
fn python_client_against_the_node() {
    let node = TestNode::start();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../..");
    // `MORUNA_API_TEST_PYTHON` names another interpreter, or a wrapper that measures coverage.
    let python = std::env::var("MORUNA_API_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let output = Command::new(&python)
        .arg(manifest.join("tests/python/client.py"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("MORUNA_TEST_PYTHON_ROOT", root.join("python"))
        .env("MORUNA_TEST_URL", &node.url)
        .env("MORUNA_TEST_PIN", &node.fingerprint)
        .env("MORUNA_TEST_WRONG_PIN", node.wrong_pin())
        .env("MORUNA_TEST_OBSERVE", node.token(Scope::Observe))
        .env("MORUNA_TEST_ADMIN", node.token(Scope::Admin))
        .env(
            "MORUNA_TEST_DOCUMENT",
            serde_json::to_string(&node.document).unwrap(),
        )
        .env("MORUNA_TEST_KEY_ID", node.key_id())
        .output()
        .unwrap_or_else(|error| panic!("{python} runs the Python client's tests: {error}"));
    assert!(
        output.status.success(),
        "the Python client's tests failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // unittest ends its report on stderr with OK only when every test passed.
    let report = String::from_utf8_lossy(&output.stderr);
    assert!(report.contains("\nOK"), "{report}");
}
