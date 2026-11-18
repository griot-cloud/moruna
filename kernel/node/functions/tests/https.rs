//! A function image fetched over HTTPS as `node-admission` fetches it (system design 3D, 3E),
//! against the network server's own logic (`moruna-netstack`) over real smoltcp loopback with its
//! TLS: a channel minted from admission's client for the address's host alone, the image read
//! whole; an image over the limit, an answer that is not 200, and an address that is not
//! `https://` at 443, each refused.

use std::{
    collections::BTreeMap,
    io::{Cursor, Read, Write},
    rc::Rc,
    sync::Arc,
    thread,
};

use moruna_netstack::{
    Shared, Stack, after_turn, factory, parse,
    tls::{Trust, TrustedTime},
};
use moruna_node_http::{
    Stream,
    net::{LOAN_BYTES, NetStream},
};
use moruna_svc::{Account, Channel, Dispatcher, Memory, Turn, deadline_in, now};
use netstack_protocol::{Addr, Endpoint, dns, net};
use node_functions::{
    Fetch, FetchError,
    https::{HttpsFetch, target},
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::{ServerConfig, ServerConnection, pki_types::PrivatePkcs8KeyDer};

/// The network server: a resolver at 127.0.0.1; `listen-admission` as the cloud profile gives
/// node-admission its client (every name at 443, `grant` and `connect`: system design 3D); and
/// `listen-peer` for the test's own servers.
const CONFIG: &str = "moruna-netstack 1
setting dns-queries 4
dns 127.0.0.1
grant listen-admission rights=grant,connect local=any names=*@443 sockets=64
grant listen-peer rights=connect,listen,udp,resolve remote=127.0.0.0/8 local=127.0.0.0/8 sockets=16
";

const SECOND: u64 = 1_000_000_000;

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
        Account::carve(8 << 20, "fetch-test").unwrap(),
    )
    .unwrap();
    c.core().set_timeout(10 * SECOND);
    c
}

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

fn write_all(stream: &mut impl Stream, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let taken = stream.write(bytes, deadline_in(10 * SECOND)).unwrap();
        bytes = &bytes[taken..];
    }
}

/// An HTTPS server for `answers` connections, one after another: each reads one request and
/// answers its status and body. The requests it read.
fn https_peer(
    mut peer: net::Client,
    listening: u32,
    config: Arc<ServerConfig>,
    answers: Vec<(u16, Vec<u8>)>,
) -> thread::JoinHandle<Vec<String>> {
    thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in answers {
            let opened = peer.accept(listening).unwrap();
            let loan = Memory::create_in(LOAN_BYTES as u64, &peer.account().0).unwrap();
            let mut stream = NetStream::adopt(&mut peer, loan, opened.socket);
            let mut tls = ServerConnection::new(config.clone()).unwrap();
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
                    break;
                }
                if request.ends_with(b"\r\n\r\n") {
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    tls.writer().write_all(head.as_bytes()).unwrap();
                    tls.writer().write_all(&body).unwrap();
                    tls.send_close_notify();
                    answered = true;
                    continue;
                }
                let n = match stream.read(&mut buffer, deadline_in(10 * SECOND)) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                tls.read_tls(&mut Cursor::new(&buffer[..n])).unwrap();
                if tls.process_new_packets().is_err() {
                    break;
                }
                let mut plain = [0u8; 4096];
                while let Ok(n) = tls.reader().read(&mut plain) {
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&plain[..n]);
                }
            }
            requests.push(String::from_utf8(request).unwrap());
        }
        requests
    })
}

#[test]
fn an_image_is_fetched_over_https_through_a_channel_minted_for_its_host() {
    let host = "fn.example.test";
    let (root, config) = credentials(host);
    let (mut slots, server) = serve(root);
    let peer_listener = slots.remove("listen-peer").unwrap();
    let admission_listener = slots.remove("listen-admission").unwrap();
    let dns = resolver(&peer_listener);
    let mut peer = client(&peer_listener);
    let (listening, _) = peer
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 443), 4)
        .unwrap();
    let image: Vec<u8> = (0..300_000u32).map(|i| (i * 13 % 247) as u8).collect();
    let served = https_peer(
        peer,
        listening,
        config,
        vec![
            (200, image.clone()),
            (200, image.clone()),
            (404, b"not here".to_vec()),
        ],
    );

    let mut admission = client(&admission_listener);
    let account = Account::carve(16 << 20, "admission").unwrap();
    let mut fetch = HttpsFetch::new(&mut admission, &account, 10 * SECOND);
    // Read whole, by its host's name, the query kept (a pre-signed URL's), the fragment not.
    let fetched = fetch
        .fetch(
            "https://FN.example.test:443/f/image.wasm?sig=abc#part",
            1 << 20,
        )
        .unwrap();
    assert!(fetched == image, "the image changed on the way");
    // Over the limit: refused, and no more of it read than the limit and a head.
    assert_eq!(
        fetch.fetch("https://fn.example.test/f/image.wasm", 1000),
        Err(FetchError::TooLarge)
    );
    // Not there: the address and the answer.
    assert_eq!(
        fetch.fetch("https://fn.example.test/gone", 1 << 20),
        Err(FetchError::Failed(
            "https://fn.example.test/gone: answered 404".into()
        ))
    );
    let requests = served.join().unwrap();
    assert!(
        requests[0].starts_with(
            "GET /f/image.wasm?sig=abc HTTP/1.1\r\nHost: fn.example.test\r\nConnection: close\r\n"
        ),
        "{}",
        requests[0]
    );
    // A host with no server: the address and the failure, and nothing else reached.
    let refused = fetch.fetch("https://other.example.test/f", 1 << 20);
    assert!(
        matches!(&refused, Err(FetchError::Failed(why)) if why.starts_with("https://other.example.test/f: ")),
        "{refused:?}"
    );

    drop(admission);
    drop(slots);
    drop(peer_listener);
    drop(admission_listener);
    dns.join().unwrap();
    server.join().unwrap();
}

#[test]
fn only_https_at_443_by_name_is_an_address() {
    assert_eq!(
        target("https://Store.Example.Test/a/b?x=1"),
        Ok(("store.example.test".into(), "/a/b?x=1".into()))
    );
    assert_eq!(
        target("https://store.example.test"),
        Ok(("store.example.test".into(), "/".into()))
    );
    assert_eq!(
        target("https://store.example.test?sv=1"),
        Ok(("store.example.test".into(), "/?sv=1".into()))
    );
    for (address, why) in [
        ("http://store.example.test/a", "the address is not https://"),
        ("s3://bucket/key", "the address is not https://"),
        (
            "https://user:pass@store.example.test/a",
            "the address carries credentials before its host",
        ),
        (
            "https://store.example.test:8443/a",
            "the address names a port other than 443",
        ),
        ("https:///a", "the address names no host"),
        (
            "https://[::1]/a",
            "the address names an address, not a host",
        ),
    ] {
        assert_eq!(target(address), Err(why), "{address}");
    }
}
