//! A blocking client of a Moruna node's API (system design, section 2E).
//!
//! The client is built on `moruna-api`, the crate the OpenAPI document is generated from: its
//! answers are that crate's types, and every call goes to the method and path that crate's
//! operations table ([`moruna_api::OPERATIONS`]) gives. A token is sent only to an operation the
//! table says needs one.
//!
//! ```no_run
//! # fn main() -> Result<(), moruna_api_client::Error> {
//! let pin = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
//! let client = moruna_api_client::Client::new("https://203.0.113.7", "<token>", Some(pin))?;
//! let node = client.node()?;
//! println!("{} cores on {}", node.cores, node.instance);
//! # Ok(())
//! # }
//! ```
//!
//! **Jobs** (system design 3B). A job is its document, as version 1 JSON or version 2's
//! FlatBuffers in base64; the node runs one at a time:
//!
//! ```no_run
//! # fn main() -> Result<(), moruna_api_client::Error> {
//! # let client = moruna_api_client::Client::new("https://203.0.113.7", "<token>", None)?;
//! use moruna_api_client::api::Job;
//! let job = Job::v1(serde_json::json!({ "moruna_spec": 1 }));
//! if client.jobs_validate(&job)?.ok {
//!     let id = client.jobs_submit(&job)?.id;
//!     for event in client.jobs_events(&id, None)? {
//!         let event = event?;
//!         println!("{}: {}", event.name, event.data);
//!     }
//!     println!("{}", client.jobs_report(&id)?);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! **Pinning.** With a pin, the server's certificate is accepted exactly when the SHA-256 of its
//! DER equals the pin, the fingerprint the node prints on its console
//! (`moruna: tls sha256=<hex>`); nothing else about the certificate is checked, though the TLS
//! handshake's own signature must still verify under the pinned certificate's key. Without a pin,
//! the certificate is verified as usual, against the system's trusted roots.
//!
//! The `moruna node` command is [`cli::node`].

use std::io::{BufRead, BufReader};

use moruna_api::{
    Audit, ErrorBody, ErrorCode, ErrorDetail, Health, Job, JobStatus, Jobs, Method, Node,
    Operation, Submitted, Validation,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub mod cli;
mod pin;

pub use moruna_api as api;
pub use pin::Fingerprint;

/// A client of one node's API.
#[derive(Debug)]
pub struct Client {
    /// The node's address: `https://`, a host and a port, nothing else.
    base: reqwest::Url,
    /// `Bearer <token>`, marked sensitive so that it is never printed.
    authorization: HeaderValue,
    /// The pin, when there is one.
    pin: Option<Fingerprint>,
    http: reqwest::blocking::Client,
    /// As `http`, with no limit on how long an answer takes: a stream of events lasts as long
    /// as its job.
    stream: reqwest::blocking::Client,
}

/// One Server-Sent Event of a job's stream ([`Client::jobs_events`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// The record's number in the job's stream, from 1, for an `event`; `None` for the others.
    /// A later call with `from` this number replays the records after this one, as Server-Sent
    /// Events' `Last-Event-ID` does.
    pub id: Option<u64>,
    /// What it is: `event` (an event or log record), `heartbeat`, `report` or `exit`.
    pub name: String,
    /// Its JSON: the event's line as `moruna_event` renders it, or the engine's message.
    pub data: Value,
}

/// A job's events as they arrive ([`Client::jobs_events`]): the stream ends after the job's
/// `exit`, or when the node closes it.
pub struct Events {
    lines: BufReader<reqwest::blocking::Response>,
    url: String,
}

impl Iterator for Events {
    type Item = Result<Event, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let failed = |reason: String| Error::Transport {
            operation: "jobs_events",
            url: self.url.clone(),
            reason,
        };
        let (mut id, mut name, mut data) = (None, String::new(), None);
        let mut line = String::new();
        loop {
            line.clear();
            match self.lines.read_line(&mut line) {
                // The end of the stream: an event cut short is not an event.
                Ok(0) => return None,
                Ok(_) => {}
                Err(error) => return Some(Err(failed(chain(&error)))),
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                let Some(data) = data.take() else {
                    continue;
                };
                return Some(Ok(Event { id, name, data }));
            }
            if let Some(value) = line.strip_prefix("id: ") {
                id = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("event: ") {
                name = value.to_string();
            } else if let Some(value) = line.strip_prefix("data: ") {
                match serde_json::from_str(value) {
                    Ok(value) => data = Some(value),
                    Err(error) => {
                        return Some(Err(Error::Decode {
                            operation: "jobs_events",
                            reason: error.to_string(),
                        }));
                    }
                }
            }
        }
    }
}

impl Client {
    /// A client of the node at `url` (`https://<host>[:<port>]`), sending `token` to every
    /// operation that needs one. With `pin`, the SHA-256 of the server's certificate in hex, the
    /// server is accepted exactly when its certificate has that fingerprint; without it, the
    /// certificate is verified against the system's trusted roots.
    pub fn new(url: &str, token: impl AsRef<str>, pin: Option<&str>) -> Result<Self, Error> {
        let mut base = reqwest::Url::parse(url).map_err(|error| Error::Url {
            url: url.to_string(),
            reason: error.to_string(),
        })?;
        if base.scheme() != "https" {
            return Err(Error::Url {
                url: url.to_string(),
                reason: "a node's API is served over HTTPS only".to_string(),
            });
        }
        // The host and port only: a path, a query or a fragment has no place in a node's address,
        // and a user name or password would be sent as a second Authorization header.
        base.set_path("/");
        base.set_query(None);
        base.set_fragment(None);
        let _ = base.set_username("");
        let _ = base.set_password(None);
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", token.as_ref()))
            .map_err(|_| Error::Token)?;
        authorization.set_sensitive(true);
        let pin = pin.map(Fingerprint::parse).transpose()?;
        // HTTP/1.1 only, as the node serves it (2B), and never a redirect: the API has none, and
        // a redirect would carry the token elsewhere.
        let build = |limit: Option<std::time::Duration>| {
            let builder = reqwest::blocking::Client::builder()
                .https_only(true)
                .http1_only()
                .timeout(limit)
                .redirect(reqwest::redirect::Policy::none());
            let builder = match pin {
                Some(pin) => builder.use_preconfigured_tls(
                    pin::config(pin).map_err(|error| Error::Build(error.to_string()))?,
                ),
                None => builder.use_rustls_tls(),
            };
            builder.build().map_err(|error| Error::Build(chain(&error)))
        };
        Ok(Self {
            base,
            authorization,
            pin,
            http: build(Some(TIMEOUT))?,
            stream: build(None)?,
        })
    }

    /// `GET /v1/health`: whether the node is up. It needs no token, and none is sent.
    pub fn health(&self) -> Result<Health, Error> {
        self.call(operation("health")?)
    }

    /// `GET /v1/node`: the node document. It needs an `observe` token.
    pub fn node(&self) -> Result<Node, Error> {
        self.call(operation("node")?)
    }

    /// `GET /v1/node/audit`: the most recent calls, newest first. It needs an `admin` token.
    pub fn node_audit(&self) -> Result<Audit, Error> {
        self.call(operation("node_audit")?)
    }

    /// `POST /v1/jobs/validate`: whether the engine takes the job's document, and its refusals
    /// when it does not. Nothing runs. It needs a `submit` token.
    pub fn jobs_validate<D: Serialize>(&self, job: &Job<D>) -> Result<Validation, Error> {
        self.send(operation("jobs_validate")?, None, Some(job))
    }

    /// `POST /v1/jobs`: run the job in a fresh engine; the answer is its run ID. The node
    /// answers `busy` while a job runs, and `invalid` with the engine's refusals. It needs a
    /// `submit` token.
    pub fn jobs_submit<D: Serialize>(&self, job: &Job<D>) -> Result<Submitted, Error> {
        self.send(operation("jobs_submit")?, None, Some(job))
    }

    /// `GET /v1/jobs`: the last 100 jobs, newest first. It needs an `observe` token.
    pub fn jobs_list(&self) -> Result<Jobs, Error> {
        self.call(operation("jobs_list")?)
    }

    /// `GET /v1/jobs/{id}`: one job's state, times, latest heartbeat and exit code. It needs an
    /// `observe` token.
    pub fn jobs_get(&self, id: &str) -> Result<JobStatus, Error> {
        self.send(operation("jobs_get")?, Some(id), None::<&()>)
    }

    /// `POST /v1/jobs/{id}/cancel`: tell the engine to cancel the job; the answer is the job as
    /// it stands. The node answers `conflict` if the job has ended. It needs a `submit` token.
    pub fn jobs_cancel(&self, id: &str) -> Result<JobStatus, Error> {
        self.send(operation("jobs_cancel")?, Some(id), None::<&()>)
    }

    /// `GET /v1/jobs/{id}/report`: the engine's `RunReport`, once the job has ended. It needs an
    /// `observe` token.
    pub fn jobs_report(&self, id: &str) -> Result<Value, Error> {
        self.send(operation("jobs_report")?, Some(id), None::<&()>)
    }

    /// `GET /v1/jobs/{id}/events`: the job's events as they arrive, after its record `from`, the
    /// last one already read (from its first when `None` or 0), as far back as the node's
    /// buffer holds them: each event and log record, heartbeats, then the report and the exit.
    /// It needs an `observe` token.
    pub fn jobs_events(&self, id: &str, from: Option<u64>) -> Result<Events, Error> {
        let operation = operation("jobs_events")?;
        let mut url = self.url(operation, Some(id));
        if let Some(from) = from {
            url.set_query(Some(&format!("from={from}")));
        }
        let request = self
            .stream
            .get(url.clone())
            .header(ACCEPT, "text/event-stream")
            .header(AUTHORIZATION, self.authorization.clone());
        let response = request
            .send()
            .map_err(|error| self.failed(operation, &url, error))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response
                .bytes()
                .map_err(|error| self.failed(operation, &url, error))?;
            // Not a stream: the error the node answered with.
            return Err(match answer::<Value>(operation.id, status, &body) {
                Err(error) => error,
                Ok(_) => Error::Status {
                    operation: operation.id,
                    status,
                },
            });
        }
        Ok(Events {
            lines: BufReader::new(response),
            url: url.to_string(),
        })
    }

    /// Call one operation of the table that takes neither a run's ID nor a body, and read its
    /// answer as `T`.
    pub fn call<T: DeserializeOwned>(&self, operation: &Operation) -> Result<T, Error> {
        self.send(operation, None, None::<&()>)
    }

    /// Call one operation of the table, for the run `id` when its path carries one and with
    /// `body` when it takes one, and read its answer as `T`. The typed methods above are this,
    /// each with its operation's answer type.
    pub fn send<T: DeserializeOwned, B: Serialize>(
        &self,
        operation: &Operation,
        id: Option<&str>,
        body: Option<&B>,
    ) -> Result<T, Error> {
        let url = self.url(operation, id);
        let method = match operation.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
        };
        let mut request = self.http.request(method, url.clone());
        if operation.scope.is_some() {
            request = request.header(AUTHORIZATION, self.authorization.clone());
        }
        if let Some(body) = body {
            let written = serde_json::to_vec(body)
                .map_err(|error| Error::Build(format!("the body cannot be written: {error}")))?;
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(written);
        }
        let response = request
            .send()
            .map_err(|error| self.failed(operation, &url, error))?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .map_err(|error| self.failed(operation, &url, error))?;
        answer(operation.id, status, &body)
    }

    /// The URL of `operation`, for the run `id` when its path carries one.
    fn url(&self, operation: &Operation, id: Option<&str>) -> reqwest::Url {
        let mut url = self.base.clone();
        url.set_path(&operation.path_for(id.unwrap_or_default()));
        url
    }

    /// What a call that did not reach its answer failed with.
    fn failed(&self, operation: &Operation, url: &reqwest::Url, error: reqwest::Error) -> Error {
        match (self.pin, pin::mismatch(&error)) {
            (Some(pin), Some(found)) => Error::PinMismatch { pin, found },
            _ => Error::Transport {
                operation: operation.id,
                url: url.to_string(),
                reason: chain(&error),
            },
        }
    }
}

/// How long a call may take, connecting included; a stream of events has no such limit.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The operation of the table with this ID.
pub fn operation(id: &str) -> Result<&'static Operation, Error> {
    moruna_api::OPERATIONS
        .iter()
        .find(|operation| operation.id == id)
        .ok_or_else(|| Error::NoOperation(id.to_string()))
}

/// What a call failed with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The URL is not an `https://` URL of a node.
    #[error("the URL {url:?} cannot be called: {reason}")]
    Url {
        /// The URL as given.
        url: String,
        /// Why it cannot be called.
        reason: String,
    },
    /// The token cannot be sent in an HTTP header.
    #[error("the token cannot be sent: it holds characters an HTTP header cannot carry")]
    Token,
    /// The pin is not a SHA-256 in hex.
    #[error("the pin {0:?} is not a SHA-256 fingerprint: 64 hexadecimal digits")]
    Pin(String),
    /// The server's certificate is not the pinned one. Nothing was sent to it.
    #[error("the server's certificate has SHA-256 {found}, not the pinned {pin}")]
    PinMismatch {
        /// The pin.
        pin: Fingerprint,
        /// The SHA-256 of the certificate the server showed.
        found: Fingerprint,
    },
    /// The HTTP client could not be built.
    #[error("the HTTP client could not be built: {0}")]
    Build(String),
    /// The API's operations table has no such operation.
    #[error("the API has no operation {0:?}")]
    NoOperation(String),
    /// The node could not be reached, or the connection failed.
    #[error("could not call {operation} at {url}: {reason}")]
    Transport {
        /// The operation's ID.
        operation: &'static str,
        /// The URL called.
        url: String,
        /// Why, with every cause.
        reason: String,
    },
    /// The node answered with an error.
    #[error("the node refused {operation}: {} ({status}): {}", code_name(error.code), error.message)]
    Api {
        /// The operation's ID.
        operation: &'static str,
        /// The answer's HTTP status.
        status: u16,
        /// The error the node gave.
        error: ErrorDetail,
    },
    /// The node answered with an error status and no error body.
    #[error("the node answered {operation} with status {status} and no error body")]
    Status {
        /// The operation's ID.
        operation: &'static str,
        /// The answer's HTTP status.
        status: u16,
    },
    /// The answer is not the operation's document.
    #[error("the node's answer to {operation} is not its document: {reason}")]
    Decode {
        /// The operation's ID.
        operation: &'static str,
        /// Why it could not be read.
        reason: String,
    },
}

/// Read an answer: on success, the operation's document; otherwise the error body it carries.
fn answer<T: DeserializeOwned>(
    operation: &'static str,
    status: u16,
    body: &[u8],
) -> Result<T, Error> {
    if (200..300).contains(&status) {
        return serde_json::from_slice(body).map_err(|error| Error::Decode {
            operation,
            reason: error.to_string(),
        });
    }
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(body) => Err(Error::Api {
            operation,
            status,
            error: body.error,
        }),
        Err(_) => Err(Error::Status { operation, status }),
    }
}

/// An error code as the API writes it.
pub fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Invalid => "invalid",
        ErrorCode::Unauthorized => "unauthorized",
        ErrorCode::Forbidden => "forbidden",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Busy => "busy",
        ErrorCode::Internal => "internal",
    }
}

/// An error and every cause under it, each said once, joined by `": "`.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut said: Vec<String> = Vec::new();
    let mut next = Some(error);
    while let Some(error) = next {
        let text = error.to_string();
        if !said.iter().any(|earlier| earlier.contains(&text)) {
            said.push(text);
        }
        next = error.source();
    }
    said.join(": ")
}

#[cfg(test)]
mod tests;
