//! The cloud node's network (system design 1B and 1C, work item 1.3), on a host: its
//! configuration is read as written; its first DHCP lease gives the boot's network
//! line, once, `gateway none` when the lease names no router; no lease is due 30 s
//! after the server's start; and its one grant, which only listens on port 443,
//! refuses every way out, and holds at most 81 sockets of 64 KiB buffers each way, 8 a
//! peer, each closing one for at most 5 s (work item 1.3b).

mod common;

use common::{MAC_A, MAC_B, PAIR, Rng, Wire, dhcp_server, serve};
use moruna_netstack::{Card, Stack, boot::LEASE_WITHIN_NS, parse};
use moruna_svc::{CallError, Channel, deadline_in};
use netstack_protocol::{Addr, Cidr, Endpoint, Policy, Rule, Target, net, rights};

/// `kernel/node/cloud/network.conf`, exactly as work item 1.3 gives it, with the grant
/// work item 1.3b gives `api`: 80 connections, system design 2B's cap, and the
/// listener; 64 KiB buffers each way; 8 sockets a peer; 5 s to finish a close.
const NETWORK_CONF: &str = "moruna-netstack 1
interface wan card=net0 dhcp
grant listen-api rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000
";

const MS: u64 = 1_000_000;
const SECOND: u64 = 1000 * MS;

/// The cloud configuration's stack over one card, made at `now` on a clock the test
/// moves.
fn cloud_stack(now: u64) -> Stack {
    let config = parse(NETWORK_CONF).expect("the configuration");
    let card = Card {
        mac: MAC_A,
        mtu: 1500,
        up: true,
    };
    Stack::new(&config, &[card], 0x1234_5678, now).expect("the stack")
}

/// Polls `stack` every 10 ms of its own clock, from `*t` for `span`, with `wire` at
/// the other end of its card, until `done` says so. Whether it did.
fn drive(
    stack: &mut Stack,
    wire: &mut Wire,
    t: &mut u64,
    span: u64,
    mut done: impl FnMut(&Stack) -> bool,
) -> bool {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let end = *t + span;
    while *t < end {
        stack.poll(*t);
        wire(stack, &mut rng);
        if done(stack) {
            return true;
        }
        *t += 10 * MS;
    }
    false
}

/// A wire nothing answers on: every frame the card sends is lost.
fn silence() -> Wire {
    Box::new(|stack: &mut Stack, _: &mut Rng| stack.card_mut(0).port.tx.clear())
}

fn bound(stack: &Stack) -> usize {
    stack
        .notes
        .iter()
        .filter(|n| n.contains("DHCP bound"))
        .count()
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

#[test]
fn the_cloud_configuration_is_read_as_written() {
    let config = parse(NETWORK_CONF).expect("the configuration parses as written");
    assert_eq!(config.interfaces.len(), 1);
    let wan = &config.interfaces[0];
    assert_eq!((wan.name.as_str(), wan.card.as_str()), ("wan", "net0"));
    assert!(wan.dhcp && wan.addrs.is_empty() && wan.gateway4.is_none());
    assert!(config.dns.is_empty());
    assert_eq!(
        config.grants,
        vec![(
            String::from("listen-api"),
            Policy {
                rights: rights::LISTEN,
                remote: vec![],
                local: vec![Rule {
                    net: Cidr::new(Addr::v4(0, 0, 0, 0), 0),
                    first: 443,
                    last: 443,
                }],
                names: vec![],
                sockets: 81,
                rx: Some(65536),
                tx: Some(65536),
                per_peer: Some(8),
                linger_ms: Some(5000),
            }
        )]
    );
}

#[test]
fn the_first_lease_prints_the_network_line_once() {
    let started = 3 * SECOND;
    let mut stack = cloud_stack(started);
    let mut wire = dhcp_server(true);
    let mut t = started;
    assert!(stack.boot.is_empty());
    assert_eq!(stack.lease_due(started), Some(started + LEASE_WITHIN_NS));
    let leased = drive(&mut stack, &mut wire, &mut t, 30 * SECOND, |s| {
        !s.boot.is_empty()
    });
    assert!(leased, "no lease in 30 s: {:?}", stack.notes);
    // QEMU's user network, as the booted check will see it.
    assert_eq!(
        stack.boot,
        ["moruna: network wan 10.0.2.15/24 gateway 10.0.2.2"]
    );
    assert_eq!(bound(&stack), 1);
    // Nothing is waited for any more, however late it is.
    assert_eq!(stack.lease_due(started), None);
    assert!(!stack.lease_overdue(started, started + 10 * LEASE_WITHIN_NS));

    // The link goes and comes back: the lease is lost and taken again, and the boot's
    // line is not said twice.
    stack.boot.clear();
    stack.set_link(0, false);
    stack.set_link(0, true);
    assert!(stack.notes.iter().any(|n| n == "wan: DHCP lease lost"));
    let again = drive(&mut stack, &mut wire, &mut t, 30 * SECOND, |s| {
        bound(s) == 2
    });
    assert!(again, "no second lease in 30 s: {:?}", stack.notes);
    assert!(stack.boot.is_empty(), "{:?}", stack.boot);
    assert_eq!(stack.lease_due(started), None);
}

#[test]
fn no_lease_is_due_thirty_seconds_after_the_server_s_start() {
    // The server started 2 s before its stack was made: opening its cards took that.
    let started = 5 * SECOND;
    let mut t = started + 2 * SECOND;
    let mut stack = cloud_stack(t);
    let mut wire = silence();
    let due = started + LEASE_WITHIN_NS;
    let never = drive(&mut stack, &mut wire, &mut t, 40 * SECOND, |s| {
        assert_eq!(s.lease_due(started), Some(due));
        !s.boot.is_empty()
    });
    assert!(!never, "a lease with nothing answering: {:?}", stack.boot);
    assert!(!stack.lease_overdue(started, started));
    assert!(!stack.lease_overdue(started, due - 1));
    assert!(stack.lease_overdue(started, due));
    assert!(stack.lease_overdue(started, due + SECOND));
    assert_eq!(bound(&stack), 0);
}

#[test]
fn a_lease_with_no_router_says_gateway_none() {
    let mut stack = cloud_stack(0);
    let mut wire = dhcp_server(false);
    let mut t = 0;
    let leased = drive(&mut stack, &mut wire, &mut t, 30 * SECOND, |s| {
        bound(s) == 1
    });
    assert!(leased, "no lease in 30 s: {:?}", stack.notes);
    assert!(
        stack
            .notes
            .iter()
            .any(|n| n == "wan: DHCP bound 10.0.2.15/24, router none, dns 10.0.2.3"),
        "{:?}",
        stack.notes
    );
    assert_eq!(
        stack.boot,
        ["moruna: network wan 10.0.2.15/24 gateway none"]
    );
    assert_eq!(stack.lease_due(0), None);
}

#[test]
fn a_server_with_no_dhcp_interface_waits_for_no_lease() {
    let config = parse(PAIR).expect("the configuration");
    let cards = [MAC_A, MAC_B].map(|mac| Card {
        mac,
        mtu: 1500,
        up: true,
    });
    let stack = Stack::new(&config, &cards, 0x1234_5678, 0).expect("the stack");
    assert_eq!(stack.lease_due(0), None);
    assert!(!stack.lease_overdue(0, u64::MAX));
}

/// Work item 1.3's policy test: the cloud node's one grant has `rights=listen` only,
/// so nothing that holds it can connect out, by address or by name, or reach out any
/// other way; and it listens on port 443 and no other.
#[test]
fn a_listen_only_grant_refuses_every_way_out() {
    let (mut slots, join) = serve(NETWORK_CONF, &[MAC_A], dhcp_server(true));
    assert_eq!(slots.len(), 1, "one grant, so one way in to the server");
    let listener = slots.remove("listen-api").expect("slot");
    let mut api = client(&listener);

    let policy = api.info().expect("info").policy;
    assert_eq!(policy.rights, rights::LISTEN);
    assert!(policy.remote.is_empty() && policy.names.is_empty());

    let denied = |r: Result<(), CallError<net::Error>>, what: &str| {
        assert!(
            matches!(r, Err(CallError::Remote(net::Error::NotPermitted))),
            "{what}: expected NotPermitted, got {r:?}"
        );
    };
    // Outbound connects: the instance data address, the gateway on the API's own
    // port, loopback, and a name.
    for to in [
        Endpoint::new(Addr::v4(169, 254, 169, 254), 80),
        Endpoint::new(Addr::v4(10, 0, 2, 2), 443),
        Endpoint::new(Addr::v4(127, 0, 0, 1), 443),
    ] {
        denied(
            api.open(Target::Endpoint(to), None).map(|_| ()),
            &format!("a connect to {to}"),
        );
    }
    denied(
        api.open(Target::Name(String::from("example.com"), 443), None)
            .map(|_| ()),
        "a connect by name",
    );
    // Every other way out.
    denied(
        api.resolve(String::from("example.com")).map(|_| ()),
        "a resolve",
    );
    denied(
        api.bind(Endpoint::new(Addr::v4(127, 0, 0, 1), 443))
            .map(|_| ()),
        "a UDP bind",
    );
    denied(api.ping(Addr::v4(10, 0, 2, 2), 8).map(|_| ()), "a ping");
    denied(
        api.grant(policy.clone()).map(|_| ()),
        "a grant of its own policy",
    );

    // What it is for: once the lease is taken, a listener on the node's address, on
    // port 443 and no other.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let address = loop {
        let info = api.info().expect("info");
        let wan = info.interfaces.iter().find(|i| i.name == "wan");
        if let Some(a) = wan.and_then(|i| i.addrs.iter().find(|a| a.addr.is_v4())) {
            break a.addr;
        }
        assert!(std::time::Instant::now() < deadline, "no lease in 30 s");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(address, Addr::v4(10, 0, 2, 15));
    for port in [444, 80, 8443] {
        denied(
            api.listen(Endpoint::new(address, port), 1).map(|_| ()),
            &format!("a listener on port {port}"),
        );
    }
    let (_, at) = api
        .listen(Endpoint::new(address, 443), 64)
        .expect("the listener on 443");
    assert_eq!(at, Endpoint::new(address, 443));
    // A port of the server's choosing is chosen inside the grant, where only 443 is.
    assert!(matches!(
        api.listen(Endpoint::new(address, 0), 1),
        Err(CallError::Remote(net::Error::AddressInUse))
    ));
    // With the listener held, a connect out is still refused.
    denied(
        api.open(
            Target::Endpoint(Endpoint::new(Addr::v4(10, 0, 2, 2), 80)),
            None,
        )
        .map(|_| ()),
        "a connect out beside the listener",
    );

    drop(api);
    drop(listener);
    drop(slots);
    join.join().expect("the server");
}
