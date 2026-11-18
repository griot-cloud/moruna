//! The client's arguments, its reading of answers and its errors, without a node. The calls
//! themselves are tested against a node in `tests/clients.rs`.

use moruna_api::{ErrorCode, Node, Scope};

use super::*;

const PIN: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

#[test]
fn the_table_has_every_operation_the_client_calls() {
    for id in ["health", "node", "node_audit"] {
        assert_eq!(operation(id).unwrap().id, id);
    }
    assert_eq!(operation("node").unwrap().scope, Some(Scope::Observe));
    assert_eq!(operation("node_audit").unwrap().scope, Some(Scope::Admin));
    assert_eq!(operation("health").unwrap().scope, None);
    assert!(matches!(operation("jobs"), Err(Error::NoOperation(id)) if id == "jobs"));
}

#[test]
fn only_https_urls_are_called() {
    for url in [
        "http://203.0.113.7",
        "ftp://203.0.113.7",
        "203.0.113.7",
        "https://",
    ] {
        match Client::new(url, "t", None) {
            Err(Error::Url { url: given, .. }) => assert_eq!(given, url),
            other => panic!("{url}: expected the URL to be refused, got {other:?}"),
        }
    }
    let refused = Client::new("http://203.0.113.7", "t", None).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "the URL \"http://203.0.113.7\" cannot be called: a node's API is served over HTTPS only"
    );
}

#[test]
fn the_url_keeps_its_host_and_port_only() {
    let client = Client::new(
        "https://user:secret@node.example:8443/some/path?q=1#f",
        "t",
        Some(PIN),
    )
    .unwrap();
    assert_eq!(client.base.as_str(), "https://node.example:8443/");
}

#[test]
fn a_token_an_http_header_cannot_carry_is_refused() {
    assert!(matches!(
        Client::new("https://203.0.113.7", "line\nbreak", None),
        Err(Error::Token)
    ));
}

#[test]
fn the_token_is_never_printed() {
    let client = Client::new("https://203.0.113.7", "secret-token", Some(PIN)).unwrap();
    let printed = format!("{client:?}");
    assert!(!printed.contains("secret-token"), "{printed}");
}

#[test]
fn a_bad_pin_is_refused_before_any_call() {
    match Client::new("https://203.0.113.7", "t", Some("abc")) {
        Err(Error::Pin(pin)) => assert_eq!(pin, "abc"),
        other => panic!("expected the pin to be refused, got {other:?}"),
    }
}

#[test]
fn a_client_is_built_with_and_without_a_pin() {
    let pinned = Client::new("https://203.0.113.7", "t", Some(PIN)).unwrap();
    assert_eq!(pinned.pin.unwrap().to_string(), PIN);
    let verified = Client::new("https://203.0.113.7:443", "t", None).unwrap();
    assert!(verified.pin.is_none());
}

#[test]
fn a_success_is_read_as_the_operation_document() {
    let body = br#"{"version":"0.7.0","instance":"i","cloud":"azure","cores":2,
        "memory_bytes":1,"job_memory_bytes":1,"executors":[],"boot_ms":3,"time":4,"health":"degraded"}"#;
    let node: Node = answer("node", 200, body).unwrap();
    assert_eq!((node.cores, node.instance.as_str()), (2, "i"));
}

#[test]
fn a_success_that_is_not_the_document_is_a_decode_error() {
    let error = answer::<Node>("node", 200, b"{\"version\":1}").unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Decode {
                operation: "node",
                ..
            }
        ),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .starts_with("the node's answer to node is not its document: ")
    );
}

#[test]
fn an_error_answer_carries_the_node_error() {
    let body = br#"{"error":{"code":"busy","message":"try later"}}"#;
    let error = answer::<Node>("node", 503, body).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the node refused node: busy (503): try later"
    );
    assert!(
        matches!(error, Error::Api { status: 503, error, .. } if error.code == ErrorCode::Busy)
    );
}

#[test]
fn an_error_answer_without_an_error_body() {
    let error = answer::<Node>("node", 502, b"Bad Gateway").unwrap_err();
    assert_eq!(
        error.to_string(),
        "the node answered node with status 502 and no error body"
    );
}

#[test]
fn error_codes_are_named_as_the_api_writes_them() {
    for code in [
        ErrorCode::Invalid,
        ErrorCode::Unauthorized,
        ErrorCode::Forbidden,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::Busy,
        ErrorCode::Internal,
    ] {
        assert_eq!(
            serde_json::to_value(code).unwrap(),
            serde_json::Value::from(code_name(code))
        );
    }
}

#[derive(Debug)]
struct Layer(&'static str, Option<Box<Layer>>);

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Layer {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.1.as_deref().map(|layer| layer as _)
    }
}

#[test]
fn a_chain_says_each_cause_once() {
    let error = Layer(
        "error sending request: connection refused",
        Some(Box::new(Layer(
            "client error (Connect)",
            Some(Box::new(Layer("connection refused", None))),
        ))),
    );
    assert_eq!(
        chain(&error),
        "error sending request: connection refused: client error (Connect)"
    );
}

#[test]
fn the_other_errors_say_what_failed() {
    let cases = [
        (
            Error::Token,
            "the token cannot be sent: it holds characters an HTTP header cannot carry",
        ),
        (
            Error::Pin("x".into()),
            "the pin \"x\" is not a SHA-256 fingerprint: 64 hexadecimal digits",
        ),
        (
            Error::Build("no roots".into()),
            "the HTTP client could not be built: no roots",
        ),
        (
            Error::NoOperation("jobs".into()),
            "the API has no operation \"jobs\"",
        ),
        (
            Error::Transport {
                operation: "node",
                url: "https://203.0.113.7/v1/node".into(),
                reason: "refused".into(),
            },
            "could not call node at https://203.0.113.7/v1/node: refused",
        ),
    ];
    for (error, said) in cases {
        assert_eq!(error.to_string(), said);
    }
}
