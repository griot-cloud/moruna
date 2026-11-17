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
//! **Pinning.** With a pin, the server's certificate is accepted exactly when the SHA-256 of its
//! DER equals the pin, the fingerprint the node prints on its console
//! (`moruna: tls sha256=<hex>`); nothing else about the certificate is checked, though the TLS
//! handshake's own signature must still verify under the pinned certificate's key. Without a pin,
//! the certificate is verified as usual, against the system's trusted roots.
//!
//! The `moruna node` command is [`cli::node`].

use moruna_api::{Audit, ErrorBody, ErrorCode, ErrorDetail, Health, Method, Node, Operation};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::de::DeserializeOwned;

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
        let builder = reqwest::blocking::Client::builder()
            .https_only(true)
            .http1_only()
            .redirect(reqwest::redirect::Policy::none());
        let builder = match pin {
            Some(pin) => builder.use_preconfigured_tls(
                pin::config(pin).map_err(|error| Error::Build(error.to_string()))?,
            ),
            None => builder.use_rustls_tls(),
        };
        let http = builder
            .build()
            .map_err(|error| Error::Build(chain(&error)))?;
        Ok(Self {
            base,
            authorization,
            pin,
            http,
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

    /// Call one operation of the table and read its answer as `T`. The typed methods above are
    /// this, each with its operation's answer type.
    pub fn call<T: DeserializeOwned>(&self, operation: &Operation) -> Result<T, Error> {
        let mut url = self.base.clone();
        url.set_path(operation.path);
        let method = match operation.method {
            Method::Get => reqwest::Method::GET,
        };
        let mut request = self.http.request(method, url.clone());
        if operation.scope.is_some() {
            request = request.header(AUTHORIZATION, self.authorization.clone());
        }
        let failed = |error: reqwest::Error| match (self.pin, pin::mismatch(&error)) {
            (Some(pin), Some(found)) => Error::PinMismatch { pin, found },
            _ => Error::Transport {
                operation: operation.id,
                url: url.to_string(),
                reason: chain(&error),
            },
        };
        let response = request.send().map_err(failed)?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(failed)?;
        answer(operation.id, status, &body)
    }
}

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
