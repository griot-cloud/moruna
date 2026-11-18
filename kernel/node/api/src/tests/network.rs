//! The network side (work items 1.3 and 2.2b), against the network server's own stack,
//! with no API server: the listener follows the lease, and each accepted socket is a
//! byte stream whose bytes move both ways and whose close frees its socket.

use std::{cell::RefCell, rc::Rc};

use moruna_api_server::{Transport, TransportError};
use netstack_protocol::Target;

use super::{common::receive, *};
use crate::network::{Conn, Network, STAGED_HOLD};

/// The most a turn waits, so a test's loop looks again often.
const TURN_NS: u64 = 20_000_000;

/// One turn of the process's loop, with no server: takes the answers that came, looks at
/// the lease when it is time to, sends what is to send, and waits a little for more. The
/// sockets accepted, and what the listener did.
fn turn(network: &Rc<RefCell<Network>>) -> Result<(Vec<u32>, Option<Event>), Error> {
    let accepted = network.borrow_mut().take()?;
    let event = network.borrow_mut().tend(now())?;
    network.borrow_mut().flush()?;
    let deadline = network.borrow().look_at().min(now() + TURN_NS);
    network.borrow_mut().wait(deadline, &[])?;
    Ok((accepted, event))
}

/// Turns until `done` holds, within 30 s: every event until then.
fn turn_until(
    network: &Rc<RefCell<Network>>,
    mut done: impl FnMut(&[u32], Option<Event>) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    loop {
        let (accepted, event) = turn(network).expect("a turn");
        events.extend(event);
        if done(&accepted, event) {
            return events;
        }
        assert!(Instant::now() < deadline, "not within 30 s: {events:?}");
    }
}

fn shared(client: net::Client) -> Rc<RefCell<Network>> {
    Rc::new(RefCell::new(Network::new(client)))
}

/// Turns until a socket is accepted: it.
fn accept(network: &Rc<RefCell<Network>>) -> u32 {
    let mut socket = None;
    turn_until(network, |accepted, _| {
        socket = accepted.first().copied();
        socket.is_some()
    });
    socket.unwrap()
}

/// Turns until `read` gives something other than `WouldBlock`: what it gave.
fn read(network: &Rc<RefCell<Network>>, conn: &mut Conn) -> Result<Vec<u8>, TransportError> {
    let mut answer = None;
    turn_until(network, |_, _| {
        let mut buf = [0u8; 4096];
        answer = match conn.read(&mut buf) {
            Err(TransportError::WouldBlock) => None,
            Ok(count) => Some(Ok(buf[..count].to_vec())),
            Err(error) => Some(Err(error)),
        };
        answer.is_some()
    });
    answer.unwrap()
}

/// The listener opens once the lease comes and not before, on its address at 443.
#[test]
fn the_listener_opens_on_the_leased_address_once_there_is_one() {
    let mut node = Node::start(false);
    let (client, slot) = node.api();
    let network = shared(client);
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(400) {
        assert_eq!(turn(&network).expect("a turn"), (vec![], None));
    }
    assert_eq!(network.borrow().listening(), None);
    node.lease.answers.store(true, Ordering::SeqCst);
    let at = Endpoint::new(NODE_ADDRESS, 443);
    let events = turn_until(&network, |_, event| event.is_some());
    assert_eq!(events, [Event::Listening(at)]);
    assert_eq!(network.borrow().listening(), Some(at));
    // Unchanged, the lease is looked at again a lease poll later.
    let looked = network.borrow().look_at();
    assert!(looked >= now() + LEASE_POLL_NS - SECOND / 2, "{looked}");
    drop((network, slot));
    node.end();
}

/// The listener follows the lease (system design 1C): lost, it closes; given again at
/// another address, it opens there, and connections to it are accepted.
#[test]
fn the_listener_follows_the_lease_to_its_new_address() {
    let mut node = Node::start(true);
    let outside = node.slot("listen-host");
    let (client, slot) = node.api();
    let network = shared(client);
    let first = Endpoint::new(NODE_ADDRESS, 443);
    turn_until(&network, |_, event| event == Some(Event::Listening(first)));

    // The lease is lost, and the DHCP server holds its answer.
    node.lease.offer.store(16, Ordering::SeqCst);
    node.lease.answers.store(false, Ordering::SeqCst);
    node.lease.renew.store(true, Ordering::SeqCst);
    let events = turn_until(&network, |_, event| event.is_some());
    assert_eq!(events, [Event::Unlistened(first)]);
    assert_eq!(network.borrow().listening(), None);
    for _ in 0..5 {
        assert_eq!(turn(&network).expect("a turn").1, None);
    }

    // The new lease, at another address.
    node.lease.answers.store(true, Ordering::SeqCst);
    let second = Endpoint::new(Addr::v4(10, 0, 2, 16), 443);
    let events = turn_until(&network, |_, event| event.is_some());
    assert_eq!(events, [Event::Listening(second)]);
    let mut host = host(&outside);
    let opened = host
        .open(Target::Endpoint(second), None)
        .expect("a connection to the new address");
    assert_eq!(opened.remote, second);
    accept(&network);
    drop((network, slot, host, outside));
    node.end();
}

#[test]
fn the_port_has_one_owner() {
    let mut node = Node::start(true);
    let network = node.slot("listen-api");
    let first = shared(connect(&network, account()).expect("a connection"));
    let second = shared(connect(&network, account()).expect("another"));
    let at = Endpoint::new(NODE_ADDRESS, 443);
    turn_until(&first, |_, event| event == Some(Event::Listening(at)));
    assert_eq!(
        second.borrow_mut().tend(now()),
        Err(Error::Network(
            "listen",
            CallError::Remote(net::Error::AddressInUse)
        ))
    );
    drop((first, second, network));
    node.end();
}

#[test]
fn a_network_server_with_no_wan_is_refused() {
    let lo = "moruna-netstack 1
grant listen-api rights=listen local=0.0.0.0/0@443 sockets=64
";
    let (mut slots, server) = common::serve(lo, &[], common::no_external_cards());
    let slot = slots.remove("listen-api").expect("the api's slot");
    let network = shared(connect(&slot, account()).expect("the network server"));
    assert_eq!(network.borrow_mut().tend(now()), Err(Error::NoInterface));
    drop((network, slot, slots));
    server.join().expect("the network server");
}

/// What the process sees when the network server finds no lease in time and exits, or
/// later ends: its loop's next request fails, and it stops on that.
#[test]
fn the_loop_ends_when_the_network_server_does() {
    for answers in [false, true] {
        let mut node = Node::start(answers);
        let (client, slot) = node.api();
        let stop = node.stop.clone();
        let looping = thread::spawn(move || {
            let network = shared(client);
            loop {
                if let Err(error) = turn(&network) {
                    return (error, network.borrow().listening().is_some());
                }
            }
        });
        thread::sleep(Duration::from_millis(300));
        assert!(!looping.is_finished(), "the loop ended with no reason to");
        stop.store(true, Ordering::SeqCst);
        let (error, listening) = looping.join().expect("the loop");
        assert!(
            matches!(error, Error::Network(_, CallError::PeerClosed)),
            "{error:?}"
        );
        assert_eq!(listening, answers);
        drop(slot);
        node.end();
    }
}

/// A connection's bytes move both ways; the client's end of its stream is read as the
/// end; and the connection's close frees its socket, after what was written has gone.
#[test]
fn a_connection_s_bytes_move_both_ways_and_its_close_frees_its_socket() {
    let mut node = Node::start(true);
    let outside = node.slot("listen-host");
    let (client, slot) = node.api();
    let network = shared(client);
    let at = Endpoint::new(NODE_ADDRESS, 443);
    turn_until(&network, |_, event| event == Some(Event::Listening(at)));
    let mut host = host(&outside);
    let opened = host.open(Target::Endpoint(at), None).expect("connected");
    let mut conn = Conn::new(network.clone(), accept(&network));
    assert_eq!(network.borrow().connections(), 1);

    common::send(&mut host, opened.socket, b"from the host").expect("sent");
    assert_eq!(read(&network, &mut conn), Ok(b"from the host".to_vec()));
    assert_eq!(conn.write(b"from the node"), Ok(13));
    turn_until(&network, |_, _| true);
    assert_eq!(
        receive(&mut host, opened.socket, 64),
        Ok(b"from the node".to_vec())
    );

    // The host finishes sending: the node reads the end, and can still write.
    host.shutdown(opened.socket).expect("shut down");
    assert_eq!(read(&network, &mut conn), Ok(vec![]));
    assert_eq!(conn.write(b"last"), Ok(4));
    // The node closes: what it wrote goes first, then the end; then the socket is free.
    conn.close();
    assert_eq!(conn.write(b"more"), Err(TransportError::Broken));
    turn_until(&network, |_, _| network.borrow().connections() == 0);
    assert_eq!(receive(&mut host, opened.socket, 64), Ok(b"last".to_vec()));
    assert_eq!(receive(&mut host, opened.socket, 64), Ok(vec![]));
    let mut buf = [0u8; 8];
    assert_eq!(conn.read(&mut buf), Err(TransportError::Broken));
    drop((network, slot, conn, host, outside));
    node.end();
}

/// What the server writes is staged up to a bound, and sent in pieces of a connection's
/// send memory; a write past the bound waits until some has gone.
#[test]
fn a_write_past_the_staging_bound_waits_until_some_has_gone() {
    let mut node = Node::start(true);
    let outside = node.slot("listen-host");
    let (client, slot) = node.api();
    let network = shared(client);
    let at = Endpoint::new(NODE_ADDRESS, 443);
    turn_until(&network, |_, event| event == Some(Event::Listening(at)));
    let mut host = host(&outside);
    let opened = host.open(Target::Endpoint(at), None).expect("connected");
    let mut conn = Conn::new(network.clone(), accept(&network));

    let bytes: Vec<u8> = (0..STAGED_HOLD + 1000).map(|i| (i % 251) as u8).collect();
    assert_eq!(conn.write(&bytes), Ok(STAGED_HOLD));
    assert_eq!(
        conn.write(&bytes[STAGED_HOLD..]),
        Err(TransportError::WouldBlock)
    );
    let mut got = Vec::new();
    let mut written = STAGED_HOLD;
    let deadline = Instant::now() + Duration::from_secs(30);
    while got.len() < bytes.len() {
        turn(&network).expect("a turn");
        if written < bytes.len()
            && let Ok(count) = conn.write(&bytes[written..])
        {
            written += count;
        }
        if network.borrow().connections() == 1 {
            got.extend(receive_some(&mut host, opened.socket));
        }
        assert!(
            Instant::now() < deadline,
            "{} of {}",
            got.len(),
            bytes.len()
        );
    }
    assert!(got == bytes, "the bytes arrived changed");
    conn.close();
    turn_until(&network, |_, _| network.borrow().connections() == 0);
    drop((network, slot, conn, host, outside));
    node.end();
}

/// What has come on `socket`, waiting at most a moment for it.
fn receive_some(host: &mut net::Client, socket: u32) -> Vec<u8> {
    let status = host.status(socket).expect("status");
    if status.receive_queued == 0 {
        return Vec::new();
    }
    receive(host, socket, status.receive_queued).expect("received")
}

/// Seventy connections one after another from one host, as an agent polling with a new
/// connection each time makes them: each close holds its socket only until the close is
/// done (the grant's linger of 5 s at most), and none is its peer's any more once
/// closing (work item 1.3b), so none is refused.
#[test]
fn seventy_connections_one_after_another_each_free_their_socket() {
    let mut node = Node::start(true);
    let outside = node.slot("listen-host");
    let (client, slot) = node.api();
    let network = shared(client);
    let at = Endpoint::new(NODE_ADDRESS, 443);
    turn_until(&network, |_, event| event == Some(Event::Listening(at)));
    let mut host = host(&outside);
    for _ in 0..70 {
        let opened = host.open(Target::Endpoint(at), None).expect("connected");
        let mut conn = Conn::new(network.clone(), accept(&network));
        conn.close();
        turn_until(&network, |_, _| network.borrow().connections() == 0);
        assert_eq!(receive(&mut host, opened.socket, 64), Ok(vec![]));
        host.close(opened.socket).expect("closed");
    }
    drop((network, slot, host, outside));
    node.end();
}

/// When every socket of the grant is held, a further connection is reset at accept
/// (work item 1.3b: the network server gives its listener no socket for it), and the loop
/// does not spin asking; once a connection has closed and its close is done, the next is
/// accepted.
#[test]
fn with_every_socket_of_the_grant_held_accepting_waits_for_one_to_close() {
    // Three sockets: the listener and two connections; a close done in a tenth of a
    // second.
    let small = NODE.replace(
        "local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000",
        "local=0.0.0.0/0@443 sockets=3 linger=100",
    );
    assert_ne!(small, NODE);
    let mut node = Node::configured(&small, true);
    let outside = node.slot("listen-host");
    let (client, slot) = node.api();
    let network = shared(client);
    let at = Endpoint::new(NODE_ADDRESS, 443);
    turn_until(&network, |_, event| event == Some(Event::Listening(at)));
    let mut host = host(&outside);
    let mut conns = Vec::new();
    for _ in 0..2 {
        host.open(Target::Endpoint(at), None).expect("connected");
        conns.push(Conn::new(network.clone(), accept(&network)));
    }
    // A third is reset, and the loop does not spin asking.
    assert_eq!(
        host.open(Target::Endpoint(at), None).map(|_| ()),
        Err(CallError::Remote(net::Error::Refused)),
        "a connection past the grant's sockets"
    );
    let mut asked = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(500) {
        let (accepted, _) = turn(&network).expect("a turn");
        assert!(accepted.is_empty());
        asked += 1;
    }
    assert!(asked < 100, "{asked} turns in half a second");
    assert_eq!(network.borrow().connections(), 2);
    // One closes: the third is accepted.
    conns.remove(0).close();
    // Once its close is done (in the grant's linger of a tenth of a second at most), a
    // third connects and is accepted.
    let started = Instant::now();
    let third = loop {
        let _ = turn(&network).expect("a turn");
        match host.open(Target::Endpoint(at), None) {
            Ok(opened) => break opened,
            Err(CallError::Remote(net::Error::Refused))
                if started.elapsed() < Duration::from_secs(5) => {}
            other => panic!("a third connection once one closed: {other:?}"),
        }
    };
    let socket = accept(&network);
    let mut conn = Conn::new(network.clone(), socket);
    common::send(&mut host, third.socket, b"third").expect("sent");
    assert_eq!(read(&network, &mut conn), Ok(b"third".to_vec()));
    drop((network, slot, conns, conn, host, outside));
    node.end();
}
