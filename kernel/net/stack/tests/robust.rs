//! KN-T8 to KN-T10 (SDD 31, k): TCP delivers every byte in order over a link that
//! loses, duplicates and reorders frames; frames that are garbage, or real frames
//! mangled, never panic the stack and leave it serving; and DHCP's lease becomes an
//! interface's address, gateway and resolver.

mod common;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use common::{Flaws, MAC_A, MAC_B, PAIR, Rng, Wire, link, serve};
use moruna_netstack::Stack;
use moruna_svc::{CallError, Channel, deadline_in};
use netstack_protocol::{Addr, Cidr, Endpoint, Target, net};
use smoltcp::wire::{
    DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
};

fn client(listener: Channel) -> net::Client {
    let channel = moruna_svc::connect(&listener).expect("connect");
    let mut c = net::Client::connect(
        channel,
        deadline_in(10_000_000_000),
        moruna_svc::Account::carve(8 << 20, "net-test").unwrap(),
    )
    .expect("hello");
    c.core().set_timeout(120_000_000_000);
    c
}

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    (0..len).map(|_| r.next() as u8).collect()
}

/// `bytes` from a to b over one TCP connection, the sender and the receiver each on
/// a thread of its own; what b reads.
fn stream(a: net::Client, b: net::Client, bytes: &[u8]) -> Vec<u8> {
    let (mut a, mut b) = (a, b);
    let (l, at) = b
        .listen(Endpoint::new(Addr::v4(10, 47, 0, 2), 0), 1)
        .expect("listen");
    let c = a
        .open(Target::Endpoint(at), Some(Addr::v4(10, 47, 0, 1)))
        .expect("open");
    let s = b.accept(l).expect("accept");
    let data = bytes.to_vec();
    let sender = thread::spawn(move || {
        let mut sent = 0;
        while sent < data.len() {
            let n = (data.len() - sent).min(netstack_protocol::SEND_MAX);
            sent += common::send(&mut a, c.socket, &data[sent..sent + n]).expect("send") as usize;
        }
        a.shutdown(c.socket).expect("shutdown");
        a
    });
    let mut got = Vec::new();
    loop {
        let chunk = common::receive(&mut b, s.socket, netstack_protocol::RECEIVE_MAX as u32)
            .expect("receive");
        if chunk.is_empty() {
            break;
        }
        got.extend_from_slice(&chunk);
    }
    drop(sender.join().expect("the sender"));
    got
}

#[test]
fn kn_t8_tcp_delivers_every_byte_over_a_lossy_link() {
    let flaws = Flaws {
        lose: 50,
        duplicate: 20,
        reorder: 50,
        cut: false,
    };
    let (mut slots, join) = serve(PAIR, &[MAC_A, MAC_B], link(flaws));
    let a = client(slots.remove("listen-a").expect("slot"));
    let b = client(slots.remove("listen-b").expect("slot"));
    let data = pattern(2 << 20, 77);
    let got = stream(a, b, &data);
    assert_eq!(got.len(), data.len());
    assert!(got == data, "the bytes arrived changed");
    drop(slots);
    join.join().expect("the server");
}

/// The pair's link, and after it garbage: frames of random bytes and lengths, and
/// mangled copies of the frames that passed (bytes flipped, cut short, lengthened),
/// `count` of them in all; the count so far is `injected`'s.
fn mangling(count: u64, injected: Arc<AtomicU64>) -> Wire {
    let mut inner = link(Flaws::default());
    let mut seen: Vec<Vec<u8>> = Vec::new();
    Box::new(move |st: &mut Stack, rng: &mut Rng| {
        for card in 0..2 {
            for f in st.card_mut(card).port.tx.iter().take(4) {
                if seen.len() < 512 {
                    seen.push(f.clone());
                } else {
                    let k = rng.next() as usize % seen.len();
                    seen[k] = f.clone();
                }
            }
        }
        inner(st, rng);
        for _ in 0..256 {
            if injected.load(Ordering::Relaxed) >= count {
                break;
            }
            injected.fetch_add(1, Ordering::Relaxed);
            let to = (rng.next() % 2) as usize;
            let frame = match rng.next() % 4 {
                0 if !seen.is_empty() => {
                    let mut f = seen[rng.next() as usize % seen.len()].clone();
                    for _ in 0..1 + rng.next() % 4 {
                        let at = rng.next() as usize % f.len();
                        f[at] ^= 1 << (rng.next() % 8);
                    }
                    f
                }
                1 if !seen.is_empty() => {
                    let f = &seen[rng.next() as usize % seen.len()];
                    f[..rng.next() as usize % (f.len() + 1)].to_vec()
                }
                2 if !seen.is_empty() => {
                    let mut f = seen[rng.next() as usize % seen.len()].clone();
                    f.extend((0..rng.next() % 64).map(|_| rng.next() as u8));
                    // Lengths in the headers that now disagree with the frame.
                    if f.len() > 20 {
                        let at = 14 + (rng.next() as usize % 8);
                        f[at] = rng.next() as u8;
                    }
                    f
                }
                _ => {
                    let len = rng.next() as usize % 1600;
                    (0..len).map(|_| rng.next() as u8).collect()
                }
            };
            st.card_mut(to).port.deliver(frame);
        }
    })
}

#[test]
fn kn_t9_untrusted_frames_never_panic_and_the_stack_serves_on() {
    const FRAMES: u64 = 200_000;
    let injected = Arc::new(AtomicU64::new(0));
    let (mut slots, join) = serve(PAIR, &[MAC_A, MAC_B], mangling(FRAMES, injected.clone()));
    let mut a = client(slots.remove("listen-a").expect("slot"));
    let mut b = client(slots.remove("listen-b").expect("slot"));
    // Traffic while the garbage arrives, so mangled copies of every kind of frame
    // (ARP, NDP, TCP, UDP, ICMP) are among it, until all of it has arrived.
    let (l, at) = b
        .listen(Endpoint::new(Addr::v4(10, 47, 0, 2), 0), 1)
        .expect("listen");
    let six = Addr("fd4b:4532::2".parse().expect("v6"));
    while injected.load(Ordering::Relaxed) < FRAMES {
        // Echoes and connections may be lost to a mangled frame; what matters here is
        // that the stack goes on.
        let _ = a.ping(Addr::v4(10, 47, 0, 2), 64);
        let _ = a.ping(six, 64);
        if let Ok(c) = a.open(Target::Endpoint(at), None) {
            let _ = common::send(&mut a, c.socket, &[7; 3000]);
            let _ = a.close(c.socket);
            if let Ok(s) = b.accept(l) {
                let _ = b.close(s.socket);
            }
        }
    }
    b.close(l).expect("close");
    // Loopback carries none of the garbage: it serves at once.
    let mut lo = client(slots.remove("listen-suite").expect("slot"));
    let here = Addr::v4(127, 0, 0, 1);
    let (ll, lat) = lo
        .listen(Endpoint::new(here, 0), 1)
        .expect("listen on loopback");
    let lc = lo
        .open(Target::Endpoint(lat), None)
        .expect("open on loopback");
    let ls = lo.accept(ll).expect("accept on loopback");
    common::send(&mut lo, lc.socket, &[9; 50_000]).expect("send on loopback");
    let mut n = 0;
    while n < 50_000 {
        n += common::receive(&mut lo, ls.socket, netstack_protocol::RECEIVE_MAX as u32)
            .expect("receive on loopback")
            .len();
    }
    // ARP and NDP are unauthenticated: a mangled copy of a real answer, one bit of its
    // hardware address flipped, is an answer, and is believed for a neighbour entry's
    // 60 s, as on any host. The link heals once those entries expire.
    let healed = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let v4 = a.ping(Addr::v4(10, 47, 0, 2), 64).is_ok();
        let v6 = a.ping(six, 64).is_ok();
        let back = b.ping(Addr::v4(10, 47, 0, 1), 64).is_ok();
        if v4 && v6 && back {
            break;
        }
        assert!(
            std::time::Instant::now() < healed,
            "the link did not heal in 90 s"
        );
    }
    let data = pattern(1 << 20, 5);
    let got = stream(a, b, &data);
    assert!(got == data, "the bytes arrived changed");
    drop(lo);
    drop(slots);
    join.join().expect("the server did not panic");
}

const WAN: &str = "moruna-netstack 1
interface wan card=net4 dhcp
grant listen rights=connect remote=any local=any
";

/// A DHCP server for card 0: offers 10.0.2.15/24, router 10.0.2.2, DNS 10.0.2.3, and
/// acknowledges the request for it.
fn dhcp_server() -> Wire {
    let server_mac = EthernetAddress([0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]);
    let server_ip = Ipv4Address::new(10, 0, 2, 2);
    Box::new(move |st: &mut Stack, _: &mut Rng| {
        for f in std::mem::take(&mut st.card_mut(0).port.tx) {
            let Some((client, xid, kind)) = dhcp_request(&f) else {
                continue;
            };
            let reply_type = match kind {
                DhcpMessageType::Discover => DhcpMessageType::Offer,
                DhcpMessageType::Request => DhcpMessageType::Ack,
                _ => continue,
            };
            let mut dns = heapless::Vec::new();
            let _ = dns.push(Ipv4Address::new(10, 0, 2, 3));
            let repr = DhcpRepr {
                message_type: reply_type,
                transaction_id: xid,
                secs: 0,
                client_hardware_address: client,
                client_ip: Ipv4Address::UNSPECIFIED,
                your_ip: Ipv4Address::new(10, 0, 2, 15),
                server_ip,
                router: Some(server_ip),
                subnet_mask: Some(Ipv4Address::new(255, 255, 255, 0)),
                relay_agent_ip: Ipv4Address::UNSPECIFIED,
                broadcast: false,
                requested_ip: None,
                client_identifier: None,
                server_identifier: Some(server_ip),
                parameter_request_list: None,
                dns_servers: Some(dns),
                max_size: None,
                lease_duration: Some(3600),
                renew_duration: None,
                rebind_duration: None,
                additional_options: &[],
            };
            st.card_mut(0)
                .port
                .deliver(dhcp_frame(server_mac, server_ip, &repr));
        }
    })
}

/// The client's address, transaction and message type, if `f` is a DHCP request.
fn dhcp_request(f: &[u8]) -> Option<(EthernetAddress, u32, DhcpMessageType)> {
    let eth = EthernetFrame::new_checked(f).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp {
        return None;
    }
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    if udp.dst_port() != 67 {
        return None;
    }
    let dhcp = DhcpPacket::new_checked(udp.payload()).ok()?;
    let repr = DhcpRepr::parse(&dhcp).ok()?;
    Some((
        repr.client_hardware_address,
        repr.transaction_id,
        repr.message_type,
    ))
}

fn dhcp_frame(src_mac: EthernetAddress, src: Ipv4Address, repr: &DhcpRepr<'_>) -> Vec<u8> {
    let caps = smoltcp::phy::ChecksumCapabilities::default();
    let dst = Ipv4Address::BROADCAST;
    let udp = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };
    let udp_len = 8 + repr.buffer_len();
    let ip = Ipv4Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Udp,
        payload_len: udp_len,
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: src_mac,
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Ipv4,
    };
    let mut buf = vec![0u8; 14 + ip.buffer_len() + udp_len];
    let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.emit(&mut frame);
    let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
    ip.emit(&mut packet, &caps);
    let mut datagram = UdpPacket::new_unchecked(packet.payload_mut());
    udp.emit(
        &mut datagram,
        &src.into(),
        &dst.into(),
        repr.buffer_len(),
        |payload| {
            let _ = repr.emit(&mut DhcpPacket::new_unchecked(payload));
        },
        &caps,
    );
    buf
}

#[test]
fn kn_t10_a_dhcp_lease_becomes_the_interface_s_address_gateway_and_resolver() {
    let (mut slots, join) = serve(WAN, &[MAC_A], dhcp_server());
    let mut c = client(slots.remove("listen").expect("slot"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let wan = loop {
        let info = c.info().expect("info");
        let wan = info
            .interfaces
            .iter()
            .find(|i| i.name == "wan")
            .expect("wan")
            .clone();
        if wan.addrs.iter().any(|a| a.addr.is_v4()) {
            assert_eq!(info.dns, vec![Addr::v4(10, 0, 2, 3)]);
            break wan;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no lease in 30 s: {wan:?}"
        );
        thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(wan.dhcp);
    assert!(
        wan.addrs.contains(&Cidr::new(Addr::v4(10, 0, 2, 15), 24)),
        "{wan:?}"
    );
    assert_eq!(wan.gateway4, Some(Addr::v4(10, 0, 2, 2)));
    drop(c);
    drop(slots);
    join.join().expect("the server");
}

#[test]
fn kn_t11_driver_ip_mtu_is_not_reduced_by_a_second_ethernet_header() {
    let (mut slots, join) = serve(PAIR, &[MAC_A, MAC_B], link(Flaws::default()));
    let mut a = client(slots.remove("listen-a").unwrap());
    let mut b = client(slots.remove("listen-b").unwrap());
    assert_eq!(
        a.info()
            .unwrap()
            .interfaces
            .iter()
            .find(|i| i.name == "a")
            .unwrap()
            .mtu,
        1500
    );
    let (source, _) = a.bind(Endpoint::new(Addr::v4(10, 47, 0, 1), 0)).unwrap();
    let (target, at) = b.bind(Endpoint::new(Addr::v4(10, 47, 0, 2), 0)).unwrap();
    let bytes = vec![0x5a; 1500 - 28];
    a.send_to(source, at, bytes.clone()).unwrap();
    assert_eq!(b.receive_from(target).unwrap().1, bytes);
    assert_eq!(
        a.send_to(source, at, vec![0; 1500 - 28 + 1]),
        Err(CallError::Remote(net::Error::TooLarge))
    );
    drop(a);
    drop(b);
    drop(slots);
    join.join().unwrap();
}

#[test]
fn kn_t12_down_cards_are_not_routes_or_busy_poll_sources() {
    let mut first = true;
    let mut underlying = link(Flaws::default());
    let wire = Box::new(move |stack: &mut Stack, rng: &mut Rng| {
        if first {
            stack.set_link(0, false);
            first = false;
        }
        assert!(
            stack.card_mut(0).port.tx.is_empty(),
            "a down card emitted frames"
        );
        underlying(stack, rng);
    });
    let (mut slots, join) = serve(PAIR, &[MAC_A, MAC_B], wire);
    let mut a = client(slots.remove("listen-a").unwrap());
    assert!(
        !a.info()
            .unwrap()
            .interfaces
            .iter()
            .find(|i| i.name == "a")
            .unwrap()
            .up
    );
    assert_eq!(
        a.open(
            Target::Endpoint(Endpoint::new(Addr::v4(10, 47, 0, 2), 9999)),
            Some(Addr::v4(10, 47, 0, 1))
        ),
        Err(CallError::Remote(net::Error::NoRoute))
    );
    drop(a);
    drop(slots);
    join.join().unwrap();
}
