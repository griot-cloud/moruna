//! A grant's bounds (work item 1.3b), through `moruna.net` over loopback: `sockets=N`
//! counts every socket of the grant's listeners and connections until it is freed, its
//! closing ones included, whichever client connection through the grant's slot holds
//! them; `rx=` and `tx=` are the buffers of the grant's TCP sockets and its UDP
//! sockets' payload buffers; and a listener minted by `grant` has buffers within its
//! minter's (moruna.net 3.1).

mod common;

use std::time::{Duration, Instant};

use common::{no_external_cards, serve};
use moruna_svc::{CallError, Channel, deadline_in};
use netstack_protocol::{Addr, Endpoint, Policy, Target, net, rights};

const SECOND: u64 = 1_000_000_000;
const HERE: Addr = Addr::v4(127, 0, 0, 1);

fn client(listener: &Channel) -> net::Client {
    let channel = moruna_svc::connect(listener).expect("connect");
    let mut c = net::Client::connect(
        channel,
        deadline_in(10 * SECOND),
        moruna_svc::Account::carve(8 << 20, "net-test").unwrap(),
    )
    .expect("hello");
    c.core().set_timeout(30 * SECOND);
    c
}

fn too_many<T: std::fmt::Debug>(r: Result<T, CallError<net::Error>>, what: &str) {
    assert!(
        matches!(r, Err(CallError::Remote(net::Error::TooMany))),
        "{what}: expected TooMany, got {r:?}"
    );
}

#[test]
fn a_grant_s_bound_is_shared_by_its_connections_and_held_until_sockets_are_freed() {
    let config = "moruna-netstack 1
setting tcp-linger-ms 1000
grant listen-x rights=connect,listen,udp remote=127.0.0.0/8 local=127.0.0.0/8 sockets=4
";
    let (mut slots, join) = serve(config, &[], no_external_cards());
    let slot = slots.remove("listen-x").expect("slot");
    let mut a = client(&slot);
    let mut b = client(&slot);

    // A listener holds two: itself, and its listening socket, the next connection.
    let (l, at) = a.listen(Endpoint::new(HERE, 0), 4).expect("listen");
    // b's connect is the third, and the connection it makes the fourth: b's own client
    // connection through the slot does not renew the bound.
    let opened = b
        .open(Target::Endpoint(at), None)
        .expect("a connect within the bound");
    too_many(b.open(Target::Endpoint(at), None), "a fifth socket");
    too_many(
        b.bind(Endpoint::new(HERE, 0)),
        "a UDP socket past the bound",
    );
    too_many(
        a.listen(Endpoint::new(HERE, 0), 1),
        "a listener past the bound",
    );
    // The connection counted from its SYN, so accepting it takes nothing more.
    let accepted = a.accept(l).expect("accept, at the bound");
    assert_eq!(accepted.remote, opened.local);

    // A closed socket holds its place until it is freed. a closes first: its socket
    // waits for b's FIN.
    a.close(accepted.socket).expect("close");
    too_many(
        b.bind(Endpoint::new(HERE, 0)),
        "a socket while the first closed one waits",
    );
    // b's close ends both: b's socket is freed once a acknowledges its FIN, and a's
    // waits in TIME-WAIT, still counted, until the linger's second has passed.
    b.close(opened.socket).expect("close");
    let freed = |b: &mut net::Client| {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            match b.bind(Endpoint::new(HERE, 0)) {
                Ok((s, _)) => break s,
                Err(CallError::Remote(net::Error::TooMany)) if Instant::now() < end => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => panic!("a socket once a closing one was freed: {other:?}"),
            }
        }
    };
    let one = freed(&mut b);
    too_many(
        b.bind(Endpoint::new(HERE, 0)),
        "a socket while one waits in TIME-WAIT",
    );
    let two = freed(&mut b);
    b.close(one).expect("close");
    b.close(two).expect("close");

    // A client connection that goes takes its sockets with it: a's listener freed, the
    // whole bound is b's.
    drop(a);
    let end = Instant::now() + Duration::from_secs(10);
    let mut held = Vec::new();
    while held.len() < 4 {
        match b.bind(Endpoint::new(HERE, 0)) {
            Ok((s, _)) => held.push(s),
            Err(CallError::Remote(net::Error::TooMany)) if Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => panic!("the whole bound after a's connection went: {other:?}"),
        }
    }
    too_many(b.bind(Endpoint::new(HERE, 0)), "a fifth UDP socket");
    drop(b);
    drop(slot);
    drop(slots);
    join.join().expect("the server");
}

#[test]
fn a_listener_at_the_bound_refuses_connections_until_one_is_freed() {
    // A listener alone at its bound has no listening socket, so a SYN is answered with a
    // reset and the connect is refused; one freed socket gives it one again.
    let config = "moruna-netstack 1
setting tcp-linger-ms 300
grant listen-x rights=connect,listen remote=127.0.0.0/8 local=127.0.0.0/8 sockets=3
grant listen-y rights=connect remote=127.0.0.0/8 local=127.0.0.0/8 sockets=4
";
    let (mut slots, join) = serve(config, &[], no_external_cards());
    let x = slots.remove("listen-x").expect("slot");
    let y = slots.remove("listen-y").expect("slot");
    let mut a = client(&x);
    let mut b = client(&y);
    let (l, at) = a.listen(Endpoint::new(HERE, 0), 4).expect("listen");
    let first = b
        .open(Target::Endpoint(at), None)
        .expect("the first connection");
    // x holds three: the listener, the first connection, the listening socket again.
    let second = b
        .open(Target::Endpoint(at), None)
        .expect("the second connection");
    // Now x is at its bound with no listening socket: the third is reset at once.
    match b.open(Target::Endpoint(at), None) {
        Err(CallError::Remote(net::Error::Refused)) => {}
        other => panic!("a connection past the grant's bound: {other:?}"),
    }
    let one = a.accept(l).expect("accept");
    let two = a.accept(l).expect("accept");
    assert_eq!(
        [one.remote, two.remote],
        [first.local, second.local],
        "the two connections, in order"
    );
    a.close(one.socket).expect("close");
    b.close(first.socket).expect("close");
    let end = Instant::now() + Duration::from_secs(10);
    let third = loop {
        match b.open(Target::Endpoint(at), None) {
            Ok(o) => break o,
            Err(CallError::Remote(net::Error::Refused)) if Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => panic!("a connection once one was freed: {other:?}"),
        }
    };
    assert_eq!(a.accept(l).expect("accept").remote, third.local);
    drop((a, b, x, y, slots));
    join.join().expect("the server");
}

#[test]
fn a_listener_with_no_room_for_a_connection_listens_and_refuses() {
    // A grant of one socket holds the listener and nothing more: it listens, and every
    // connection to it is refused.
    let config = "moruna-netstack 1
grant listen-x rights=listen local=127.0.0.0/8 sockets=1
grant listen-y rights=connect remote=127.0.0.0/8 local=127.0.0.0/8 sockets=4
";
    let (mut slots, join) = serve(config, &[], no_external_cards());
    let x = slots.remove("listen-x").expect("slot");
    let y = slots.remove("listen-y").expect("slot");
    let mut a = client(&x);
    let mut b = client(&y);
    let (_, at) = a.listen(Endpoint::new(HERE, 0), 4).expect("listen");
    for _ in 0..2 {
        match b.open(Target::Endpoint(at), None) {
            Err(CallError::Remote(net::Error::Refused)) => {}
            other => panic!("a connection to a listener with no room: {other:?}"),
        }
    }
    too_many(a.listen(Endpoint::new(HERE, 0), 1), "a second listener");
    drop((a, b, x, y, slots));
    join.join().expect("the server");
}

#[test]
fn a_grant_s_tcp_sockets_have_its_buffers_and_others_the_settings() {
    let config = "moruna-netstack 1
setting tcp-receive-buffer 32768
setting tcp-send-buffer 16384
grant listen-x rights=listen local=127.0.0.0/8 rx=4096 tx=8192
grant listen-y rights=connect remote=127.0.0.0/8 local=127.0.0.0/8
";
    let (mut slots, join) = serve(config, &[], no_external_cards());
    let x = slots.remove("listen-x").expect("slot");
    let y = slots.remove("listen-y").expect("slot");
    let mut a = client(&x);
    let mut b = client(&y);
    let (l, at) = a.listen(Endpoint::new(HERE, 0), 4).expect("listen");
    let opened = b.open(Target::Endpoint(at), None).expect("open");
    let accepted = a.accept(l).expect("accept");
    let theirs = a.status(accepted.socket).expect("status");
    assert_eq!(
        (theirs.receive_capacity, theirs.send_capacity),
        (4096, 8192),
        "the grant's rx= and tx="
    );
    let ours = b.status(opened.socket).expect("status");
    assert_eq!(
        (ours.receive_capacity, ours.send_capacity),
        (32768, 16384),
        "the settings, for a grant that sets no buffers"
    );
    drop((a, b, x, y, slots));
    join.join().expect("the server");
}

#[test]
fn a_grant_s_udp_sockets_have_its_buffers() {
    // The UDP settings are large, so only the grant's buffers bound these sockets.
    let config = "moruna-netstack 1
grant listen-x rights=udp remote=127.0.0.0/8 local=127.0.0.0/8 rx=2048 tx=2048
grant listen-y rights=udp remote=127.0.0.0/8 local=127.0.0.0/8
";
    let (mut slots, join) = serve(config, &[], no_external_cards());
    let x = slots.remove("listen-x").expect("slot");
    let y = slots.remove("listen-y").expect("slot");
    let mut a = client(&x);
    let mut b = client(&y);
    let (sa, at_a) = a.bind(Endpoint::new(HERE, 0)).expect("bind");
    let (sb, at_b) = b.bind(Endpoint::new(HERE, 0)).expect("bind");
    // Sending: nothing larger than the grant's send buffer.
    match a.send_to(sa, at_b, vec![1; 3000]) {
        Err(CallError::Remote(net::Error::TooLarge)) => {}
        other => panic!("3000 bytes past a 2048-byte send buffer: {other:?}"),
    }
    a.send_to(sa, at_b, vec![2; 2000]).expect("2000 bytes");
    let (from, bytes) = b.receive_from(sb).expect("receive");
    assert_eq!((from, bytes.len()), (at_a, 2000));
    // Receiving: a datagram larger than the receive buffer is dropped; the next is not.
    b.send_to(sb, at_a, vec![3; 3000])
        .expect("3000 bytes from a grant of the settings");
    b.send_to(sb, at_a, vec![4; 100]).expect("100 bytes");
    let (_, bytes) = a.receive_from(sa).expect("receive");
    assert_eq!(bytes, vec![4; 100], "the 3000 bytes did not fit 2048");
    drop((a, b, x, y, slots));
    join.join().expect("the server");
}

#[test]
fn a_minted_listener_has_buffers_within_its_minter_s() {
    let config = "moruna-netstack 1
grant listen-x rights=listen,grant local=127.0.0.0/8 sockets=8 rx=8192 tx=8192 per-peer=4 linger=2000
grant listen-y rights=connect remote=127.0.0.0/8 local=127.0.0.0/8
";
    let (mut slots, join) = serve(config, &[], no_external_cards());
    let x = slots.remove("listen-x").expect("slot");
    let y = slots.remove("listen-y").expect("slot");
    let mut a = client(&x);
    let mut b = client(&y);
    let parent = a.info().expect("info").policy;
    assert_eq!(
        (parent.rx, parent.tx, parent.per_peer, parent.linger_ms),
        (Some(8192), Some(8192), Some(4), Some(2000)),
        "info says the grant's bounds"
    );
    let child = Policy {
        rights: rights::LISTEN,
        tx: Some(4096),
        per_peer: Some(2),
        linger_ms: Some(1000),
        ..parent.clone()
    };
    let refused = |r: Result<Channel, CallError<net::Error>>, what: &str| {
        assert!(
            matches!(r, Err(CallError::Remote(net::Error::NotPermitted))),
            "{what}: expected NotPermitted, got {r:?}"
        );
    };
    for (wider, what) in [
        (
            Policy {
                rx: Some(8193),
                ..child.clone()
            },
            "larger buffers",
        ),
        (
            Policy {
                rx: None,
                ..child.clone()
            },
            "the server's buffers, under a grant that gives its own",
        ),
        (
            Policy {
                per_peer: Some(5),
                ..child.clone()
            },
            "more sockets a peer",
        ),
        (
            Policy {
                linger_ms: Some(2001),
                ..child.clone()
            },
            "a longer linger",
        ),
    ] {
        refused(a.grant(wider), what);
    }
    let minted = a.grant(child.clone()).expect("a narrower grant");
    let mut c = client(&minted);
    assert_eq!(c.info().expect("info").policy, child);
    let (l, at) = c.listen(Endpoint::new(HERE, 0), 4).expect("listen");
    b.open(Target::Endpoint(at), None).expect("open");
    let accepted = c.accept(l).expect("accept");
    let status = c.status(accepted.socket).expect("status");
    assert_eq!(
        (status.receive_capacity, status.send_capacity),
        (8192, 4096),
        "the minted grant's buffers"
    );
    drop((a, b, c, minted, x, y, slots));
    join.join().expect("the server");
}
