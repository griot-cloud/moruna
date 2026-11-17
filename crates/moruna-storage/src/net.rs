//! Object storage through the node's network server (system design 3D, item 3.4): the
//! `moruna-net` connector, beside the `unix://` and `vsock://` one of MH 4.6 (`proxy`).
//!
//! On a node the engine has no network interface and no Linux socket of its own. It
//! reaches object storage through the network server's own client, on the channel
//! node-admission minted for its job, which reaches the job's object-store hosts on
//! port 443 and nothing else. [`Network`] is that client as the connector needs it:
//! open a TCP connection to a host by name (the network server resolves it and checks
//! the job's policy), have the network server run TLS on it for that name
//! (`start_tls`, under the node's roots and clock), and move bytes.
//!
//! [`NetConnector`] is the `object_store` connector over it. The client is given the
//! store's real `https://` endpoint, so it builds, names and signs requests exactly as
//! it would over TCP. For each request the connector opens the request's own host and
//! port by name, starts TLS for that host, and exchanges plain HTTP/1.1 over the
//! stream; the bytes on the wire are TLS, which the network server keeps. One
//! connection per request, which the request asks the server to close
//! (`Connection: close`) once it has answered. A request that is not `https`, or names
//! an address rather than a host, is refused: the network server reaches names only.
//!
//! A refusal by the job's policy is not retried, so a job that reaches for a host it
//! does not list fails at once, with the host's name in the error.

use std::fmt;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;

use object_store::client::{
    ClientOptions, HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse,
    HttpService,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::proxy::{exchange_on, failed};

/// The port HTTPS is reached on when a URL names none.
pub const HTTPS_PORT: u16 = 443;

/// Bytes the connector moves between the network server and HTTP at a time, each way.
const CHUNK: usize = 64 * 1024;

/// The network server's client, as the connector needs it.
///
/// On a node it is `moruna.net` over the job's channel; in a test, anything that behaves
/// as the network server does.
pub trait Network: Send + Sync + fmt::Debug + 'static {
    /// A TCP connection to `host` at `port`. The network server resolves the name and
    /// checks it against the job's policy; a name or a port the policy does not hold is
    /// [`io::ErrorKind::PermissionDenied`].
    fn open(&self, host: &str, port: u16) -> io::Result<Box<dyn NetSocket>>;
}

/// One connection the network server holds for the engine. Dropping it closes it.
///
/// `send` and `receive` may run at once, from two threads: one waits for bytes while the
/// other writes.
pub trait NetSocket: Send + Sync {
    /// Upgrades the connection, before any byte has moved, to TLS verified for
    /// `server_name` under the node's roots and clock. A failed handshake, chain or name
    /// is [`io::ErrorKind::InvalidData`]; there is never a plaintext fallback.
    fn start_tls(&self, server_name: &str) -> io::Result<()>;
    /// Queues bytes to send: how many were taken, at least one.
    fn send(&self, bytes: &[u8]) -> io::Result<usize>;
    /// Bytes received into `buffer`: how many, zero only at the end of the stream.
    fn receive(&self, buffer: &mut [u8]) -> io::Result<usize>;
    /// Finishes the sending half: what is queued goes, then the end of the stream.
    fn shutdown(&self) -> io::Result<()>;
}

/// The connector `object_store` asks for a client; every client it makes reaches its
/// stores through `network`.
#[derive(Debug, Clone)]
pub struct NetConnector {
    network: Arc<dyn Network>,
}

impl NetConnector {
    /// Every request goes through `network`.
    pub fn new(network: Arc<dyn Network>) -> NetConnector {
        NetConnector { network }
    }
}

impl HttpConnector for NetConnector {
    fn connect(&self, _options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(NetService {
            network: Arc::clone(&self.network),
        }))
    }
}

/// One HTTP/1.1 exchange per call over a fresh connection through the network server.
#[derive(Debug)]
struct NetService {
    network: Arc<dyn Network>,
}

impl HttpService for NetService {
    // `HttpService` is an `#[async_trait]` trait; this is the signature that macro produces,
    // written out as `proxy` writes it.
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
        let network = Arc::clone(&self.network);
        Box::pin(exchange(network, req))
    }
}

/// Where a request goes: its host by name, and its port.
fn destination(req: &HttpRequest) -> Result<(String, u16), (String, &'static str)> {
    let uri = req.uri();
    let shown = uri.authority().map_or_else(String::new, |a| a.to_string());
    if uri.scheme_str() != Some("https") {
        return Err((
            shown,
            "the network server carries HTTPS only: the endpoint must be https://",
        ));
    }
    let Some(host) = uri.host().filter(|h| !h.is_empty()) else {
        return Err((shown, "the request names no host"));
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.parse::<IpAddr>().is_ok() {
        return Err((
            shown,
            "the network server reaches host names only, and this is an address",
        ));
    }
    Ok((
        host.to_ascii_lowercase(),
        uri.port_u16().unwrap_or(HTTPS_PORT),
    ))
}

/// The kind of failure an error of the network server's is: a refusal of the job's
/// policy, a TLS failure or an unusable request is never retried; anything else may
/// pass, as a refused or timed-out connection may.
fn kind_of(e: &io::Error) -> HttpErrorKind {
    match e.kind() {
        io::ErrorKind::PermissionDenied
        | io::ErrorKind::InvalidData
        | io::ErrorKind::InvalidInput
        | io::ErrorKind::Unsupported => HttpErrorKind::Unknown,
        _ => HttpErrorKind::Connect,
    }
}

async fn exchange(
    network: Arc<dyn Network>,
    mut req: HttpRequest,
) -> std::result::Result<HttpResponse, HttpError> {
    let (host, port) = destination(&req).map_err(|(shown, why)| {
        failed(
            HttpErrorKind::Unknown,
            &format!("moruna-net://{shown}"),
            why,
        )
    })?;
    let label: Arc<str> = Arc::from(format!("moruna-net://{host}:{port}"));
    // Open and start TLS on the blocking pool: the network server's calls wait.
    let socket = {
        let (network, host) = (Arc::clone(&network), host.clone());
        tokio::task::spawn_blocking(move || -> io::Result<Arc<dyn NetSocket>> {
            let socket: Arc<dyn NetSocket> = Arc::from(network.open(&host, port)?);
            socket.start_tls(&host)?;
            Ok(socket)
        })
        .await
        .map_err(|e| failed(HttpErrorKind::Connect, &label, e))?
        .map_err(|e| failed(kind_of(&e), &label, e))?
    };
    // One request on this connection: the server closes it once it has answered, which
    // is how the receiving side learns the exchange is over.
    req.headers_mut().insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("close"),
    );
    let (ours, theirs) = tokio::io::duplex(CHUNK);
    let (mut from_http, mut to_http) = tokio::io::split(theirs);
    let runtime = tokio::runtime::Handle::current();
    // The network server's bytes to HTTP, until the end of the stream or HTTP has gone.
    {
        let (socket, runtime) = (Arc::clone(&socket), runtime.clone());
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0; CHUNK];
            while let Ok(n) = socket.receive(&mut buffer) {
                if n == 0 || runtime.block_on(to_http.write_all(&buffer[..n])).is_err() {
                    break;
                }
            }
            let _ = runtime.block_on(to_http.shutdown());
        });
    }
    // HTTP's bytes to the network server, until HTTP is done with the connection.
    tokio::task::spawn_blocking(move || {
        let mut buffer = vec![0; CHUNK];
        loop {
            let n = match runtime.block_on(from_http.read(&mut buffer)) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut sent = 0;
            while sent < n {
                match socket.send(&buffer[sent..n]) {
                    Ok(taken) if taken > 0 => sent += taken,
                    _ => return,
                }
            }
        }
        let _ = socket.shutdown();
    });
    exchange_on(ours, label, req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(uri: &str) -> HttpRequest {
        http::Request::builder()
            .uri(uri)
            .body(object_store::client::HttpRequestBody::empty())
            .unwrap()
    }

    #[test]
    fn a_request_goes_to_its_own_host_by_name_at_443_unless_it_names_a_port() {
        assert_eq!(
            destination(&request("https://Store.Objects.Test/bucket/key")),
            Ok(("store.objects.test".into(), 443))
        );
        assert_eq!(
            destination(&request("https://store.objects.test:8443/bucket/key")),
            Ok(("store.objects.test".into(), 8443))
        );
    }

    #[test]
    fn plain_http_and_addresses_are_refused() {
        let (shown, why) = destination(&request("http://store.objects.test/b/k")).unwrap_err();
        assert_eq!(shown, "store.objects.test");
        assert!(why.contains("HTTPS only"), "{why}");
        for uri in ["https://10.0.0.5/b/k", "https://[::1]:443/b/k"] {
            let (_, why) = destination(&request(uri)).unwrap_err();
            assert!(why.contains("host names only"), "{uri}: {why}");
        }
        let (_, why) = destination(&request("/b/k")).unwrap_err();
        assert!(why.contains("HTTPS only"), "{why}");
    }

    #[test]
    fn refusals_and_tls_failures_are_never_retried() {
        let not = |kind| kind_of(&io::Error::new(kind, "x"));
        assert!(matches!(
            not(io::ErrorKind::PermissionDenied),
            HttpErrorKind::Unknown
        ));
        assert!(matches!(
            not(io::ErrorKind::InvalidData),
            HttpErrorKind::Unknown
        ));
        assert!(matches!(
            not(io::ErrorKind::Unsupported),
            HttpErrorKind::Unknown
        ));
        assert!(matches!(
            not(io::ErrorKind::ConnectionRefused),
            HttpErrorKind::Connect
        ));
        assert!(matches!(
            not(io::ErrorKind::TimedOut),
            HttpErrorKind::Connect
        ));
    }
}
