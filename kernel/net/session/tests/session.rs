//! A socket's session (`moruna-net-session`) against the network server's own logic
//! (`moruna-netstack`) over real smoltcp loopback, with its TLS: a receive waits while a send goes
//! on the same socket from another thread; a close answers a receive that waits; TLS through the
//! reader; a name the job does not list refused; and many sockets at once, each both ways.

use std::{
    collections::BTreeMap,
    io::{Cursor, ErrorKind, Read, Write},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use moruna_net_session::{Network, Socket};
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
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::{ServerConfig, ServerConnection, pki_types::PrivatePkcs8KeyDer};

/// The network server: a resolver at 127.0.0.1; `listen-job`, a job's channel that reaches one
/// name at 443 (system design 3D); and `listen-peer` for the test's own servers.
const CONFIG: &str = "moruna-netstack 1
setting dns-queries 4
dns 127.0.0.1
grant listen-job rights=connect local=any names=peer.session.test@443 sockets=32
grant listen-peer rights=connect,listen,udp,resolve remote=127.0.0.0/8 local=127.0.0.0/8 sockets=48
";

const NAME: &str = "peer.session.test";
const SECOND: u64 = 1_000_000_000;

fn credentials() -> (Vec<u8>, Arc<ServerConfig>) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![NAME.to_string()])
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
        Account::carve(8 << 20, "session-peer").unwrap(),
    )
    .unwrap();
    c.core().set_timeout(30 * SECOND);
    c
}

fn resolver(peer: &Channel) -> thread::JoinHandle<()> {
    let mut resolver = client(peer);
    let (udp, _) = resolver
        .bind(Endpoint::new(Addr::v4(127, 0, 0, 1), 53))
        .unwrap();
    resolver.core().set_timeout(3 * SECOND);
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
        let taken = stream.write(bytes, deadline_in(30 * SECOND)).unwrap();
        bytes = &bytes[taken..];
    }
}

/// The test's server at 127.0.0.1:443 for `connections` connections, one after another: each
/// sends back what it receives until the client finishes sending, then finishes too.
fn echo(peer: &Channel, connections: usize) -> thread::JoinHandle<()> {
    let mut peer = client(peer);
    let (listening, _) = peer
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 443), 16)
        .unwrap();
    thread::spawn(move || {
        for _ in 0..connections {
            let opened = peer.accept(listening).unwrap();
            let loan = Memory::create_in(LOAN_BYTES as u64, &peer.account().0).unwrap();
            let socket = opened.socket;
            let mut stream = NetStream::adopt(&mut peer, loan, socket);
            let mut buffer = vec![0u8; 16384];
            loop {
                match stream.read(&mut buffer, deadline_in(30 * SECOND)) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => write_all(&mut stream, &buffer[..n]),
                }
            }
            drop(stream);
        }
    })
}

/// The job's network over `listener`.
fn network(listener: Channel) -> Network {
    Network::new(listener, Account::carve(16 << 20, "job").unwrap())
}

/// Sends all of `bytes` on `socket`.
fn send_all(socket: &Socket, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let taken = socket.send(bytes).unwrap();
        assert!(taken > 0);
        bytes = &bytes[taken..];
    }
}

/// Receives exactly `want` bytes from `socket`.
fn receive_exactly(socket: &Socket, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buffer = vec![0u8; 32 * 1024];
    while got.len() < want {
        let n = socket.receive(&mut buffer).unwrap();
        assert!(n > 0, "the stream ended after {} of {want}", got.len());
        got.extend_from_slice(&buffer[..n]);
    }
    got
}

#[test]
fn a_receive_waits_while_a_send_goes_on_the_same_socket() {
    let (root, _) = credentials();
    let (mut slots, server) = serve(root);
    let peer = slots.remove("listen-peer").unwrap();
    let job = network(slots.remove("listen-job").unwrap());
    let dns = resolver(&peer);
    let echoed = echo(&peer, 1);
    let socket = Arc::new(job.open(NAME, 443).unwrap());
    // One thread waits to receive before anything was sent: its request is out, its reply not.
    let waiting = Arc::new(AtomicBool::new(false));
    let receiver = {
        let (socket, waiting) = (Arc::clone(&socket), Arc::clone(&waiting));
        thread::spawn(move || {
            waiting.store(true, Ordering::SeqCst);
            receive_exactly(&socket, 4)
        })
    };
    while !waiting.load(Ordering::SeqCst) {
        thread::yield_now();
    }
    thread::sleep(Duration::from_millis(200));
    // The send goes on while it waits, and its echo reaches the waiting thread.
    send_all(&socket, b"ping");
    assert_eq!(receiver.join().unwrap(), b"ping");
    drop(socket);
    echoed.join().unwrap();
    drop((job, slots, peer));
    dns.join().unwrap();
    server.join().unwrap();
}

#[test]
fn a_close_answers_a_receive_that_waits() {
    let (root, _) = credentials();
    let (mut slots, server) = serve(root);
    let peer = slots.remove("listen-peer").unwrap();
    let job = network(slots.remove("listen-job").unwrap());
    let dns = resolver(&peer);
    let echoed = echo(&peer, 1);
    let socket = Arc::new(job.open(NAME, 443).unwrap());
    let receiver = {
        let socket = Arc::clone(&socket);
        thread::spawn(move || socket.receive(&mut [0u8; 64]))
    };
    thread::sleep(Duration::from_millis(200));
    let started = std::time::Instant::now();
    socket.close();
    let answered = receiver.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    // Answered, not left waiting: an error, or the end of the stream.
    assert!(matches!(&answered, Err(_) | Ok(0)), "{answered:?}");
    // Closing again does nothing, and nothing more moves.
    socket.close();
    assert!(socket.send(b"late").is_err());
    drop(socket);
    echoed.join().unwrap();
    drop((job, slots, peer));
    dns.join().unwrap();
    server.join().unwrap();
}

#[test]
fn tls_through_the_reader_and_a_name_the_job_does_not_list() {
    let (root, config) = credentials();
    let (mut slots, server) = serve(root);
    let peer_listener = slots.remove("listen-peer").unwrap();
    let job = network(slots.remove("listen-job").unwrap());
    let dns = resolver(&peer_listener);
    // An HTTPS server for one connection: one request, one answer, then its close_notify.
    let mut peer = client(&peer_listener);
    let (listening, _) = peer
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 443), 4)
        .unwrap();
    let body: Vec<u8> = (0..150_000u32).map(|i| (i % 241) as u8).collect();
    let answer = body.clone();
    let served = thread::spawn(move || {
        let opened = peer.accept(listening).unwrap();
        let loan = Memory::create_in(LOAN_BYTES as u64, &peer.account().0).unwrap();
        let mut stream = NetStream::adopt(&mut peer, loan, opened.socket);
        let mut tls = ServerConnection::new(config).unwrap();
        tls.set_buffer_limit(None);
        let (mut request, mut answered) = (Vec::new(), false);
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
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    answer.len()
                );
                tls.writer().write_all(head.as_bytes()).unwrap();
                tls.writer().write_all(&answer).unwrap();
                tls.send_close_notify();
                answered = true;
                continue;
            }
            let n = match stream.read(&mut buffer, deadline_in(30 * SECOND)) {
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
    });

    // A name the job's channel does not reach: refused, as the connector reads a refusal.
    let refused = job.open("elsewhere.session.test", 443).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::PermissionDenied, "{refused}");

    let socket = job.open(NAME, 443).unwrap();
    socket.start_tls(NAME).unwrap();
    send_all(
        &socket,
        b"GET /object HTTP/1.1\r\nHost: peer.session.test\r\n\r\n",
    );
    let mut read = Vec::new();
    let mut buffer = vec![0u8; 32 * 1024];
    loop {
        let n = socket.receive(&mut buffer).unwrap();
        if n == 0 {
            break;
        }
        read.extend_from_slice(&buffer[..n]);
    }
    let head_end = read.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(
        read[head_end..] == body[..],
        "the answer changed on the way"
    );
    assert!(
        String::from_utf8(served.join().unwrap())
            .unwrap()
            .starts_with("GET /object HTTP/1.1\r\n")
    );
    // TLS once only, as the network server allows it.
    assert!(socket.start_tls(NAME).is_err());
    drop(socket);
    drop((job, slots, peer_listener));
    dns.join().unwrap();
    server.join().unwrap();
}

#[test]
fn many_sockets_at_once_each_both_ways() {
    let (root, _) = credentials();
    let (mut slots, server) = serve(root);
    let peer = slots.remove("listen-peer").unwrap();
    let job = Arc::new(network(slots.remove("listen-job").unwrap()));
    let dns = resolver(&peer);
    const SOCKETS: usize = 8;
    let echoed = echo(&peer, SOCKETS);
    let workers: Vec<_> = (0..SOCKETS)
        .map(|k| {
            let job = Arc::clone(&job);
            thread::spawn(move || {
                let socket = Arc::new(job.open(NAME, 443).unwrap());
                let data: Vec<u8> = (0..200_000u32)
                    .map(|i| (i as usize * 7 + k) as u8)
                    .collect();
                // One thread sends while another receives the echo, on the same socket.
                let receiver = {
                    let (socket, want) = (Arc::clone(&socket), data.len());
                    thread::spawn(move || receive_exactly(&socket, want))
                };
                send_all(&socket, &data);
                let back = receiver.join().unwrap();
                assert!(back == data, "socket {k}: the bytes changed on the way");
                k
            })
        })
        .collect();
    let mut done: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    done.sort_unstable();
    assert_eq!(done, (0..SOCKETS).collect::<Vec<_>>());
    echoed.join().unwrap();
    drop((job, slots, peer));
    dns.join().unwrap();
    server.join().unwrap();
}
