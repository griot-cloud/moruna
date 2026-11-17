//! The process's logic on a host (work item 1.3), against the network server's own
//! stack: the cloud node's interface and grant, a DHCP server on its wire, and beside
//! it a second card that stands for the host at the other end of the node's network.

#[path = "../../../net/stack/tests/common/mod.rs"]
mod common;

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use common::{
    MAC_A, MAC_B, Rng, Wire, dhcp_answer, dhcp_request, no_external_cards, receive, serve,
    serve_until,
};
use moruna_netstack::Stack;
use moruna_svc::deadline_in;
use netstack_protocol::{Cidr, Interface, Policy, Target};

use super::*;

/// The cloud node's network configuration (work item 1.3: its interface, and its one
/// grant exactly as written), and a second card on the same wire with the gateway's
/// address, whose grant reaches the node as a host outside it would.
const NODE: &str = "moruna-netstack 1
interface wan card=net0 dhcp
interface host card=net1 ipv4=10.0.2.2/24
grant listen-api rights=listen local=0.0.0.0/0@443 sockets=64
grant listen-host rights=connect remote=10.0.2.15/32 local=10.0.2.2/32 sockets=8
";

const NODE_ADDRESS: Addr = Addr::v4(10, 0, 2, 15);
const SECOND: u64 = 1_000_000_000;

/// The node's wire: card 0 (`wan`) and card 1 (`host`) on one link, and on it a DHCP
/// server that holds what the node asks until `leases` is set, then answers it.
fn wire(leases: Arc<AtomicBool>) -> Wire {
    let mut asked: Vec<Vec<u8>> = Vec::new();
    Box::new(move |stack: &mut Stack, _: &mut Rng| {
        for frame in std::mem::take(&mut stack.card_mut(0).port.tx) {
            if dhcp_request(&frame).is_some() {
                asked.push(frame);
            } else {
                stack.card_mut(1).port.deliver(frame);
            }
        }
        if leases.load(Ordering::SeqCst) {
            for frame in asked.drain(..) {
                if let Some(answer) = dhcp_answer(&frame, true) {
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
    /// Set: its DHCP server answers.
    leases: Arc<AtomicBool>,
    /// Set: the server ends, as its process does when no lease comes in time.
    stop: Arc<AtomicBool>,
}

impl Node {
    /// The server, its DHCP server answering from the start, or only once `leases` is
    /// set.
    fn start(leases: bool) -> Node {
        let leases = Arc::new(AtomicBool::new(leases));
        let stop = Arc::new(AtomicBool::new(false));
        let (slots, server) = serve_until(
            NODE,
            &[MAC_A, MAC_B],
            wire(leases.clone()),
            None,
            stop.clone(),
        );
        Node {
            slots,
            server,
            leases,
            stop,
        }
    }

    fn slot(&mut self, name: &str) -> Channel {
        self.slots.remove(name).expect("a listener slot")
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
}

#[test]
fn a_failure_says_what_it_is() {
    assert_eq!(Error::NoSlot("network").to_string(), "no network slot");
    assert_eq!(
        Error::NoInterface.to_string(),
        "the network server has no interface wan"
    );
    assert_eq!(
        Error::Network("accept", CallError::PeerClosed).to_string(),
        "accept: PeerClosed"
    );
    assert_eq!(
        Error::Network("listen", CallError::Remote(net::Error::NotPermitted)).to_string(),
        "listen: Remote(NotPermitted)"
    );
}

/// The whole of step 1: the listener opens on the address DHCP gives `wan`, on port
/// 443; a connection from outside is accepted and then closed by the node; and no
/// other port answers.
#[test]
fn the_listener_opens_on_the_leased_address_and_every_connection_is_closed() {
    let mut node = Node::start(false);
    let network = node.slot("listen-api");
    let outside = node.slot("listen-host");
    let mut api = connect(&network, account()).expect("the network server");

    // No lease yet: the node has no address, and the wait for it goes on.
    assert_eq!(address(&api.info().expect("info")), Ok(None));
    let waited = Instant::now();
    let hold = Duration::from_millis(350);
    let leases = node.leases.clone();
    let release = thread::spawn(move || {
        thread::sleep(hold);
        leases.store(true, Ordering::SeqCst);
    });
    let found = await_address(&mut api).expect("the node's address");
    assert_eq!(found, NODE_ADDRESS);
    assert!(
        waited.elapsed() >= hold,
        "the address came before its lease: {:?}",
        waited.elapsed()
    );
    release.join().expect("the DHCP server");

    let (listener, at) = listen(&mut api, found).expect("the listener");
    assert_eq!(at, Endpoint::new(NODE_ADDRESS, 443));

    let mut host = host(&outside);
    for _ in 0..3 {
        let opened = host
            .open(Target::Endpoint(at), None)
            .expect("a connection to 443 is accepted");
        assert_eq!(opened.remote, at);
        // The node's side: accepted, and closed at once.
        assert_eq!(turn(&mut api, listener), Ok(opened.local));
        // The host's side: the end of the stream, with nothing in it.
        assert_eq!(receive(&mut host, opened.socket, 64), Ok(vec![]));
        host.close(opened.socket).expect("close");
    }
    // The node keeps no socket of a connection it closed: its listener alone.
    assert_eq!(api.status(listener).map(|s| s.local), Ok(at));
    assert_eq!(
        api.status(listener + 1).map(|s| s.local),
        Err(CallError::Remote(net::Error::NoSocket))
    );

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
    assert_eq!(turn(&mut api, listener), Ok(opened.local));

    drop((api, host, network, outside));
    node.end();
}

#[test]
fn a_lease_already_taken_is_found_without_a_wait_of_its_own() {
    let mut node = Node::start(true);
    let network = node.slot("listen-api");
    let mut api = connect(&network, account()).expect("the network server");
    assert_eq!(await_address(&mut api), Ok(NODE_ADDRESS));
    // Once the node has its address, asking again answers at once.
    let asked = Instant::now();
    assert_eq!(await_address(&mut api), Ok(NODE_ADDRESS));
    assert!(asked.elapsed() < Duration::from_nanos(ADDRESS_POLL_NS));
    drop((api, network));
    node.end();
}

#[test]
fn only_port_443_on_the_node_s_address_is_listened_on() {
    let mut node = Node::start(true);
    let network = node.slot("listen-api");
    let mut api = connect(&network, account()).expect("the network server");
    let found = await_address(&mut api).expect("the node's address");
    // An address the node does not have.
    assert_eq!(
        listen(&mut api, Addr::v4(10, 0, 2, 16)),
        Err(Error::Network(
            "listen",
            CallError::Remote(net::Error::BadAddress)
        ))
    );
    let (listener, _) = listen(&mut api, found).expect("the listener");
    // The port has one owner.
    assert_eq!(
        listen(&mut api, found),
        Err(Error::Network(
            "listen",
            CallError::Remote(net::Error::AddressInUse)
        ))
    );
    // Only the listener is accepted on.
    assert_eq!(
        turn(&mut api, listener + 1),
        Err(Error::Network(
            "accept",
            CallError::Remote(net::Error::NoSocket)
        ))
    );
    drop((api, network));
    node.end();
}

#[test]
fn a_network_server_with_no_wan_is_refused() {
    let lo = "moruna-netstack 1
grant listen-api rights=listen local=0.0.0.0/0@443 sockets=64
";
    let (mut slots, server) = serve(lo, &[], no_external_cards());
    let network = slots.remove("listen-api").expect("the api's slot");
    let mut api = connect(&network, account()).expect("the network server");
    assert_eq!(await_address(&mut api), Err(Error::NoInterface));
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

/// What the process sees when the network server finds no lease in time and exits:
/// its wait for the node's address ends with the failure it then stops on.
#[test]
fn the_wait_for_the_address_ends_when_the_network_server_does() {
    let mut node = Node::start(false);
    let network = node.slot("listen-api");
    let mut api = connect(&network, account()).expect("the network server");
    let wait = thread::spawn(move || await_address(&mut api));
    thread::sleep(Duration::from_nanos(3 * ADDRESS_POLL_NS));
    assert!(!wait.is_finished(), "the wait ended with no address");
    node.stop.store(true, Ordering::SeqCst);
    assert_eq!(
        wait.join().expect("the wait"),
        Err(Error::Network("info", CallError::PeerClosed))
    );
    drop(network);
    node.end();
}

/// And the same once it serves: an accept that waits for a connection ends when the
/// network server does.
#[test]
fn the_wait_for_a_connection_ends_when_the_network_server_does() {
    let mut node = Node::start(true);
    let network = node.slot("listen-api");
    let mut api = connect(&network, account()).expect("the network server");
    let found = await_address(&mut api).expect("the node's address");
    let (listener, _) = listen(&mut api, found).expect("the listener");
    let serving = thread::spawn(move || turn(&mut api, listener));
    thread::sleep(Duration::from_millis(250));
    assert!(!serving.is_finished(), "an accept with no connection");
    node.stop.store(true, Ordering::SeqCst);
    assert_eq!(
        serving.join().expect("the accept"),
        Err(Error::Network("accept", CallError::PeerClosed))
    );
    drop(network);
    node.end();
}
