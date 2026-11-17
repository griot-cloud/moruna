//! The whole process (work item 2.2b), as the node runs it: `platform`'s hand-over over a
//! real channel, the identity's line, `ready` once the listener opens, and the API served
//! by the real `moruna-api-server` over the network server's real stack, to an HTTPS
//! client on the host's card that pins the fingerprint the console showed.

use std::{
    io::{Read, Write},
    sync::Mutex,
};

use moruna_node_platform::{
    machine::{Link, LinkError},
    svc::ChannelLink,
};
use netstack_protocol::Target;
use ring::digest;
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, ring::default_provider, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};

use super::{
    common::{receive, send},
    *,
};
use crate::network::Network;

/// Accepts a server's certificate when the SHA-256 of its DER is the pin, and checks
/// nothing else about it; the handshake's signature is still verified (2E's pinning).
#[derive(Debug)]
struct Pin {
    fingerprint: String,
    provider: CryptoProvider,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest = digest::digest(&digest::SHA256, end_entity.as_ref());
        if hex(digest.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not the pinned certificate".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
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

/// An answer, as the client read it.
#[derive(Debug)]
struct Answer {
    status: u16,
    body: serde_json::Value,
}

/// An HTTPS connection from the host to the node, pinning `fingerprint`.
struct Https {
    host: net::Client,
    socket: u32,
    tls: ClientConnection,
    received: Vec<u8>,
}

impl Https {
    fn open(outside: &Channel, at: Endpoint, fingerprint: &str) -> Https {
        let mut host = host(outside);
        let opened = host.open(Target::Endpoint(at), None).expect("connected");
        let mut config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin {
                fingerprint: String::from(fingerprint),
                provider: default_provider(),
            }))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tls = ClientConnection::new(
            Arc::new(config),
            ServerName::try_from("node.moruna.test").unwrap(),
        )
        .unwrap();
        Https {
            host,
            socket: opened.socket,
            tls,
            received: Vec::new(),
        }
    }

    /// `GET path`, with `token` when given: the answer.
    fn get(&mut self, path: &str, token: Option<&str>) -> Answer {
        let authorization =
            token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        let request = format!("GET {path} HTTP/1.1\r\nHost: node\r\n{authorization}\r\n");
        self.tls.writer().write_all(request.as_bytes()).unwrap();
        loop {
            while self.tls.wants_write() {
                let mut out = Vec::new();
                self.tls.write_tls(&mut out).unwrap();
                let mut at = 0;
                while at < out.len() {
                    at += send(&mut self.host, self.socket, &out[at..]).expect("sent") as usize;
                }
            }
            if let Some(answer) = self.answer() {
                return answer;
            }
            let bytes = receive(&mut self.host, self.socket, 16 * 1024).expect("received");
            assert!(!bytes.is_empty(), "the node closed the connection");
            self.tls.read_tls(&mut &bytes[..]).unwrap();
            self.tls.process_new_packets().expect("the node's TLS");
            let mut plain = [0u8; 16 * 1024];
            while let Ok(count) = self.tls.reader().read(&mut plain) {
                if count == 0 {
                    break;
                }
                self.received.extend_from_slice(&plain[..count]);
            }
        }
    }

    /// One whole answer from what has been received, if one has.
    fn answer(&mut self) -> Option<Answer> {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut parsed = httparse::Response::new(&mut headers);
        let httparse::Status::Complete(head) = parsed.parse(&self.received).ok()? else {
            return None;
        };
        let length: usize = parsed
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("content-length"))
            .map(|h| std::str::from_utf8(h.value).unwrap().parse().unwrap())
            .unwrap_or(0);
        if self.received.len() < head + length {
            return None;
        }
        let answer = Answer {
            status: parsed.code.unwrap(),
            body: serde_json::from_slice(&self.received[head..head + length]).unwrap(),
        };
        self.received.drain(..head + length);
        Some(answer)
    }
}

/// The node, `api` running on a thread of its own with 3 cores, and `platform`'s end of
/// its link: what it has said, and its thread, which ends when it stops.
struct Api {
    lines: Arc<Mutex<Vec<String>>>,
    thread: thread::JoinHandle<Error>,
    platform: ChannelLink,
    /// `api`'s listener slot of the network server, for another connection through it.
    slot: Channel,
}

fn start_api(node: &mut Node) -> Api {
    let (client, slot) = node.api();
    let (ours, theirs) = Channel::create().expect("a channel");
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let said = lines.clone();
    let thread = thread::spawn(move || {
        let Err(error) = run(client, &theirs, 3, &mut |line: &str| {
            said.lock().unwrap().push(line.to_string())
        });
        error
    });
    Api {
        lines,
        thread,
        platform: ChannelLink(ours),
        slot,
    }
}

/// The node serves its API through the real server over the real stack: the identity's
/// line before `ready`, `ready` once the listener opens and once only, and `health`,
/// `node`, the audit record and a refused token over HTTPS pinned to that line's
/// fingerprint.
#[test]
fn the_node_serves_its_api_over_https_on_the_leased_address() {
    let mut node = Node::start(false);
    let outside = node.slot("listen-host");
    let mut api = start_api(&mut node);
    let signer = Signer::new();
    let handover = handover_of(&user_data(&[signer.public()], None));
    api.platform
        .send(
            &handover.encode().expect("encoded"),
            deadline_in(5 * SECOND),
        )
        .expect("the hand-over");

    // The identity is made from the hand-over and said at once; with no lease there is no
    // listener, so no `ready`.
    assert_eq!(
        api.platform.receive(deadline_in(SECOND / 2)),
        Err(LinkError::TimedOut)
    );
    let fingerprint = {
        let lines = api.lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let fingerprint = lines[0]
            .strip_prefix("moruna: tls sha256=")
            .expect("the identity's line")
            .to_string();
        assert_eq!(fingerprint.len(), 64);
        fingerprint
    };

    node.lease.answers.store(true, Ordering::SeqCst);
    assert_eq!(
        api.platform.receive(deadline_in(30 * SECOND)),
        Ok(READY.to_vec())
    );
    let ready_at = WALL_CLOCK;
    assert_eq!(
        api.lines.lock().unwrap()[1],
        "api: listening on 10.0.2.15:443"
    );

    let at = Endpoint::new(NODE_ADDRESS, 443);
    let mut https = Https::open(&outside, at, &fingerprint);
    let health = https.get("/v1/health", None);
    assert_eq!(health.status, 200);
    assert_eq!(health.body, serde_json::json!({"status": "ok"}));

    let observe = signer.token(INSTANCE, WALL_CLOCK + 1_800, &["observe"]);
    let document = https.get("/v1/node", Some(&observe));
    assert_eq!(document.status, 200, "{:?}", document.body);
    let body = &document.body;
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["instance"], INSTANCE);
    assert_eq!(body["cloud"], "openstack");
    assert_eq!(body["cores"], 3);
    assert_eq!(body["memory_bytes"], MEMORY_BYTES);
    assert_eq!(
        body["executors"],
        serde_json::json!([{"kind": "cpu", "slots": 3}])
    );
    assert_eq!(body["health"], "ok");
    let boot_ms = body["boot_ms"].as_u64().expect("boot_ms");
    assert!(boot_ms > 0 && boot_ms <= now() / 1_000_000, "{boot_ms}");
    let time = body["time"].as_u64().expect("time");
    assert!((ready_at..ready_at + 60).contains(&time), "{time}");

    // A token another key signed is refused, on its own connection.
    let mut other = Https::open(&outside, at, &fingerprint);
    let forged = Signer::new().token(INSTANCE, WALL_CLOCK + 1_800, &["observe"]);
    let refused = other.get("/v1/node", Some(&forged));
    assert_eq!(refused.status, 401);
    assert_eq!(refused.body["error"]["code"], "unauthorized");

    // Both calls are in the audit record, newest first.
    let admin = signer.token(INSTANCE, WALL_CLOCK + 1_800, &["admin"]);
    let audit = https.get("/v1/node/audit", Some(&admin));
    assert_eq!(audit.status, 200, "{:?}", audit.body);
    let records: Vec<&serde_json::Value> = audit.body["records"]
        .as_array()
        .expect("records")
        .iter()
        .filter(|r| r["operation"] != "node_audit")
        .collect();
    let calls: Vec<(&str, u64)> = records
        .iter()
        .map(|r| {
            (
                r["operation"].as_str().unwrap(),
                r["status"].as_u64().unwrap(),
            )
        })
        .collect();
    // `health` leaves none.
    assert_eq!(calls, [("node", 401), ("node", 200)]);
    assert_eq!(records[0]["key"], "");
    assert_eq!(records[1]["key"], TokenKey::new(signer.public()).id());
    assert_eq!(records[1]["scope"], serde_json::json!(["observe"]));

    // The lease moves: the listener follows it and the API answers there; `ready` is not
    // said again.
    node.lease.offer.store(16, Ordering::SeqCst);
    node.lease.renew.store(true, Ordering::SeqCst);
    let moved = "api: listening on 10.0.2.16:443";
    let deadline = Instant::now() + Duration::from_secs(30);
    while !api.lines.lock().unwrap().iter().any(|l| l == moved) {
        assert!(Instant::now() < deadline, "{:?}", api.lines.lock().unwrap());
        thread::sleep(Duration::from_millis(50));
    }
    let mut moved = Https::open(
        &outside,
        Endpoint::new(Addr::v4(10, 0, 2, 16), 443),
        &fingerprint,
    );
    assert_eq!(moved.get("/v1/health", None).status, 200);
    assert_eq!(
        api.platform.receive(deadline_in(SECOND)),
        Err(LinkError::TimedOut)
    );

    node.stop.store(true, Ordering::SeqCst);
    let error = api.thread.join().expect("api");
    assert!(
        matches!(error, Error::Network(_, CallError::PeerClosed)),
        "{error:?}"
    );
    drop((https, other, moved, outside));
    node.end();
}

/// A hand-over that is refused stops the process before it makes an identity or listens.
#[test]
fn a_refused_hand_over_stops_the_process_before_it_listens() {
    let mut node = Node::start(true);
    let mut api = start_api(&mut node);
    api.platform
        .send(b"{}", deadline_in(5 * SECOND))
        .expect("sent");
    let error = api.thread.join().expect("api");
    assert!(matches!(error, Error::Handover(_)), "{error:?}");
    assert!(
        Fatal(&error)
            .to_string()
            .starts_with("moruna: fatal api: the hand-over is refused: ")
    );
    assert!(api.lines.lock().unwrap().is_empty());
    // Nothing listens: the port is free for another.
    let mut other = Network::new(connect(&api.slot, account()).expect("another connection"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match other.tend(now()) {
            Ok(Some(Event::Listening(_))) => break,
            Ok(_) => {}
            Err(error) => panic!("{error:?}"),
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(50));
    }
    drop((other, api.slot, api.platform));
    node.end();
}
