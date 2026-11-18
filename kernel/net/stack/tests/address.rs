//! `address` (`moruna.net` 3.3, work item 1.11f), on a host: a request that waits for an
//! interface's IPv4 address, as `accept` waits for a connection. It is answered at once when
//! the interface holds one, and otherwise as soon as the interface takes one: its first DHCP
//! lease, or the next after a lost one. One waits a connection; the caller's own deadline
//! bounds it; an interface that is not set by DHCP and holds no IPv4 address never takes
//! one, and is `NotFound` as a name no interface has is.

mod common;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use common::{MAC_A, MAC_B, Rng, Wire, dhcp_answer_offering, dhcp_request, serve};
use moruna_netstack::Stack;
use moruna_svc::{CallError, Channel, deadline_in};
use netstack_protocol::{Addr, Cidr, net};

/// A DHCP interface, as the cloud node's `wan`, and beside it one with only a fixed IPv6
/// address, which never holds an IPv4 one. The grant holds no right: `address` needs none,
/// as `info` needs none.
const CONFIG: &str = "moruna-netstack 1
interface wan card=net0 dhcp
interface six card=net1 ipv6=fd4b:4532::1/64
grant listen-node rights=connect remote=169.254.169.254/32@80 local=0.0.0.0/0 sockets=4
";

const SECOND: u64 = 1_000_000_000;

/// What the test's DHCP server does.
#[derive(Default)]
struct Lease {
    /// Set: it answers what the node asks; unset, it holds the questions.
    answers: AtomicBool,
    /// The last octet of the address it offers, in 10.0.2.0/24; 15 when 0.
    offer: AtomicU8,
    /// Set: the node's link goes down and up once, so its lease is lost and asked again.
    renew: AtomicBool,
}

/// `wan`'s wire, with the DHCP server `lease` drives on it; what `six` sends goes nowhere.
fn wire(lease: Arc<Lease>) -> Wire {
    let mut asked: Vec<Vec<u8>> = Vec::new();
    Box::new(move |stack: &mut Stack, _: &mut Rng| {
        if lease.renew.swap(false, Ordering::SeqCst) {
            stack.set_link(0, false);
            stack.set_link(0, true);
            asked.clear();
        }
        for frame in std::mem::take(&mut stack.card_mut(0).port.tx) {
            if dhcp_request(&frame).is_some() {
                asked.push(frame);
            }
        }
        stack.card_mut(1).port.tx.clear();
        if lease.answers.load(Ordering::SeqCst) {
            let offer = match lease.offer.load(Ordering::SeqCst) {
                0 => 15,
                octet => octet,
            };
            for frame in asked.drain(..) {
                if let Some(answer) = dhcp_answer_offering(&frame, [10, 0, 2, offer], true) {
                    stack.card_mut(0).port.deliver(answer);
                }
            }
        }
    })
}

/// The server, its DHCP server answering from the start or only once told; the node's
/// listener, and the lease.
fn start(answers: bool) -> (Channel, thread::JoinHandle<()>, Arc<Lease>) {
    let lease = Arc::new(Lease::default());
    lease.answers.store(answers, Ordering::SeqCst);
    let (mut slots, server) = serve(CONFIG, &[MAC_A, MAC_B], wire(lease.clone()));
    let listener = slots.remove("listen-node").expect("the node's slot");
    (listener, server, lease)
}

/// A connection, whose calls wait at most `timeout`.
fn client(listener: &Channel, timeout: u64) -> net::Client {
    let channel = moruna_svc::connect(listener).expect("connect");
    let mut c = net::Client::connect(
        channel,
        deadline_in(10 * SECOND),
        moruna_svc::Account::carve(8 << 20, "net-test").unwrap(),
    )
    .expect("hello");
    c.core().set_timeout(timeout);
    c
}

fn leased(octet: u8) -> Cidr {
    Cidr::new(Addr::v4(10, 0, 2, octet), 24)
}

/// Whether `wan` holds an IPv4 address now, as `info` says.
fn holds(c: &mut net::Client) -> bool {
    c.info()
        .expect("info")
        .interfaces
        .iter()
        .find(|x| x.name == "wan")
        .expect("wan")
        .addrs
        .iter()
        .any(|a| a.addr.is_v4())
}

/// Sets `flag` (and the offer to `octet`) `after` from now, on a thread of its own.
fn later(lease: &Arc<Lease>, after: Duration, octet: u8) -> thread::JoinHandle<()> {
    let lease = lease.clone();
    thread::spawn(move || {
        thread::sleep(after);
        lease.offer.store(octet, Ordering::SeqCst);
        lease.answers.store(true, Ordering::SeqCst);
    })
}

#[test]
fn an_interface_that_holds_an_address_is_answered_at_once() {
    let (listener, server, _lease) = start(true);
    let mut c = client(&listener, 10 * SECOND);
    // Loopback holds its address from the start.
    assert_eq!(
        c.address("lo".into()).expect("lo's address"),
        Cidr::new(Addr::v4(127, 0, 0, 1), 8)
    );
    // The first is answered by the lease, the next at once.
    assert_eq!(c.address("wan".into()).expect("wan's address"), leased(15));
    let asked = Instant::now();
    assert_eq!(c.address("wan".into()).expect("wan's address"), leased(15));
    assert!(asked.elapsed() < Duration::from_secs(1));
    drop((c, listener));
    server.join().unwrap();
}

#[test]
fn a_wait_is_answered_as_soon_as_the_lease_is_taken() {
    let (listener, server, lease) = start(false);
    let mut c = client(&listener, 10 * SECOND);
    assert!(
        !holds(&mut c),
        "no lease while the server holds the questions"
    );
    let asked = Instant::now();
    let release = later(&lease, Duration::from_millis(300), 15);
    assert_eq!(c.address("wan".into()).expect("wan's address"), leased(15));
    // It waited for the lease, and did not answer before it.
    assert!(asked.elapsed() >= Duration::from_millis(300));
    assert!(holds(&mut c));
    release.join().unwrap();
    drop((c, listener));
    server.join().unwrap();
}

#[test]
fn a_lost_lease_is_waited_for_again_and_the_next_one_answers() {
    let (listener, server, lease) = start(true);
    let mut c = client(&listener, 10 * SECOND);
    assert_eq!(
        c.address("wan".into()).expect("the first lease"),
        leased(15)
    );
    // The link goes down and up with the DHCP server deaf: the lease is lost.
    lease.answers.store(false, Ordering::SeqCst);
    lease.renew.store(true, Ordering::SeqCst);
    let lost = Instant::now();
    while holds(&mut c) {
        assert!(
            lost.elapsed() < Duration::from_secs(10),
            "the lease was never lost"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // The next lease, of another address, answers the wait.
    let release = later(&lease, Duration::from_millis(300), 16);
    let asked = Instant::now();
    assert_eq!(c.address("wan".into()).expect("the next lease"), leased(16));
    assert!(asked.elapsed() >= Duration::from_millis(300));
    release.join().unwrap();
    drop((c, listener));
    server.join().unwrap();
}

#[test]
fn the_caller_s_deadline_bounds_its_wait_and_one_waits_a_connection() {
    let (listener, server, lease) = start(false);
    // The caller's deadline passes first: the client stops waiting.
    let mut first = client(&listener, SECOND / 5);
    let asked = Instant::now();
    assert_eq!(
        first.address("wan".into()),
        Err(CallError::TimedOut),
        "answered with no lease"
    );
    assert!(asked.elapsed() >= Duration::from_millis(200));
    // Its wait stands at the server, so another on its connection is refused.
    assert_eq!(
        first.address("wan".into()),
        Err(CallError::Remote(net::Error::InUse))
    );
    // Another connection waits for itself, and one that goes takes its wait with it.
    let mut second = client(&listener, 10 * SECOND);
    let mut going = client(&listener, SECOND / 5);
    assert_eq!(going.address("wan".into()), Err(CallError::TimedOut));
    drop(going);
    let release = later(&lease, Duration::from_millis(100), 15);
    assert_eq!(second.address("wan".into()).expect("the lease"), leased(15));
    release.join().unwrap();
    // The first connection's wait was answered with the lease, after its caller gave up; the
    // answer is filed, and the connection may ask again, answered at once.
    first.core().set_timeout(10 * SECOND);
    assert_eq!(first.address("wan".into()).expect("the lease"), leased(15));
    drop((first, second, listener));
    server.join().unwrap();
}

#[test]
fn an_interface_that_never_takes_an_ipv4_address_is_not_found() {
    let (listener, server, _lease) = start(false);
    let mut c = client(&listener, 10 * SECOND);
    for name in ["six", "eth0", ""] {
        assert_eq!(
            c.address(name.into()),
            Err(CallError::Remote(net::Error::NotFound)),
            "{name:?}"
        );
    }
    drop((c, listener));
    server.join().unwrap();
}

#[test]
fn the_request_is_minor_3_s() {
    assert_eq!(net::ID.minor, 3);
    assert!(net::REQUESTS.contains(&22));
}
