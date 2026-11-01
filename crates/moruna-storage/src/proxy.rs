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
//! One connection per request. Response frames stream to the SDK without collecting the
//! entire object range; the bucket adapter lands each chunk into charged arena pages.
//! Dropping a body cancels its own connection driver. Driver failures remain body errors.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper_util::rt::TokioIo;
use moruna_kernel::Result;
use object_store::client::{
    ClientOptions, HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse,
    HttpResponseBody, HttpService,
};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

fn config(msg: &str) -> moruna_kernel::MorunaError {
    moruna_kernel::MorunaError::Config {
        name: "object_store",
        msg: msg.to_owned(),
    }
}

/// The endpoint the S3 client is given when the real one is a socket: path-style requests,
/// plain HTTP, and a host name the proxy can recognise.
pub const SOCKET_ENDPOINT: &str = "http://localhost";

/// Where a socket endpoint leads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SocketTarget {
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
pub fn parse_endpoint(endpoint: &str) -> Result<Option<SocketTarget>> {
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
pub struct SocketConnector {
    target: Arc<SocketTarget>,
}

impl SocketConnector {
    /// Connect every request to the explicitly selected host-proxy socket.
    pub fn new(target: SocketTarget) -> SocketConnector {
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
    let driver = tokio::spawn(connection);
    let req = origin_form(req);
    let response = match sender.send_request(req).await {
        Ok(response) => response,
        Err(e) => {
            driver.abort(); // The failed request owns this connection; cancel its driver.
            return Err(failed(HttpErrorKind::Request, &target, e));
        }
    };
    drop(sender);
    let (parts, body) = response.into_parts();
    Ok(HttpResponse::from_parts(
        parts,
        HttpResponseBody::new(DrivenBody {
            body,
            driver: Some(driver),
            target,
            ended: false,
        }),
    ))
}

/// Body owns the one connection driver through its final completion or explicit drop.
#[derive(Debug)]
struct DrivenBody {
    body: Incoming,
    driver: Option<tokio::task::JoinHandle<std::result::Result<(), hyper::Error>>>,
    target: Arc<SocketTarget>,
    ended: bool,
}
impl Body for DrivenBody {
    type Data = Bytes;
    type Error = HttpError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, HttpError>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => return Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(e))) => {
                self.ended = true;
                return Poll::Ready(Some(Err(failed(HttpErrorKind::Decode, &self.target, e))));
            }
            Poll::Ready(None) => {}
        }
        let Some(driver) = self.driver.as_mut() else {
            self.ended = true;
            return Poll::Ready(None);
        };
        let result = match Pin::new(driver).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        self.driver = None;
        self.ended = true;
        match result {
            Ok(Ok(())) => Poll::Ready(None),
            Ok(Err(e)) => Poll::Ready(Some(Err(failed(HttpErrorKind::Decode, &self.target, e)))),
            Err(e) => Poll::Ready(Some(Err(failed(HttpErrorKind::Decode, &self.target, e)))),
        }
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.ended
    }
}
impl Drop for DrivenBody {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
    }
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

#[cfg(all(test, unix))]
mod streaming_tests {
    use super::*;
    use futures::StreamExt;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        path: PathBuf,
        release: Option<std::sync::mpsc::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                // Cleanup must also unblock the worker after a failed assertion; the
                // receiver may already have exited on the disconnect branch.
                let _receiver_already_closed = release.send(());
            }
            drop(std::os::unix::net::UnixStream::connect(&self.path));
            if let Some(thread) = self.thread.take() {
                if let Err(cause) = thread.join() {
                    if !std::thread::panicking() {
                        std::panic::resume_unwind(cause);
                    }
                }
            }
            std::fs::remove_dir_all(self.path.parent().unwrap()).unwrap();
        }
    }
    fn fixture(cancel: bool) -> (Fixture, tokio::sync::oneshot::Receiver<()>) {
        let dir = std::env::temp_dir().join(format!(
            "moruna-proxy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let (release, released) = std::sync::mpsc::channel();
        let (finished, done) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match connection.read_exact(&mut byte) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return,
                    Err(error) => panic!("controlled request read: {error}"),
                }
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")
                .unwrap();
            connection.flush().unwrap();
            if cancel {
                assert_eq!(
                    connection.read(&mut byte).unwrap(),
                    0,
                    "cancelled body owns and closes its own socket"
                );
            } else {
                released.recv().unwrap();
                connection.write_all(b"def").unwrap();
            }
            // The test may unwind and drop its observer before fixture cleanup joins us.
            let _observer_already_closed = finished.send(());
        });
        (
            Fixture {
                path,
                release: Some(release),
                thread: Some(thread),
            },
            done,
        )
    }
    fn request() -> HttpRequest {
        http::Request::builder()
            .uri("http://localhost/bucket/key")
            .body(object_store::client::HttpRequestBody::empty())
            .unwrap()
    }
    #[test]
    fn st_t6_proxy_exposes_first_frames_before_releasing_remaining_body() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut fixture, done) = fixture(false);
        runtime.block_on(async {
            // Timeout is a deadlock guard; the server's channel, not elapsed time, controls
            // whether the body may finish. This is not a latency/performance assertion.
            let response = tokio::time::timeout(
                Duration::from_secs(5),
                exchange(
                    Arc::new(SocketTarget::Unix(fixture.path.clone())),
                    request(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            let mut stream = response.into_body().bytes_stream();
            assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"abc");
            fixture.release.take().unwrap().send(()).unwrap();
            assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"def");
            assert!(stream.next().await.is_none());
            done.await.unwrap();
        });
    }
    #[test]
    fn st_t6_dropping_proxy_body_disconnects_its_owned_driver() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (fixture, done) = fixture(true);
        runtime.block_on(async {
            let response = exchange(
                Arc::new(SocketTarget::Unix(fixture.path.clone())),
                request(),
            )
            .await
            .unwrap();
            drop(response);
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .unwrap()
                .unwrap();
        });
    }
}
