//! A stand-in for `moruna-api-server` (item 2.2) until it is merged: the operations of
//! `kernel/api/openapi.json`, served on the host over TLS 1.3 by rustls, with 2C's tokens and
//! audit record. The job operations (3B) are served from a list of jobs the stand-in keeps: a
//! submitted job runs until it is cancelled, a document with a `refuse` member is refused with
//! its words, and a job's events are a fixed stream.
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
    Audit, AuditRecord, ErrorBody, ErrorCode, Health, HealthStatus, Job, JobState, JobStatus,
    JobSummary, Jobs, Node, OPERATIONS, Refusal, Response, Submitted, Validation,
};
use ring::digest::{SHA256, digest};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::Value;

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
    /// The jobs submitted, oldest first.
    jobs: Mutex<Vec<JobStatus>>,
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
        jobs: Mutex::new(Vec::new()),
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
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    let head = loop {
        if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
        if request.len() > HEAD_LIMIT {
            return;
        }
    };
    let text = String::from_utf8_lossy(&request[..head]).into_owned();
    let length: usize = text
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    while request.len() < head + length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let (status, body) = answer(state, &text, &request[head..head + length]);
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        409 => "Conflict",
        503 => "Service Unavailable",
        _ => "Not Found",
    };
    // A stream of events has no length: it ends when the connection closes.
    let response = if body.starts_with("id: ") || body.starts_with("event: ") {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\n\
             Connection: close\r\n\r\n{body}"
        )
    } else {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let _ = stream.write_all(response.as_bytes());
    stream.conn.send_close_notify();
    let _ = stream.flush();
}

/// The status and body of the answer to a request, and its audit record.
fn answer(state: &State, head: &str, body: &[u8]) -> (u16, String) {
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (request.next().unwrap_or(""), request.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let authorization = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    });
    let on_path: Vec<_> = OPERATIONS
        .iter()
        .filter_map(|op| op.matches(path).map(|id| (op, id)))
        .collect();
    let Some((any, _)) = on_path.first() else {
        return refuse(state, "", None, ErrorCode::NotFound, "no such path");
    };
    let Some((operation, id)) = on_path.iter().find(|(op, _)| op.method.as_str() == method) else {
        return refuse(state, any.id, None, ErrorCode::Invalid, "no such method");
    };
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
    let id = id.unwrap_or_default();
    let result = match operation.response {
        Response::Health => Ok(json(&Health {
            status: HealthStatus::Ok,
        })),
        Response::Node => Ok(json(&state.config.document)),
        Response::Audit => {
            let records = state.audit.lock().expect("the audit").clone();
            Ok(json(&Audit { records }))
        }
        Response::Validation => document(body).map(|refused| {
            json(&Validation {
                ok: refused.is_none(),
                refusals: refused
                    .map(|message| Refusal {
                        field: "spec".to_string(),
                        message,
                    })
                    .into_iter()
                    .collect(),
            })
        }),
        Response::Submitted => submit(state, body),
        Response::Jobs => {
            let jobs = state.jobs.lock().expect("the jobs");
            let jobs = jobs.iter().rev().map(summary).collect();
            Ok(json(&Jobs { jobs }))
        }
        Response::JobStatus if method == "POST" => cancel(state, id),
        Response::JobStatus => job(state, id).map(|job| json(&job)),
        Response::Report => job(state, id).and_then(|job| match job.state.ended() {
            true => Ok(r#"{"rows_out":3}"#.to_string()),
            false => Err((ErrorCode::Conflict, "the job has not ended".to_string())),
        }),
        Response::Events => job(state, id).map(|job| events(&job, query)),
    };
    match result {
        Ok(body) => {
            record(state, operation.id, caller, operation.status);
            (operation.status, body)
        }
        Err((code, message)) => refuse(state, operation.id, caller, code, &message),
    }
}

fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("an answer serializes")
}

fn summary(job: &JobStatus) -> JobSummary {
    JobSummary {
        id: job.id.clone(),
        state: job.state,
        submitted: job.submitted,
        started: job.started,
        ended: job.ended,
    }
}

/// The job in a body: `Ok(None)` when its document is sound, `Ok(Some(words))` when the
/// document says to refuse it (a `refuse` member), `Err` when the body is no job.
fn document(body: &[u8]) -> Result<Option<String>, (ErrorCode, String)> {
    let job: Job<Value> = serde_json::from_slice(body).map_err(|error| {
        (
            ErrorCode::Invalid,
            format!("the body is not a job: {error}"),
        )
    })?;
    match (&job.v1, &job.v2) {
        (Some(document), None) => Ok(document["refuse"].as_str().map(str::to_string)),
        (None, Some(_)) => Ok(None),
        _ => Err((
            ErrorCode::Invalid,
            "a job is exactly one of v1 and v2".to_string(),
        )),
    }
}

/// `POST /v1/jobs`: one job at a time; a refused document is `invalid` with its words.
fn submit(state: &State, body: &[u8]) -> Result<String, (ErrorCode, String)> {
    if let Some(words) = document(body)? {
        return Err((ErrorCode::Invalid, words));
    }
    let mut jobs = state.jobs.lock().expect("the jobs");
    if jobs.iter().any(|job| !job.state.ended()) {
        return Err((ErrorCode::Busy, "a job is running".to_string()));
    }
    let id = format!("{:032x}", jobs.len() + 1);
    jobs.push(JobStatus {
        id: id.clone(),
        state: JobState::Running,
        submitted: now(),
        started: Some(now()),
        ended: None,
        heartbeat: Some(moruna_api::Heartbeat {
            t_ms: 1500,
            rows_out: 3,
            bytes_out: 24,
            active_workers: 2,
            bottleneck: Some("sink".to_string()),
            ..Default::default()
        }),
        exit_code: None,
    });
    Ok(json(&Submitted { id }))
}

fn job(state: &State, id: &str) -> Result<JobStatus, (ErrorCode, String)> {
    let jobs = state.jobs.lock().expect("the jobs");
    jobs.iter()
        .find(|job| job.id == id)
        .cloned()
        .ok_or((ErrorCode::NotFound, "no job has this ID".to_string()))
}

/// `POST /v1/jobs/{id}/cancel`: the job ends cancelled, with exit code 130.
fn cancel(state: &State, id: &str) -> Result<String, (ErrorCode, String)> {
    let mut jobs = state.jobs.lock().expect("the jobs");
    let job = jobs
        .iter_mut()
        .find(|job| job.id == id)
        .ok_or((ErrorCode::NotFound, "no job has this ID".to_string()))?;
    if job.state.ended() {
        return Err((ErrorCode::Conflict, "the job has ended".to_string()));
    }
    let before = json(job);
    job.state = JobState::Cancelled;
    job.ended = Some(now());
    job.exit_code = Some(130);
    Ok(before)
}

/// A job's stream of events: two records, a heartbeat, and for an ended job its report and
/// exit; `from=<n>` leaves out the records before the `n`th.
fn events(job: &JobStatus, query: &str) -> String {
    let from: u64 = query
        .strip_prefix("from=")
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    let mut stream = String::new();
    for (id, kind) in [(1, "attempt_started"), (2, "task_started")] {
        if id >= from {
            stream.push_str(&format!(
                "id: {id}\nevent: event\ndata: {{\"type\":\"event\",\"v\":7,\"seq\":{id},\"kind\":\"{kind}\"}}\n\n"
            ));
        }
    }
    stream.push_str("event: heartbeat\ndata: {\"type\":\"heartbeat\",\"rows_out\":3}\n\n");
    if job.state.ended() {
        stream.push_str(
            "event: report\ndata: {\"type\":\"report\",\"report\":{\"rows_out\":3},\"plan\":null}\n\n",
        );
        stream.push_str(&format!(
            "event: exit\ndata: {{\"type\":\"exit\",\"code\":{},\"diagnostic\":null}}\n\n",
            job.exit_code.unwrap_or(0)
        ));
    }
    stream
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
