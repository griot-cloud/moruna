//! What every end-to-end test needs: an in-memory transport, a clock the test moves, and real
//! rustls clients that pin the server's certificate by its fingerprint, as the clients of item
//! 2.5 will.
#![allow(dead_code)]

pub mod signer;

use moruna_api::{Cloud, Executor, ExecutorKind, NodeHealth};
use moruna_api_server::{
    Clock, Config, Identity, NodeFacts, Server, TokenKey, Transport, TransportError,
};
use ring::digest;
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, ring::default_provider, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use signer::Signer;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    io::{Read, Write},
    rc::Rc,
    sync::Arc,
};

/// The design's example clock, 2026-11-17T16:00:00Z.
pub const WALL: u64 = 1_794_931_200;
/// The node's instance ID in every test.
pub const INSTANCE: &str = "i-0123456789abcdef";

/// The bytes between one client and the server, each way, and who has closed.
#[derive(Debug, Default)]
pub struct Pipe {
    pub to_server: VecDeque<u8>,
    pub to_client: VecDeque<u8>,
    /// The client has closed its sending side: the server reads the end of the stream, and may
    /// still write.
    pub client_closed: bool,
    /// The server has called `close`.
    pub server_closed: bool,
    /// Every call from the server fails as broken.
    pub broken: bool,
    /// The most bytes the server's side holds unread by the client; `None` for no bound.
    pub capacity: Option<usize>,
}

/// The server's end of a pipe.
pub struct End(pub Rc<RefCell<Pipe>>);

impl Transport for End {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let mut pipe = self.0.borrow_mut();
        if pipe.broken {
            return Err(TransportError::Broken);
        }
        if pipe.to_server.is_empty() {
            return if pipe.client_closed {
                Ok(0)
            } else {
                Err(TransportError::WouldBlock)
            };
        }
        let count = buf.len().min(pipe.to_server.len());
        for (slot, byte) in buf.iter_mut().zip(pipe.to_server.drain(..count)) {
            *slot = byte;
        }
        Ok(count)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, TransportError> {
        let mut pipe = self.0.borrow_mut();
        if pipe.broken || pipe.server_closed {
            return Err(TransportError::Broken);
        }
        let room = pipe
            .capacity
            .map_or(buf.len(), |c| c.saturating_sub(pipe.to_client.len()));
        let count = room.min(buf.len());
        if count == 0 {
            return Err(TransportError::WouldBlock);
        }
        pipe.to_client.extend(&buf[..count]);
        Ok(count)
    }

    fn close(&mut self) {
        let mut pipe = self.0.borrow_mut();
        assert!(!pipe.server_closed, "closed twice");
        pipe.server_closed = true;
    }
}

/// A clock the test moves.
#[derive(Clone, Debug)]
pub struct TestClock {
    pub ms: Rc<Cell<u64>>,
    pub unix: Rc<Cell<u64>>,
}

impl TestClock {
    pub fn new() -> Self {
        Self {
            ms: Rc::new(Cell::new(1_000)),
            unix: Rc::new(Cell::new(WALL)),
        }
    }

    /// Moves both clocks on by `ms` milliseconds.
    pub fn advance(&self, ms: u64) {
        self.ms.set(self.ms.get() + ms);
        self.unix.set(WALL + (self.ms.get() - 1_000) / 1_000);
    }
}

impl Clock for TestClock {
    fn monotonic_ms(&self) -> u64 {
        self.ms.get()
    }

    fn unix_seconds(&self) -> u64 {
        self.unix.get()
    }
}

/// The node's facts in every test: the design's example document but its clock.
pub fn facts() -> NodeFacts {
    NodeFacts {
        version: "0.7.0".to_string(),
        instance: INSTANCE.to_string(),
        cloud: Cloud::OpenStack,
        cores: 4,
        memory_bytes: 8_589_934_592,
        executors: vec![Executor {
            kind: ExecutorKind::Cpu,
            slots: 4,
        }],
        boot_ms: 2140,
        health: NodeHealth::Ok,
    }
}

/// Accepts a server's certificate when the SHA-256 of its DER equals the pin, and checks nothing
/// else about it; the handshake's signature is still verified.
#[derive(Debug)]
pub struct Pin {
    pub fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl Pin {
    pub fn new(fingerprint: &str) -> Self {
        Self {
            fingerprint: fingerprint.to_string(),
            provider: Arc::new(default_provider()),
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest = digest::digest(&digest::SHA256, end_entity.as_ref());
        if hex(digest.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the certificate is not the pinned one".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not spoken".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A client configuration that pins `fingerprint`, TLS 1.3 only, offering HTTP/1.1.
pub fn pinned(fingerprint: &str) -> Arc<ClientConfig> {
    let mut config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pin::new(fingerprint)))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// One client: a rustls client over a pipe to the server.
pub struct Client {
    pub tls: ClientConnection,
    pub pipe: Rc<RefCell<Pipe>>,
    /// Plaintext the server has sent and the test has not yet read.
    pub received: Vec<u8>,
    /// The client's TLS failed with this.
    pub error: Option<rustls::Error>,
    /// The server sent its close_notify.
    pub closed_by_server: bool,
}

impl Client {
    /// Moves bytes both ways between the client's TLS and the pipe. Says whether any moved.
    pub fn pump(&mut self) -> bool {
        let mut moved = false;
        while self.tls.wants_write() {
            let mut out = Vec::new();
            let count = self.tls.write_tls(&mut out).unwrap();
            if count == 0 {
                break;
            }
            let mut pipe = self.pipe.borrow_mut();
            if !pipe.client_closed {
                pipe.to_server.extend(out);
            }
            moved = true;
        }
        let incoming: Vec<u8> = self.pipe.borrow_mut().to_client.drain(..).collect();
        if !incoming.is_empty() && self.error.is_none() {
            moved = true;
            let mut at = 0;
            while at < incoming.len() {
                let count = self.tls.read_tls(&mut &incoming[at..]).unwrap();
                at += count;
                match self.tls.process_new_packets() {
                    Ok(state) => {
                        if state.peer_has_closed() {
                            self.closed_by_server = true;
                        }
                    }
                    Err(error) => {
                        self.error = Some(error);
                        break;
                    }
                }
                let mut buf = [0u8; 4096];
                loop {
                    match self.tls.reader().read(&mut buf) {
                        Ok(0) => break,
                        Ok(count) => self.received.extend_from_slice(&buf[..count]),
                        Err(_) => break,
                    }
                }
            }
        }
        moved
    }

    /// Queues `bytes` as plaintext for the server.
    pub fn send(&mut self, bytes: &[u8]) {
        self.tls.writer().write_all(bytes).unwrap();
    }

    /// Closes the client's side of the stream, without a close_notify.
    pub fn hang_up(&mut self) {
        self.pipe.borrow_mut().client_closed = true;
    }

    pub fn server_closed(&self) -> bool {
        self.pipe.borrow().server_closed
    }

    /// Takes one whole response from what has been received, if one has.
    pub fn response(&mut self) -> Option<Response> {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut parsed = httparse::Response::new(&mut headers);
        let httparse::Status::Complete(head) = parsed.parse(&self.received).ok()? else {
            return None;
        };
        let headers: Vec<(String, String)> = parsed
            .headers
            .iter()
            .map(|h| {
                (
                    h.name.to_ascii_lowercase(),
                    String::from_utf8(h.value.to_vec()).unwrap(),
                )
            })
            .collect();
        let length: usize = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .map(|(_, value)| value.parse().unwrap())
            .unwrap_or(0);
        if self.received.len() < head + length {
            return None;
        }
        let response = Response {
            status: parsed.code.unwrap(),
            headers,
            body: self.received[head..head + length].to_vec(),
        };
        self.received.drain(..head + length);
        Some(response)
    }
}

/// A response as the client read it.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    /// The error body's code, checking the body is exactly 2A's error body.
    pub fn error_code(&self) -> String {
        let body = self.json();
        let object = body.as_object().unwrap();
        assert_eq!(object.len(), 1, "{body}");
        let error = object["error"].as_object().unwrap();
        assert_eq!(error.len(), 2, "{body}");
        assert!(error["message"].is_string());
        error["code"].as_str().unwrap().to_string()
    }
}

/// A server and its clients over in-memory pipes, with a clock the test moves, and the token
/// issuer whose key is the node's.
pub struct Harness {
    pub server: Server<End, TestClock>,
    pub clock: TestClock,
    pub fingerprint: String,
    pub clients: Vec<Client>,
    pub signer: Signer,
}

impl Harness {
    /// A server with the node's own identity, made at the test's wall clock.
    pub fn new() -> Self {
        Self::with_identity(Identity::generate(INSTANCE, WALL).unwrap())
    }

    pub fn with_identity(identity: Identity) -> Self {
        let signer = Signer::new();
        Self::with(identity, vec![TokenKey::new(signer.public())], signer)
    }

    /// A server with `identity` and `token_keys`, whose tests sign with `signer`.
    pub fn with(identity: Identity, token_keys: Vec<TokenKey>, signer: Signer) -> Self {
        let clock = TestClock::new();
        let fingerprint = identity.fingerprint();
        let server = Server::new(
            Config {
                identity,
                facts: facts(),
                token_keys,
            },
            clock.clone(),
        )
        .unwrap();
        Self {
            server,
            clock,
            fingerprint,
            clients: Vec::new(),
            signer,
        }
    }

    /// A valid token for this node with `scope`, expiring in ten minutes.
    pub fn token(&self, scope: &[&str]) -> String {
        self.signer
            .token(INSTANCE, self.clock.unix_seconds() + 600, scope)
    }

    /// The header line that carries `token`.
    pub fn bearer(token: &str) -> String {
        format!("Authorization: Bearer {token}\r\n")
    }

    /// A GET of `path` on client `index` with a valid token for `scope`.
    pub fn get_as(&mut self, index: usize, path: &str, scope: &[&str]) -> Response {
        let header = Self::bearer(&self.token(scope));
        self.get(index, path, &header)
    }

    /// Connects a new client with `config`, and says which it is.
    pub fn connect_with(&mut self, config: Arc<ClientConfig>) -> usize {
        let pipe = Rc::new(RefCell::new(Pipe::default()));
        self.server.accept(End(pipe.clone()));
        let mut tls = ClientConnection::new(config, ServerName::try_from("node").unwrap()).unwrap();
        // A test may send a whole 1 MiB body at once.
        tls.set_buffer_limit(None);
        self.clients.push(Client {
            tls,
            pipe,
            received: Vec::new(),
            error: None,
            closed_by_server: false,
        });
        self.clients.len() - 1
    }

    /// Connects a client that pins the server's certificate.
    pub fn connect(&mut self) -> usize {
        let config = pinned(&self.fingerprint);
        self.connect_with(config)
    }

    /// Polls the server and pumps every client until nothing moves.
    pub fn settle(&mut self) {
        for _ in 0..10_000 {
            self.server.poll();
            let mut moved = false;
            for client in &mut self.clients {
                moved |= client.pump();
            }
            if !moved {
                // One more poll, for what the last pump delivered.
                self.server.poll();
                if !self.clients.iter_mut().any(Client::pump) {
                    return;
                }
            }
        }
        panic!("the exchange never settled");
    }

    /// Sends `request` on client `index` and takes its response.
    pub fn exchange(&mut self, index: usize, request: &[u8]) -> Response {
        self.clients[index].send(request);
        self.settle();
        self.clients[index]
            .response()
            .unwrap_or_else(|| panic!("no response; client error {:?}", self.clients[index].error))
    }

    /// A GET of `path` on client `index`, with these extra header lines.
    pub fn get(&mut self, index: usize, path: &str, headers: &str) -> Response {
        let request = format!("GET {path} HTTP/1.1\r\nHost: node\r\n{headers}\r\n");
        self.exchange(index, request.as_bytes())
    }
}
