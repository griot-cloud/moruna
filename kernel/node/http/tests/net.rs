//! The stream through the network server's sockets (`net::NetStream`) against the server's own
//! logic (`moruna-netstack`) over real smoltcp loopback, with its TLS: an HTTPS `GET` by name,
//! verified for that name, as node-admission fetches a function image (system design 3E); a
//! name the policy does not list, and a certificate for another name, refused; and a plain
//! stream to an address, as platform asks instance data.

use std::{
    collections::BTreeMap,
    io::{Cursor, Read, Write},
    rc::Rc,
    sync::Arc,
    thread, vec,
};

use moruna_netstack::{
    Shared, Stack, after_turn, factory, parse,
    tls::{Trust, TrustedTime},
};
use moruna_node_http::{
    Request, Stream, exchange,
    net::{LOAN_BYTES, NetStream},
};
use moruna_svc::{Channel, Dispatcher, Memory, Turn, deadline_in, now};
use netstack_protocol::{Addr, Endpoint, Target, dns, net};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::{ServerConfig, ServerConnection, pki_types::PrivatePkcs8KeyDer};

/// The network server: a resolver it asks at 127.0.0.1; `listen-job`, a job's channel that
/// reaches one name at 443 (system design 3D); and `listen-peer` for the test's own servers.
const CONFIG: &str = "moruna-netstack 1
setting dns-queries 4
dns 127.0.0.1
grant listen-job rights=connect local=any names=fn.example.test@443 sockets=8
grant listen-peer rights=connect,listen,udp,resolve remote=127.0.0.0/8 local=127.0.0.0/8 sockets=16
";

const NAME: &str = "fn.example.test";
const SECOND: u64 = 1_000_000_000;

/// A CA, and the TLS server config of a peer whose certificate it signed for `name`.
fn credentials(name: &str) -> (Vec<u8>, Arc<ServerConfig>) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![name.to_string()])
        .unwrap()
        .signed_by(&key, &ca, &ca_key)
        .unwrap();
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
    (ca.der().to_vec(), Arc::new(config))
}

/// Serves `CONFIG` with `root` as TLS's one root and a set wall clock; the client end of every
/// slot's listener.
fn serve(root: Vec<u8>) -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    let config = parse(CONFIG).expect("the configuration");
    let mut ours = BTreeMap::new();
    let mut theirs = Vec::new();
    for (slot, policy) in &config.grants {
        let (o, t) = Channel::create().expect("a channel");
        ours.insert(slot.clone(), o);
        theirs.push((t, policy.clone()));
    }
    let join = thread::spawn(move || {
        let mut stack = Stack::new(&config, &[], 0x5eed, now()).expect("the stack");
        let time = TrustedTime::new(|| Some(1_800_000_000 * SECOND));
        stack
            .install_tls(Trust::new(vec![root], time).expect("the roots"))
            .expect("TLS");
        let shared = Shared::new(stack);
        let mut d = Dispatcher::new();
        for (listener, policy) in theirs {
            d.listen(listener, factory(Rc::clone(&shared), policy))
                .expect("listen");
        }
        loop {
            after_turn(&shared, &mut d, now());
            let deadline = {
                let t = now();
                let mut st = shared.stack.borrow_mut();
                st.poll_at()
                    .unwrap_or(t + 20_000_000)
                    .clamp(t, t + 20_000_000)
            };
            if d.channels() == 0 || d.turn(deadline).expect("a turn") == Turn::Empty {
                break;
            }
        }
    });
    (ours, join)
}

fn client(listener: &Channel) -> net::Client {
    let mut c = net::Client::connect(
        moruna_svc::connect(listener).unwrap(),
        deadline_in(10 * SECOND),
        moruna_svc::Account::carve(8 << 20, "http-test").unwrap(),
    )
    .unwrap();
    c.core().set_timeout(10 * SECOND);
    c
}

/// Answers every DNS question with 127.0.0.1 until none comes for two seconds.
fn resolver(peer: &Channel) -> thread::JoinHandle<()> {
    let mut resolver = client(peer);
    let (udp, _) = resolver
        .bind(Endpoint::new(Addr::v4(127, 0, 0, 1), 53))
        .unwrap();
    resolver.core().set_timeout(2 * SECOND);
    thread::spawn(move || {
        while let Ok((from, bytes)) = resolver.receive_from(udp) {
            let question = dns::parse_query(&bytes).unwrap();
            let answer = dns::answer(&question, true, &[Addr::v4(127, 0, 0, 1)], 0);
            resolver.send_to(udp, from, answer).unwrap();
        }
    })
}

/// Writes all of `bytes` on `stream`.
fn write_all(stream: &mut impl Stream, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let taken = stream.write(bytes, deadline_in(10 * SECOND)).unwrap();
        bytes = &bytes[taken..];
    }
}

/// An HTTPS server on the peer's next connection: it reads one request and answers `body` in
/// `Content-Length`, or stops at a failed handshake. What it read of the request.
fn https_peer(
    mut peer: net::Client,
    listening: u32,
    config: Arc<ServerConfig>,
    body: Vec<u8>,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let opened = peer.accept(listening).unwrap();
        let loan = Memory::create_in(LOAN_BYTES as u64, &peer.account().0).unwrap();
        let mut stream = NetStream::adopt(&mut peer, loan, opened.socket);
        let mut tls = ServerConnection::new(config).unwrap();
        // The whole answer is queued at once.
        tls.set_buffer_limit(None);
        let mut request = Vec::new();
        let mut answered = false;
        let mut buffer = vec![0u8; 16384];
        loop {
            while tls.wants_write() {
                let mut out = Vec::new();
                tls.write_tls(&mut out).unwrap();
                write_all(&mut stream, &out);
            }
            if answered {
                return request;
            }
            if request.ends_with(b"\r\n\r\n") {
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                tls.writer().write_all(head.as_bytes()).unwrap();
                tls.writer().write_all(&body).unwrap();
                tls.send_close_notify();
                answered = true;
                continue;
            }
            let n = match stream.read(&mut buffer, deadline_in(10 * SECOND)) {
                Ok(0) | Err(_) => return request,
                Ok(n) => n,
            };
            tls.read_tls(&mut Cursor::new(&buffer[..n])).unwrap();
            if tls.process_new_packets().is_err() {
                return request;
            }
            let mut plain = [0u8; 4096];
            while let Ok(n) = tls.reader().read(&mut plain) {
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&plain[..n]);
            }
        }
    })
}

#[test]
fn an_https_get_by_name_is_verified_for_that_name_and_read_whole() {
    let (root, config) = credentials(NAME);
    let (mut slots, server) = serve(root);
    let peer_listener = slots.remove("listen-peer").unwrap();
    let job_listener = slots.remove("listen-job").unwrap();
    let dns = resolver(&peer_listener);
    let mut peer = client(&peer_listener);
    let (listening, _) = peer
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 443), 4)
        .unwrap();
    // An image larger than one loan, so the stream moves it in several calls.
    let image: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let served = https_peer(peer, listening, config, image.clone());

    let mut job = client(&job_listener);
    let mut stream = NetStream::open(
        &mut job,
        Target::Name(NAME.into(), 443),
        Some(NAME),
        deadline_in(10 * SECOND),
    )
    .unwrap();
    let response = moruna_node_http::exchange_within(
        &mut stream,
        &Request::get(NAME, "/images/f.wasm", &[]),
        deadline_in(10 * SECOND),
        1 << 20,
    )
    .unwrap();
    assert_eq!(response.status, 200);
    assert!(response.body == image, "the image changed on the way");
    drop(stream);
    let request = String::from_utf8(served.join().unwrap()).unwrap();
    assert!(request.starts_with("GET /images/f.wasm HTTP/1.1\r\nHost: fn.example.test\r\n"));

    drop(job);
    drop(slots);
    drop(peer_listener);
    drop(job_listener);
    dns.join().unwrap();
    server.join().unwrap();
}

#[test]
fn a_name_the_policy_does_not_list_or_a_certificate_for_another_is_refused() {
    let (root, config) = credentials("other.example.test");
    let (mut slots, server) = serve(root);
    let peer_listener = slots.remove("listen-peer").unwrap();
    let job_listener = slots.remove("listen-job").unwrap();
    let dns = resolver(&peer_listener);
    let mut peer = client(&peer_listener);
    let (listening, _) = peer
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 443), 4)
        .unwrap();
    let served = https_peer(peer, listening, config, b"never".to_vec());
    let mut job = client(&job_listener);

    // A name the job's channel does not reach: refused before anything is resolved.
    let refused = NetStream::open(
        &mut job,
        Target::Name("other.example.test".into(), 443),
        Some("other.example.test"),
        deadline_in(10 * SECOND),
    )
    .err()
    .unwrap();
    assert_eq!(
        refused.to_string(),
        "the network's policy does not permit it"
    );
    // The right name, but the peer's certificate is for another: no TLS, and no request sent.
    let failed = NetStream::open(
        &mut job,
        Target::Name(NAME.into(), 443),
        Some(NAME),
        deadline_in(10 * SECOND),
    )
    .err()
    .unwrap();
    assert!(failed.to_string().starts_with("TLS failed"), "{failed}");
    assert!(served.join().unwrap().is_empty());

    drop(job);
    drop(slots);
    drop(peer_listener);
    drop(job_listener);
    dns.join().unwrap();
    server.join().unwrap();
}

#[test]
fn a_plain_stream_to_an_address_carries_http() {
    let (root, _) = credentials(NAME);
    let (mut slots, server) = serve(root);
    let peer_listener = slots.remove("listen-peer").unwrap();
    let mut listener_side = client(&peer_listener);
    let (listening, at) = listener_side
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let answering = thread::spawn(move || {
        let opened = listener_side.accept(listening).unwrap();
        let loan = Memory::create_in(LOAN_BYTES as u64, &listener_side.account().0).unwrap();
        let mut stream = NetStream::adopt(&mut listener_side, loan, opened.socket);
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.ends_with(b"\r\n\r\n") {
            let n = stream.read(&mut buffer, deadline_in(10 * SECOND)).unwrap();
            request.extend_from_slice(&buffer[..n]);
        }
        write_all(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nDate: Tue, 17 Nov 2026 09:30:00 GMT\r\n\r\nhello",
        );
        request
    });
    let mut asker = client(&peer_listener);
    let mut stream = NetStream::open(
        &mut asker,
        Target::Endpoint(at),
        None,
        deadline_in(10 * SECOND),
    )
    .unwrap();
    let response = exchange(
        &mut stream,
        &Request::get("169.254.169.254", "/latest", &[]),
        deadline_in(10 * SECOND),
    )
    .unwrap();
    assert_eq!(
        (response.status, response.body.as_slice()),
        (200, &b"hello"[..])
    );
    assert!(
        answering
            .join()
            .unwrap()
            .starts_with(b"GET /latest HTTP/1.1\r\n")
    );
    // A deadline already passed is no call at all.
    assert_eq!(
        stream.read(&mut [0u8; 8], now().saturating_sub(1)).err(),
        Some(moruna_node_http::StreamError::Deadline)
    );

    drop(stream);
    drop(asker);
    drop(slots);
    drop(peer_listener);
    server.join().unwrap();
}
