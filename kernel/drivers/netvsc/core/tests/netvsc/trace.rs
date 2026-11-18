//! Work item 1.11c, with the feature `trace-first-frames` only: the first frames each way,
//! their commits and completions, and the host's indications, each said with its time on
//! the clock the card was given; and what the trace says a frame is.

use std::{cell::Cell, rc::Rc};

use moruna_netvsc::{
    NetvscError,
    trace::{FIRST, Trace, describe},
};
use netvsp_protocol::{protocol::Status, rndisprot};

use crate::fake::{Card, Got, Host, MAC, PAGE, ROOMY, Settings, assert_clean, drain, parts};

const BROADCAST: [u8; 6] = [0xff; 6];

/// A card started against a host `prepare` has set up, traced on a clock the test sets.
fn traced(
    settings: Settings,
    prepare: impl FnOnce(&mut Host),
) -> (Rc<Cell<u64>>, Card, Rc<std::cell::RefCell<Host>>) {
    let clock = Rc::new(Cell::new(0));
    let time = clock.clone();
    let parts = parts(16 * PAGE, 16 * PAGE, settings);
    let host = parts.host.clone();
    prepare(&mut host.borrow_mut());
    let card = parts
        .start_traced(Trace::new(Box::new(move || time.get())))
        .unwrap_or_else(|e| panic!("the card did not start: {e}"));
    (clock, card, host)
}

/// An Ethernet frame from the card's address.
fn ethernet(to: [u8; 6], kind: u16, payload: &[u8]) -> Vec<u8> {
    [&to[..], &MAC[..], &kind.to_be_bytes(), payload].concat()
}

/// An IPv4 packet of `protocol` with a header of 20 bytes.
fn ipv4(protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut header = [0u8; 20];
    header[0] = 0x45;
    header[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    header[8] = 64;
    header[9] = protocol;
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

/// The frame of a DHCP message of `kind` (option 53) from port `from` to port `to`.
fn dhcp_frame(to: [u8; 6], from_port: u16, to_port: u16, kind: u8, xid: u32) -> Vec<u8> {
    let op = if from_port == 68 { 1 } else { 2 };
    let message = dhcp(op, xid, &[53, 1, kind, 255]);
    ethernet(to, 0x0800, &ipv4(17, &udp(from_port, to_port, &message)))
}

#[test]
fn a_dhcp_exchange_is_traced_on_the_cards_clock() {
    let (clock, mut card, host) = traced(ROOMY, |_| ());
    // The start sent no frame and the host indicated nothing.
    assert!(card.take_trace().is_empty());
    let discover = dhcp_frame(BROADCAST, 68, 67, 1, 0x3d1d_5e4f);
    let offer = dhcp_frame(MAC, 67, 68, 2, 0x3d1d_5e4f);
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
            "231.456 ms: trace: out 1: 286 bytes in section 0, to ff:ff:ff:ff:ff:ff, IPv4 UDP 68>67, DHCP DISCOVER xid 0x3d1d5e4f",
            "231.456 ms: trace: out 1 committed; the host signalled",
            "231.502 ms: trace: out 1 completed: status 1 (success)",
            "4233.000 ms: trace: in 1: 286 bytes, to 00:15:5d:01:02:03, IPv4 UDP 67>68, DHCP OFFER xid 0x3d1d5e4f",
        ]
    );
    // Taken once.
    assert!(card.take_trace().is_empty());
    assert_eq!(host.borrow().frames, [discover]);
    assert_clean(&host);
}

#[test]
fn a_commit_says_which_frames_it_carries_and_whether_the_host_was_signalled() {
    let (clock, mut card, host) = traced(ROOMY, |_| ());
    // A host that reads nothing: the ring to it stays as the card left it.
    host.borrow_mut().paused = true;
    clock.set(1_000_000);
    let frame = |n: u8| ethernet([0x02, 0, 0, 0, 0, n], 0x88b5, &[n; 46]);
    assert!(card.send(&frame(1)).unwrap());
    assert!(card.send(&frame(2)).unwrap());
    assert!(card.put(&frame(3)).unwrap());
    assert!(card.put(&frame(4)).unwrap());
    card.commit().unwrap();
    let to = |n: u8| format!("to 02:00:00:00:00:0{n}, EtherType 0x88b5");
    assert_eq!(
        card.take_trace(),
        [
            format!("1.000 ms: trace: out 1: 60 bytes in section 0, {}", to(1)),
            String::from("1.000 ms: trace: out 1 committed; the host signalled"),
            format!("1.000 ms: trace: out 2: 60 bytes in section 1, {}", to(2)),
            String::from(
                "1.000 ms: trace: out 2 committed; no signal: the ring was not empty, or the host masked it"
            ),
            format!("1.000 ms: trace: out 3: 60 bytes in section 2, {}", to(3)),
            format!("1.000 ms: trace: out 4: 60 bytes in section 3, {}", to(4)),
            String::from(
                "1.000 ms: trace: out 3, 4 committed; no signal: the ring was not empty, or the host masked it"
            ),
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
    let (clock, mut card, host) = traced(ROOMY, |host| host.send_status = Status::FAILURE);
    clock.set(5_000);
    let discover = dhcp_frame(BROADCAST, 68, 67, 1, 7);
    assert!(card.send(&discover).unwrap());
    match card.poll() {
        Err(NetvscError::NotSent(2)) => {}
        other => panic!("expected the host's failure, got {other:?}"),
    }
    assert_eq!(
        card.take_trace(),
        [
            "0.005 ms: trace: out 1: 286 bytes in section 0, to ff:ff:ff:ff:ff:ff, IPv4 UDP 68>67, DHCP DISCOVER xid 0x00000007",
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
    let (clock, mut card, host) = traced(ROOMY, |_| ());
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
    let (_, mut card, host) = traced(ROOMY, |host| {
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
    let (_, mut card, host) = traced(ROOMY, |_| ());
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
    assert_eq!(count("trace: out "), 3 * FIRST as usize);
    assert_eq!(count(" committed;"), FIRST as usize);
    assert_eq!(count(" completed: "), FIRST as usize);
    assert_eq!(count("trace: in "), FIRST as usize);
    assert_eq!(count("trace: indication "), FIRST as usize);
    assert!(lines.iter().any(|l| l.contains("trace: out 20: 60 bytes")));
    assert!(!lines.iter().any(|l| l.contains("trace: out 21")));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("trace: in 20: 60 bytes, to ff:ff:ff:ff:ff:ff, ARP"))
    );
    assert!(!lines.iter().any(|l| l.contains("trace: in 21")));
    assert_eq!(host.borrow().frames.len(), extra);
    assert_clean(&host);
}

#[test]
fn the_trace_says_what_a_frame_is() {
    let to = "to ff:ff:ff:ff:ff:ff";
    let says =
        |payload_kind: u16, payload: &[u8]| describe(&ethernet(BROADCAST, payload_kind, payload));
    assert_eq!(describe(&[0; 13]), "shorter than an Ethernet header");
    assert_eq!(says(0x0806, &[]), format!("{to}, ARP"));
    assert_eq!(says(0x86dd, &[]), format!("{to}, IPv6"));
    assert_eq!(says(0x88cc, &[]), format!("{to}, EtherType 0x88cc"));
    assert_eq!(says(0x0800, &[]), format!("{to}, IPv4, truncated"));
    assert_eq!(
        says(0x0800, &[0x45, 0, 0]),
        format!("{to}, IPv4, truncated")
    );
    assert_eq!(
        says(0x0800, &ipv4(6, &[0; 20])),
        format!("{to}, IPv4 protocol 6")
    );
    assert_eq!(
        says(0x0800, &ipv4(17, &[0; 7])),
        format!("{to}, IPv4 UDP, truncated")
    );
    assert_eq!(
        says(0x0800, &ipv4(17, &udp(49152, 53, &[0; 12]))),
        format!("{to}, IPv4 UDP 49152>53")
    );
    assert_eq!(
        says(0x0800, &ipv4(17, &udp(68, 53, &[0; 12]))),
        format!("{to}, IPv4 UDP 68>53")
    );
    let dhcp_says = |message: &[u8]| says(0x0800, &ipv4(17, &udp(67, 68, message)));
    let dhcp_to = format!("{to}, IPv4 UDP 67>68, DHCP");
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
    // Pad and other options before option 53.
    assert_eq!(
        dhcp_says(&dhcp(2, 1, &[0, 0, 1, 4, 255, 255, 255, 0, 53, 1, 5, 255])),
        format!("{dhcp_to} ACK xid 0x00000001")
    );
    // No option 53, no cookie, an option cut short, an option 53 of the wrong length.
    let none = format!("{dhcp_to} with no message type xid 0x00000001");
    assert_eq!(dhcp_says(&dhcp(2, 1, &[1, 4, 255, 255, 255, 0, 255])), none);
    assert_eq!(dhcp_says(&dhcp(2, 1, &[1, 4, 255])), none);
    assert_eq!(dhcp_says(&dhcp(2, 1, &[53])), none);
    assert_eq!(dhcp_says(&dhcp(2, 1, &[53, 2, 1, 1, 255])), none);
    let mut no_cookie = dhcp(2, 1, &[53, 1, 2, 255]);
    no_cookie[236] = 0;
    assert_eq!(dhcp_says(&no_cookie), none);
    assert_eq!(dhcp_says(&dhcp(2, 1, &[])[..238]), none);
}
