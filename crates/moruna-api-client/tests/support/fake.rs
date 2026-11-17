//! A stand-in for `moruna-api-server` (item 2.2) until it is merged: the three operations of
//! `kernel/api/openapi.json`, served on the host over TLS 1.3 by rustls, with 2C's tokens and
//! audit record.
//!
//! It is as small as the clients' tests need: one request per connection, answered with
//! `Connection: close`; operations, scopes and answers from `moruna-api`'s table and types; a
//! token is checked as 2C says (an EdDSA JWT under one of the keys, `aud` this node, `exp` in the
//! future and at most an hour ahead, a scope that covers the operation), and every call leaves a
//! record, newest first in `node/audit`. Nothing here is what the clients are tested against for
//! good: [`super::TestNode::start`] is the one place that names it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use moruna_api::{
    Audit, AuditRecord, ErrorBody, ErrorCode, Health, HealthStatus, Node, OPERATIONS, Response,
};
use ring::digest::{SHA256, digest};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use super::cert::Identity;
use super::{now, token};

/// What the stand-in serves: the node's instance ID and document, its token keys and its TLS
/// identity. These are what node config and `platform` give the real server.
pub struct Config {
    pub instance: String,
    pub document: Node,
    pub token_keys: Vec<Vec<u8>>,
    pub identity: Identity,
}

/// The longest request head read.
const HEAD_LIMIT: usize = 16 * 1024;
/// An hour, the furthest ahead a token's `exp` may be.
const HOUR: u64 = 3600;

struct State {
    config: Config,
    audit: Mutex<Vec<AuditRecord>>,
}

/// Serve on a port of 127.0.0.1 until the test process ends; the address it listens on.
pub fn serve(config: Config) -> SocketAddr {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(config.identity.certificate.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(config.identity.key.clone())),
        )
        .expect("the identity's certificate and key");
    let tls = Arc::new(tls);
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port on 127.0.0.1");
    let address = listener.local_addr().expect("the listener's address");
    let state = Arc::new(State {
        config,
        audit: Mutex::new(Vec::new()),
    });
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (tls, state) = (tls.clone(), state.clone());
            thread::spawn(move || connection(stream, tls, &state));
        }
    });
    address
}

/// One connection: the handshake, one request, its answer, then close. A connection the client
/// gives up on (a refused certificate) ends here without an answer, and without a record.
fn connection(stream: TcpStream, tls: Arc<ServerConfig>, state: &State) {
    let Ok(server) = ServerConnection::new(tls) else {
        return;
    };
    let mut stream = StreamOwned::new(server, stream);
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
        if head.len() > HEAD_LIMIT {
            return;
        }
    }
    let (status, body) = answer(state, &String::from_utf8_lossy(&head));
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    stream.conn.send_close_notify();
    let _ = stream.flush();
}

/// The status and body of the answer to a request head, and its audit record.
fn answer(state: &State, head: &str) -> (u16, String) {
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (request.next().unwrap_or(""), request.next().unwrap_or(""));
    let authorization = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    });
    let Some(operation) = OPERATIONS.iter().find(|op| op.path == path) else {
        return refuse(state, "", None, ErrorCode::NotFound, "no such path");
    };
    if operation.method.as_str() != method {
        return refuse(
            state,
            operation.id,
            None,
            ErrorCode::Invalid,
            "no such method",
        );
    }
    let mut caller = None;
    if let Some(needed) = operation.scope {
        let claims = authorization
            .as_deref()
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|jwt| token::verify(jwt, &state.config.token_keys))
            .filter(|(_, claims)| {
                claims.aud == state.config.instance
                    && claims.exp > now()
                    && claims.exp <= now() + HOUR
            });
        let Some((key, claims)) = claims else {
            return refuse(
                state,
                operation.id,
                None,
                ErrorCode::Unauthorized,
                "no valid token",
            );
        };
        caller = Some((key_id(&state.config.token_keys[key]), claims.scope.clone()));
        if !claims.scope.iter().any(|scope| scope.covers(needed)) {
            return refuse(
                state,
                operation.id,
                caller,
                ErrorCode::Forbidden,
                "the token's scope does not cover the operation",
            );
        }
    }
    let body = match operation.response {
        Response::Health => serde_json::to_string(&Health {
            status: HealthStatus::Ok,
        }),
        Response::Node => serde_json::to_string(&state.config.document),
        Response::Audit => {
            let records = state.audit.lock().expect("the audit").clone();
            serde_json::to_string(&Audit { records })
        }
    }
    .expect("an answer serializes");
    record(state, operation.id, caller, 200);
    (200, body)
}

/// An error answer, recorded.
fn refuse(
    state: &State,
    operation: &str,
    caller: Option<(String, Vec<moruna_api::Scope>)>,
    code: ErrorCode,
    message: &str,
) -> (u16, String) {
    record(state, operation, caller, code.status());
    let body = serde_json::to_string(&ErrorBody::new(code, message)).expect("an error serializes");
    (code.status(), body)
}

/// One call's record, newest first.
fn record(
    state: &State,
    operation: &str,
    caller: Option<(String, Vec<moruna_api::Scope>)>,
    status: u16,
) {
    let (key, scope) = caller.unwrap_or_default();
    state.audit.lock().expect("the audit").insert(
        0,
        AuditRecord {
            time: now(),
            key,
            scope,
            operation: operation.to_string(),
            status,
        },
    );
}

/// A token key's ID: the first 16 hex characters of the SHA-256 of its public bytes (2.1).
pub fn key_id(public: &[u8]) -> String {
    digest(&SHA256, public).as_ref()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
