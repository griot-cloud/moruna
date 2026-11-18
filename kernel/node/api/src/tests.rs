//! The process's logic on a host (work items 1.3 and 2.2b), against the network server's
//! own stack: the cloud node's interface and grant, a DHCP server on its wire that a test
//! can hold back or make move the lease, and beside it a second card that stands for the
//! host at the other end of the node's network.
//!
//! This file holds that harness and the tests that need no network; `tests/network.rs`
//! tests the network side, and `tests/serve.rs` the whole process serving HTTPS.

#[path = "../../../net/stack/tests/common/mod.rs"]
mod common;
#[path = "../../../api/server/tests/common/signer.rs"]
mod signer;

mod admission;
mod jobs;
mod network;
mod serve;

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use common::{MAC_A, MAC_B, Rng, Wire, dhcp_answer_offering, dhcp_request, serve_until};
use moruna_api::{Cloud, Executor, ExecutorKind, NodeHealth};
use moruna_api_server::{Clock, IdentityError, TokenKey};
use moruna_netstack::Stack;
use moruna_node_config::{NodeConfig, PublicKey, base64};
use moruna_svc::deadline_in;
use netstack_protocol::{Cidr, Interface, Policy};
use rcgen::{CertificateParams, KeyPair};
use signer::Signer;

use super::*;

/// The cloud node's network configuration (work item 1.3: its interface; and its one
/// grant exactly as written, with work item 1.3b's bounds), and a second card on the same
/// wire with the gateway's address, whose grant reaches the node's network as a host
/// outside it would.
const NODE: &str = "moruna-netstack 1
interface wan card=net0 dhcp
interface host card=net1 ipv4=10.0.2.2/24
grant listen-api rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000
grant listen-host rights=connect remote=10.0.2.0/24 local=10.0.2.2/32 sockets=8
";

const NODE_ADDRESS: Addr = Addr::v4(10, 0, 2, 15);
const SECOND: u64 = 1_000_000_000;
/// The node's memory in the tests' hand-overs: 2 GiB.
const MEMORY_BYTES: u64 = 2 << 30;
/// The tests' instance ID, and the wall clock their hand-overs carry.
const INSTANCE: &str = "3f6c1d2e-8b7a-4c55-9e1f-0a2b3c4d5e6f";
const WALL_CLOCK: u64 = 1_794_931_200;

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
        Node::configured(NODE, answers)
    }

    /// The same, with the network server's configuration `config`.
    fn configured(config: &str, answers: bool) -> Node {
        let lease = Arc::new(Lease::default());
        lease.answers.store(answers, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let (slots, server) = serve_until(
            config,
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

    /// `api`'s connection to the server through its slot, and the slot.
    fn api(&mut self) -> (net::Client, Channel) {
        let network = self.slot("listen-api");
        let client = connect(&network, account()).expect("the network server");
        (client, network)
    }

    /// Waits for the server to end, which it does once its last client and listener
    /// have gone.
    fn end(self) {
        drop(self.slots);
        self.server.join().expect("the network server");
    }
}

fn account() -> Account {
    Account::carve(16 << 20, "api-test").expect("an account")
}

/// The host outside the node.
fn host(listener: &Channel) -> net::Client {
    let channel = moruna_svc::connect(listener).expect("connect");
    let mut client =
        net::Client::connect(channel, deadline_in(10 * SECOND), account()).expect("hello");
    client.core().set_timeout(30 * SECOND);
    client
}

/// Node config with these token keys and, when given, the operator's TLS identity.
fn user_data(keys: &[[u8; 32]], tls: Option<(&str, &str)>) -> String {
    let keys: Vec<String> = keys
        .iter()
        .map(|k| format!("\"{}\"", PublicKey(*k).to_base64()))
        .collect();
    let tls = tls.map_or(String::new(), |(certificate, key)| {
        format!(
            r#","tls":{{"certificate":{},"key":{}}}"#,
            serde_json::Value::from(certificate),
            serde_json::Value::from(key)
        )
    });
    format!(
        r#"{{"moruna":1,"api":{{"token_keys":[{}]{tls}}}}}"#,
        keys.join(",")
    )
}

/// A hand-over as `platform` makes one, of `user_data`, its wall clock read now.
fn handover_of(user_data: &str) -> Handover {
    Handover {
        config: NodeConfig::validate(user_data.as_bytes(), MEMORY_BYTES)
            .expect("a valid node config"),
        instance: String::from(INSTANCE),
        cloud: Cloud::OpenStack,
        wall_clock: WALL_CLOCK,
        read_at_ns: now(),
        memory_bytes: MEMORY_BYTES,
        boot_ms: None,
    }
}

/// A hand-over whose node config holds one token key.
fn handed() -> Handover {
    handover_of(&user_data(&[[7; 32]], None))
}

/// `der` as a PEM block of `label`.
fn pem(label: &str, der: &[u8]) -> String {
    let text = base64::encode(der);
    let lines: Vec<&str> = text
        .as_bytes()
        .chunks(64)
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

/// An operator's certificate for `name` and its key, in PEM, and the certificate's DER.
fn operator(name: &str) -> (String, String, Vec<u8>) {
    let key = KeyPair::generate().unwrap();
    let certificate = CertificateParams::new(vec![String::from(name)])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    (
        pem("CERTIFICATE", certificate.der()),
        pem("PRIVATE KEY", &key.serialize_der()),
        certificate.der().to_vec(),
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    let fatal = |error: Error| Fatal(&error).to_string();
    assert_eq!(
        fatal(Error::Network("accept", CallError::PeerClosed)),
        "moruna: fatal api: accept: PeerClosed"
    );
    assert_eq!(
        fatal(Error::NoSlot("network")),
        "moruna: fatal api: no network slot"
    );
    assert_eq!(
        fatal(Error::NoInterface),
        "moruna: fatal api: the network server has no interface wan"
    );
    assert_eq!(
        fatal(Error::Link(
            "waiting for the hand-over",
            moruna_svc::Error::PeerClosed
        )),
        format!(
            "moruna: fatal api: the link to platform: waiting for the hand-over: {}",
            moruna_svc::Error::PeerClosed.name()
        )
    );
    assert_eq!(
        fatal(Error::Kernel(
            "a connection's memory",
            moruna_svc::Error::NoMemory
        )),
        format!(
            "moruna: fatal api: a connection's memory: {}",
            moruna_svc::Error::NoMemory.name()
        )
    );
    assert_eq!(
        fatal(Error::LinkHandles),
        "moruna: fatal api: a message from platform carries handles"
    );
    assert_eq!(
        fatal(Error::Handover(HandoverError::Instance)),
        "moruna: fatal api: the hand-over's `instance` is not an instance ID"
    );
    assert_eq!(
        fatal(Error::Identity(true, IdentityError::Mismatch)),
        format!(
            "moruna: fatal api: node config's tls: {}",
            IdentityError::Mismatch
        )
    );
    assert_eq!(
        fatal(Error::Identity(false, IdentityError::Random)),
        format!(
            "moruna: fatal api: the node's own TLS identity: {}",
            IdentityError::Random
        )
    );
    assert_eq!(
        fatal(Error::Server(String::from("no"))),
        "moruna: fatal api: the API server: no"
    );
}

#[test]
fn the_process_says_when_it_listens_and_when_it_stops() {
    let at = Endpoint::new(NODE_ADDRESS, 443);
    assert_eq!(
        Event::Listening(at).line(),
        "api: listening on 10.0.2.15:443"
    );
    assert_eq!(
        Event::Unlistened(at).line(),
        "api: stopped listening on 10.0.2.15:443: wan's address changed"
    );
}

/// The wall clock is the hand-over's, carried forward by the monotonic time since
/// `platform` read it, a whole second at a time.
#[test]
fn the_wall_clock_is_carried_forward_from_the_hand_over() {
    let mut handover = handed();
    handover.read_at_ns = 50 * SECOND;
    let clock = node::NodeClock::new(&handover);
    assert_eq!(clock.unix_seconds_at(50 * SECOND), WALL_CLOCK);
    assert_eq!(clock.unix_seconds_at(51 * SECOND - 1), WALL_CLOCK);
    assert_eq!(clock.unix_seconds_at(51 * SECOND), WALL_CLOCK + 1);
    assert_eq!(clock.unix_seconds_at(3_650 * SECOND), WALL_CLOCK + 3_600);
    // A monotonic time before the reading is the reading's.
    assert_eq!(clock.unix_seconds_at(0), WALL_CLOCK);
    // Now, on the core's clocks.
    let now_clock = node::NodeClock::new(&handed());
    assert!((WALL_CLOCK..WALL_CLOCK + 2).contains(&now_clock.unix_seconds()));
    let ms = now_clock.monotonic_ms();
    assert!((ms..ms + 1_000).contains(&(now() / 1_000_000)));
}

/// The function keys come from node config's `functions.keys` (item 3.6).
#[test]
fn the_function_keys_come_from_node_config() {
    let key = base64::encode(&[5; 32]);
    let user_data = format!(
        r#"{{"moruna":1,"api":{{"token_keys":["{}"]}},"functions":{{"keys":["{key}"]}}}}"#,
        base64::encode(&[7; 32])
    );
    let config = node::config(&handover_of(&user_data), 1, 0, now()).expect("a config");
    assert_eq!(
        config.function_keys,
        [moruna_api_server::FunctionKey::new([5; 32])]
    );
}

/// The node document's facts and the token keys come from the hand-over (work item 2.2b).
#[test]
fn the_server_s_config_comes_from_the_hand_over() {
    let keys = [[7; 32], [9; 32]];
    let handover = handover_of(&user_data(&keys, None));
    let config = node::config(&handover, 3, 6_849_298_432, now()).expect("a config");
    let facts = &config.facts;
    // The job bound node-admission answered (system design 3A).
    assert_eq!(facts.job_memory_bytes, 6_849_298_432);
    assert_eq!(facts.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(facts.version, node::VERSION);
    assert_eq!(facts.instance, INSTANCE);
    assert_eq!(facts.cloud, Cloud::OpenStack);
    assert_eq!(facts.cores, 3);
    assert_eq!(facts.memory_bytes, MEMORY_BYTES);
    assert_eq!(
        facts.executors,
        [Executor {
            kind: ExecutorKind::Cpu,
            slots: 3
        }]
    );
    assert_eq!(facts.boot_ms, 0);
    assert_eq!(facts.health, NodeHealth::Ok);
    assert_eq!(config.token_keys, keys.map(TokenKey::new));
    // No `functions`: no key may sign a function image.
    assert!(config.function_keys.is_empty());
    // No `tls`: the node's own identity, a fresh one each time.
    let fingerprint = config.identity.fingerprint();
    assert_eq!(fingerprint.len(), 64);
    assert!(
        fingerprint
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    let again = node::config(&handover, 3, 0, now()).expect("a config");
    assert_ne!(again.identity.fingerprint(), fingerprint);
}

#[test]
fn the_operator_s_tls_is_the_identity_when_node_config_has_one() {
    let (certificate, key, der) = operator("node.example");
    let handover = handover_of(&user_data(&[[7; 32]], Some((&certificate, &key))));
    let config = node::config(&handover, 1, 0, now()).expect("a config");
    assert_eq!(config.identity.certificate(), der.as_slice());
    let digest = ring::digest::digest(&ring::digest::SHA256, &der);
    assert_eq!(config.identity.fingerprint(), hex(digest.as_ref()));

    // A key that is not the certificate's is refused, and the process stops on it.
    let (_, other_key, _) = operator("other.example");
    let handover = handover_of(&user_data(&[[7; 32]], Some((&certificate, &other_key))));
    assert_eq!(
        node::config(&handover, 1, 0, now()).err(),
        Some(Error::Identity(true, IdentityError::Mismatch))
    );
}

/// The hand-over over a real channel, `platform`'s own end of the link on the other
/// side: `api` takes it whole and answers `ready`.
#[test]
fn the_hand_over_crosses_a_channel_from_platform_s_half() {
    use moruna_node_platform::{machine::Link, svc::ChannelLink};
    let (ours, theirs) = Channel::create().expect("a channel");
    let handed = handed();
    let sent = handed.clone();
    let platform = thread::spawn(move || {
        let mut link = ChannelLink(ours);
        link.send(&sent.encode().expect("encoded"), deadline_in(5 * SECOND))
            .expect("sent");
        link.receive(deadline_in(5 * SECOND))
    });
    assert_eq!(handover(&theirs), Ok(handed));
    assert_eq!(ready(&theirs), Ok(()));
    assert_eq!(platform.join().expect("platform"), Ok(READY.to_vec()));
}

#[test]
fn the_link_to_platform_fails_when_platform_has_gone_or_sends_handles() {
    // Gone before its hand-over.
    let (ours, theirs) = Channel::create().expect("a channel");
    drop(ours);
    assert_eq!(
        handover(&theirs),
        Err(Error::Link(
            "waiting for the hand-over",
            moruna_svc::Error::PeerClosed
        ))
    );
    assert_eq!(
        ready(&theirs),
        Err(Error::Link(
            "answering ready",
            moruna_svc::Error::PeerClosed
        ))
    );
    // Gone while api waits for it.
    let (ours, theirs) = Channel::create().expect("a channel");
    let waiting = thread::spawn(move || handover(&theirs));
    thread::sleep(Duration::from_millis(100));
    assert!(!waiting.is_finished());
    drop(ours);
    assert_eq!(
        waiting.join().expect("the wait"),
        Err(Error::Link(
            "waiting for the hand-over",
            moruna_svc::Error::PeerClosed
        ))
    );
    // A message with a handle in it.
    let (ours, theirs) = Channel::create().expect("a channel");
    let (spare, _kept) = Channel::create().expect("another");
    ours.write_with(&handed().encode().expect("encoded"), &mut [Some(spare.0)])
        .expect("written");
    assert_eq!(handover(&theirs), Err(Error::LinkHandles));
    // An empty message is no hand-over.
    ours.write_with(b"", &mut []).expect("written");
    assert!(matches!(handover(&theirs), Err(Error::Handover(_))));
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
