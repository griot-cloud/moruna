//! The network server on a host (SDD 31, k): its stack and dispatcher on a thread of
//! their own, run as the server's loop runs them, with its cards joined to whatever
//! a test puts at the other end of their wire.

#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use moruna_netstack::{Card, Shared, Stack, after_turn, factory, parse};
use moruna_svc::{Channel, Dispatcher, Turn, now};
use smoltcp::wire::{
    DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
};

pub const MAC_A: u64 = 0x5254_004b_4502;
pub const MAC_B: u64 = 0x5254_004b_4503;

/// The configuration of the suites' server: loopback for the one-server suite, and
/// two cards on one link, `a` and `b`, for the pair suite, with the resolver on `b`.
pub const PAIR: &str = "moruna-netstack 1
setting tcp-receive-buffer 131072
setting tcp-send-buffer 131072
setting connect-timeout-ms 10000
setting resolve-timeout-ms 10000
setting ping-timeout-ms 2000
interface a card=net2 ipv4=10.47.0.1/24 ipv6=fd4b:4532::1/64
interface b card=net3 ipv4=10.47.0.2/24 ipv6=fd4b:4532::2/64
dns 10.47.0.2
grant listen-suite rights=connect,listen,udp,ping,grant remote=127.0.0.0/8,::1/128 local=127.0.0.0/8,::1/128 sockets=32
grant listen-a rights=connect,listen,udp,ping remote=10.47.0.0/24,fd4b:4532::/64 local=10.47.0.1/32,fd4b:4532::1/128 names=*.suite.ke2.test sockets=32
grant listen-b rights=connect,listen,udp,ping remote=10.47.0.0/24,fd4b:4532::/64 local=10.47.0.2/32,fd4b:4532::2/128 sockets=32
";

/// A tiny deterministic generator, for the wire's flaws and the fuzzing.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// True `per_mille` times in a thousand.
    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.next() % 1000 < per_mille
    }
}

/// What a link between card 0 and card 1 does to frames: loses, duplicates and
/// reorders them, each so many times in a thousand.
#[derive(Clone, Copy, Default)]
pub struct Flaws {
    pub lose: u64,
    pub duplicate: u64,
    pub reorder: u64,
    /// Every frame is lost.
    pub cut: bool,
}

/// What stands at the other end of the cards' wires: called once a loop with the
/// stack, it takes what the cards sent and delivers what they receive.
pub type Wire = Box<dyn FnMut(&mut Stack, &mut Rng) + Send>;

/// No external NICs: the stack itself still moves every loopback IP packet.
/// Unlike `link`, this fixture must not address card zero or card one.
pub fn no_external_cards() -> Wire {
    Box::new(|stack, _| assert_eq!(stack.cards(), 0))
}

/// The pair's wire: card 0 and card 1 on one link, with `flaws`.
pub fn link(flaws: Flaws) -> Wire {
    let mut held: Vec<(usize, Vec<u8>)> = Vec::new();
    Box::new(move |st: &mut Stack, rng: &mut Rng| {
        for (from, to) in [(0, 1), (1, 0)] {
            for f in std::mem::take(&mut st.card_mut(from).port.tx) {
                if flaws.cut || rng.chance(flaws.lose) {
                    continue;
                }
                if rng.chance(flaws.reorder) {
                    held.push((to, f));
                    continue;
                }
                if rng.chance(flaws.duplicate) {
                    st.card_mut(to).port.deliver(f.clone());
                }
                st.card_mut(to).port.deliver(f);
                // A frame held back goes out after a later one.
                if let Some(k) =
                    (!held.is_empty() && rng.chance(500)).then(|| rng.next() as usize % held.len())
                {
                    let (t, h) = held.swap_remove(k);
                    st.card_mut(t).port.deliver(h);
                }
            }
        }
    })
}

/// Serves `config` over cards of `macs`, their wires `wire`; returns the client end of
/// every granted slot's listener, and the server's thread, which ends when every
/// client and listener has gone.
pub fn serve(
    config: &str,
    macs: &[u64],
    wire: Wire,
) -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    serve_with_tls(config, macs, wire, None)
}

pub fn serve_with_tls(
    config: &str,
    macs: &[u64],
    wire: Wire,
    trust: Option<moruna_netstack::tls::Trust>,
) -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    serve_until(config, macs, wire, trust, Arc::new(AtomicBool::new(false)))
}

/// The same server, which also ends as soon as `stop` is set, whoever is still
/// connected: every channel to it closes, as when the server's process exits.
pub fn serve_until(
    config: &str,
    macs: &[u64],
    wire: Wire,
    trust: Option<moruna_netstack::tls::Trust>,
    stop: Arc<AtomicBool>,
) -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    let config = parse(config).expect("the configuration");
    let mut ours = BTreeMap::new();
    let mut theirs = Vec::new();
    for (slot, policy) in &config.grants {
        let (o, t) = Channel::create().expect("a channel");
        ours.insert(slot.clone(), o);
        theirs.push((t, policy.clone()));
    }
    let cards: Vec<Card> = macs
        .iter()
        .map(|&mac| Card {
            mac,
            mtu: 1500,
            up: true,
        })
        .collect();
    let join = thread::spawn(move || {
        let mut wire = wire;
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut stack = Stack::new(&config, &cards, 0x1234_5678, now()).expect("the stack");
        if let Some(trust) = trust {
            stack.install_tls(trust).expect("TLS trust");
        }
        let shared = Shared::new(stack);
        let mut d = Dispatcher::new();
        for (listener, policy) in theirs {
            d.listen(listener, factory(Rc::clone(&shared), policy))
                .expect("listen");
        }
        loop {
            after_turn(&shared, &mut d, now());
            let deadline = {
                let mut st = shared.stack.borrow_mut();
                wire(&mut st, &mut rng);
                let t = now();
                st.poll_at()
                    .unwrap_or(t + 50_000_000)
                    .clamp(t, t + 50_000_000)
            };
            if d.channels() == 0 || stop.load(Ordering::SeqCst) {
                break;
            }
            if d.turn(deadline).expect("a turn") == Turn::Empty {
                break;
            }
        }
    });
    (ours, join)
}

/// Test-only byte collection from the canonical caller-owned Memory operation.
/// Production clients retain and consume their charged destination directly.
pub fn receive(
    client: &mut netstack_protocol::net::Client,
    socket: u32,
    max: u32,
) -> Result<Vec<u8>, moruna_svc::CallError<netstack_protocol::net::Error>> {
    use moruna_svc::{CallError, Memory, Rights, map};
    let size = u64::from(max.max(1)).div_ceil(4096) * 4096;
    let memory = Memory::create_in(size, &client.account().0).map_err(CallError::Kernel)?;
    let loan = Memory(
        memory
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .map_err(CallError::Kernel)?,
    );
    let count = client.receive_into(socket, loan, 0, max)?;
    if count > max {
        return Err(CallError::Malformed);
    }
    let mut bytes = vec![0; count as usize];
    memory
        .map(0, size, map::READ)
        .map_err(CallError::Kernel)?
        .read(0, &mut bytes)
        .map_err(CallError::Kernel)?;
    Ok(bytes)
}

/// Test-only byte fixture staging into the sole net3 caller-owned TX operation.
pub fn send(
    client: &mut netstack_protocol::net::Client,
    socket: u32,
    bytes: &[u8],
) -> Result<u32, moruna_svc::CallError<netstack_protocol::net::Error>> {
    use moruna_svc::{CallError, Memory, Rights, map};
    let capacity = (bytes.len().max(1) as u64).div_ceil(4096) * 4096;
    let memory = Memory::create_in(capacity, &client.account().0).map_err(CallError::Kernel)?;
    memory
        .map(0, capacity, map::WRITE)
        .and_then(|m| m.write(0, bytes))
        .map_err(CallError::Kernel)?;
    let loan = Memory(
        memory
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .map_err(CallError::Kernel)?,
    );
    client.send_from(socket, loan, 0, bytes.len() as u32)
}

/// A DHCP server for card 0: offers 10.0.2.15/24, DNS 10.0.2.3 and, with `router`, the
/// router 10.0.2.2, and acknowledges the request for it.
pub fn dhcp_server(router: bool) -> Wire {
    Box::new(move |st: &mut Stack, _: &mut Rng| {
        for f in std::mem::take(&mut st.card_mut(0).port.tx) {
            if let Some(answer) = dhcp_answer(&f, router) {
                st.card_mut(0).port.deliver(answer);
            }
        }
    })
}

/// That server's answer to the frame `f`: an offer for a discover and an acknowledgement
/// for a request, and nothing for any other frame.
pub fn dhcp_answer(f: &[u8], router: bool) -> Option<Vec<u8>> {
    let server_mac = EthernetAddress([0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]);
    let server_ip = Ipv4Address::new(10, 0, 2, 2);
    let (client, xid, kind) = dhcp_request(f)?;
    let reply_type = match kind {
        DhcpMessageType::Discover => DhcpMessageType::Offer,
        DhcpMessageType::Request => DhcpMessageType::Ack,
        _ => return None,
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
        router: router.then_some(server_ip),
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
    Some(dhcp_frame(server_mac, server_ip, &repr))
}

/// The client's address, transaction and message type, if `f` is a DHCP request.
pub fn dhcp_request(f: &[u8]) -> Option<(EthernetAddress, u32, DhcpMessageType)> {
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
