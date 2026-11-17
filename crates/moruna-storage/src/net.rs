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
//! it would over TCP. A request goes to its own host and port by name, as plain
//! HTTP/1.1 over a connection on which the network server runs TLS for that host; the
//! bytes on the wire are TLS, which the network server keeps. A request that is not
//! `https`, or names an address rather than a host, is refused: the network server
//! reaches names only.
//!
//! Connections are reused (3D): every client a connector makes shares one HTTP/1.1
//! keep-alive pool, per host, so a range read does not pay a new TCP and TLS handshake.
//! At most the job's socket bound are open at once ([`SOCKETS`], or what
//! [`NetConnector::with_sockets`] is given). Past it, a request frees a connection idle on
//! another host, or waits for one to come back. A connection the server closes is
//! dropped, and the next request opens another; a request a kept connection could not
//! take, because it closed before the request went, goes on another.
//!
//! A refusal by the job's policy is not retried, so a job that reaches for a host it
//! does not list fails at once, with the host's name in the error.

use std::fmt;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use object_store::client::{
    ClientOptions, HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest,
    HttpRequestBody, HttpResponse, HttpResponseBody, HttpService,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::proxy::{failed, origin_form};

/// The port HTTPS is reached on when a URL names none.
pub const HTTPS_PORT: u16 = 443;

/// Connections open at once unless the connector is told otherwise: a job's socket bound
/// (system design 3D).
pub const SOCKETS: usize = 64;

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

/// One connection the network server holds for the engine. Dropping it closes it, and
/// the network server frees its socket.
///
/// `send`, `receive` and `close` may run at once, from different threads: one waits for
/// bytes while another writes, and a third closes.
pub trait NetSocket: Send + Sync {
    /// Upgrades the connection, before any byte has moved, to TLS verified for
    /// `server_name` under the node's roots and clock. A failed handshake, chain or name
    /// is [`io::ErrorKind::InvalidData`]; there is never a plaintext fallback.
    fn start_tls(&self, server_name: &str) -> io::Result<()>;
    /// Queues bytes to send: how many were taken, at least one.
    fn send(&self, bytes: &[u8]) -> io::Result<usize>;
    /// Bytes received into `buffer`: how many, zero only at the end of the stream.
    fn receive(&self, buffer: &mut [u8]) -> io::Result<usize>;
    /// Ends the connection both ways, now: a `send` or `receive` waiting on another
    /// thread returns (an error, or the end of the stream), and nothing more moves.
    fn close(&self);
}

/// The connector `object_store` asks for a client. Every client it makes reaches its
/// stores through one network, and shares one pool of connections.
#[derive(Debug, Clone)]
pub struct NetConnector {
    pool: Arc<Pool>,
}

impl NetConnector {
    /// Every request goes through `network`, on at most [`SOCKETS`] connections at once.
    pub fn new(network: Arc<dyn Network>) -> NetConnector {
        NetConnector::with_sockets(network, SOCKETS)
    }

    /// Every request goes through `network`, on at most `sockets` connections at once
    /// (at least one).
    pub fn with_sockets(network: Arc<dyn Network>, sockets: usize) -> NetConnector {
        NetConnector {
            pool: Arc::new(Pool {
                network,
                sockets: sockets.max(1),
                state: Mutex::default(),
                changed: Notify::new(),
            }),
        }
    }
}

impl HttpConnector for NetConnector {
    fn connect(&self, _options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(NetService {
            pool: Arc::clone(&self.pool),
        }))
    }
}

/// HTTP/1.1 exchanges on the pool's connections through the network server.
#[derive(Debug)]
struct NetService {
    pool: Arc<Pool>,
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
        Box::pin(exchange(Arc::clone(&self.pool), req))
    }
}

/// A host by name, and a port: what a connection reaches.
type Host = (String, u16);

/// Where a request goes: its host by name, and its port.
fn destination(req: &HttpRequest) -> Result<Host, (String, &'static str)> {
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

/// The connections of every client one connector makes.
struct Pool {
    network: Arc<dyn Network>,
    /// At most this many connections are open at once.
    sockets: usize,
    state: Mutex<State>,
    /// A connection came back to the pool, or one was freed.
    changed: Notify,
}

#[derive(Default)]
struct State {
    /// Connections open, whether in use, idle or closing: each holds a [`Place`] until
    /// its socket is freed.
    open: usize,
    /// Connections waiting for a request, the longest idle first.
    idle: Vec<(Host, Conn)>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state();
        f.debug_struct("Pool")
            .field("network", &self.network)
            .field("sockets", &self.sockets)
            .field("open", &state.open)
            .field("idle", &state.idle.len())
            .finish()
    }
}

/// What a request is given: a kept connection to its host, or a place to open one in.
enum Given {
    Kept(Conn),
    Place(Place),
}

impl Pool {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A connection to `host`: the most recently idle one kept for it, or else a place to
    /// open one. With every place taken, the longest idle connection (another host's) is
    /// freed for it, or it waits for one to come back.
    async fn take(self: &Arc<Self>, host: &Host) -> Given {
        loop {
            // Listen before looking, so that a change between the two is not missed.
            let changed = self.changed.notified();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            let freed = {
                let mut state = self.state();
                if let Some(at) = state.idle.iter().rposition(|(h, _)| h == host) {
                    return Given::Kept(state.idle.remove(at).1);
                }
                if state.open < self.sockets {
                    state.open += 1;
                    return Given::Place(Place(Arc::clone(self)));
                }
                (!state.idle.is_empty()).then(|| state.idle.remove(0))
            };
            // Ended outside the lock: its place comes back once its socket is freed.
            drop(freed);
            changed.await;
        }
    }

    /// `conn` is free for `host`'s next request, unless it has closed.
    fn put(&self, host: Host, conn: Conn) {
        if conn.sender.is_closed() {
            return;
        }
        self.state().idle.push((host, conn));
        self.changed.notify_waiters();
    }
}

/// A place among the pool's connections, held from opening a socket until it is freed.
struct Place(Arc<Pool>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.state().open -= 1;
        self.0.changed.notify_waiters();
    }
}

/// One open connection's HTTP side. Dropping it ends the connection; its socket is freed,
/// and its place given back, once its pumps have stopped.
struct Conn {
    sender: SendRequest<HttpRequestBody>,
    driver: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// What a connection's two pumps share: its socket, and its place, which comes back after
/// the socket is freed (fields drop in this order).
struct Held {
    socket: Box<dyn NetSocket>,
    _place: Place,
}

/// Opens a connection to `host` in `place`: TLS for its name, then HTTP/1.1 over it.
async fn open(pool: &Pool, host: &Host, place: Place, label: &Arc<str>) -> Result<Conn, HttpError> {
    let network = Arc::clone(&pool.network);
    let (name, port) = host.clone();
    // The network server's calls wait: open and start TLS on the blocking pool.
    let held = tokio::task::spawn_blocking(move || -> io::Result<Held> {
        let socket = network.open(&name, port)?;
        socket.start_tls(&name)?;
        Ok(Held {
            socket,
            _place: place,
        })
    })
    .await
    .map_err(|e| failed(HttpErrorKind::Connect, label, e))?
    .map_err(|e| failed(kind_of(&e), label, e))?;
    let (ours, theirs) = tokio::io::duplex(CHUNK);
    pump(Arc::new(held), theirs);
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(ours))
        .await
        .map_err(|e| failed(HttpErrorKind::Connect, label, e))?;
    // Its errors reach the request or the body that meets them.
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(Conn { sender, driver })
}

/// Moves bytes between the network server and HTTP's end of `stream`, one blocking thread
/// each way, until either side is done; then closes the socket, which is freed with the
/// last of the two.
fn pump(held: Arc<Held>, stream: DuplexStream) {
    let (mut from_http, mut to_http) = tokio::io::split(stream);
    let runtime = tokio::runtime::Handle::current();
    // The network server's bytes to HTTP, until the end of the stream or HTTP has gone.
    {
        let (held, runtime) = (Arc::clone(&held), runtime.clone());
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0; CHUNK];
            while let Ok(n) = held.socket.receive(&mut buffer) {
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
        'http: loop {
            let n = match runtime.block_on(from_http.read(&mut buffer)) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut sent = 0;
            while sent < n {
                match held.socket.send(&buffer[sent..n]) {
                    Ok(taken) if taken > 0 => sent += taken,
                    _ => break 'http,
                }
            }
        }
        // Nothing more goes either way: a receive waiting for the server returns.
        held.socket.close();
    });
}

async fn exchange(
    pool: Arc<Pool>,
    req: HttpRequest,
) -> std::result::Result<HttpResponse, HttpError> {
    let host = destination(&req).map_err(|(shown, why)| {
        failed(
            HttpErrorKind::Unknown,
            &format!("moruna-net://{shown}"),
            why,
        )
    })?;
    let label: Arc<str> = Arc::from(format!("moruna-net://{}:{}", host.0, host.1));
    let mut req = origin_form(req);
    loop {
        let (mut conn, kept) = match pool.take(&host).await {
            Given::Kept(conn) => (conn, true),
            Given::Place(place) => (open(&pool, &host, place, &label).await?, false),
        };
        // A kept connection takes a request once it has read the last answer to its end,
        // unless the server has closed it meanwhile; then it is dropped.
        if let Err(e) = conn.sender.ready().await {
            if kept {
                continue;
            }
            return Err(failed(HttpErrorKind::Connect, &label, e));
        }
        match conn.sender.try_send_request(req).await {
            Ok(response) => {
                let (parts, body) = response.into_parts();
                let lease = Lease {
                    pool: Arc::clone(&pool),
                    host,
                    conn,
                };
                return Ok(HttpResponse::from_parts(
                    parts,
                    HttpResponseBody::new(PooledBody {
                        body,
                        lease: Some(lease),
                        label,
                    }),
                ));
            }
            Err(mut e) => match e.take_message() {
                // The kept connection closed before the request went: it goes on another.
                Some(again) if kept => req = again,
                _ => return Err(failed(HttpErrorKind::Request, &label, e.into_error())),
            },
        }
    }
}

/// A connection lent to one response.
struct Lease {
    pool: Arc<Pool>,
    host: Host,
    conn: Conn,
}

/// A response's body. Read to its end, it gives its connection back to the pool; dropped
/// before, or failed, it ends the connection, as HTTP/1.1 cannot skip the rest.
struct PooledBody {
    body: Incoming,
    lease: Option<Lease>,
    /// Names the peer in errors.
    label: Arc<str>,
}

impl PooledBody {
    fn give_back(&mut self) {
        if let Some(Lease { pool, host, conn }) = self.lease.take() {
            pool.put(host, conn);
        }
    }
}

impl Body for PooledBody {
    type Data = Bytes;
    type Error = HttpError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, HttpError>>> {
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(e))) => {
                self.lease = None;
                Poll::Ready(Some(Err(failed(HttpErrorKind::Decode, &self.label, e))))
            }
            Poll::Ready(None) => {
                self.give_back();
                Poll::Ready(None)
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
}

impl Drop for PooledBody {
    fn drop(&mut self) {
        // A body with nothing left to read (a HEAD's, an empty one, one read to its last
        // byte) gives its connection back whether its end was read or not.
        if self.body.is_end_stream() {
            self.give_back();
        }
    }
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

    #[test]
    fn a_connector_holds_at_least_one_socket() {
        #[derive(Debug)]
        struct Nowhere;
        impl Network for Nowhere {
            fn open(&self, _: &str, _: u16) -> io::Result<Box<dyn NetSocket>> {
                Err(io::ErrorKind::ConnectionRefused.into())
            }
        }
        let connector = NetConnector::with_sockets(Arc::new(Nowhere), 0);
        assert_eq!(connector.pool.sockets, 1);
        assert_eq!(NetConnector::new(Arc::new(Nowhere)).pool.sockets, SOCKETS);
        let shown = format!("{connector:?}");
        assert!(
            shown.contains("sockets: 1") && shown.contains("open: 0"),
            "{shown}"
        );
    }
}
