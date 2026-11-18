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
    Audit, ErrorCode, Health, HealthStatus, Job, JobState, Method, Node, OPERATIONS, Operation,
    Request, Response, Scope, Validation,
};
use moruna_api_client::{Client, Error, Event, cli};
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
    // Every operation that takes neither a run's ID nor a body.
    let plain = OPERATIONS
        .iter()
        .filter(|op| !op.takes_id() && op.request == Request::None);
    for operation in plain {
        let answer: serde_json::Value = admin.call(operation).unwrap();
        assert!(answer.is_object(), "{}: {answer}", operation.id);
    }
    let audit = admin.node_audit().unwrap();
    let called: Vec<_> = audit.records.iter().map(|r| r.operation.as_str()).collect();
    assert_eq!(
        called,
        [
            "functions_list",
            "jobs_list",
            "node_audit",
            "node",
            "health"
        ]
    );
}

#[test]
fn rust_client_registers_and_lists_functions() {
    use base64::Engine;
    let node = TestNode::start();
    let admin = client(&node, Scope::Admin);
    // 3E: Ed25519 over the 32 bytes of the image's SHA-256, by one of node config's keys.
    let signed = |key: &support::token::Key| {
        base64::engine::general_purpose::STANDARD.encode(key.sign(&[0xab; 32]))
    };
    let registration = moruna_api::FunctionRegistration {
        name: "gate-score".to_string(),
        address: "https://images.example.com/gate-score".to_string(),
        sha256: "ab".repeat(32),
        signature: signed(&node.function_key),
    };
    let function = admin.functions_register(&registration).unwrap();
    assert_eq!(
        (
            function.name.as_str(),
            function.sha256.as_str(),
            function.key.as_str()
        ),
        (
            "gate-score",
            registration.sha256.as_str(),
            support::fake::key_id(&node.function_key.public()).as_str()
        )
    );
    let listed = client(&node, Scope::Observe).functions_list().unwrap();
    assert_eq!(listed.functions, [function]);
    // Signed by a key node config does not hold, garbled, or of another image: refused.
    for signature in [
        signed(&support::token::Key::generate()),
        "c2lnbmVk".to_string(),
        "not base64!".to_string(),
    ] {
        let bad = moruna_api::FunctionRegistration {
            signature,
            ..registration.clone()
        };
        let Err(Error::Api { status, error, .. }) = admin.functions_register(&bad) else {
            panic!("a registration that does not verify is refused");
        };
        assert_eq!((status, error.code), (400, ErrorCode::Invalid));
        assert!(error.message.contains("does not verify"));
    }
    let other_image = moruna_api::FunctionRegistration {
        sha256: "cd".repeat(32),
        ..registration.clone()
    };
    assert!(admin.functions_register(&other_image).is_err());
    let plain = moruna_api::FunctionRegistration {
        address: "http://images.example.com/gate-score".to_string(),
        ..registration
    };
    let Err(Error::Api { error, .. }) = admin.functions_register(&plain) else {
        panic!("an address that is not https:// is refused");
    };
    assert!(error.message.contains("https://"));
}

#[test]
fn rust_client_runs_a_job_through_to_its_report() {
    let node = TestNode::start();
    let submit = client(&node, Scope::Submit);
    let job = Job::v1(serde_json::json!({ "moruna_spec": 1, "source": { "kind": "rows" } }));
    assert_eq!(
        submit.jobs_validate(&job).unwrap(),
        Validation {
            ok: true,
            refusals: vec![]
        }
    );
    let refused = Job::v1(serde_json::json!({ "refuse": "source.kind: no such kind" }));
    let validation = submit.jobs_validate(&refused).unwrap();
    assert!(!validation.ok);
    assert_eq!(validation.refusals[0].message, "source.kind: no such kind");
    match submit.jobs_submit(&refused) {
        Err(Error::Api {
            status: 400, error, ..
        }) => {
            assert_eq!(error.code, ErrorCode::Invalid);
            assert_eq!(error.message, "source.kind: no such kind");
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    let id = submit.jobs_submit(&job).unwrap().id;
    assert_eq!(id.len(), 32);
    assert!(matches!(
        submit.jobs_submit(&Job::<serde_json::Value>::v2("TUpCMg==")),
        Err(Error::Api { status: 503, error, .. }) if error.code == ErrorCode::Busy
    ));
    // Reading needs only observe; cancelling needs submit.
    let observe = client(&node, Scope::Observe);
    let status = observe.jobs_get(&id).unwrap();
    assert_eq!(status.state, JobState::Running);
    assert_eq!(status.heartbeat.unwrap().rows_out, 3);
    assert_eq!(observe.jobs_list().unwrap().jobs[0].id, id);
    assert!(matches!(
        observe.jobs_report(&id),
        Err(Error::Api { status: 409, error, .. }) if error.code == ErrorCode::Conflict
    ));
    assert!(matches!(
        observe.jobs_cancel(&id),
        Err(Error::Api { status: 403, .. })
    ));
    let running: Vec<Event> = observe
        .jobs_events(&id, None)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let names: Vec<&str> = running.iter().map(|event| event.name.as_str()).collect();
    assert_eq!(names, ["event", "event", "heartbeat"]);
    assert_eq!((running[0].id, running[2].id), (Some(1), None));
    assert_eq!(running[1].data["kind"], "task_started");
    assert_eq!(submit.jobs_cancel(&id).unwrap().state, JobState::Running);
    let status = observe.jobs_get(&id).unwrap();
    assert_eq!(
        (status.state, status.exit_code),
        (JobState::Cancelled, Some(130))
    );
    assert!(matches!(
        submit.jobs_cancel(&id),
        Err(Error::Api { status: 409, .. })
    ));
    // The ended job's events after its first record, to its exit; then its report.
    let ended: Vec<Event> = observe
        .jobs_events(&id, Some(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let names: Vec<&str> = ended.iter().map(|event| event.name.as_str()).collect();
    assert_eq!(names, ["event", "heartbeat", "report", "exit"]);
    assert_eq!(ended[0].id, Some(2));
    assert_eq!(ended[3].data["code"], 130);
    assert_eq!(
        observe.jobs_report(&id).unwrap(),
        serde_json::json!({ "rows_out": 3 })
    );
    // A job the node does not have.
    for refused in [
        observe.jobs_get("ffff").map(|_| ()),
        observe.jobs_report("ffff").map(|_| ()),
        observe.jobs_events("ffff", None).map(|_| ()),
    ] {
        assert!(
            matches!(refused, Err(Error::Api { status: 404, .. })),
            "{refused:?}"
        );
    }
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
        request: Request::None,
        query: &[],
        response: Response::Node,
        status: 200,
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

/// `moruna.register`'s signing, upload and registration (work item 5.4) against this node,
/// which checks each signature under its one function key, and a fake store over HTTPS.
#[test]
fn python_register_against_the_node() {
    let node = TestNode::start();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../..");
    let python = std::env::var("MORUNA_API_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let key: String = node
        .function_key
        .pkcs8()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let output = Command::new(&python)
        .arg(manifest.join("tests/python/register.py"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("MORUNA_TEST_PYTHON_ROOT", root.join("python"))
        .env("MORUNA_TEST_URL", &node.url)
        .env("MORUNA_TEST_PIN", &node.fingerprint)
        .env("MORUNA_TEST_OBSERVE", node.token(Scope::Observe))
        .env("MORUNA_TEST_ADMIN", node.token(Scope::Admin))
        .env("MORUNA_TEST_FUNCTION_KEY", key)
        .env(
            "MORUNA_TEST_FUNCTION_KEY_ID",
            support::fake::key_id(&node.function_key.public()),
        )
        .output()
        .unwrap_or_else(|error| panic!("{python} runs the registration's tests: {error}"));
    assert!(
        output.status.success(),
        "the registration's tests failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8_lossy(&output.stderr);
    assert!(report.contains("\nOK"), "{report}");
}
