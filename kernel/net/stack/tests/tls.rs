//! Server-owned TLS over real smoltcp loopback and service channels (SDD 31 k).
mod common;
use common::{no_external_cards, serve_with_tls};
use moruna_netstack::tls::{Trust, TrustedTime};
use moruna_svc::{Channel, deadline_in};
use netstack_protocol::{Addr, Endpoint, Target, net};
use rcgen::{BasicConstraints, Certificate, CertificateParams, IsCa, KeyPair};
use rustls::{ServerConfig, ServerConnection, pki_types::PrivatePkcs8KeyDer};
use std::{
    io::{Cursor, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

const CONFIG: &str = "moruna-netstack 1
setting tls-handshake-timeout-ms 2000
grant listen rights=connect,listen,resolve remote=127.0.0.0/8 local=127.0.0.0/8 sockets=8
";
// A fixed test UTC instant, inside rcgen's fixture validity (1975..4096), in nanoseconds.
const UTC_NS: u64 = 1_800_000_000_000_000_000;
/// The node's wall clock at a fixed time, as the clock page would say it.
fn wall_at(utc_ns: u64) -> TrustedTime {
    TrustedTime::new(move || Some(utc_ns))
}
fn client(channel: Channel) -> net::Client {
    let mut c = net::Client::connect(
        channel,
        deadline_in(10_000_000_000),
        moruna_svc::Account::carve(8 << 20, "net-test").unwrap(),
    )
    .unwrap();
    c.core().set_timeout(10_000_000_000);
    c
}
fn credentials() -> (Certificate, Arc<ServerConfig>) {
    credentials_with_names(1)
}
fn credentials_with_names(count: usize) -> (Certificate, Arc<ServerConfig>) {
    credentials_adjusted(count, |_| {})
}
/// A CA and a peer under it, the peer's certificate as `adjust` leaves it.
fn credentials_adjusted(
    count: usize,
    adjust: impl FnOnce(&mut CertificateParams),
) -> (Certificate, Arc<ServerConfig>) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().unwrap();
    let ca = ca.self_signed(&key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let mut names = vec![String::from("peer.ke2.test")];
    for i in 1..count {
        names.push(format!("additional-{i}.ke2.test"));
    }
    let mut leaf = CertificateParams::new(names).unwrap();
    adjust(&mut leaf);
    let leaf = leaf.signed_by(&leaf_key, &ca, &key).unwrap();
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
            )
            .unwrap();
    (ca, Arc::new(config))
}
fn trust(ca: &Certificate) -> Trust {
    Trust::new(vec![ca.der().to_vec()], wall_at(UTC_NS)).unwrap()
}
fn send(c: &mut net::Client, socket: u32, bytes: &[u8]) -> bool {
    let mut at = 0;
    while at < bytes.len() {
        let end = (at + netstack_protocol::SEND_MAX).min(bytes.len());
        match common::send(c, socket, &bytes[at..end]) {
            Ok(n) if n > 0 => at += n as usize,
            _ => return false,
        }
    }
    true
}
/// A TLS peer in a different client process/channel. No shared socket state or
/// bypass of rustls verification is used by the client being tested.
fn peer(mut c: net::Client, listener: u32, config: Arc<ServerConfig>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let opened = c.accept(listener).unwrap();
        let mut tls = ServerConnection::new(config).unwrap();
        loop {
            let bytes =
                match common::receive(&mut c, opened.socket, netstack_protocol::RECEIVE_MAX as u32)
                {
                    Ok(b) if !b.is_empty() => b,
                    _ => break,
                };
            tls.read_tls(&mut Cursor::new(bytes)).unwrap();
            if tls.process_new_packets().is_err() {
                break;
            }
            let mut plain = [0u8; 32];
            if let Ok(n) = tls.reader().read(&mut plain)
                && n > 0
            {
                assert_eq!(&plain[..n], b"ke2-private-data");
                tls.writer().write_all(b"verified-reply").unwrap();
                tls.send_close_notify();
            }
            let mut output = Vec::new();
            tls.write_tls(&mut output).unwrap();
            if !send(&mut c, opened.socket, &output) {
                break;
            }
            if !tls.is_handshaking() && plain.starts_with(b"ke2-private-data") {
                c.shutdown(opened.socket).unwrap();
                break;
            }
        }
    })
}

#[test]
fn verified_tls_exchanges_bytes_and_authenticates_eof() {
    let (ca, config) = credentials();
    let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let a = moruna_svc::connect(&listener).unwrap();
    let b = moruna_svc::connect(&listener).unwrap();
    let mut a = client(a);
    let mut b = client(b);
    let (listening, address) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(address), None).unwrap();
    let peer = peer(b, listening, config);
    a.start_tls(opened.socket, "peer.ke2.test".into()).unwrap();
    assert!(send(&mut a, opened.socket, b"ke2-private-data"));
    let mut reply = Vec::new();
    loop {
        let bytes = common::receive(&mut a, opened.socket, 32).unwrap();
        if bytes.is_empty() {
            break;
        }
        reply.extend(bytes);
    }
    assert_eq!(reply, b"verified-reply");
    a.shutdown(opened.socket).unwrap();
    drop(a);
    peer.join().unwrap();
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn wrong_name_and_untrusted_chain_fail_without_plaintext_downgrade() {
    for mode in ["chain", "name", "expired-certificate"] {
        let (ca, config) = credentials();
        let (other_ca, _) = credentials();
        let roots = if mode == "chain" { &other_ca } else { &ca };
        let configured_trust = if mode == "expired-certificate" {
            // 1973, before the test CA's certificates begin.
            Trust::new(vec![roots.der().to_vec()], wall_at(100_000_000_000_000_000)).unwrap()
        } else {
            trust(roots)
        };
        let (mut slots, server) =
            serve_with_tls(CONFIG, &[], no_external_cards(), Some(configured_trust));
        let listener = slots.remove("listen").unwrap();
        let mut a = client(moruna_svc::connect(&listener).unwrap());
        let mut b = client(moruna_svc::connect(&listener).unwrap());
        let (l, at) = b
            .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
            .unwrap();
        let opened = a.open(Target::Endpoint(at), None).unwrap();
        let peer = peer(b, l, config);
        assert_eq!(
            a.start_tls(
                opened.socket,
                if mode == "name" {
                    "wrong.ke2.test"
                } else {
                    "peer.ke2.test"
                }
                .into()
            ),
            Err(moruna_svc::CallError::Remote(net::Error::TlsFailed))
        );
        assert!(common::send(&mut a, opened.socket, b"must-not-be-plaintext").is_err());
        assert!(common::receive(&mut a, opened.socket, 32).is_err());
        drop(a);
        peer.join().unwrap();
        drop(listener);
        drop(slots);
        server.join().unwrap();
    }
}

#[test]
fn plaintext_activity_prevents_an_upgrade() {
    let (ca, _) = credentials();
    let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (l, at) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let outgoing = a.open(Target::Endpoint(at), None).unwrap();
    let incoming = b.accept(l).unwrap();
    assert_eq!(
        b.start_tls(incoming.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::NotConnected))
    );
    assert_eq!(
        common::send(&mut a, outgoing.socket, b"already-plaintext").unwrap(),
        17
    );
    assert_eq!(
        common::receive(&mut b, incoming.socket, 32).unwrap(),
        b"already-plaintext"
    );
    assert_eq!(
        a.start_tls(outgoing.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::InUse))
    );
    // The accepted socket cannot be reinterpreted as an outbound TLS client.
    assert_eq!(
        b.start_tls(incoming.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::InUse))
    );
    drop(a);
    drop(b);
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn unauthenticated_tcp_eof_cannot_finish_a_handshake() {
    let (ca, _) = credentials();
    let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (l, at) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(at), None).unwrap();
    let peer = thread::spawn(move || {
        let incoming = b.accept(l).unwrap();
        assert!(
            !common::receive(&mut b, incoming.socket, 32)
                .unwrap()
                .is_empty()
        );
        b.shutdown(incoming.socket).unwrap();
    });
    assert_eq!(
        a.start_tls(opened.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::TlsFailed))
    );
    assert!(common::send(&mut a, opened.socket, b"no-downgrade").is_err());
    drop(a);
    peer.join().unwrap();
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn handshake_reserves_the_global_tls_budget_until_socket_close() {
    use moruna_svc::{Encoder, Wire};
    let (ca, _) = credentials();
    let config = CONFIG.replace(
        "setting tls-handshake-timeout-ms 2000",
        "setting tls-handshake-timeout-ms 2000\nsetting tls-sockets-max 1",
    );
    let (mut slots, server) = serve_with_tls(&config, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (l, at) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 2)
        .unwrap();
    let first = a.open(Target::Endpoint(at), None).unwrap();
    let _accepted = b.accept(l).unwrap();
    let second = a.open(Target::Endpoint(at), None).unwrap();
    let _accepted2 = b.accept(l).unwrap();
    let mut args = Encoder::new();
    first.socket.encode(&mut args).unwrap();
    String::from("peer.ke2.test").encode(&mut args).unwrap();
    let _pending = a.core().send(16, args).unwrap();
    assert_eq!(
        a.start_tls(second.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::TooMany))
    );
    a.close(first.socket).unwrap();
    // Closing releases capacity. Without a TLS peer this one reaches its own
    // deadline, rather than failing the previous connection's quota.
    assert_eq!(
        a.start_tls(second.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::TimedOut))
    );
    assert!(common::send(&mut a, second.socket, b"no-downgrade").is_err());
    drop(a);
    drop(b);
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn oversized_genuine_handshake_is_bounded_and_never_downgrades() {
    let (ca, config) = credentials_with_names(1500);
    let config_text = CONFIG.replace(
        "setting tls-handshake-timeout-ms 2000",
        "setting tls-handshake-timeout-ms 2000\nsetting tls-buffer-bytes 16640",
    );
    let (mut slots, server) =
        serve_with_tls(&config_text, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (l, at) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(at), None).unwrap();
    let peer = peer(b, l, config);
    assert_eq!(
        a.start_tls(opened.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::TooLarge))
    );
    assert!(common::send(&mut a, opened.socket, b"must-not-be-plaintext").is_err());
    drop(a);
    peer.join().unwrap();
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

/// Roots from the boot, and TLS's time from a wall clock the test sets: zero is unset, as the
/// clock page is until the core or `platform` sets it (system design 3D).
fn on_the_wall_clock(ca: &Certificate) -> (Trust, Arc<AtomicU64>) {
    let page = Arc::new(AtomicU64::new(0));
    let wall = page.clone();
    let time = TrustedTime::new(move || match wall.load(Ordering::Relaxed) {
        0 => None,
        ns => Some(ns),
    });
    (Trust::new(vec![ca.der().to_vec()], time).unwrap(), page)
}

#[test]
fn tls_waits_for_the_wall_clock_then_verifies() {
    let (ca, config) = credentials();
    let (trust, page) = on_the_wall_clock(&ca);
    let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust));
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (listening, address) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 2)
        .unwrap();
    // While the wall clock is unset: refused before the socket is touched (no handshake byte
    // reaches the peer), and the socket takes no plaintext after: no downgrade.
    let early = a.open(Target::Endpoint(address), None).unwrap();
    let early_peer = b.accept(listening).unwrap();
    assert_eq!(
        a.start_tls(early.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::TlsUnavailable))
    );
    refused_and_untouched(&mut a, early.socket, &mut b, early_peer.socket);
    // The clock is set: from now on TLS is verified against it, with nothing told to the
    // network server.
    page.store(UTC_NS, Ordering::Relaxed);
    let opened = a.open(Target::Endpoint(address), None).unwrap();
    let peer = peer(b, listening, config);
    a.start_tls(opened.socket, "peer.ke2.test".into()).unwrap();
    assert!(send(&mut a, opened.socket, b"ke2-private-data"));
    let mut reply = Vec::new();
    loop {
        let bytes = common::receive(&mut a, opened.socket, 32).unwrap();
        if bytes.is_empty() {
            break;
        }
        reply.extend(bytes);
    }
    assert_eq!(reply, b"verified-reply");
    a.shutdown(opened.socket).unwrap();
    drop(a);
    peer.join().unwrap();
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

/// After a `start_tls` refused before any handshake: the socket takes no plaintext (no
/// downgrade), and once it is closed its peer has received nothing at all, no handshake byte.
fn refused_and_untouched(a: &mut net::Client, socket: u32, b: &mut net::Client, peer: u32) {
    assert_eq!(
        common::send(a, socket, b"must-not-be-plaintext"),
        Err(moruna_svc::CallError::Remote(net::Error::TlsFailed))
    );
    a.close(socket).unwrap();
    let mut seen = Vec::new();
    while let Ok(bytes) = common::receive(b, peer, 64) {
        if bytes.is_empty() {
            break;
        }
        seen.extend(bytes);
    }
    assert_eq!(seen, b"", "the peer received bytes from a refused socket");
}

/// A TLS server peer that records every application byte it decrypts, until the handshake or
/// the connection fails.
fn recording_peer(
    mut c: net::Client,
    listener: u32,
    config: Arc<ServerConfig>,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let opened = c.accept(listener).unwrap();
        let mut tls = ServerConnection::new(config).unwrap();
        let mut plain = Vec::new();
        while let Ok(bytes) =
            common::receive(&mut c, opened.socket, netstack_protocol::RECEIVE_MAX as u32)
        {
            if bytes.is_empty()
                || tls.read_tls(&mut Cursor::new(bytes)).is_err()
                || tls.process_new_packets().is_err()
            {
                break;
            }
            let _ = tls.reader().read_to_end(&mut plain);
            let mut output = Vec::new();
            tls.write_tls(&mut output).unwrap();
            if !output.is_empty() && !send(&mut c, opened.socket, &output) {
                break;
            }
        }
        plain
    })
}

/// UTC nanoseconds at midnight of a day.
const JUNE_2025: u64 = 1_748_736_000_000_000_000;
const JUNE_2026: u64 = 1_780_272_000_000_000_000;
const JUNE_2027: u64 = 1_811_808_000_000_000_000;

#[test]
fn a_wall_clock_outside_the_peer_s_certificate_fails_the_handshake_and_moves_no_byte() {
    // The peer's certificate is valid for 2026 only. Its chain is trusted, its name right: the
    // wall clock alone decides.
    for (wall, verified) in [(JUNE_2025, false), (JUNE_2027, false), (JUNE_2026, true)] {
        let (ca, config) = credentials_adjusted(1, |leaf| {
            leaf.not_before = rcgen::date_time_ymd(2026, 1, 1);
            leaf.not_after = rcgen::date_time_ymd(2027, 1, 1);
        });
        let trust = Trust::new(vec![ca.der().to_vec()], wall_at(wall)).unwrap();
        let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust));
        let listener = slots.remove("listen").unwrap();
        let mut a = client(moruna_svc::connect(&listener).unwrap());
        let mut b = client(moruna_svc::connect(&listener).unwrap());
        let (l, at) = b
            .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
            .unwrap();
        let opened = a.open(Target::Endpoint(at), None).unwrap();
        let peer = recording_peer(b, l, config);
        let started = a.start_tls(opened.socket, "peer.ke2.test".into());
        if verified {
            assert_eq!(started, Ok(()), "{wall}");
            assert!(send(&mut a, opened.socket, b"ke2-private-data"));
            a.shutdown(opened.socket).unwrap();
            drop(a);
            assert_eq!(peer.join().unwrap(), b"ke2-private-data");
        } else {
            // Before the certificate begins, and after it ends: no TLS, and no byte of the
            // caller's goes anywhere, encrypted or plain.
            assert_eq!(
                started,
                Err(moruna_svc::CallError::Remote(net::Error::TlsFailed)),
                "{wall}"
            );
            assert!(common::send(&mut a, opened.socket, b"must-not-be-sent").is_err());
            assert!(common::receive(&mut a, opened.socket, 32).is_err());
            drop(a);
            assert_eq!(peer.join().unwrap(), b"", "{wall}");
        }
        drop(listener);
        drop(slots);
        server.join().unwrap();
    }
}

#[test]
fn with_no_roots_start_tls_is_refused_whatever_the_wall_clock_says() {
    // A server given no roots has no TLS at all: it takes no time either, so no clock, set or
    // not, can make a handshake start. Refused before the socket is touched, as an unset clock
    // is.
    let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), None);
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (l, at) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(at), None).unwrap();
    let incoming = b.accept(l).unwrap();
    assert_eq!(
        a.start_tls(opened.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::TlsUnavailable))
    );
    refused_and_untouched(&mut a, opened.socket, &mut b, incoming.socket);
    drop(a);
    drop(b);
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn caller_memory_validation_does_not_consume_queued_plaintext() {
    use moruna_svc::{CallError, Memory, Rights, map};
    let (mut slots, join) = common::serve(CONFIG, &[], no_external_cards());
    let service_listener = slots.remove("listen").unwrap();
    // The boot slot is a factory listener, not an established net session.
    let mut c = client(moruna_svc::connect(&service_listener).unwrap());
    let (listener, at) = c
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let outgoing = c.open(Target::Endpoint(at), None).unwrap();
    let incoming = c.accept(listener).unwrap();
    let foreign_account = moruna_svc::Account::carve(8 << 20, "foreign").unwrap();
    let foreign = Memory::create_in(4096, &foreign_account.0).unwrap();
    foreign
        .map(0, 4096, map::WRITE)
        .unwrap()
        .write(0, b"bad")
        .unwrap();
    let foreign_tx = Memory(
        foreign
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .unwrap(),
    );
    assert!(matches!(
        c.send_from(outgoing.socket, foreign_tx, 0, 3),
        Err(CallError::Remote(net::Error::BadArgument))
    ));
    common::send(&mut c, outgoing.socket, b"abc").unwrap();
    let foreign_rx = Memory(
        foreign
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .unwrap(),
    );
    assert!(matches!(
        c.receive_into(incoming.socket, foreign_rx, 0, 3),
        Err(CallError::Remote(net::Error::BadArgument))
    ));
    let memory = Memory::create_in(4096, &c.account().0).unwrap();
    let invalid = Memory(
        memory
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .unwrap(),
    );
    assert!(matches!(
        c.receive_into(incoming.socket, invalid, 0, 3),
        Err(CallError::Remote(net::Error::BadArgument))
    ));
    let loan = || {
        Memory(
            memory
                .0
                .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
                .unwrap(),
        )
    };
    assert!(matches!(
        c.receive_into(incoming.socket, loan(), u64::MAX, 3),
        Err(CallError::Remote(net::Error::BadArgument))
    ));
    assert!(matches!(
        c.receive_into(incoming.socket, loan(), 4095, 3),
        Err(CallError::Remote(net::Error::BadArgument))
    ));
    assert_eq!(c.receive_into(incoming.socket, loan(), 37, 3).unwrap(), 3);
    let mut bytes = [0; 3];
    memory
        .map(0, 4096, map::READ)
        .unwrap()
        .read(37, &mut bytes)
        .unwrap();
    assert_eq!(&bytes, b"abc");
    drop(c);
    drop(service_listener);
    drop(slots);
    join.join().unwrap();
}

fn server_identity(
    c: &mut net::Client,
    chain: Vec<Vec<u8>>,
    key: &[u8],
) -> Result<u32, moruna_svc::CallError<net::Error>> {
    server_identity_for(c, chain, key, 60_000_000_000)
}
fn server_identity_for(
    c: &mut net::Client,
    chain: Vec<Vec<u8>>,
    key: &[u8],
    lifetime_ns: u64,
) -> Result<u32, moruna_svc::CallError<net::Error>> {
    use moruna_svc::{Memory, Rights, map};
    let encoded = netstack_protocol::ServerIdentitySpec {
        certificates: chain,
        alpn: vec![b"ke2-test".to_vec()],
        lifetime_ns,
    }
    .encode()
    .unwrap();
    let loan = |bytes: &[u8]| {
        let pages = (bytes.len() as u64).div_ceil(4096) * 4096;
        let memory = Memory::create_in(pages, &c.account().0).unwrap();
        memory
            .map(0, pages, map::WRITE)
            .unwrap()
            .write(0, bytes)
            .unwrap();
        Memory(
            memory
                .0
                .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                .unwrap(),
        )
    };
    let control = loan(&encoded);
    let secret = loan(key);
    c.install_server_identity(control, encoded.len() as u32, secret, key.len() as u32)
}
#[test]
fn native_unbuffered_server_interoperates_with_verified_rustls_client_and_authenticated_eof() {
    use rustls::{ClientConfig, ClientConnection, RootCertStore, pki_types::ServerName};
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["peer.ke2.test".into()])
        .unwrap()
        .signed_by(&key, &ca, &ca_key)
        .unwrap();
    let secret = zeroize::Zeroizing::new(key.serialize_der());
    let (mut slots, join) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let mut server = client(moruna_svc::connect(&listener).unwrap());
    let mut peer = client(moruna_svc::connect(&listener).unwrap());
    let identity = server_identity(
        &mut server,
        vec![cert.der().to_vec(), ca.der().to_vec()],
        &secret,
    )
    .unwrap();
    let (socket, at) = server
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let outgoing = peer.open(Target::Endpoint(at), None).unwrap();
    let accepted = server.accept(socket).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let mut config = ClientConfig::builder_with_details(
        Arc::new(rustls::crypto::ring::default_provider()),
        Arc::new(wall_at(UTC_NS)),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"ke2-test".to_vec()];
    let mut tls = ClientConnection::new(
        Arc::new(config),
        ServerName::try_from("peer.ke2.test").unwrap(),
    )
    .unwrap();
    let mut hello = Vec::new();
    tls.write_tls(&mut hello).unwrap();
    assert!(send(&mut peer, outgoing.socket, &hello)); // ClientHello is queued BEFORE upgrade.
    let peer = thread::spawn(move || {
        let mut requested = false;
        let mut replied = false;
        loop {
            let bytes = common::receive(
                &mut peer,
                outgoing.socket,
                netstack_protocol::RECEIVE_MAX as u32,
            )
            .unwrap();
            assert!(!bytes.is_empty(), "unauthenticated TCP EOF");
            tls.read_tls(&mut Cursor::new(bytes)).unwrap();
            let state = tls.process_new_packets().unwrap();
            if !tls.is_handshaking() && !requested {
                tls.writer().write_all(b"from-client").unwrap();
                requested = true;
            }
            let mut plain = [0; 64];
            if let Ok(n) = tls.reader().read(&mut plain)
                && n > 0
            {
                assert_eq!(&plain[..n], b"from-server");
                replied = true;
                tls.send_close_notify();
            }
            let mut output = Vec::new();
            tls.write_tls(&mut output).unwrap();
            assert!(send(&mut peer, outgoing.socket, &output));
            if state.peer_has_closed() {
                assert!(requested && replied);
                break;
            }
        }
    });
    server.start_tls_server(accepted.socket, identity).unwrap();
    assert_eq!(
        server.tls_alpn(accepted.socket).unwrap(),
        Some(b"ke2-test".to_vec())
    );
    assert_eq!(
        common::receive(&mut server, accepted.socket, 64).unwrap(),
        b"from-client"
    );
    assert!(send(&mut server, accepted.socket, b"from-server"));
    assert!(
        common::receive(&mut server, accepted.socket, 64)
            .unwrap()
            .is_empty()
    );
    server.shutdown(accepted.socket).unwrap();
    peer.join().unwrap();
    server.release_server_identity(identity).unwrap();
    drop(server);
    drop(listener);
    drop(slots);
    join.join().unwrap();
}

#[test]
fn server_rejects_mismatched_keys_plaintext_upgrade_and_garbage_without_downgrade() {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let other = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["peer.ke2.test".into()])
        .unwrap()
        .signed_by(&key, &ca, &ca_key)
        .unwrap();
    let secret = zeroize::Zeroizing::new(key.serialize_der());
    let wrong = zeroize::Zeroizing::new(other.serialize_der());
    let (mut slots, join) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let mut server = client(moruna_svc::connect(&listener).unwrap());
    let mut peer = client(moruna_svc::connect(&listener).unwrap());
    assert!(matches!(
        server_identity(&mut server, vec![cert.der().to_vec()], &wrong),
        Err(moruna_svc::CallError::Remote(net::Error::TlsFailed))
    ));
    let identity = server_identity(&mut server, vec![cert.der().to_vec()], &secret).unwrap();
    let (listening, at) = server
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 2)
        .unwrap();
    let outgoing = peer.open(Target::Endpoint(at), None).unwrap();
    let incoming = server.accept(listening).unwrap();
    assert!(send(&mut peer, outgoing.socket, b"plaintext"));
    assert_eq!(
        common::receive(&mut server, incoming.socket, 32).unwrap(),
        b"plaintext"
    );
    assert!(matches!(
        server.start_tls_server(incoming.socket, identity),
        Err(moruna_svc::CallError::Remote(net::Error::InUse))
    ));
    server.close(incoming.socket).unwrap();
    peer.close(outgoing.socket).unwrap();
    let outgoing = peer.open(Target::Endpoint(at), None).unwrap();
    let incoming = server.accept(listening).unwrap();
    assert!(send(&mut peer, outgoing.socket, b"not-a-TLS-record"));
    assert!(matches!(
        server.start_tls_server(incoming.socket, identity),
        Err(moruna_svc::CallError::Remote(net::Error::TlsFailed))
    ));
    assert!(matches!(
        common::send(&mut server, incoming.socket, b"no-downgrade"),
        Err(moruna_svc::CallError::Remote(net::Error::TlsFailed))
    ));
    assert!(matches!(
        common::receive(&mut server, incoming.socket, 32),
        Err(moruna_svc::CallError::Remote(net::Error::TlsFailed))
    ));
    server.release_server_identity(identity).unwrap();
    assert!(matches!(
        server.release_server_identity(identity),
        Err(moruna_svc::CallError::Remote(net::Error::BadArgument))
    ));
    // Expiry is checked by the real dispatcher before its next request. A
    // one-nanosecond identity cannot be renewed by looking it up or releasing it.
    let expired = server_identity_for(&mut server, vec![cert.der().to_vec()], &secret, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1));
    assert!(matches!(
        server.release_server_identity(expired),
        Err(moruna_svc::CallError::Remote(net::Error::BadArgument))
    ));
    drop(server);
    drop(peer);
    drop(listener);
    drop(slots);
    join.join().unwrap();
}

#[test]
fn net3_refuses_old_majors_and_unbound_or_rebound_accounts() {
    use moruna_svc::{
        Account, CallError, ClientCore, Encoder, ProtocolError, ProtocolId, Rights, Wire,
    };
    let (mut slots, join) = serve_with_tls(CONFIG, &[], no_external_cards(), None);
    let listener = slots.remove("listen").unwrap();
    for major in [1, 2] {
        let result = ClientCore::connect(
            moruna_svc::connect(&listener).unwrap(),
            ProtocolId::new("moruna.net", major, 0),
            deadline_in(10_000_000_000),
        );
        assert!(matches!(result, Err(CallError::Refused(_))));
    }
    let mut raw = ClientCore::connect(
        moruna_svc::connect(&listener).unwrap(),
        net::ID,
        deadline_in(10_000_000_000),
    )
    .unwrap();
    assert!(matches!(raw.call(1, Encoder::new()),
        Err(CallError::Remote(code)) if code == net::Error::NotPermitted.code()));
    let account = Account::carve(8 << 20, "same-name").unwrap();
    let foreign = Account::carve(8 << 20, "same-name").unwrap();
    let encode = |account: &Account| {
        let mut body = Encoder::new();
        account
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER)
            .unwrap()
            .encode(&mut body)
            .unwrap();
        body
    };
    raw.call(17, encode(&account)).unwrap();
    raw.call(1, Encoder::new()).unwrap();
    assert!(matches!(raw.call(17, encode(&foreign)),
        Err(CallError::Remote(code)) if code == net::Error::InUse.code()));
    // Rejected replacement cannot remove the original mandatory binding.
    raw.call(1, Encoder::new()).unwrap();
    drop(raw);
    drop(listener);
    drop(slots);
    join.join().unwrap();
}

#[test]
fn the_cloud_profile_s_roots_are_mozilla_s_set_and_the_server_takes_them() {
    use moruna_svc::{Decoder, Wire};
    // `kernel/node/cloud/tls.roots`, as tls-roots.py converted curl's release (system design
    // 3D), read as the server reads its `tls-roots` capability: its length, then the Wire list.
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../node/cloud/tls.roots"
    ))
    .unwrap();
    let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(length + 4, bytes.len());
    assert!(length <= 1 << 20);
    let mut handles = [];
    let mut decoder = Decoder::new(&bytes[4..], &mut handles);
    let roots = Vec::<Vec<u8>>::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    assert!(roots.len() > 100, "{}", roots.len());
    assert!(roots.len() <= moruna_netstack::tls::ROOTS_MAX);
    // Every one is a certificate rustls takes as a root.
    Trust::new(roots, wall_at(UTC_NS)).unwrap();
}

#[test]
fn a_long_answer_closed_by_its_peer_is_read_whole_however_slowly() {
    // The peer answers 300 KiB, sends its close_notify and its FIN at once, as an HTTP server
    // does after a `Connection: close` answer, while the client has not read: the records
    // behind the full buffer, the close_notify among them, are still read, and the end is
    // the authenticated one.
    let (ca, config) = credentials();
    let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (listening, address) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(address), None).unwrap();
    let body: Vec<u8> = (0..300 * 1024u32).map(|i| (i % 253) as u8).collect();
    let answer = body.clone();
    let peer = thread::spawn(move || {
        let accepted = b.accept(listening).unwrap();
        let mut tls = ServerConnection::new(config).unwrap();
        tls.set_buffer_limit(None);
        let mut answered = false;
        loop {
            let mut output = Vec::new();
            tls.write_tls(&mut output).unwrap();
            if !output.is_empty() && !send(&mut b, accepted.socket, &output) {
                break;
            }
            if answered {
                b.shutdown(accepted.socket).unwrap();
                break;
            }
            let bytes = match common::receive(&mut b, accepted.socket, 4096) {
                Ok(bytes) if !bytes.is_empty() => bytes,
                _ => break,
            };
            tls.read_tls(&mut Cursor::new(bytes)).unwrap();
            tls.process_new_packets().unwrap();
            let mut request = [0u8; 64];
            if let Ok(n) = tls.reader().read(&mut request)
                && n > 0
            {
                tls.writer().write_all(&answer).unwrap();
                tls.send_close_notify();
                answered = true;
            }
        }
        drop(b);
    });
    a.start_tls(opened.socket, "peer.ke2.test".into()).unwrap();
    assert!(send(&mut a, opened.socket, b"get"));
    // Asleep while the whole answer, its close_notify and the FIN arrive.
    thread::sleep(std::time::Duration::from_millis(500));
    let mut read = Vec::new();
    loop {
        let bytes = common::receive(&mut a, opened.socket, 4096).unwrap();
        if bytes.is_empty() {
            break;
        }
        read.extend(bytes);
    }
    assert!(read == body, "{} bytes of {}", read.len(), body.len());
    drop(a);
    peer.join().unwrap();
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn an_answer_cut_by_a_fin_without_close_notify_is_delivered_then_the_truncation_reported() {
    // The peer answers and sends its FIN with no close_notify, as some HTTPS servers do after a
    // Content-Length body, while the client has not read. The bytes it sent before the FIN were
    // each authenticated: the client reads them all, and only the read past them fails, as
    // truncated (found by 3.6's boot, a fetch from such a server).
    let (ca, config) = credentials();
    let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(trust(&ca)));
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (listening, address) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(address), None).unwrap();
    let body: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    let answer = body.clone();
    let peer = thread::spawn(move || {
        let accepted = b.accept(listening).unwrap();
        let mut tls = ServerConnection::new(config).unwrap();
        let mut answered = false;
        loop {
            let mut output = Vec::new();
            tls.write_tls(&mut output).unwrap();
            if !output.is_empty() && !send(&mut b, accepted.socket, &output) {
                break;
            }
            if answered {
                // The FIN, with no close_notify before it.
                b.shutdown(accepted.socket).unwrap();
                break;
            }
            let bytes = match common::receive(&mut b, accepted.socket, 4096) {
                Ok(bytes) if !bytes.is_empty() => bytes,
                _ => break,
            };
            tls.read_tls(&mut Cursor::new(bytes)).unwrap();
            tls.process_new_packets().unwrap();
            let mut request = [0u8; 64];
            if let Ok(n) = tls.reader().read(&mut request)
                && n > 0
            {
                tls.writer().write_all(&answer).unwrap();
                answered = true;
            }
        }
        drop(b);
    });
    a.start_tls(opened.socket, "peer.ke2.test".into()).unwrap();
    assert!(send(&mut a, opened.socket, b"get"));
    // Asleep while the whole answer and the FIN arrive.
    thread::sleep(std::time::Duration::from_millis(500));
    let mut read = Vec::new();
    let end = loop {
        match common::receive(&mut a, opened.socket, 1024) {
            Ok(bytes) if !bytes.is_empty() => read.extend(bytes),
            other => break other,
        }
    };
    assert!(read == body, "{} bytes of {}", read.len(), body.len());
    assert!(
        matches!(
            end,
            Err(moruna_svc::CallError::Remote(net::Error::TlsFailed))
        ),
        "the end is {end:?}, not the truncation"
    );
    drop(a);
    peer.join().unwrap();
    drop(listener);
    drop(slots);
    server.join().unwrap();
}

/// A TLS peer that answers the client's request `get` with `total` bytes, then ends as an
/// HTTPS server may: close_notify, then its TCP end.
fn sender(
    mut c: net::Client,
    listener: u32,
    config: Arc<ServerConfig>,
    total: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let opened = c.accept(listener).unwrap();
        let mut tls = ServerConnection::new(config).unwrap();
        let mut asked = false;
        while !asked {
            let bytes =
                common::receive(&mut c, opened.socket, netstack_protocol::RECEIVE_MAX as u32)
                    .unwrap();
            assert!(!bytes.is_empty(), "the client ended before its request");
            tls.read_tls(&mut Cursor::new(bytes)).unwrap();
            tls.process_new_packets().unwrap();
            let mut request = [0u8; 16];
            if let Ok(n) = tls.reader().read(&mut request)
                && n > 0
            {
                assert_eq!(&request[..n], b"get");
                asked = true;
            }
            let mut output = Vec::new();
            tls.write_tls(&mut output).unwrap();
            assert!(send(&mut c, opened.socket, &output));
        }
        let chunk = vec![0x5a; 16 * 1024];
        let mut sent = 0;
        while sent < total {
            sent += tls
                .writer()
                .write(&chunk[..(total - sent).min(chunk.len())])
                .unwrap();
            let mut output = Vec::new();
            while tls.wants_write() {
                tls.write_tls(&mut output).unwrap();
            }
            assert!(send(&mut c, opened.socket, &output));
        }
        tls.send_close_notify();
        let mut output = Vec::new();
        while tls.wants_write() {
            tls.write_tls(&mut output).unwrap();
        }
        assert!(send(&mut c, opened.socket, &output));
        c.shutdown(opened.socket).unwrap();
    })
}

/// `total` bytes read over TLS from a peer on loopback through a server configured by
/// `config`: the server's meter at its end, and how long the read took, in nanoseconds.
fn download(config: &str, total: usize) -> (moruna_netstack::Meter, u64) {
    let (ca, peer_config) = credentials();
    let (mut slots, server) = common::serve_metered(
        config,
        &[],
        no_external_cards(),
        Some(trust(&ca)),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (listening, address) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(address), None).unwrap();
    let peer = sender(b, listening, peer_config, total);
    a.start_tls(opened.socket, "peer.ke2.test".into()).unwrap();
    let began = moruna_svc::now();
    assert!(send(&mut a, opened.socket, b"get"));
    let mut read = 0;
    loop {
        let bytes =
            common::receive(&mut a, opened.socket, netstack_protocol::RECEIVE_MAX as u32).unwrap();
        if bytes.is_empty() {
            break;
        }
        assert!(bytes.iter().all(|b| *b == 0x5a));
        read += bytes.len();
    }
    let took = moruna_svc::now() - began;
    assert_eq!(read, total);
    a.shutdown(opened.socket).unwrap();
    drop(a);
    peer.join().unwrap();
    drop(listener);
    drop(slots);
    (server.join().unwrap(), took)
}

/// The network server's meter (work item 6.3ab) counts a TLS download where it happens: the
/// ciphertext TLS took from TCP, the plaintext it opened, the bytes copied to the clients, and
/// the time of each part.
#[test]
fn the_meter_counts_a_tls_download() {
    let total = 1 << 20;
    let (meter, _) = download(CONFIG, total);
    let c = meter.counts;
    // The client's TLS opened exactly the peer's answer, from more ciphertext than that.
    assert_eq!(c.opened_bytes, total as u64);
    assert!(c.tls_in_bytes > total as u64, "{c:?}");
    assert!(c.open_ns > 0, "{c:?}");
    // It sealed the request, and gave TCP its record and the handshake's.
    assert_eq!(c.sealed_bytes, 3);
    assert!(c.seal_ns > 0 && c.tls_out_bytes > 3, "{c:?}");
    // Copied to the clients: the client's plaintext, and the client's handshake and request to
    // the peer; out of them, the peer's ciphertext and the client's request.
    assert!(c.to_clients_bytes > total as u64, "{c:?}");
    assert!(c.from_clients_bytes > total as u64, "{c:?}");
    assert!(
        c.receives >= (total / netstack_protocol::RECEIVE_MAX) as u64,
        "{c:?}"
    );
    // Each receive tried once, when its socket had bytes or its end (work item 6.3ad): every
    // try answered, the last with the end of the stream.
    assert_eq!(c.receive_tries, c.receives + 1, "{c:?}");
    assert!(c.sends > 0, "{c:?}");
    assert!(c.to_clients_ns > 0 && c.from_clients_ns > 0, "{c:?}");
    // The polls and the loop, timed.
    assert!(
        c.polls > 0 && c.ingress_ns > 0 && c.service_ns > 0 && c.egress_ns > 0,
        "{c:?}"
    );
    assert!(c.turns > 0 && c.busy_ns > 0, "{c:?}");
    assert!(c.requests > 0 && c.requests_ns > 0, "{c:?}");
    // The TLS socket went with its client.
    assert_eq!(c.tls_sockets, 0);
}

/// A receive waiting on a socket with no data is not tried at the server's polls (work item
/// 6.3ad: run 10 tried each answered receive about 35 times, mapping its loan at each try). It
/// is tried once its socket has data, and answered then.
#[test]
fn a_pending_receive_is_tried_only_once_its_socket_has_data() {
    let (mut slots, server) = common::serve_metered(
        CONFIG,
        &[],
        no_external_cards(),
        None,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let mut c = client(moruna_svc::connect(&listener).unwrap());
    let (listening, address) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(address), None).unwrap();
    let accepted = b.accept(listening).unwrap();
    // `a` waits, on a thread of its own, for bytes its peer has not sent.
    let waiting = thread::spawn(move || {
        let bytes = common::receive(&mut a, opened.socket, 64).unwrap();
        (a, bytes)
    });
    // Meanwhile the server polls, at another client's requests and at its own timers.
    thread::sleep(std::time::Duration::from_millis(100));
    for _ in 0..200 {
        c.info().unwrap();
    }
    thread::sleep(std::time::Duration::from_millis(100));
    assert!(send(&mut b, accepted.socket, b"x"));
    let (mut a, bytes) = waiting.join().unwrap();
    assert_eq!(bytes, b"x");
    a.close(opened.socket).unwrap();
    b.close(accepted.socket).unwrap();
    drop((a, b, c, listener, slots));
    let counts = server.join().unwrap().counts;
    assert!(counts.polls > 200, "{counts:?}");
    // One try, once the byte had come, and it answered.
    assert_eq!(
        (counts.receive_tries, counts.receives),
        (1, 1),
        "{counts:?}"
    );
}

/// What the network server's own work costs a TLS download on this host, by its meter (work
/// item 6.3ab): 64 MiB over loopback at Ethernet's MTU, so TCP takes the node's segments. It
/// asserts only what it read: run it in a release build with `--ignored --nocapture` for the
/// figures.
#[test]
#[ignore = "a measurement: run in release with --ignored --nocapture"]
fn the_network_servers_own_cost_of_a_tls_download() {
    let total = 64 << 20;
    let config = "moruna-netstack 1
setting tls-handshake-timeout-ms 2000
setting loopback-mtu 1500
grant listen rights=connect,listen,resolve remote=127.0.0.0/8 local=127.0.0.0/8 sockets=8
";
    let (meter, took) = download(config, total);
    let c = meter.counts;
    assert_eq!(c.opened_bytes, total as u64);
    let rate = |bytes: u64, ns: u64| bytes as f64 * 1e3 / ns.max(1) as f64;
    let mb = total as f64 / 1e6;
    std::eprintln!(
        "6.3ab host: {mb:.0} MB over TLS at MTU 1500 in {:.3} s ({:.0} MB/s end to end); \
         server busy {:.3} s ({:.0} MB/s): its loop {:.3} s in {} turns, {} polls, \
         {} requests {:.3} s; \
         ingress {:.3} s, service {:.3} s, egress {:.3} s; \
         TLS open {:.3} s ({:.0} MB/s of plaintext); copies to clients {:.3} s over {} tries \
         ({:.0} MB/s of {} MB), from clients {:.3} s ({:.0} MB/s of {} MB)",
        took as f64 / 1e9,
        rate(total as u64, took),
        (c.busy_ns + c.requests_ns) as f64 / 1e9,
        rate(total as u64, c.busy_ns + c.requests_ns),
        c.busy_ns as f64 / 1e9,
        c.turns,
        c.polls,
        c.requests,
        c.requests_ns as f64 / 1e9,
        c.ingress_ns as f64 / 1e9,
        c.service_ns as f64 / 1e9,
        c.egress_ns as f64 / 1e9,
        c.open_ns as f64 / 1e9,
        rate(c.opened_bytes, c.open_ns),
        c.to_clients_ns as f64 / 1e9,
        c.receive_tries,
        rate(c.to_clients_bytes, c.to_clients_ns),
        c.to_clients_bytes / 1_000_000,
        c.from_clients_ns as f64 / 1e9,
        rate(c.from_clients_bytes, c.from_clients_ns),
        c.from_clients_bytes / 1_000_000,
    );
}
