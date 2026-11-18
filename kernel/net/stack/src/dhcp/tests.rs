//! The DISCOVER schedule (work item 1.11c): 1 s, doubling to 64 s, each delay within a
//! tenth of its base, drawn once, started over on a restart; and which frames are DISCOVERs.

extern crate std;

use alloc::{vec, vec::Vec};

use smoltcp::{
    phy::ChecksumCapabilities,
    wire::{
        DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
        EthernetRepr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
    },
};

use super::{CAP_NS, Discovery, FIRST_NS, is_discover};

const SECOND: u64 = 1_000_000_000;

/// The bases of the first nine delays: 1, 2, 4, 8, 16, 32, then 64 s.
const BASES: [u64; 9] = [1, 2, 4, 8, 16, 32, 64, 64, 64];

#[test]
fn each_delay_doubles_to_the_cap_within_a_tenth_of_its_base() {
    for seed in 0..200 {
        let mut d = Discovery::new(seed);
        for (sent, base) in BASES.iter().enumerate() {
            assert_eq!(d.sent(), sent as u32);
            let base = base * SECOND;
            let delay = d.next_ns();
            assert!(
                (base - base / 10..=base + base / 10).contains(&delay),
                "seed {seed}, DISCOVER {sent}: {delay} ns for a base of {base} ns"
            );
            d.discovered();
        }
    }
    assert_eq!((FIRST_NS, CAP_NS), (SECOND, 64 * SECOND));
}

#[test]
fn a_delay_is_drawn_once_and_the_draws_spread_over_the_whole_tenth() {
    let mut d = Discovery::new(7);
    assert_eq!(d.next_ns(), d.next_ns());
    let first: Vec<u64> = (0..1000)
        .map(|seed| Discovery::new(seed).next_ns())
        .collect();
    let (low, high) = (*first.iter().min().unwrap(), *first.iter().max().unwrap());
    // Uniform over 0.9 s to 1.1 s: a thousand draws reach within 5 ms of each end.
    assert!(low < SECOND - SECOND / 10 + 5_000_000, "{low}");
    assert!(high > SECOND + SECOND / 10 - 5_000_000, "{high}");
    // Seeds give different schedules, a seed the same one.
    assert_ne!(first[1], first[2]);
    d.discovered();
    let again = {
        let mut e = Discovery::new(7);
        e.discovered();
        e.next_ns()
    };
    assert_eq!(d.next_ns(), again);
}

#[test]
fn a_restart_starts_the_schedule_over() {
    let mut d = Discovery::new(3);
    for _ in 0..20 {
        d.discovered();
    }
    assert_eq!(d.sent(), 20);
    assert!(d.next_ns() >= CAP_NS - CAP_NS / 10);
    d.restart();
    assert_eq!(d.sent(), 0);
    assert!((SECOND - SECOND / 10..=SECOND + SECOND / 10).contains(&d.next_ns()));
}

/// A DHCP message of `kind` from port `from` to port `to`, in an Ethernet frame of IPv4.
fn dhcp(kind: DhcpMessageType, from: u16, to: u16) -> Vec<u8> {
    let repr = DhcpRepr {
        message_type: kind,
        transaction_id: 0x1234_5678,
        secs: 0,
        client_hardware_address: EthernetAddress([2, 0, 0, 0, 0, 1]),
        client_ip: Ipv4Address::UNSPECIFIED,
        your_ip: Ipv4Address::UNSPECIFIED,
        server_ip: Ipv4Address::UNSPECIFIED,
        router: None,
        subnet_mask: None,
        relay_agent_ip: Ipv4Address::UNSPECIFIED,
        broadcast: false,
        requested_ip: None,
        client_identifier: None,
        server_identifier: None,
        parameter_request_list: None,
        dns_servers: None,
        max_size: None,
        lease_duration: None,
        renew_duration: None,
        rebind_duration: None,
        additional_options: &[],
    };
    let caps = ChecksumCapabilities::default();
    let udp = UdpRepr {
        src_port: from,
        dst_port: to,
    };
    let udp_len = 8 + repr.buffer_len();
    let ip = Ipv4Repr {
        src_addr: Ipv4Address::UNSPECIFIED,
        dst_addr: Ipv4Address::BROADCAST,
        next_header: IpProtocol::Udp,
        payload_len: udp_len,
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 1]),
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
        &Ipv4Address::UNSPECIFIED.into(),
        &Ipv4Address::BROADCAST.into(),
        repr.buffer_len(),
        |payload| {
            let _ = repr.emit(&mut DhcpPacket::new_unchecked(payload));
        },
        &caps,
    );
    buf
}

#[test]
fn only_a_dhcp_discover_from_the_client_s_port_is_a_discover() {
    let discover = dhcp(DhcpMessageType::Discover, 68, 67);
    assert!(is_discover(&discover));
    assert!(!is_discover(&dhcp(DhcpMessageType::Request, 68, 67)));
    assert!(!is_discover(&dhcp(DhcpMessageType::Offer, 67, 68)));
    assert!(!is_discover(&dhcp(DhcpMessageType::Discover, 68, 53)));
    assert!(!is_discover(&dhcp(DhcpMessageType::Discover, 1068, 67)));
    // Not IPv4, not UDP, cut short, not a frame.
    let mut arp = discover.clone();
    arp[12..14].copy_from_slice(&[0x08, 0x06]);
    assert!(!is_discover(&arp));
    let mut tcp = discover.clone();
    tcp[14 + 9] = 6;
    assert!(!is_discover(&tcp));
    assert!(!is_discover(&discover[..14 + 20 + 4]));
    assert!(!is_discover(&discover[..14 + 20 + 8 + 100]));
    assert!(!is_discover(&discover[..10]));
    let mut ip_cut = discover.clone();
    ip_cut.truncate(14 + 10);
    assert!(!is_discover(&ip_cut));
}
