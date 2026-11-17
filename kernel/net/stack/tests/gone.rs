//! A dead client never holds a port or a budget (work item 1.3b, found by 2.2c: a
//! restarted `api` listens on 443 at once). Before the network server answers
//! `AddressInUse`, or refuses for a grant's or its own bound, it reaps every client
//! whose channel has closed, whether or not its dispatcher has seen the close yet; and
//! a client that goes takes every connection it held with it, reset, those it had
//! closed and left finishing their close included, so none of them counts any more.

mod common;

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use common::{no_external_cards, serve};
use moruna_netstack::{Grant, Stack, parse};
use moruna_svc::{CallError, Channel, Outcome, ProtocolError, Rights, deadline_in, now};
use netstack_protocol::{Addr, Endpoint, Target, net};

const SECOND: u64 = 1_000_000_000;
const HERE: Addr = Addr::v4(127, 0, 0, 1);

fn failed(o: &Outcome) -> Option<net::Error> {
    match o {
        Outcome::Failure(code) => net::Error::from_code(*code),
        _ => None,
    }
}

/// The stack alone over `config`, with no dispatcher, and a grant for each of its slots.
fn alone(config: &str) -> (Stack, Vec<Rc<Grant>>) {
    let config = parse(config).expect("the configuration");
    let st = Stack::new(&config, &[], 0x1234_5678, now()).expect("the stack");
    let grants = config
        .grants
        .iter()
        .map(|(slot, policy)| Rc::new(Grant::new(slot.clone(), policy.clone())))
        .collect();
    (st, grants)
}

/// A client of `grant`, registered as connection `id` with a handle to its channel: the
/// channel's two ends, the client's second, which closing is the client going.
fn registered(st: &mut Stack, id: u64, grant: &Rc<Grant>) -> (Channel, Channel) {
    let (server, client) = Channel::create().expect("a channel");
    st.connection(id, grant, || server.0.duplicate(Rights::WAIT).ok());
    (server, client)
}

/// With no dispatcher and no poll between the calls: a client listens on 443; its
/// channel closes, which nothing tells the stack; a second client listens on 443, and
/// succeeds.
#[test]
fn a_client_that_went_unseen_does_not_hold_its_port() {
    let (mut st, grants) = alone(
        "moruna-netstack 1
grant listen-a rights=listen local=127.0.0.1/32@443
grant listen-b rights=listen local=127.0.0.1/32@443
",
    );
    let at = Endpoint::new(HERE, 443);
    let (_a_server, a_client) = registered(&mut st, 1, &grants[0]);
    assert!(
        matches!(st.listen(1, at, 64), Outcome::Reply(_)),
        "a's listener"
    );
    let (_b_server, _b_client) = registered(&mut st, 2, &grants[1]);
    // While the first lives, the port is its.
    assert_eq!(
        failed(&st.listen(2, at, 64)),
        Some(net::Error::AddressInUse)
    );
    drop(a_client);
    let listened = st.listen(2, at, 64);
    assert!(
        matches!(listened, Outcome::Reply(_)),
        "the port of a client that went: {:?}",
        failed(&listened)
    );
    assert_eq!(
        grants[0].held(),
        0,
        "nothing of the first client's is counted"
    );
    assert_eq!(grants[1].held(), 2);
    // Its close, when a dispatcher sees it, is a close of nothing.
    st.connection_closed(1);
    st.poll(now());
    assert_eq!(st.sockets(), 1, "the second client's listening socket");
}

/// The same for a grant's budget: two clients of one grant of 2 sockets, the first's
/// listener holding both.
#[test]
fn a_client_that_went_unseen_does_not_hold_its_budget() {
    let (mut st, grants) = alone(
        "moruna-netstack 1
grant listen-api rights=listen local=127.0.0.1/32@443-444 sockets=2
",
    );
    let (_a_server, a_client) = registered(&mut st, 1, &grants[0]);
    assert!(matches!(
        st.listen(1, Endpoint::new(HERE, 443), 64),
        Outcome::Reply(_)
    ));
    let (_b_server, _b_client) = registered(&mut st, 2, &grants[0]);
    let at = Endpoint::new(HERE, 444);
    assert_eq!(failed(&st.listen(2, at, 64)), Some(net::Error::TooMany));
    drop(a_client);
    let listened = st.listen(2, at, 64);
    assert!(
        matches!(listened, Outcome::Reply(_)),
        "the budget of a client that went: {:?}",
        failed(&listened)
    );
    assert_eq!(
        grants[0].held(),
        2,
        "the second client's listener and its socket"
    );
}

/// The same for a port the server chooses (port 0, where a grant allows one port) and a
/// UDP socket's endpoint.
#[test]
fn a_client_that_went_unseen_does_not_hold_a_chosen_port_or_a_udp_endpoint() {
    let (mut st, grants) = alone(
        "moruna-netstack 1
grant listen-a rights=listen,udp local=127.0.0.1/32@443
grant listen-b rights=listen,udp local=127.0.0.1/32@443
",
    );
    let any = Endpoint::new(HERE, 0);
    let (_a_server, a_client) = registered(&mut st, 1, &grants[0]);
    assert!(matches!(st.listen(1, any, 4), Outcome::Reply(_)));
    assert!(matches!(st.bind(1, any), Outcome::Reply(_)));
    let (_b_server, _b_client) = registered(&mut st, 2, &grants[1]);
    assert_eq!(
        failed(&st.listen(2, any, 4)),
        Some(net::Error::AddressInUse)
    );
    assert_eq!(failed(&st.bind(2, any)), Some(net::Error::AddressInUse));
    assert_eq!(
        failed(&st.bind(2, Endpoint::new(HERE, 443))),
        Some(net::Error::AddressInUse)
    );
    drop(a_client);
    assert!(
        matches!(st.listen(2, any, 4), Outcome::Reply(_)),
        "the port chosen for a client that went"
    );
    assert!(
        matches!(st.bind(2, Endpoint::new(HERE, 443)), Outcome::Reply(_)),
        "the UDP endpoint of a client that went"
    );

    // A UDP socket alone, port 0, found by the bind.
    let (mut st, grants) = alone(
        "moruna-netstack 1
grant listen-a rights=udp local=127.0.0.1/32@5353
grant listen-b rights=udp local=127.0.0.1/32@5353
",
    );
    let (_a_server, a_client) = registered(&mut st, 1, &grants[0]);
    assert!(matches!(st.bind(1, any), Outcome::Reply(_)));
    let (_b_server, _b_client) = registered(&mut st, 2, &grants[1]);
    assert_eq!(failed(&st.bind(2, any)), Some(net::Error::AddressInUse));
    drop(a_client);
    assert!(matches!(st.bind(2, any), Outcome::Reply(_)));
}

/// A SYN from 10.0.2.2:40000 to the node's 443, as a card delivers it.
fn syn() -> Vec<u8> {
    use smoltcp::{
        phy::ChecksumCapabilities,
        wire::{
            EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpProtocol,
            Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr, TcpSeqNumber,
        },
    };
    let caps = ChecksumCapabilities::default();
    let (from, to) = (
        Ipv4Address::new(10, 0, 2, 2),
        Ipv4Address::new(10, 0, 2, 15),
    );
    let tcp = TcpRepr {
        src_port: 40000,
        dst_port: 443,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(0),
        ack_number: None,
        window_len: 65535,
        window_scale: None,
        max_seg_size: Some(1460),
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload: &[],
    };
    let ip = Ipv4Repr {
        src_addr: from,
        dst_addr: to,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let mut buf = vec![0u8; 14 + ip.buffer_len() + tcp.buffer_len()];
    let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
    let mac = common::MAC_A.to_be_bytes();
    EthernetRepr {
        src_addr: EthernetAddress([0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]),
        dst_addr: EthernetAddress([mac[2], mac[3], mac[4], mac[5], mac[6], mac[7]]),
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut frame);
    let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
    ip.emit(&mut packet, &caps);
    tcp.emit(
        &mut TcpPacket::new_unchecked(packet.payload_mut()),
        &from.into(),
        &to.into(),
        &caps,
    );
    buf
}

/// What a reaped client left closing, it no longer holds either: a half-open
/// connection its closed listener left, still counted until its reset goes, stops
/// counting, and is reset, once the client is found gone.
#[test]
fn a_reaped_client_s_closing_connections_are_reset_and_uncounted() {
    let config = parse(
        "moruna-netstack 1
interface wan card=net0 ipv4=10.0.2.15/24
grant listen-api rights=listen local=10.0.2.15/32@443-444 sockets=2
",
    )
    .expect("the configuration");
    let card = moruna_netstack::Card {
        mac: common::MAC_A,
        mtu: 1500,
        up: true,
    };
    let mut st = Stack::new(&config, &[card], 0x1234_5678, now()).expect("the stack");
    let grant = Rc::new(Grant::new("listen-api", config.grants[0].1.clone()));
    let (_a_server, a_client) = registered(&mut st, 1, &grant);
    let Outcome::Reply(_) = st.listen(1, Endpoint::new(Addr::v4(10, 0, 2, 15), 443), 4) else {
        panic!("a's listener");
    };
    st.card_mut(0).port.deliver(syn());
    st.poll(now());
    // The listener and the half-open connection; no room for another listening socket.
    assert_eq!(grant.held(), 2);
    // a closes its listener (its first socket): the half-open one closes on its own,
    // counted until its reset has gone, which takes a poll this test does not make.
    assert!(matches!(st.close(1, 1), Outcome::Reply(_)));
    assert_eq!(grant.held(), 1);
    let (_b_server, _b_client) = registered(&mut st, 2, &grant);
    let at = Endpoint::new(Addr::v4(10, 0, 2, 15), 444);
    assert!(matches!(st.listen(2, at, 4), Outcome::Reply(_)));
    // b's listener fills the bound: a second is refused while a lives.
    let other = Endpoint::new(Addr::v4(10, 0, 2, 15), 443);
    assert_eq!(failed(&st.listen(2, other, 4)), Some(net::Error::TooMany));
    drop(a_client);
    assert_eq!(st.sockets(), 1, "the half-open socket, still in its set");
    let listened = st.listen(2, other, 4);
    assert!(
        matches!(listened, Outcome::Reply(_)),
        "a's closing connection still counted: {:?}",
        failed(&listened)
    );
    assert_eq!(grant.held(), 2, "b's two listeners");
    // The reset goes with the next poll, and the socket with it.
    st.poll(now());
    st.poll(now());
    assert_eq!(st.sockets(), 0);
}

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

/// Through the server: a client that goes leaves connections behind, one it closed and
/// left finishing its close (for a minute's linger), two open. A fresh client through
/// the same grant listens on the same port at once and has the grant's whole budget:
/// were the three still counted, it would have a listener with no room for a
/// connection.
#[test]
fn a_client_s_lingering_connections_stop_counting_once_it_has_gone() {
    let config = "moruna-netstack 1
grant listen-x rights=listen local=127.0.0.0/8@8443 sockets=4 linger=60000
grant listen-y rights=connect remote=127.0.0.0/8 local=127.0.0.0/8
";
    let (mut slots, join) = serve(config, &[], no_external_cards());
    let x = slots.remove("listen-x").expect("slot");
    let y = slots.remove("listen-y").expect("slot");
    let mut b = client(&y);
    let at = Endpoint::new(HERE, 8443);
    {
        let mut a = client(&x);
        let (l, _) = a.listen(at, 4).expect("listen");
        // Three connections, and the grant is full: the listener and the three.
        for _ in 0..3 {
            b.open(Target::Endpoint(at), None).expect("open");
        }
        let one = a.accept(l).expect("accept");
        for _ in 0..2 {
            a.accept(l).expect("accept");
        }
        // a closes one: b never closes its end, so it would wait out the minute.
        a.close(one.socket).expect("close");
        match b.open(Target::Endpoint(at), None) {
            Err(CallError::Remote(net::Error::Refused)) => {}
            other => panic!("a fourth connection to a full grant: {other:?}"),
        }
    }
    // a has gone. A fresh client through the same grant, at once.
    let began = Instant::now();
    let mut a2 = client(&x);
    let (l, _) = a2.listen(at, 4).expect("the same port, at once");
    let third = b.open(Target::Endpoint(at), None).expect("open");
    let fourth = b.open(Target::Endpoint(at), None).expect("open");
    let accepted = [a2.accept(l).expect("accept"), a2.accept(l).expect("accept")];
    assert_eq!(
        accepted.map(|o| o.remote),
        [third.local, fourth.local],
        "the whole budget, not what a's lingering connections left of it"
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "took {:?}: a's connections were waited out, not reset",
        began.elapsed()
    );
    drop((a2, b, x, y, slots));
    join.join().expect("the server");
}
