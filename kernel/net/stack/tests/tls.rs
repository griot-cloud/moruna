//! Server-owned TLS over real smoltcp loopback and service channels (SDD 31 k).
mod common;
use common::{no_external_cards, serve_with_tls};
use moruna_netstack::tls::{Trust, TrustedTime};
use moruna_svc::{Channel, deadline_in, now};
use netstack_protocol::{Addr, Endpoint, Target, net};
use rcgen::{BasicConstraints, Certificate, CertificateParams, IsCa, KeyPair};
use rustls::{ServerConfig, ServerConnection, pki_types::PrivatePkcs8KeyDer};
use std::{
    io::{Cursor, Read, Write},
    sync::Arc,
    thread,
};

const CONFIG: &str = "moruna-netstack 1
setting tls-handshake-timeout-ms 2000
grant listen rights=connect,listen,resolve remote=127.0.0.0/8 local=127.0.0.0/8 sockets=8
";
// A fixed test UTC instant, inside rcgen's fixture validity (1975..4096).
const UTC: u64 = 1_800_000_000;
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
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().unwrap();
    let ca = ca.self_signed(&key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let mut names = vec![String::from("peer.ke2.test")];
    for i in 1..count {
        names.push(format!("additional-{i}.ke2.test"));
    }
    let leaf = CertificateParams::new(names)
        .unwrap()
        .signed_by(&leaf_key, &ca, &key)
        .unwrap();
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
    Trust::new(
        vec![ca.der().to_vec()],
        TrustedTime::new(UTC, now(), 60_000_000_000).unwrap(),
    )
    .unwrap()
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
            Trust::new(
                vec![roots.der().to_vec()],
                TrustedTime::new(253_402_300_000, now(), 60_000_000_000).unwrap(),
            )
            .unwrap()
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

#[test]
fn an_expired_boot_anchor_refuses_tls_before_any_handshake() {
    let (ca, _) = credentials();
    let roots = trust(&ca);
    roots.time.update(now().saturating_add(60_000_000_000));
    let (mut slots, server) = serve_with_tls(CONFIG, &[], no_external_cards(), Some(roots));
    let listener = slots.remove("listen").unwrap();
    let mut a = client(moruna_svc::connect(&listener).unwrap());
    let mut b = client(moruna_svc::connect(&listener).unwrap());
    let (l, at) = b
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)
        .unwrap();
    let opened = a.open(Target::Endpoint(at), None).unwrap();
    let _incoming = b.accept(l).unwrap();
    assert_eq!(
        a.start_tls(opened.socket, "peer.ke2.test".into()),
        Err(moruna_svc::CallError::Remote(net::Error::TlsUnavailable))
    );
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
        Arc::new(TrustedTime::new(UTC, now(), 60_000_000_000).unwrap()),
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
