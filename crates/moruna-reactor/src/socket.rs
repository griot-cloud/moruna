//! Object storage through a socket (MH 4.6, H7): an S3 endpoint named `unix:///path` or
//! `vsock://cid:port` is reached by dialling that socket and speaking plain HTTP/1.1 over it.
//!
//! A machine with no network interface still has a socket to its host, and the host proxies:
//! it terminates the stream and applies whatever egress policy it has. `object_store`'s S3
//! client builds, signs and retries requests exactly as it would over TCP; only the connector
//! beneath it changes, so no source and no sink knows the difference. The client is given
//! `http://localhost` as its endpoint, so requests are path-style (`/bucket/key`) and signed
//! for host `localhost`; the connector ignores the authority and dials the socket.
//!
//! One connection per request: the proxy is local, and a pool would buy nothing a host that
//! cares cannot buy by keeping its proxy warm. The response body is read whole before it is
//! handed back, which is what the reactor does with a ranged read anyway (06 l).

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt as _;
use hyper_util::rt::TokioIo;
use moruna_kernel::Result;
use object_store::client::{
    ClientOptions, HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse,
    HttpResponseBody, HttpService,
};

use crate::object::config;

/// The endpoint the S3 client is given when the real one is a socket: path-style requests,
/// plain HTTP, and a host name the proxy can recognise.
pub(crate) const SOCKET_ENDPOINT: &str = "http://localhost";

/// Where a socket endpoint leads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SocketTarget {
    /// A Unix domain socket at this path.
    Unix(PathBuf),
    /// A vsock address: the host's context id and a port (Linux only).
    Vsock {
        /// Context id; 2 is the host.
        cid: u32,
        /// Port.
        port: u32,
    },
}

impl std::fmt::Display for SocketTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketTarget::Unix(path) => write!(f, "unix://{}", path.display()),
            SocketTarget::Vsock { cid, port } => write!(f, "vsock://{cid}:{port}"),
        }
    }
}

/// `Some` when `endpoint` names a socket rather than an HTTP URL; a malformed socket endpoint
/// is `Config { name: "object_store" }` naming it, at the first operation that needs the
/// client (06 d.1).
pub(crate) fn parse_endpoint(endpoint: &str) -> Result<Option<SocketTarget>> {
    if let Some(path) = endpoint.strip_prefix("unix://") {
        if !path.starts_with('/') {
            return Err(config(&format!(
                "s3 endpoint {endpoint}: a unix socket needs an absolute path, unix:///path"
            )));
        }
        return Ok(Some(SocketTarget::Unix(PathBuf::from(path))));
    }
    if let Some(address) = endpoint.strip_prefix("vsock://") {
        let parsed = address
            .split_once(':')
            .and_then(|(cid, port)| Some((cid.parse::<u32>().ok()?, port.parse::<u32>().ok()?)));
        let Some((cid, port)) = parsed else {
            return Err(config(&format!(
                "s3 endpoint {endpoint}: a vsock endpoint is vsock://<cid>:<port>"
            )));
        };
        return Ok(Some(SocketTarget::Vsock { cid, port }));
    }
    Ok(None)
}

/// The connector `object_store` asks for a client; every client it makes dials `target`.
#[derive(Debug)]
pub(crate) struct SocketConnector {
    target: Arc<SocketTarget>,
}

impl SocketConnector {
    pub(crate) fn new(target: SocketTarget) -> SocketConnector {
        SocketConnector {
            target: Arc::new(target),
        }
    }
}

impl HttpConnector for SocketConnector {
    fn connect(&self, _options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(SocketService {
            target: Arc::clone(&self.target),
        }))
    }
}

/// One HTTP/1.1 exchange per call over a fresh connection to the socket.
#[derive(Debug)]
struct SocketService {
    target: Arc<SocketTarget>,
}

impl HttpService for SocketService {
    // `HttpService` is an `#[async_trait]` trait; this is the signature that macro produces,
    // written out because `async-trait` is not in the preamble's dependency table.
    fn call<'life0, 'async_trait>(
        &'life0 self,
        req: HttpRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<HttpResponse, HttpError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let target = Arc::clone(&self.target);
        Box::pin(exchange(target, req))
    }
}

/// A failure that names the socket, so a host's wiring mistake is read from the error (MH 6:
/// "`IoError` naming the socket at the first read").
#[derive(Debug)]
struct SocketError(String);

impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SocketError {}

fn failed(kind: HttpErrorKind, target: &SocketTarget, what: impl std::fmt::Display) -> HttpError {
    HttpError::new(kind, SocketError(format!("{target}: {what}")))
}

async fn exchange(
    target: Arc<SocketTarget>,
    req: HttpRequest,
) -> std::result::Result<HttpResponse, HttpError> {
    let stream = dial(&target)
        .await
        .map_err(|e| failed(HttpErrorKind::Connect, &target, e))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| failed(HttpErrorKind::Connect, &target, e))?;
    // The connection is driven beside the request and ends when the response has been read
    // and the sender dropped.
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let req = origin_form(req);
    let response = sender
        .send_request(req)
        .await
        .map_err(|e| failed(HttpErrorKind::Request, &target, e))?;
    let (parts, body) = response.into_parts();
    let bytes: Bytes = body
        .collect()
        .await
        .map_err(|e| failed(HttpErrorKind::Decode, &target, e))?
        .to_bytes();
    Ok(HttpResponse::from_parts(
        parts,
        HttpResponseBody::from(bytes),
    ))
}

/// The request line in origin form (`/bucket/key?query`) with the authority moved to `Host`,
/// which is what an HTTP/1.1 server expects from a client that is not talking to a proxy.
fn origin_form(req: HttpRequest) -> HttpRequest {
    let (mut parts, body) = req.into_parts();
    if !parts.headers.contains_key(http::header::HOST)
        && let Some(authority) = parts.uri.authority()
        && let Ok(value) = http::HeaderValue::from_str(authority.as_str())
    {
        parts.headers.insert(http::header::HOST, value);
    }
    if let Some(path) = parts.uri.path_and_query().cloned() {
        let mut uri = http::uri::Parts::default();
        uri.path_and_query = Some(path);
        if let Ok(origin) = http::Uri::from_parts(uri) {
            parts.uri = origin;
        }
    }
    HttpRequest::from_parts(parts, body)
}

/// A stream to the target. A vsock connection is made blocking on the runtime's blocking pool
/// and handed to tokio as a stream socket: reads and writes on it are the same system calls as
/// on a Unix stream, which is all tokio's `UnixStream` makes.
async fn dial(target: &SocketTarget) -> std::io::Result<tokio::net::UnixStream> {
    match target {
        SocketTarget::Unix(path) => tokio::net::UnixStream::connect(path).await,
        SocketTarget::Vsock { cid, port } => dial_vsock(*cid, *port).await,
    }
}

#[cfg(target_os = "linux")]
async fn dial_vsock(cid: u32, port: u32) -> std::io::Result<tokio::net::UnixStream> {
    let std_stream = tokio::task::spawn_blocking(move || {
        let socket = socket2::Socket::new(socket2::Domain::VSOCK, socket2::Type::STREAM, None)?;
        socket.connect(&socket2::SockAddr::vsock(cid, port))?;
        socket.set_nonblocking(true)?;
        let fd: std::os::fd::OwnedFd = socket.into();
        Ok::<_, std::io::Error>(std::os::unix::net::UnixStream::from(fd))
    })
    .await
    .map_err(std::io::Error::other)??;
    tokio::net::UnixStream::from_std(std_stream)
}

#[cfg(not(target_os = "linux"))]
async fn dial_vsock(_cid: u32, _port: u32) -> std::io::Result<tokio::net::UnixStream> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "vsock is a Linux socket family; this host has none",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_endpoints_parse_and_http_ones_pass_through() {
        assert_eq!(parse_endpoint("http://127.0.0.1:9000").expect("http"), None);
        assert_eq!(
            parse_endpoint("unix:///run/moruna/egress.sock").expect("unix"),
            Some(SocketTarget::Unix(PathBuf::from("/run/moruna/egress.sock")))
        );
        assert_eq!(
            parse_endpoint("vsock://2:5000").expect("vsock"),
            Some(SocketTarget::Vsock { cid: 2, port: 5000 })
        );
        for bad in [
            "unix://relative.sock",
            "vsock://2",
            "vsock://host:5000",
            "vsock://2:x",
        ] {
            assert!(
                matches!(
                    parse_endpoint(bad),
                    Err(moruna_kernel::MorunaError::Config {
                        name: "object_store",
                        ..
                    })
                ),
                "{bad} is refused"
            );
        }
        assert_eq!(
            SocketTarget::Vsock { cid: 3, port: 7 }.to_string(),
            "vsock://3:7"
        );
    }

    #[test]
    fn a_request_goes_out_in_origin_form_with_its_host() {
        let req = http::Request::builder()
            .uri("http://localhost/bucket/key?x=1")
            .body(object_store::client::HttpRequestBody::empty())
            .expect("a request");
        let out = origin_form(req);
        assert_eq!(out.uri().to_string(), "/bucket/key?x=1");
        assert_eq!(
            out.headers().get(http::header::HOST).map(|v| v.as_bytes()),
            Some(&b"localhost"[..])
        );
    }

    #[test]
    fn a_socket_that_is_not_there_is_named() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let target = Arc::new(SocketTarget::Unix(PathBuf::from(
            "/nonexistent/moruna/egress.sock",
        )));
        let req = http::Request::builder()
            .uri("http://localhost/b/k")
            .body(object_store::client::HttpRequestBody::empty())
            .expect("a request");
        let error = rt
            .block_on(exchange(target, req))
            .expect_err("nothing listens there");
        assert!(matches!(error.kind(), HttpErrorKind::Connect));
        let text = format!("{error} {:?}", std::error::Error::source(&error));
        assert!(text.contains("/nonexistent/moruna/egress.sock"), "{text}");
        #[cfg(not(target_os = "linux"))]
        {
            let vsock = Arc::new(SocketTarget::Vsock { cid: 2, port: 5000 });
            let req = http::Request::builder()
                .uri("http://localhost/b/k")
                .body(object_store::client::HttpRequestBody::empty())
                .expect("a request");
            let error = rt.block_on(exchange(vsock, req)).expect_err("no vsock");
            assert!(format!("{error:?}").contains("vsock"));
        }
    }
}
