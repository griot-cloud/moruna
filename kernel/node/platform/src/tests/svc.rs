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
use moruna_node::Bootstrap;
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
    machine::{Clock, Link, LinkError, Network, Stream, StreamError},
    svc::{ChannelLink, Monotonic, Wan, node_memory},
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

const LOOPBACK: &str = "moruna-netstack 1
grant listen-cloud rights=listen local=127.0.0.1/32@8080 sockets=4
grant listen-platform rights=connect remote=127.0.0.1/32@8080-8081 local=127.0.0.1/32 sockets=4
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

/// The cloud profile's `network.conf` as item 1.3 writes it, and the line item 1.4 adds.
const NETWORK_CONF: &str = "moruna-netstack 1
interface wan card=net0 dhcp
grant listen-api rights=listen local=0.0.0.0/0@443 sockets=64
";
/// `platform`'s grant, with the local rule a connection needs to leave at all (PM, 2026-11-18).
const PLATFORM_GRANT: &str = "grant listen-platform rights=connect \
     remote=169.254.169.254/32@80,168.63.129.16/32@80 local=0.0.0.0/0 sockets=4\n";

/// The cloud profile's `node.conf` as items 1.1 and 1.3 write it, and the lines item 1.4 adds.
const NODE_CONF: &str = "moruna-node 1
service devices image.devices 67108864
bootstrap devices devices
grant devices drivers drivers
grant devices driver.virtio-blk driver.virtio-blk
grant devices driver.virtio-net driver.virtio-net
grant devices driver.nvme driver.nvme
grant devices driver.gve driver.gve
service network image.netstack 67108864
service api image.api 67108864
grant network config network.config
link devices directory-net network directory
link network listen-api api network
";
const PLATFORM_LINES: &str = "service platform image.platform 16777216
bootstrap platform platform
link network listen-platform platform network
link platform api api platform
";

#[test]
fn the_profile_lines_this_item_adds_are_read_by_their_parsers() {
    // node.conf: `platform` is the last service, so the supervisor starts it after `network` and
    // `api`, with a 16 MiB account, a network listener and its link to `api`.
    let text = format!("{NODE_CONF}{PLATFORM_LINES}");
    let node = moruna_node::Config::parse(text.as_bytes(), 1 << 30).expect("node.conf");
    let names: Vec<&str> = node.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["devices", "network", "api", "platform"]);
    let platform = node.services.last().unwrap();
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
    assert!(link("platform", "api", "api", "platform"));
    // Its bootstrap: the supervisor's message with the node's memory, on its `supervisor` slot.
    assert!(
        node.bootstraps
            .iter()
            .any(|(kind, service)| *kind == Bootstrap::Platform && service == "platform")
    );

    // network.conf: the grant is taken as written, with connect as its only right, to the two
    // addresses on port 80 and nowhere else, from any of the node's addresses, and four
    // sockets.
    let config = parse(&format!("{NETWORK_CONF}{PLATFORM_GRANT}")).expect("network.conf");
    let (_, policy) = config
        .grants
        .iter()
        .find(|(slot, _)| slot == "listen-platform")
        .expect("platform's grant");
    assert_eq!(policy.rights, rights::CONNECT);
    assert_eq!(policy.sockets, 4);
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
    let (slots, server) = serve(&format!("{NETWORK_CONF}{PLATFORM_GRANT}"), &[CARD]);
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
fn the_node_s_memory_is_the_supervisor_s_bootstrap_message() {
    // As the supervisor does it: the message queued, then its end closed, before `platform` runs.
    let queued = |message: &[u8], handles: &mut [Option<moruna_svc::Handle>]| {
        let (supervisor, platform) = Channel::create().unwrap();
        supervisor.write_with(message, handles).unwrap();
        drop(supervisor);
        platform
    };
    let memory = 24_526_286_848;
    let message = Bootstrap::platform_message(memory);
    assert_eq!(node_memory(queued(&message, &mut []), now()), Ok(memory));
    // Anything but that bootstrap is refused, and nothing queued is not waited for past the
    // deadline.
    let not_it = Err(String::from(
        "the supervisor's message is not platform's bootstrap",
    ));
    assert_eq!(
        node_memory(queued(Bootstrap::Linux.header(), &mut []), now()),
        not_it
    );
    assert_eq!(
        node_memory(queued(&Bootstrap::platform_message(0), &mut []), now()),
        not_it
    );
    assert_eq!(node_memory(queued(&message[..12], &mut []), now()), not_it);
    let (extra, _kept) = Channel::create().unwrap();
    assert_eq!(
        node_memory(queued(&message, &mut [Some(extra.0)]), now()),
        Err(String::from(
            "the supervisor's message: a message on the link carries handles"
        ))
    );
    let (_supervisor, platform) = Channel::create().unwrap();
    assert_eq!(
        node_memory(platform, deadline_in(20 * MS)),
        Err(String::from(
            "the supervisor's message: the deadline passed"
        ))
    );
    let (supervisor, platform) = Channel::create().unwrap();
    drop(supervisor);
    assert_eq!(
        node_memory(platform, now()),
        Err(String::from("the supervisor closed its end and sent none"))
    );
}

#[test]
fn the_hand_over_crosses_a_channel_to_api_s_half() {
    let (ours, theirs) = Channel::create().unwrap();
    // `api`'s half, as its program will do it: read one message, decode it, answer `ready`.
    let api = thread::spawn(move || {
        let mut link = ChannelLink(theirs);
        let message = link.receive(deadline_in(5 * SECOND)).unwrap();
        let handover = Handover::decode(&message).unwrap();
        link.send(READY, deadline_in(5 * SECOND)).unwrap();
        handover
    });
    // The node's memory, from the supervisor's message, as the program reads it.
    let (supervisor, slot) = Channel::create().unwrap();
    supervisor
        .write_with(&Bootstrap::platform_message(24_526_286_848), &mut [])
        .unwrap();
    drop(supervisor);
    let memory_bytes = node_memory(slot, now()).unwrap();
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| openstack(sent));
    let mut lines = Vec::new();
    let handed = boot::run(
        &mut network,
        &mut clock,
        &mut ChannelLink(ours),
        &mut |line: &str| lines.push(line.to_string()),
        Inputs {
            started_ns: 0,
            memory_bytes,
        },
    )
    .unwrap();
    // `api` has the node's memory the supervisor gave, and the config judged on it.
    let taken = api.join().unwrap();
    assert_eq!(taken, handed);
    assert_eq!(taken.memory_bytes, 24_526_286_848);
    assert_eq!(taken.config.functions.cache_bytes, 1 << 30);
    assert_eq!(lines.len(), 2);
    assert!(lines[1].starts_with("moruna: ready api=10.0.2.15:443 boot_ms="));
}
