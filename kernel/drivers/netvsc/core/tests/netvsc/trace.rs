//! Work item 1.11c, with the feature `trace-first-frames` only: the frames of the boot's
//! network path (DHCP, ARP, TCP's SYN and RST) each way, their commits and completions, and
//! the host's indications, each said with its time on the clock the card was given; what the
//! trace says a frame is; the media connect status asked before and after the packet
//! filter; and the NDIS capabilities a diagnosis build states.

use std::{cell::Cell, rc::Rc};

use moruna_netvsc::{
    NetvscError,
    trace::{FIRST, Trace, describe, traced},
};
use netvsp_protocol::{protocol::Status, rndisprot};

use crate::fake::{Card, Got, Host, MAC, PAGE, ROOMY, Settings, assert_clean, drain, parts};

const BROADCAST: [u8; 6] = [0xff; 6];
const GATEWAY_MAC: [u8; 6] = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
const ANY: [u8; 4] = [0, 0, 0, 0];
const ALL: [u8; 4] = [255, 255, 255, 255];
const NODE: [u8; 4] = [10, 0, 0, 4];
const GATEWAY: [u8; 4] = [10, 0, 0, 1];
const INSTANCE: [u8; 4] = [169, 254, 169, 254];

/// The clock a traced card's trace reads, which the test sets.
type Clock = Rc<Cell<u64>>;
/// The fake host, shared with the card.
type Shared = Rc<std::cell::RefCell<Host>>;

/// A card started against a host `prepare` has set up, traced on a clock the test sets, and
/// the lines its start said.
fn traced_card(
    settings: Settings,
    prepare: impl FnOnce(&mut Host),
) -> (Clock, Card, Shared, Vec<String>) {
    let clock = Rc::new(Cell::new(0));
    let time = clock.clone();
    let parts = parts(16 * PAGE, 16 * PAGE, settings);
    let host = parts.host.clone();
    prepare(&mut host.borrow_mut());
    let (card, said) = parts
        .start_traced(Trace::new(Box::new(move || time.get())))
        .unwrap_or_else(|e| panic!("the card did not start: {e}"));
    (clock, card, host, said)
}

fn started(prepare: impl FnOnce(&mut Host)) -> (Clock, Card, Shared) {
    let (clock, card, host, _) = traced_card(ROOMY, prepare);
    (clock, card, host)
}

/// An Ethernet frame from the card's address.
fn ethernet(to: [u8; 6], kind: u16, payload: &[u8]) -> Vec<u8> {
    [&to[..], &MAC[..], &kind.to_be_bytes(), payload].concat()
}

/// An IPv4 packet of `protocol` from `src` to `dst`, with a header of 20 bytes.
fn ipv4(src: [u8; 4], dst: [u8; 4], protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut header = [0u8; 20];
    header[0] = 0x45;
    header[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    header[8] = 64;
    header[9] = protocol;
    header[12..16].copy_from_slice(&src);
    header[16..20].copy_from_slice(&dst);
    [&header[..], payload].concat()
}

/// A UDP datagram from port `from` to port `to`.
fn udp(from: u16, to: u16, payload: &[u8]) -> Vec<u8> {
    let len = (8 + payload.len()) as u16;
    [
        &from.to_be_bytes()[..],
        &to.to_be_bytes(),
        &len.to_be_bytes(),
        &[0, 0],
        payload,
    ]
    .concat()
}

/// A TCP segment's header from port `from` to port `to` with `flags`.
fn tcp(from: u16, to: u16, flags: u8) -> Vec<u8> {
    let mut header = [0u8; 20];
    header[..2].copy_from_slice(&from.to_be_bytes());
    header[2..4].copy_from_slice(&to.to_be_bytes());
    header[12] = 5 << 4;
    header[13] = flags;
    header.to_vec()
}

/// A TCP frame from the node to the instance-data address.
fn tcp_frame(flags: u8) -> Vec<u8> {
    ethernet(
        GATEWAY_MAC,
        0x0800,
        &ipv4(NODE, INSTANCE, 6, &tcp(49153, 80, flags)),
    )
}

/// An ARP packet of IPv4 over Ethernet: `op` from `sender` at `sender_mac`, about `target`.
fn arp(op: u16, sender_mac: [u8; 6], sender: [u8; 4], target: [u8; 4]) -> Vec<u8> {
    [
        &[0, 1, 8, 0, 6, 4][..],
        &op.to_be_bytes(),
        &sender_mac,
        &sender,
        &[0; 6],
        &target,
    ]
    .concat()
}

/// A DHCP message: the fixed fields (op, transaction ID), the magic cookie, and `options`.
fn dhcp(op: u8, xid: u32, options: &[u8]) -> Vec<u8> {
    let mut message = vec![0u8; 236];
    message[0] = op;
    message[1] = 1;
    message[2] = 6;
    message[4..8].copy_from_slice(&xid.to_be_bytes());
    message.extend_from_slice(&[99, 130, 83, 99]);
    message.extend_from_slice(options);
    message
}

/// The frame of a DHCP message of `kind` (option 53): from the client (port 68) to every
/// address, or from the server (port 67) to the card.
fn dhcp_frame(from_client: bool, kind: u8, xid: u32) -> Vec<u8> {
    let message = dhcp(if from_client { 1 } else { 2 }, xid, &[53, 1, kind, 255]);
    if from_client {
        ethernet(
            BROADCAST,
            0x0800,
            &ipv4(ANY, ALL, 17, &udp(68, 67, &message)),
        )
    } else {
        ethernet(
            MAC,
            0x0800,
            &ipv4(GATEWAY, NODE, 17, &udp(67, 68, &message)),
        )
    }
}

#[test]
fn a_dhcp_exchange_is_traced_on_the_cards_clock() {
    let (clock, mut card, host) = started(|_| ());
    // The start sent no frame and the host indicated nothing.
    assert!(card.take_trace().is_empty());
    let discover = dhcp_frame(true, 1, 0x3d1d_5e4f);
    let offer = dhcp_frame(false, 2, 0x3d1d_5e4f);
    clock.set(231_456_789);
    assert!(card.send(&discover).unwrap());
    clock.set(231_502_000);
    assert_eq!(drain(&mut card), []);
    host.borrow_mut().inject(&offer);
    clock.set(4_233_000_001);
    assert_eq!(drain(&mut card), [Got::Frame(offer.clone())]);
    assert_eq!(
        card.take_trace(),
        [
            "231.456 ms: trace: out 1: 286 bytes in section 0, to ff:ff:ff:ff:ff:ff, IPv4 0.0.0.0>255.255.255.255 UDP 68>67, DHCP DISCOVER xid 0x3d1d5e4f",
            "231.456 ms: trace: out 1 committed; the host signalled",
            "231.502 ms: trace: out 1 completed: status 1 (success)",
            "4233.000 ms: trace: in 1: 286 bytes, to 00:15:5d:01:02:03, IPv4 10.0.0.1>10.0.0.4 UDP 67>68, DHCP OFFER xid 0x3d1d5e4f",
        ]
    );
    // Taken once.
    assert!(card.take_trace().is_empty());
    assert_eq!(host.borrow().frames, [discover]);
    assert_clean(&host);
}

#[test]
fn only_the_frames_of_the_boot_s_network_path_are_traced() {
    let (_, mut card, host) = started(|_| ());
    let quiet = [
        ethernet(BROADCAST, 0x88b5, &[0; 46]),
        ethernet(BROADCAST, 0x86dd, &[0; 46]),
        ethernet(
            GATEWAY_MAC,
            0x0800,
            &ipv4(NODE, GATEWAY, 17, &udp(49152, 53, &[0; 12])),
        ),
        ethernet(GATEWAY_MAC, 0x0800, &ipv4(NODE, GATEWAY, 1, &[0; 26])),
        tcp_frame(0x10),
        tcp_frame(0x18),
        ethernet(GATEWAY_MAC, 0x0800, &[0x45; 9]),
    ];
    for frame in &quiet {
        assert!(!traced(frame), "{}", describe(frame));
        assert!(card.send(frame).unwrap());
        host.borrow_mut().inject(frame);
        assert_eq!(drain(&mut card), [Got::Frame(frame.clone())]);
    }
    assert!(card.take_trace().is_empty());
    // Numbered among the traced frames alone.
    let syn = tcp_frame(0x02);
    let syn_ack = tcp_frame(0x12);
    let rst = tcp_frame(0x14);
    assert!(card.send(&syn).unwrap());
    host.borrow_mut().inject(&syn_ack);
    host.borrow_mut().inject(&rst);
    assert_eq!(
        drain(&mut card),
        [Got::Frame(syn_ack.clone()), Got::Frame(rst.clone())]
    );
    let lines = card.take_trace();
    let ip = "IPv4 10.0.0.4>169.254.169.254 TCP 49153>80";
    assert_eq!(
        lines,
        [
            format!(
                "0.000 ms: trace: out 1: 54 bytes in section 0, to 12:34:56:78:9a:bc, {ip} [SYN]"
            ),
            String::from("0.000 ms: trace: out 1 committed; the host signalled"),
            String::from("0.000 ms: trace: out 1 completed: status 1 (success)"),
            format!("0.000 ms: trace: in 1: 54 bytes, to 12:34:56:78:9a:bc, {ip} [SYN ACK]"),
            format!("0.000 ms: trace: in 2: 54 bytes, to 12:34:56:78:9a:bc, {ip} [ACK RST]"),
        ]
    );
    assert_clean(&host);
}

#[test]
fn a_commit_says_which_frames_it_carries_and_whether_the_host_was_signalled() {
    let (clock, mut card, host) = started(|_| ());
    // A host that reads nothing: the ring to it stays as the card left it.
    host.borrow_mut().paused = true;
    clock.set(1_000_000);
    let frame = |n: u8| ethernet(BROADCAST, 0x0806, &arp(1, MAC, NODE, [10, 0, 0, n]));
    assert!(card.send(&frame(1)).unwrap());
    assert!(card.send(&frame(2)).unwrap());
    assert!(card.put(&frame(3)).unwrap());
    assert!(card.put(&frame(4)).unwrap());
    card.commit().unwrap();
    let out = |n: u8, section: u8| {
        format!(
            "1.000 ms: trace: out {n}: 42 bytes in section {section}, to ff:ff:ff:ff:ff:ff, ARP who has 10.0.0.{n} tell 10.0.0.4"
        )
    };
    let unsignalled = "no signal: the ring was not empty, or the host masked it";
    assert_eq!(
        card.take_trace(),
        [
            out(1, 0),
            String::from("1.000 ms: trace: out 1 committed; the host signalled"),
            out(2, 1),
            format!("1.000 ms: trace: out 2 committed; {unsignalled}"),
            out(3, 2),
            out(4, 3),
            format!("1.000 ms: trace: out 3, 4 committed; {unsignalled}"),
        ]
    );
    // The host reads them all at last and completes them, in order.
    clock.set(2_000_000);
    host.borrow_mut().paused = false;
    card.run_host();
    assert_eq!(drain(&mut card), []);
    assert_eq!(
        card.take_trace(),
        (1..=4)
            .map(|n| format!("2.000 ms: trace: out {n} completed: status 1 (success)"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_frame_the_host_could_not_send_is_traced_with_the_hosts_status() {
    let (clock, mut card, host) = started(|host| host.send_status = Status::FAILURE);
    clock.set(5_000);
    let discover = dhcp_frame(true, 1, 7);
    assert!(card.send(&discover).unwrap());
    match card.poll() {
        Err(NetvscError::NotSent(2)) => {}
        other => panic!("expected the host's failure, got {other:?}"),
    }
    assert_eq!(
        card.take_trace(),
        [
            "0.005 ms: trace: out 1: 286 bytes in section 0, to ff:ff:ff:ff:ff:ff, IPv4 0.0.0.0>255.255.255.255 UDP 68>67, DHCP DISCOVER xid 0x00000007",
            "0.005 ms: trace: out 1 committed; the host signalled",
            "0.005 ms: trace: out 1 completed: status 2 (failure)",
        ]
    );
    // Every other status the host may give, by its name where NVSP names it.
    let frame = ethernet(BROADCAST, 0x0806, &[0; 46]);
    for (n, status, name) in [
        (2, 5, "an invalid RNDIS packet"),
        (3, 6, "busy"),
        (4, 9, "a status NVSP does not name"),
    ] {
        host.borrow_mut().send_status = Status(status);
        assert!(card.send(&frame).unwrap());
        match card.poll() {
            Err(NetvscError::NotSent(s)) if s == status => {}
            other => panic!("expected status {status}, got {other:?}"),
        }
        let lines = card.take_trace();
        assert_eq!(
            lines.last().map(String::as_str),
            Some(format!("0.005 ms: trace: out {n} completed: status {status} ({name})").as_str())
        );
    }
    assert_clean(&host);
}

#[test]
fn every_indication_is_traced_with_what_it_did_to_the_link() {
    let (clock, mut card, host) = started(|_| ());
    clock.set(7_000_000);
    {
        let mut host = host.borrow_mut();
        // The card takes the link to be up from its start: a connect then changes nothing,
        // and the card's caller hears nothing of it.
        host.link(true);
        host.indicate(0x4001_0018);
        host.link(false);
        host.indicate(rndisprot::STATUS_LINK_SPEED_CHANGE);
        host.link(true);
        host.indicate(rndisprot::STATUS_MEDIA_SPECIFIC_INDICATION);
    }
    assert_eq!(drain(&mut card), [Got::Link(false), Got::Link(true)]);
    assert_eq!(
        card.take_trace(),
        [
            "7.000 ms: trace: indication 0x4001000b (media connect): the link stays up",
            "7.000 ms: trace: indication 0x40010018 (network change): the link stays up",
            "7.000 ms: trace: indication 0x4001000c (media disconnect): the link goes down",
            "7.000 ms: trace: indication 0x40010013 (link speed change): the link stays down",
            "7.000 ms: trace: indication 0x4001000b (media connect): the link goes up",
            "7.000 ms: trace: indication 0x40010012 (another status): the link stays up",
        ]
    );
    assert_clean(&host);
}

#[test]
fn an_indication_during_the_start_is_traced() {
    let (_, mut card, host) = started(|host| {
        // Said while the card waits for its address.
        host.mangle = crate::fake::on(
            crate::fake::Step::Rndis(rndisprot::MESSAGE_TYPE_QUERY_CMPLT),
            0,
            |host, _| {
                host.link(true);
                crate::fake::Fate::Send
            },
        );
    });
    assert_eq!(
        card.take_trace(),
        ["0.000 ms: trace: indication 0x4001000b (media connect): the link stays up"]
    );
    assert!(card.link_up());
    assert_clean(&host);
}

#[test]
fn only_the_first_frames_each_way_and_the_first_indications_are_traced() {
    let (_, mut card, host) = started(|_| ());
    let frame = ethernet(BROADCAST, 0x0806, &[0; 46]);
    let extra = FIRST as usize + 5;
    for _ in 0..extra {
        assert!(card.send(&frame).unwrap());
        assert_eq!(drain(&mut card), []);
        host.borrow_mut().inject(&frame);
        assert_eq!(drain(&mut card), [Got::Frame(frame.clone())]);
        host.borrow_mut().link(true);
        assert_eq!(drain(&mut card), []);
    }
    let lines = card.take_trace();
    let count = |what: &str| lines.iter().filter(|l| l.contains(what)).count();
    let first = FIRST as usize;
    assert_eq!(count("trace: out "), 3 * first);
    assert_eq!(count(" committed;"), first);
    assert_eq!(count(" completed: "), first);
    assert_eq!(count("trace: in "), first);
    assert_eq!(count("trace: indication "), first);
    let has = |what: String| lines.iter().any(|l| l.contains(&what));
    assert!(has(format!("trace: out {first}: 60 bytes")));
    assert!(!has(format!("trace: out {}", first + 1)));
    assert!(has(format!(
        "trace: in {first}: 60 bytes, to ff:ff:ff:ff:ff:ff, ARP operation 0"
    )));
    assert!(!has(format!("trace: in {}", first + 1)));
    assert_eq!(host.borrow().frames.len(), extra);
    assert_clean(&host);
}

#[test]
fn the_media_connect_status_is_asked_before_and_after_the_packet_filter() {
    let at = |said: &[String]| {
        let index = |what: &str| {
            said.iter()
                .position(|l| l.starts_with(what))
                .unwrap_or_else(|| panic!("no line {what:?} in {said:#?}"))
        };
        (
            index("media connect status before the packet filter: "),
            index("packet filter 0xb sent"),
            index("packet filter set"),
            index("media connect status after the packet filter: "),
        )
    };
    let (_, _, host, said) = traced_card(ROOMY, |_| ());
    let (before, sent, set, after) = at(&said);
    assert!(before < sent && set < after, "{said:#?}");
    assert_eq!(
        said[before],
        "media connect status before the packet filter: 0, connected"
    );
    assert_eq!(
        said[after],
        "media connect status after the packet filter: 0, connected"
    );
    assert_clean(&host);
    for (value, answer) in [
        (Some(1u32.to_le_bytes().to_vec()), "1, disconnected"),
        (
            Some(7u32.to_le_bytes().to_vec()),
            "7, which NDIS does not name",
        ),
        (Some(vec![0; 2]), "an answer of 2 bytes"),
        (
            None,
            "not answered: the host refused the query of its media connect status with status 0xc00000bb (NVSP 6.1, NDIS 6.30)",
        ),
    ] {
        let (_, mut card, host, said) = traced_card(ROOMY, |host| host.media = value);
        let (before, _, _, after) = at(&said);
        assert_eq!(
            said[before],
            format!("media connect status before the packet filter: {answer}")
        );
        assert_eq!(
            said[after],
            format!("media connect status after the packet filter: {answer}")
        );
        // The card started all the same, and works.
        let frame = ethernet(BROADCAST, 0x0806, &[0; 46]);
        assert!(card.send(&frame).unwrap());
        assert_eq!(drain(&mut card), []);
        assert_eq!(host.borrow().filter, 0xb);
    }
}

#[test]
fn a_diagnosis_build_states_802_1q_and_teaming_and_nothing_else() {
    let (_, _, host, said) = traced_card(ROOMY, |_| ());
    let capabilities = host.borrow().ndis().0.capabilities;
    assert!(capabilities.ieee_8021_q() && capabilities.teaming());
    assert!(!capabilities.sriov() && !capabilities.rsc_over_vmbus());
    assert!(!capabilities.vmq() && !capabilities.chimney());
    assert!(
        said.iter()
            .any(|l| l == "NDIS capabilities stated: 802.1Q and teaming"),
        "{said:#?}"
    );
}

#[test]
fn the_trace_says_what_a_frame_is() {
    let to = "to ff:ff:ff:ff:ff:ff";
    let says =
        |payload_kind: u16, payload: &[u8]| describe(&ethernet(BROADCAST, payload_kind, payload));
    assert_eq!(describe(&[0; 13]), "shorter than an Ethernet header");
    assert!(!traced(&[0; 13]));
    assert_eq!(says(0x86dd, &[]), format!("{to}, IPv6"));
    assert_eq!(says(0x88cc, &[]), format!("{to}, EtherType 0x88cc"));
    // ARP: a question, an answer, another operation, cut short.
    assert_eq!(
        says(0x0806, &arp(1, MAC, NODE, GATEWAY)),
        format!("{to}, ARP who has 10.0.0.1 tell 10.0.0.4")
    );
    assert_eq!(
        says(0x0806, &arp(2, GATEWAY_MAC, GATEWAY, NODE)),
        format!("{to}, ARP 10.0.0.1 is at 12:34:56:78:9a:bc")
    );
    assert_eq!(
        says(0x0806, &arp(3, MAC, NODE, GATEWAY)),
        format!("{to}, ARP operation 3")
    );
    assert_eq!(says(0x0806, &[0; 27]), format!("{to}, ARP, truncated"));
    // IPv4: cut short anywhere, another protocol, UDP's and TCP's ports and flags.
    let v4 = format!("{to}, IPv4 10.0.0.4>10.0.0.1");
    assert_eq!(says(0x0800, &[]), format!("{to}, IPv4, truncated"));
    assert_eq!(
        says(0x0800, &[0x45, 0, 0]),
        format!("{to}, IPv4, truncated")
    );
    assert_eq!(
        says(0x0800, &ipv4(NODE, GATEWAY, 1, &[0; 20])),
        format!("{v4} protocol 1")
    );
    assert_eq!(
        says(0x0800, &ipv4(NODE, GATEWAY, 17, &[0; 3])),
        format!("{v4} UDP, truncated")
    );
    assert_eq!(
        says(
            0x0800,
            &ipv4(NODE, GATEWAY, 6, &tcp(443, 50000, 0x02)[..13])
        ),
        format!("{v4} TCP 443>50000, truncated")
    );
    assert_eq!(
        says(0x0800, &ipv4(NODE, GATEWAY, 6, &tcp(443, 50000, 0x11))),
        format!("{v4} TCP 443>50000 [ACK FIN]")
    );
    assert!(!traced(&ethernet(
        BROADCAST,
        0x0800,
        &ipv4(NODE, GATEWAY, 6, &tcp(1, 2, 0x11))
    )));
    assert_eq!(
        says(0x0800, &ipv4(NODE, GATEWAY, 17, &udp(49152, 53, &[0; 12]))),
        format!("{v4} UDP 49152>53")
    );
    assert_eq!(
        says(0x0800, &ipv4(NODE, GATEWAY, 17, &udp(68, 53, &[0; 12]))),
        format!("{v4} UDP 68>53")
    );
    // DHCP: every message type, cut short, options before 53, and none to be found.
    let dhcp_says = |message: &[u8]| says(0x0800, &ipv4(NODE, GATEWAY, 17, &udp(67, 68, message)));
    let dhcp_to = format!("{v4} UDP 67>68, DHCP");
    assert_eq!(
        dhcp_says(&[2, 1, 6, 0, 1, 2, 3]),
        format!("{dhcp_to} truncated")
    );
    for (kind, name) in [
        (3, "REQUEST"),
        (4, "DECLINE"),
        (5, "ACK"),
        (6, "NAK"),
        (7, "RELEASE"),
        (8, "INFORM"),
        (9, "of another type"),
    ] {
        assert_eq!(
            dhcp_says(&dhcp(2, 0xabcd, &[53, 1, kind, 255])),
            format!("{dhcp_to} {name} xid 0x0000abcd")
        );
    }
    assert_eq!(
        dhcp_says(&dhcp(2, 1, &[0, 0, 1, 4, 255, 255, 255, 0, 53, 1, 5, 255])),
        format!("{dhcp_to} ACK xid 0x00000001")
    );
    let none = format!("{dhcp_to} with no message type xid 0x00000001");
    assert_eq!(dhcp_says(&dhcp(2, 1, &[1, 4, 255, 255, 255, 0, 255])), none);
    assert_eq!(dhcp_says(&dhcp(2, 1, &[1, 4, 255])), none);
    assert_eq!(dhcp_says(&dhcp(2, 1, &[53])), none);
    assert_eq!(dhcp_says(&dhcp(2, 1, &[53, 2, 1, 1, 255])), none);
    let mut no_cookie = dhcp(2, 1, &[53, 1, 2, 255]);
    no_cookie[236] = 0;
    assert_eq!(dhcp_says(&no_cookie), none);
    assert_eq!(dhcp_says(&dhcp(2, 1, &[])[..238]), none);
    // A UDP datagram with no room for DHCP's fields after its header.
    assert_eq!(
        says(0x0800, &ipv4(NODE, GATEWAY, 17, &[0, 67, 0, 68, 0, 8])),
        format!("{v4} UDP 67>68, DHCP truncated")
    );
}
