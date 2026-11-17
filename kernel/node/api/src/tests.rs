//! The process's logic on a host (work item 1.3), against the network server's own
//! stack: the cloud node's interface and grant, a DHCP server on its wire that a test
//! can hold back or make move the lease, and beside it a second card that stands for
//! the host at the other end of the node's network.

#[path = "../../../net/stack/tests/common/mod.rs"]
mod common;

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use common::{
    MAC_A, MAC_B, Rng, Wire, dhcp_answer_offering, dhcp_request, no_external_cards, receive, serve,
    serve_until,
};
use moruna_netstack::Stack;
use moruna_svc::deadline_in;
use netstack_protocol::{Cidr, Interface, Policy, Target};

use super::*;

/// The cloud node's network configuration (work item 1.3: its interface, and its one
/// grant exactly as written), and a second card on the same wire with the gateway's
/// address, whose grant reaches the node's network as a host outside it would.
const NODE: &str = "moruna-netstack 1
interface wan card=net0 dhcp
interface host card=net1 ipv4=10.0.2.2/24
grant listen-api rights=listen local=0.0.0.0/0@443 sockets=64
grant listen-host rights=connect remote=10.0.2.0/24 local=10.0.2.2/32 sockets=8
";

const NODE_ADDRESS: Addr = Addr::v4(10, 0, 2, 15);
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

/// The node's wire: card 0 (`wan`) and card 1 (`host`) on one link, and on it the DHCP
/// server `lease` drives.
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
            } else {
                stack.card_mut(1).port.deliver(frame);
            }
        }
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
        for frame in std::mem::take(&mut stack.card_mut(1).port.tx) {
            stack.card_mut(0).port.deliver(frame);
        }
    })
}

/// The node's network server.
struct Node {
    /// Its listener slots, by name.
    slots: BTreeMap<String, Channel>,
    server: thread::JoinHandle<()>,
    lease: Arc<Lease>,
    /// Set: the server ends, as its process does when no lease comes in time.
    stop: Arc<AtomicBool>,
}

impl Node {
    /// The server, its DHCP server answering from the start, or only once told.
    fn start(answers: bool) -> Node {
        let lease = Arc::new(Lease::default());
        lease.answers.store(answers, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let (slots, server) = serve_until(
            NODE,
            &[MAC_A, MAC_B],
            wire(lease.clone()),
            None,
            stop.clone(),
        );
        Node {
            slots,
            server,
            lease,
            stop,
        }
    }

    fn slot(&mut self, name: &str) -> Channel {
        self.slots.remove(name).expect("a listener slot")
    }

    /// `api`'s service, on a connection through its slot.
    fn api(&mut self) -> (Service, Channel) {
        let network = self.slot("listen-api");
        let client = connect(&network, account()).expect("the network server");
        (Service::new(client), network)
    }

    /// Waits for the server to end, which it does once its last client and listener
    /// have gone.
    fn end(self) {
        drop(self.slots);
        self.server.join().expect("the network server");
    }
}

fn account() -> Account {
    Account::carve(8 << 20, "api-test").expect("an account")
}

/// The host outside the node.
fn host(listener: &Channel) -> net::Client {
    let channel = moruna_svc::connect(listener).expect("connect");
    let mut client =
        net::Client::connect(channel, deadline_in(10 * SECOND), account()).expect("hello");
    client.core().set_timeout(30 * SECOND);
    client
}

/// Steps `service` until an event `wanted` holds, within 30 s: every event until then,
/// that one last.
fn until(service: &mut Service, wanted: impl Fn(&Event) -> bool) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    loop {
        let event = service.step().expect("a step");
        events.push(event);
        if wanted(&event) {
            return events;
        }
        assert!(Instant::now() < deadline, "not within 30 s: {events:?}");
    }
}

fn interface(name: &str, addrs: &[Cidr]) -> Interface {
    Interface {
        name: name.into(),
        mac: MAC_A,
        mtu: 1500,
        up: true,
        dhcp: true,
        addrs: addrs.to_vec(),
        gateway4: None,
        gateway6: None,
    }
}

fn info(interfaces: Vec<Interface>) -> Info {
    Info {
        interfaces,
        dns: vec![],
        policy: Policy::default(),
    }
}

#[test]
fn the_node_s_address_is_wan_s_ipv4_address() {
    let link_local = Cidr::new(Addr("fe80::5054:ff:fe4b:4502".parse().unwrap()), 64);
    let leased = Cidr::new(NODE_ADDRESS, 24);
    let loopback = interface("lo", &[Cidr::new(Addr::v4(127, 0, 0, 1), 8)]);
    // Before the lease, the interface has its link-local address alone.
    assert_eq!(
        address(&info(vec![
            loopback.clone(),
            interface("wan", &[link_local])
        ])),
        Ok(None)
    );
    assert_eq!(
        address(&info(vec![
            loopback.clone(),
            interface("wan", &[link_local, leased])
        ])),
        Ok(Some(NODE_ADDRESS))
    );
    // Another interface's address is not the node's.
    assert_eq!(
        address(&info(vec![
            interface("lan", &[Cidr::new(Addr::v4(192, 168, 0, 9), 24)]),
            interface("wan", &[link_local]),
        ])),
        Ok(None)
    );
    assert_eq!(address(&info(vec![loopback])), Err(Error::NoInterface));
    assert_eq!(address(&info(vec![])), Err(Error::NoInterface));
}

#[test]
fn a_failure_is_the_node_s_fatal_line_with_the_service_s_name() {
    assert_eq!(
        Fatal(&Error::Network("accept", CallError::PeerClosed)).to_string(),
        "moruna: fatal api: accept: PeerClosed"
    );
    assert_eq!(
        Fatal(&Error::NoSlot("network")).to_string(),
        "moruna: fatal api: no network slot"
    );
    assert_eq!(
        Fatal(&Error::NoInterface).to_string(),
        "moruna: fatal api: the network server has no interface wan"
    );
    assert_eq!(
        Error::Network("listen", CallError::Remote(net::Error::NotPermitted)).to_string(),
        "listen: Remote(NotPermitted)"
    );
}

#[test]
fn the_process_says_when_it_listens_and_when_it_stops() {
    let at = Endpoint::new(NODE_ADDRESS, 443);
    assert_eq!(
        Event::Listening(at).line().as_deref(),
        Some("api: listening on 10.0.2.15:443")
    );
    assert_eq!(
        Event::Unlistened(at).line().as_deref(),
        Some("api: stopped listening on 10.0.2.15:443: wan's address changed")
    );
    assert_eq!(Event::Closed(at).line(), None);
    assert_eq!(Event::Idle.line(), None);
}

/// The whole of step 1: the listener opens on the address DHCP gives `wan`, on port
/// 443, and not before; a connection from outside is accepted and then closed by the
/// node; and no other port answers.
#[test]
fn the_listener_opens_on_the_leased_address_and_every_connection_is_closed() {
    let mut node = Node::start(false);
    let outside = node.slot("listen-host");
    let (mut api, network) = node.api();

    // No lease yet: no listener, and each step waits a poll's time for one.
    for _ in 0..3 {
        let asked = Instant::now();
        assert_eq!(api.step(), Ok(Event::Idle));
        assert!(asked.elapsed() >= Duration::from_nanos(ADDRESS_POLL_NS));
        assert_eq!(api.listening(), None);
    }
    node.lease.answers.store(true, Ordering::SeqCst);
    let at = Endpoint::new(NODE_ADDRESS, 443);
    let events = until(&mut api, |e| *e == Event::Listening(at));
    assert!(events[..events.len() - 1].iter().all(|e| *e == Event::Idle));
    assert_eq!(api.listening(), Some(at));

    let mut host = host(&outside);
    for _ in 0..3 {
        let opened = host
            .open(Target::Endpoint(at), None)
            .expect("a connection to 443 is accepted");
        assert_eq!(opened.remote, at);
        // The node's side: accepted, and closed at once.
        assert_eq!(api.step(), Ok(Event::Closed(opened.local)));
        // The host's side: the end of the stream, with nothing in it.
        assert_eq!(receive(&mut host, opened.socket, 64), Ok(vec![]));
        host.close(opened.socket).expect("close");
    }
    // No connection: the step waits a lease poll, finds the lease unchanged, and keeps
    // the listener.
    let asked = Instant::now();
    assert_eq!(api.step(), Ok(Event::Idle));
    assert!(asked.elapsed() >= Duration::from_nanos(LEASE_POLL_NS));
    assert_eq!(api.listening(), Some(at));

    // Any other port refuses the connection.
    for port in [444, 80, 8443] {
        assert_eq!(
            host.open(Target::Endpoint(Endpoint::new(NODE_ADDRESS, port)), None)
                .map(|_| ()),
            Err(CallError::Remote(net::Error::Refused)),
            "port {port}"
        );
    }
    // And 443 still answers after them.
    let opened = host.open(Target::Endpoint(at), None).expect("443 again");
    assert_eq!(api.step(), Ok(Event::Closed(opened.local)));

    drop((api, host, network, outside));
    node.end();
}

/// The listener follows the lease (system design 1C): lost, it closes; given again at
/// another address, it opens there, and connections to it are served.
#[test]
fn the_listener_follows_the_lease_to_its_new_address() {
    let mut node = Node::start(true);
    let outside = node.slot("listen-host");
    let (mut api, network) = node.api();
    let first = Endpoint::new(NODE_ADDRESS, 443);
    until(&mut api, |e| *e == Event::Listening(first));

    // The lease is lost, and the DHCP server holds its answer.
    node.lease.offer.store(16, Ordering::SeqCst);
    node.lease.answers.store(false, Ordering::SeqCst);
    node.lease.renew.store(true, Ordering::SeqCst);
    let events = until(&mut api, |e| *e == Event::Unlistened(first));
    assert!(events.len() <= 3, "{events:?}");
    assert_eq!(api.listening(), None);
    // No address, so no listener.
    assert_eq!(api.step(), Ok(Event::Idle));
    assert_eq!(api.listening(), None);

    // The new lease, at another address.
    node.lease.answers.store(true, Ordering::SeqCst);
    let second = Endpoint::new(Addr::v4(10, 0, 2, 16), 443);
    let events = until(&mut api, |e| *e == Event::Listening(second));
    assert!(
        events[..events.len() - 1].iter().all(|e| *e == Event::Idle),
        "{events:?}"
    );
    let mut host = host(&outside);
    let opened = host
        .open(Target::Endpoint(second), None)
        .expect("a connection to the new address");
    assert_eq!(api.step(), Ok(Event::Closed(opened.local)));
    assert_eq!(receive(&mut host, opened.socket, 64), Ok(vec![]));

    drop((api, host, network, outside));
    node.end();
}

#[test]
fn the_port_has_one_owner() {
    let mut node = Node::start(true);
    let network = node.slot("listen-api");
    let mut first = Service::new(connect(&network, account()).expect("a connection"));
    let mut second = Service::new(connect(&network, account()).expect("another"));
    let at = Endpoint::new(NODE_ADDRESS, 443);
    until(&mut first, |e| *e == Event::Listening(at));
    let deadline = Instant::now() + Duration::from_secs(30);
    let refused = loop {
        match second.step() {
            Ok(Event::Idle) if Instant::now() < deadline => continue,
            other => break other,
        }
    };
    assert_eq!(
        refused,
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
    let (mut slots, server) = serve(lo, &[], no_external_cards());
    let network = slots.remove("listen-api").expect("the api's slot");
    let mut api = Service::new(connect(&network, account()).expect("the network server"));
    assert_eq!(api.step(), Err(Error::NoInterface));
    drop((api, network, slots));
    server.join().expect("the network server");
}

#[test]
fn no_network_server_is_a_failure_to_connect() {
    let (network, server) = Channel::create().expect("a channel");
    drop(server);
    assert_eq!(
        connect(&network, account()).err(),
        Some(Error::Network("connect", CallError::PeerClosed))
    );
}

/// Steps until a step fails, on a thread: its failure.
fn failure(mut api: Service) -> thread::JoinHandle<Error> {
    thread::spawn(move || {
        loop {
            if let Err(error) = api.step() {
                return error;
            }
        }
    })
}

/// What the process sees when the network server finds no lease in time and exits:
/// its wait for the node's address ends with the failure it then stops on.
#[test]
fn the_wait_for_the_address_ends_when_the_network_server_does() {
    let mut node = Node::start(false);
    let (api, network) = node.api();
    let waiting = failure(api);
    thread::sleep(Duration::from_nanos(3 * ADDRESS_POLL_NS));
    assert!(!waiting.is_finished(), "the wait ended with no address");
    node.stop.store(true, Ordering::SeqCst);
    assert!(matches!(
        waiting.join().expect("the wait"),
        Error::Network(_, CallError::PeerClosed)
    ));
    drop(network);
    node.end();
}

/// And the same once it serves: a wait for a connection ends when the network server
/// does.
#[test]
fn the_wait_for_a_connection_ends_when_the_network_server_does() {
    let mut node = Node::start(true);
    let (mut api, network) = node.api();
    until(&mut api, |e| matches!(e, Event::Listening(_)));
    let serving = failure(api);
    thread::sleep(Duration::from_millis(250));
    assert!(!serving.is_finished(), "a step failed with no reason to");
    node.stop.store(true, Ordering::SeqCst);
    assert!(matches!(
        serving.join().expect("the service"),
        Error::Network(_, CallError::PeerClosed)
    ));
    drop(network);
    node.end();
}
