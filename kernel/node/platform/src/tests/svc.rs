//! The node's own sockets, clock and link, on the host's model of the kernel: the sockets against
//! the real network server, run on a thread as its own tests run it; and the lines this item
//! adds to the cloud profile, read by the parsers that will read them on the node.

use core::net::Ipv4Addr;
use std::{
    collections::BTreeMap,
    format,
    rc::Rc,
    string::{String, ToString},
    thread,
    time::Duration,
    vec,
    vec::Vec,
};

use moruna_netstack::{Card, Shared, Stack, after_turn, factory, parse};
use moruna_node::{Bootstrap, restart::API_STARTED};
use moruna_node_config::{Handover, MESSAGE_MAX, READY};
use moruna_svc::{
    Account, Channel, DEADLINE_INFINITE, Dispatcher, Memory, Rights, Turn, deadline_in, limits,
    map, now,
};
use netstack_protocol::{Addr, Endpoint, net, rights};

use super::{FakeClock, FakeNetwork, SECOND, answer, openstack, recorded};
use crate::{
    boot::{self, Inputs},
    http::{self, Request},
    instance,
    machine::{Clock, Control, Link, LinkError, Network, Stream, StreamError},
    svc::{ChannelControl, ChannelLink, Monotonic, Wan},
};

const MS: u64 = 1_000_000;

/// The network server on a thread, as its own host tests run it: its stack and dispatcher,
/// polled as the server's loop polls them. Frames it gives a card go nowhere. Returns the client
/// end of every granted listener, and the thread, which ends when every client has gone.
fn serve(config: &str, cards: &[Card]) -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    let config = parse(config).expect("the configuration");
    let cards = cards.to_vec();
    let mut ours = BTreeMap::new();
    let mut theirs = Vec::new();
    for (slot, policy) in &config.grants {
        let (our_end, their_end) = Channel::create().expect("a channel");
        ours.insert(slot.clone(), our_end);
        theirs.push((their_end, policy.clone()));
    }
    let server = thread::spawn(move || {
        let stack = Stack::new(&config, &cards, 0x1234_5678, now()).expect("the stack");
        let shared = Shared::new(stack);
        let mut dispatcher = Dispatcher::new();
        for (listener, policy) in theirs {
            dispatcher
                .listen(listener, factory(Rc::clone(&shared), policy))
                .expect("a listener");
        }
        loop {
            after_turn(&shared, &mut dispatcher, now());
            let deadline = {
                let mut stack = shared.stack.borrow_mut();
                for card in 0..stack.cards() {
                    stack.card_mut(card).port.tx.clear();
                }
                let at = now();
                stack
                    .poll_at()
                    .unwrap_or(at + 50 * MS)
                    .clamp(at, at + 50 * MS)
            };
            if dispatcher.channels() == 0
                || dispatcher.turn(deadline).expect("a turn") == Turn::Empty
            {
                break;
            }
        }
    });
    (ours, server)
}

fn client(listener: &Channel) -> net::Client {
    let mut client = net::Client::connect(
        moruna_svc::connect(listener).expect("a connection"),
        deadline_in(5 * SECOND),
        Account::carve(8 << 20, "platform-test").expect("an account"),
    )
    .expect("the network server's welcome");
    client.core().set_timeout(5 * SECOND);
    client
}

/// The bytes of one receive on the fake cloud's side of a connection.
fn receive(server: &mut net::Client, socket: u32) -> Vec<u8> {
    let memory = Memory::create_in(65536, &server.account().0).expect("memory");
    let loan = Memory(
        memory
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .expect("a loan"),
    );
    let Ok(count) = server.receive_into(socket, loan, 0, 65536) else {
        return Vec::new();
    };
    let mut bytes = vec![0; count as usize];
    memory
        .map(0, 65536, map::READ)
        .and_then(|mapping| mapping.read(0, &mut bytes))
        .expect("the bytes");
    bytes
}

/// Sends all of `bytes` from the fake cloud's side of a connection.
fn send(server: &mut net::Client, socket: u32, bytes: &[u8]) {
    for piece in bytes.chunks(60_000) {
        let memory = Memory::create_in(65536, &server.account().0).expect("memory");
        memory
            .map(0, 65536, map::WRITE)
            .and_then(|mapping| mapping.write(0, piece))
            .expect("the bytes");
        let mut sent = 0;
        while sent < piece.len() {
            let loan = Memory(
                memory
                    .0
                    .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                    .expect("a loan"),
            );
            sent += server
                .send_from(socket, loan, sent as u64, (piece.len() - sent) as u32)
                .expect("a send") as usize;
        }
    }
}

/// A fake cloud on the network server's loopback: for each of `script`, in turn, it accepts a
/// connection, reads a request of that many bytes, and sends that answer. A connection that
/// ends before its request is whole gets no answer. Returns the requests it read.
fn cloud(
    listener: &Channel,
    port: u16,
    script: Vec<(usize, Vec<u8>)>,
) -> thread::JoinHandle<Vec<Vec<u8>>> {
    let mut server = client(listener);
    let at = Endpoint::new(Addr::v4(127, 0, 0, 1), port);
    let (listening, _) = server.listen(at, 4).expect("a listener");
    thread::spawn(move || {
        let mut requests = Vec::new();
        for (length, answer) in script {
            let opened = server.accept(listening).expect("a connection");
            let mut request = Vec::new();
            while request.len() < length {
                let bytes = receive(&mut server, opened.socket);
                if bytes.is_empty() {
                    break;
                }
                request.extend(bytes);
            }
            if request.len() == length {
                send(&mut server, opened.socket, &answer);
                let _ = server.shutdown(opened.socket);
            }
            let _ = server.close(opened.socket);
            requests.push(request);
        }
        requests
    })
}

/// The stand-in cloud's grant and platform's, over loopback. A grant's sockets count its
/// closing ones until they are freed, TIME-WAIT's 10 s included (work item 1.3b), and the
/// test makes five connections in a row: so 16 each, where the cloud profile's platform
/// grant has 4.
const LOOPBACK: &str = "moruna-netstack 1
grant listen-cloud rights=listen local=127.0.0.1/32@8080 sockets=16
grant listen-platform rights=connect remote=127.0.0.1/32@8080-8081 local=127.0.0.1/32 sockets=16
";

#[test]
fn requests_go_through_the_network_server_s_sockets() {
    let (slots, server) = serve(LOOPBACK, &[]);
    let large = answer("200 OK", &vec![b'x'; 300_000]);
    let posted = vec![b'y'; 150_000];
    let post = Request::post("127.0.0.1", "/machine?comp=health", &[], &posted);
    let script = vec![
        (
            instance::OPENSTACK.encode().len(),
            recorded::openstack_meta_data(),
        ),
        (instance::AZURE.encode().len(), large.clone()),
        (post.encode().len(), answer("200 OK", b"taken")),
        // A connection that sends nothing and is dropped.
        (1, Vec::new()),
        // A server that closes without a word.
        (instance::OPENSTACK.encode().len(), Vec::new()),
    ];
    let cloud = cloud(&slots["listen-cloud"], 8080, script);
    let mut wan = Wan::new(client(&slots["listen-platform"])).unwrap();
    let local = Ipv4Addr::LOCALHOST;
    let soon = || deadline_in(5 * SECOND);

    // A recorded answer, through real TCP.
    let mut socket = wan.connect(local, 8080, soon()).unwrap();
    let response = http::exchange(&mut socket, &instance::OPENSTACK, soon()).unwrap();
    drop(socket);
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        recorded::openstack_meta_data()
            .rsplit(|b| *b == b'\n')
            .next()
            .unwrap()
    );
    // An answer of many receives.
    let mut socket = wan.connect(local, 8080, soon()).unwrap();
    let response = http::exchange(&mut socket, &instance::AZURE, soon()).unwrap();
    drop(socket);
    assert_eq!(response.body.len(), 300_000);
    assert!(response.body.iter().all(|byte| *byte == b'x'));
    // A request of many sends.
    let mut socket = wan.connect(local, 8080, soon()).unwrap();
    let response = http::exchange(&mut socket, &post, soon()).unwrap();
    drop(socket);
    assert_eq!(response.body, b"taken");

    // A read with nothing to read ends at its deadline, and one whose deadline has passed is
    // not made.
    let mut socket = wan.connect(local, 8080, soon()).unwrap();
    let mut buffer = [0; 64];
    let began = now();
    assert_eq!(
        socket.read(&mut buffer, deadline_in(50 * MS)),
        Err(StreamError::Deadline)
    );
    assert!(now() - began >= 50 * MS);
    assert_eq!(socket.read(&mut buffer, now()), Err(StreamError::Deadline));
    assert_eq!(socket.write(b"x", 1), Err(StreamError::Deadline));
    drop(socket);

    // A server that closes before it answers: the stream ends, and the client says so.
    let mut socket = wan.connect(local, 8080, soon()).unwrap();
    assert_eq!(
        http::exchange(&mut socket, &instance::OPENSTACK, soon()),
        Err(http::HttpError::Closed)
    );
    drop(socket);

    // Nothing listens on 8081: refused. Port 9 is outside the grant: the server says so.
    assert_eq!(
        wan.connect(local, 8081, soon()).err(),
        Some(StreamError::Refused)
    );
    let Some(StreamError::Other(why)) = wan.connect(local, 9, soon()).err() else {
        panic!("a connection outside the grant was not refused as that");
    };
    assert_eq!(why, "the network server answered Remote(NotPermitted)");
    // A connection whose deadline has passed is not attempted.
    assert_eq!(
        wan.connect(local, 8080, now()).err(),
        Some(StreamError::Deadline)
    );
    // This server has no interface `wan`.
    assert_eq!(
        wan.address(soon()),
        Err(StreamError::Other(
            "the interface wan has no IPv4 address".to_string()
        ))
    );

    let requests = cloud.join().unwrap();
    assert_eq!(requests[0], instance::OPENSTACK.encode());
    assert_eq!(requests[1], instance::AZURE.encode());
    assert_eq!(requests[2], post.encode());
    assert!(requests[3].is_empty());
    drop(wan);
    drop(slots);
    server.join().unwrap();
}

/// The cloud profile's `network.conf` and `node.conf` as committed (items 1.1, 1.3, 1.3b, 1.4,
/// 1.9c and 2.2c), read from the files themselves so that no copy here can go stale.
const NETWORK_CONF: &str = include_str!("../../../cloud/network.conf");
const NODE_CONF: &str = include_str!("../../../cloud/node.conf");
/// `platform`'s grant, with the local rule a connection needs to leave at all (PM, 2026-11-18).
const PLATFORM_GRANT: &str = "grant listen-platform rights=connect \
     remote=169.254.169.254/32@80,168.63.129.16/32@80 local=0.0.0.0/0 sockets=4\n";

#[test]
fn the_profile_lines_this_item_adds_are_read_by_their_parsers() {
    // node.conf: on a machine that is not Hyper-V, which runs no `vmbus` and none of its clients
    // (`storvsc`, `netvsc`), the supervisor starts `platform` after `network` and `api`, with a
    // 16 MiB account and a network listener, and then step 3's services (item 3.2),
    // node-admission taking the rest of a 2 GiB node.
    let node = moruna_node::Config::parse(NODE_CONF.as_bytes(), 2 << 30).expect("node.conf");
    let names: Vec<&str> = node.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "devices",
            "vmbus",
            "storvsc",
            "netvsc",
            "network",
            "api",
            "platform",
            "imagefs",
            "linux",
            "node-admission"
        ]
    );
    let node = node.on_machine(false);
    let names: Vec<&str> = node.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "devices",
            "network",
            "api",
            "platform",
            "imagefs",
            "linux",
            "node-admission"
        ]
    );
    let platform = node.services.iter().find(|s| s.name == "platform").unwrap();
    assert_eq!(
        (platform.image.as_str(), platform.account_bytes),
        ("image.platform", 16 << 20)
    );
    let link = |left: &str, left_slot: &str, right: &str, right_slot: &str| {
        node.links.iter().any(|l| {
            (
                l.left.as_str(),
                l.left_slot.as_str(),
                l.right.as_str(),
                l.right_slot.as_str(),
            ) == (left, left_slot, right, right_slot)
        })
    };
    assert!(link("network", "listen-platform", "platform", "network"));
    // No link reaches api: every start of it, the first included, gets its listener, its
    // channel to platform (item 2.2c) and its connection to node-admission (item 3.2) afresh
    // from the supervisor.
    assert!(
        node.links
            .iter()
            .all(|l| l.left != "api" && l.right != "api")
    );
    let restart = node.restart.as_ref().expect("api's restart record");
    assert_eq!(
        (
            restart.service.as_str(),
            restart.slot.as_str(),
            restart.server.as_str(),
            restart.granting.as_str()
        ),
        ("api", "network", "network", "listen-supervisor")
    );
    assert_eq!(restart.connect.as_deref(), Some("admission"));
    assert!(link("node-admission", "listen", "supervisor", "admission"));
    // Its bootstrap: the supervisor's message with the node's memory, on its `supervisor` slot.
    assert!(
        node.bootstraps
            .iter()
            .any(|(kind, service)| *kind == Bootstrap::Platform && service == "platform")
    );

    // network.conf: the supervisor's grant covers the listener the record mints, and no more
    // than listening on 443 and granting that, with item 1.3b's bounds for api's listener.
    let config = parse(NETWORK_CONF).expect("network.conf");
    let (_, supervisor) = config
        .grants
        .iter()
        .find(|(slot, _)| slot == "listen-supervisor")
        .expect("the supervisor's grant");
    assert_eq!(supervisor.rights, rights::GRANT | rights::LISTEN);
    assert!(supervisor.covers(&restart.policy));
    assert!(config.grants.iter().all(|(slot, _)| slot != "listen-api"));
    for policy in [supervisor, &restart.policy] {
        assert_eq!(
            (
                policy.sockets,
                policy.rx,
                policy.tx,
                policy.per_peer,
                policy.linger_ms
            ),
            (81, Some(65536), Some(65536), Some(8), Some(5000))
        );
    }

    // platform's grant is taken as written, with connect as its only right, to the two
    // addresses on port 80 and nowhere else, from any of the node's addresses, and sixteen
    // sockets.
    let (_, policy) = config
        .grants
        .iter()
        .find(|(slot, _)| slot == "listen-platform")
        .expect("platform's grant");
    assert_eq!(policy.rights, rights::CONNECT);
    assert_eq!(policy.sockets, 16);
    let to = |a, b, c, d, port| Endpoint::new(Addr::v4(a, b, c, d), port);
    assert!(policy.may_connect(&to(169, 254, 169, 254, 80)));
    assert!(policy.may_connect(&to(168, 63, 129, 16, 80)));
    assert!(!policy.may_connect(&to(169, 254, 169, 254, 443)));
    assert!(!policy.may_connect(&to(169, 254, 169, 253, 80)));
    assert!(!policy.may_connect(&to(10, 0, 2, 2, 80)));
    assert!(!policy.may_listen(&to(0, 0, 0, 0, 443)));
    assert!(!policy.may_listen(&to(10, 0, 2, 15, 443)));
    assert!(!policy.may_bind(&to(10, 0, 2, 15, 68)));
    assert!(policy.may_use_source(&Addr::v4(10, 0, 2, 15)));
    assert!(policy.names.is_empty());
}

/// A node with one card and a fixed address, as DHCP would give it under QEMU.
fn node(grant: &str) -> String {
    format!(
        "moruna-netstack 1
setting connect-timeout-ms 300
interface wan card=net0 ipv6=2001:db8::7/64 ipv4=10.0.2.15/24 gateway4=10.0.2.2
{grant}"
    )
}

const CARD: Card = Card {
    mac: 0x5254_004b_4502,
    mtu: 1500,
    up: true,
};

#[test]
fn the_node_s_address_is_its_wan_interface_s_ipv4_address() {
    let (slots, server) = serve(&node(PLATFORM_GRANT), &[CARD]);
    let mut wan = Wan::new(client(&slots["listen-platform"])).unwrap();
    assert_eq!(
        wan.address(deadline_in(5 * SECOND)),
        Ok(Ipv4Addr::new(10, 0, 2, 15))
    );
    assert_eq!(wan.address(now()), Err(StreamError::Deadline));
    // A call with no deadline waits for its answer.
    assert_eq!(
        wan.address(DEADLINE_INFINITE),
        Ok(Ipv4Addr::new(10, 0, 2, 15))
    );
    drop(wan);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn a_node_that_dhcp_has_not_yet_answered_has_no_route_and_no_address() {
    // The cloud profile's interface, on a card where no DHCP server answers.
    let (slots, server) = serve(NETWORK_CONF, &[CARD]);
    let mut wan = Wan::new(client(&slots["listen-platform"])).unwrap();
    let soon = deadline_in(5 * SECOND);
    assert_eq!(
        wan.connect(instance::ADDRESS, instance::PORT, soon).err(),
        Some(StreamError::Unreachable)
    );
    assert_eq!(
        wan.address(soon),
        Err(StreamError::Other(
            "the interface wan has no IPv4 address".to_string()
        ))
    );
    drop(wan);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn platform_s_grant_reaches_instance_data_and_only_with_its_local_rule() {
    let instance_data = |grant: &str| {
        let (slots, server) = serve(&node(grant), &[CARD]);
        let mut wan = Wan::new(client(&slots["listen-platform"])).unwrap();
        let met = wan
            .connect(instance::ADDRESS, instance::PORT, deadline_in(5 * SECOND))
            .err();
        drop(wan);
        drop(slots);
        server.join().unwrap();
        met
    };
    // The grant is routed to the gateway. Nothing answers on this card, so the network server
    // gives up at its connect timeout: the connection was made, not refused.
    assert_eq!(instance_data(PLATFORM_GRANT), Some(StreamError::TimedOut));
    // Without its local rule, as the work item first wrote it, the grant names no address a
    // connection may leave from, and the network server refuses every connection for that.
    let without_local = PLATFORM_GRANT.replace(" local=0.0.0.0/0", "");
    assert_eq!(
        instance_data(&without_local),
        Some(StreamError::Other(
            "the network server answered Remote(NotPermitted)".to_string()
        ))
    );
    // And the local rule opens nothing else: the policy still cannot listen.
    let config = parse(&node(PLATFORM_GRANT)).unwrap();
    let policy = &config.grants[0].1;
    assert_eq!(policy.rights, rights::CONNECT);
    assert!(!policy.may_listen(&Endpoint::new(Addr::v4(10, 0, 2, 15), 443)));
}

#[test]
fn the_clock_is_the_kernel_s_and_its_sleeps_end_on_time() {
    let mut clock = Monotonic::new().unwrap();
    let before = clock.now();
    assert!(before <= now());
    clock.sleep_until(before + 30 * MS);
    let after = clock.now();
    assert!(after >= before + 30 * MS);
    // A time that has passed is no sleep at all.
    clock.sleep_until(before);
    clock.sleep_until(0);
    assert!(clock.now() - after < SECOND);
}

#[test]
fn the_link_carries_whole_messages() {
    let (ours, theirs) = Channel::create().unwrap();
    let mut link = ChannelLink(ours);
    let soon = || deadline_in(5 * SECOND);

    // A hand-over out, and `ready` back.
    link.send(b"the hand-over", soon()).unwrap();
    let mut bytes = [0; 64];
    let (count, handles) = theirs.read(&mut bytes, &mut []).unwrap();
    assert_eq!((&bytes[..count], handles), (&b"the hand-over"[..], 0));
    theirs.write_with(READY, &mut []).unwrap();
    assert_eq!(link.receive(soon()).unwrap(), READY);

    // Nothing to read ends at the deadline; a message that comes while waiting is taken.
    assert_eq!(link.receive(deadline_in(20 * MS)), Err(LinkError::TimedOut));
    assert_eq!(LinkError::TimedOut.to_string(), "the deadline passed");
    let late = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        theirs.write_with(b"late", &mut []).unwrap();
        theirs
    });
    assert_eq!(link.receive(soon()).unwrap(), b"late");
    let theirs = late.join().unwrap();

    // An empty message is a message. One that carries a handle is not the hand-over's.
    theirs.write_with(b"", &mut []).unwrap();
    assert_eq!(link.receive(soon()).unwrap(), b"");
    let (extra, _kept) = Channel::create().unwrap();
    theirs.write_with(READY, &mut [Some(extra.0)]).unwrap();
    assert_eq!(
        link.receive(soon()),
        Err(LinkError::Other(
            "a message on the link carries handles".to_string()
        ))
    );

    // The largest hand-over is one channel message; a byte more is refused by the kernel.
    assert_eq!(MESSAGE_MAX, limits::CHANNEL_MAX_BYTES);
    link.send(&vec![b' '; MESSAGE_MAX], soon()).unwrap();
    let mut large = vec![0; MESSAGE_MAX];
    assert_eq!(theirs.read(&mut large, &mut []).unwrap(), (MESSAGE_MAX, 0));
    assert_eq!(
        link.send(&vec![b' '; MESSAGE_MAX + 1], soon()),
        Err(LinkError::Other(
            "the kernel refused: limit reached".to_string()
        ))
    );

    // A full queue is waited on until the deadline, and until the reader makes room.
    for _ in 0..limits::CHANNEL_DEPTH {
        link.send(b"queued", soon()).unwrap();
    }
    assert_eq!(
        link.send(b"one more", deadline_in(20 * MS)),
        Err(LinkError::TimedOut)
    );
    let reader = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        let mut bytes = [0; 64];
        theirs.read(&mut bytes, &mut []).unwrap();
        theirs
    });
    link.send(b"one more", soon()).unwrap();
    let theirs = reader.join().unwrap();

    // Once api has gone: what it had not read is lost, and the link says it is closed, to a
    // sender, to a reader, and to a reader already waiting.
    drop(theirs);
    assert_eq!(link.send(b"anyone?", soon()), Err(LinkError::Closed));
    assert_eq!(link.receive(soon()), Err(LinkError::Closed));
    let (ours, theirs) = Channel::create().unwrap();
    let mut link = ChannelLink(ours);
    let leaving = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        drop(theirs);
    });
    assert_eq!(link.receive(soon()), Err(LinkError::Closed));
    leaving.join().unwrap();
    // A message api wrote before it went is still read.
    let (ours, theirs) = Channel::create().unwrap();
    let mut link = ChannelLink(ours);
    theirs.write_with(READY, &mut []).unwrap();
    drop(theirs);
    assert_eq!(link.receive(soon()).unwrap(), READY);
    assert_eq!(link.receive(soon()), Err(LinkError::Closed));
}

#[test]
fn the_node_s_memory_is_the_supervisor_s_first_message() {
    // As the supervisor does it: the message queued before `platform` runs, its end kept.
    let queued = |message: &[u8], handles: &mut [Option<moruna_svc::Handle>]| {
        let (supervisor, platform) = Channel::create().unwrap();
        supervisor.write_with(message, handles).unwrap();
        (supervisor, ChannelControl(platform))
    };
    let memory = 24_526_286_848;
    let message = Bootstrap::platform_message(memory);
    let (_kept, mut control) = queued(&message, &mut []);
    assert_eq!(control.memory(now()), Ok(memory));
    // Anything but that bootstrap is refused, and nothing queued is not waited for past the
    // deadline.
    let not_it = Err(String::from(
        "the supervisor's message is not platform's bootstrap",
    ));
    let (extra, _kept) = Channel::create().unwrap();
    for (bytes, handles) in [
        (Bootstrap::Linux.header().to_vec(), vec![]),
        (Bootstrap::platform_message(0).to_vec(), vec![]),
        (message[..12].to_vec(), vec![]),
        (API_STARTED.to_vec(), vec![]),
        (message.to_vec(), vec![Some(extra.0)]),
    ] {
        let mut handles = handles;
        let (_kept, mut control) = queued(&bytes, &mut handles);
        assert_eq!(control.memory(now()), not_it, "{bytes:?}");
    }
    let (_supervisor, platform) = Channel::create().unwrap();
    assert_eq!(
        ChannelControl(platform).memory(deadline_in(20 * MS)),
        Err(String::from(
            "the supervisor's message: the deadline passed"
        ))
    );
    let (supervisor, platform) = Channel::create().unwrap();
    drop(supervisor);
    assert_eq!(
        ChannelControl(platform).memory(now()),
        Err(String::from("the supervisor closed its end and sent none"))
    );
}

#[test]
fn each_start_of_api_comes_as_api_started_and_its_channel() {
    let (supervisor, platform) = Channel::create().unwrap();
    let mut control = ChannelControl(platform);
    let soon = || deadline_in(5 * SECOND);
    // The supervisor's message at a start: `api-started`, and the other end of api's channel.
    let start = |supervisor: &Channel| {
        let (api, ours) = Channel::create().unwrap();
        supervisor
            .write_with(API_STARTED, &mut [Some(ours.0)])
            .unwrap();
        api
    };
    for n in 0..3 {
        let api = start(&supervisor);
        let mut link = control.next_api(soon()).unwrap();
        link.send(b"the hand-over", soon()).unwrap();
        let mut bytes = [0; 64];
        let (count, _) = api.read(&mut bytes, &mut []).unwrap();
        assert_eq!(&bytes[..count], b"the hand-over", "start {n}");
        api.write_with(READY, &mut []).unwrap();
        assert_eq!(link.receive(soon()).unwrap(), READY);
    }
    // A start that has not come is waited for until the deadline, and one that comes while
    // waiting is taken.
    assert_eq!(
        control.next_api(deadline_in(20 * MS)).map(drop),
        Err(LinkError::TimedOut)
    );
    let late = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        let api = start(&supervisor);
        (supervisor, api)
    });
    assert!(control.next_api(soon()).is_ok());
    let (supervisor, _api) = late.join().unwrap();

    // Anything else is refused: another message, no channel, two handles, or a handle that is
    // not a channel.
    let other = Err(LinkError::Other(String::from(
        "the supervisor sent something other than api-started",
    )));
    let (spare, _kept) = Channel::create().unwrap();
    let (more, _kept2) = Channel::create().unwrap();
    supervisor
        .write_with(b"api-restarted", &mut [Some(spare.0)])
        .unwrap();
    assert_eq!(control.next_api(soon()).map(drop), other);
    supervisor.write_with(API_STARTED, &mut []).unwrap();
    assert_eq!(control.next_api(soon()).map(drop), other);
    let (one, _k1) = Channel::create().unwrap();
    supervisor
        .write_with(API_STARTED, &mut [Some(one.0), Some(more.0)])
        .unwrap();
    assert_eq!(control.next_api(soon()).map(drop), other);
    let memory = Memory::create(4096).unwrap();
    supervisor
        .write_with(API_STARTED, &mut [Some(memory.0)])
        .unwrap();
    assert_eq!(
        control.next_api(soon()).map(drop),
        Err(LinkError::Other(String::from(
            "api-started carries something other than a channel"
        )))
    );
    // The supervisor gone, the channel says so.
    drop(supervisor);
    assert_eq!(control.next_api(soon()).map(drop), Err(LinkError::Closed));
}

#[test]
fn the_hand_over_crosses_a_channel_to_each_start_of_api() {
    // `api`'s half, as its program does it: read one message, decode it, answer `ready`.
    let api = |theirs: Channel| {
        thread::spawn(move || {
            let mut link = ChannelLink(theirs);
            let message = link.receive(deadline_in(5 * SECOND)).unwrap();
            let handover = Handover::decode(&message).unwrap();
            link.send(READY, deadline_in(5 * SECOND)).unwrap();
            handover
        })
    };
    // The supervisor's side, as it does it: the node's memory, then each start's channel.
    let (supervisor, slot) = Channel::create().unwrap();
    supervisor
        .write_with(&Bootstrap::platform_message(24_526_286_848), &mut [])
        .unwrap();
    let start = |supervisor: &Channel| {
        let (theirs, ours) = Channel::create().unwrap();
        supervisor
            .write_with(API_STARTED, &mut [Some(ours.0)])
            .unwrap();
        api(theirs)
    };
    let first = start(&supervisor);
    let mut control = ChannelControl(slot);
    let memory_bytes = control.memory(now()).unwrap();
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| openstack(sent));
    let mut lines = Vec::new();
    let mut wall = super::FakeWall::default();
    let handed = boot::run(
        &mut network,
        &mut clock,
        &mut wall,
        &mut control,
        &mut |line: &str| lines.push(line.to_string()),
        Inputs {
            started_ns: 0,
            memory_bytes,
        },
    )
    .unwrap();
    // `api` has the node's memory the supervisor gave, and the config judged on it; the node
    // was not yet ready, so no boot_ms.
    let taken = first.join().unwrap();
    assert_eq!(
        taken,
        Handover {
            boot_ms: None,
            ..handed.clone()
        }
    );
    assert_eq!(taken.memory_bytes, 24_526_286_848);
    assert_eq!(taken.config.functions.cache_bytes, 1 << 30);
    assert_eq!(lines.len(), 2);
    let boot_ms = handed.boot_ms.expect("the node's boot_ms");
    assert_eq!(
        lines[1],
        format!("moruna: ready api=10.0.2.15:443 boot_ms={boot_ms}")
    );

    // Two restarts: each start is handed the same with the ready line's boot_ms, and the
    // supervisor's end then closes.
    let starts = [start(&supervisor), start(&supervisor)];
    drop(supervisor);
    let fatal = boot::serve(
        &mut network,
        &mut Monotonic::new().unwrap(),
        &mut wall,
        &mut control,
        &handed,
        &mut |line: &str| panic!("serve said {line}"),
    );
    for started in starts {
        assert_eq!(started.join().unwrap(), handed);
    }
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: the supervisor's channel: the supervisor closed its channel"
    );
}
