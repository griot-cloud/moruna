//! [`Network::take`] against the network server's end of the channel driven by hand (work
//! item 1.3c): an answer that comes while the answers are read is never left filed with the
//! channel empty, where nothing would wake the process for it.

use core::cell::Cell;

use moruna_svc::{Account, CallError, Channel, Encoder, Kind, Wire, deadline_in, frame};
use netstack_protocol::net;

use super::{Network, RECEIVE_INTO, Request};
use crate::Error;

const SECOND: u64 = 1_000_000_000;

fn body<T: Wire>(value: T) -> Encoder {
    let mut e = Encoder::new();
    value.encode(&mut e).expect("encode");
    e
}

/// The process's side over a connection whose network server's end the test holds.
fn network() -> (Network, Channel) {
    let (mine, theirs) = Channel::create().expect("channel");
    let mut welcome = Encoder::new();
    net::ID.major.encode(&mut welcome).expect("major");
    net::ID.minor.encode(&mut welcome).expect("minor");
    frame::send(&theirs, Kind::Welcome, 0, 0, welcome).expect("welcome");
    // The account's bind, the connection's first request, answered before it is asked.
    frame::send(&theirs, Kind::Reply, 0, 1, Encoder::new()).expect("bind's answer");
    let account = Account::carve(16 << 20, "api-take").expect("an account");
    let client = net::Client::connect(mine, deadline_in(5 * SECOND), account).expect("connect");
    let hello = frame::recv(&theirs).expect("read").expect("hello");
    assert_eq!(hello.kind, Kind::Hello);
    let bind = frame::recv(&theirs).expect("read").expect("bind");
    assert_eq!((bind.kind, bind.txid), (Kind::Request, 1));
    (Network::new(client), theirs)
}

/// A receive on a new connection on `socket`, sent: its id, as the server read it.
fn receive(network: &mut Network, theirs: &Channel, socket: u32) -> u32 {
    assert!(network.open(socket));
    network.request(Request::Receive(socket)).expect("receive");
    let asked = frame::recv(theirs).expect("read").expect("a receive");
    assert_eq!((asked.kind, asked.ordinal), (Kind::Request, RECEIVE_INTO));
    asked.txid
}

fn receiving(network: &Network, socket: u32) -> bool {
    network.streams[&socket].receiving
}

fn readable(network: &mut Network) -> bool {
    let core = network.client.core();
    core.channel().needed().expect("needed").is_some()
}

#[test]
fn an_answer_that_comes_while_the_answers_are_read_is_never_left_behind() {
    let (mut network, theirs) = network();
    let a = receive(&mut network, &theirs, 10);
    let _b = receive(&mut network, &theirs, 11);
    let c = receive(&mut network, &theirs, 12);
    // `c`'s answer has come; `b`'s has not; `a`'s comes while the answers are read, once
    // a read has found the channel empty. Each says its connection has ended: no bytes.
    frame::send(&theirs, Kind::Reply, 0, c, body(0u32)).expect("c's answer");
    let late = Cell::new(true);
    let accepted = network
        .take_reading(|core| {
            let read = core.file(0);
            if matches!(read, Err(CallError::TimedOut)) && late.replace(false) {
                frame::send(&theirs, Kind::Reply, 0, a, body(0u32)).expect("a's answer");
            }
            read
        })
        .expect("take");
    assert!(accepted.is_empty());
    assert!(!receiving(&network, 12), "c's answer is taken");
    assert!(receiving(&network, 11), "b's has not come");
    // Either `a`'s answer was taken, or the channel is still readable, so the process is
    // woken for it. Read while `b`'s was looked for, it would sit filed with the channel
    // empty, and the connection would wait for nothing.
    assert!(
        !receiving(&network, 10) || readable(&mut network),
        "a's answer was read and left filed: nothing wakes the process for it"
    );
    network.take().expect("the next turn's take");
    assert!(!receiving(&network, 10));
    assert!(network.streams[&10].ended);
    assert!(receiving(&network, 11));
}

#[test]
fn a_network_server_that_has_gone_ends_the_take() {
    let (mut network, theirs) = network();
    receive(&mut network, &theirs, 10);
    drop(theirs);
    assert!(matches!(
        network.take(),
        Err(Error::Network("answers", CallError::PeerClosed))
    ));
}
