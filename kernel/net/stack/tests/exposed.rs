//! The network server on a public port (system design 1C and 2B, work item 1.3b): the
//! first code anyone on the internet reaches, so nothing they send may crash it or
//! exhaust its memory. The peers here are raw frames on the card's wire, as an
//! attacker's are: they send what they like, whatever the node advertised.

mod common;

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use common::{MAC_A, Rng, Wire, serve, serve_until};
use moruna_netstack::Stack;
use moruna_svc::{Channel, deadline_in};
use netstack_protocol::{Addr, Endpoint, net};
use smoltcp::{
    phy::ChecksumCapabilities,
    wire::{
        ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
        EthernetRepr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket,
        TcpRepr, TcpSeqNumber,
    },
};

const SECOND: u64 = 1_000_000_000;
const NODE: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]);
/// The port the node listens on, as `api` does.
const PORT: u16 = 443;
/// The most a segment of a peer here carries.
const MSS: usize = 1460;

fn node_mac() -> EthernetAddress {
    let b = MAC_A.to_be_bytes();
    EthernetAddress([b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// A TCP segment the node sent to a peer.
#[derive(Clone, Copy, Debug)]
struct Segment {
    /// The peer's address and port.
    to: (Ipv4Address, u16),
    control: TcpControl,
    seq: u32,
    ack: Option<u32>,
    window: u16,
    /// The window scale a SYN offers.
    scale: Option<u8>,
}

/// The internet beyond the node's gateway: answers the node's ARP for the gateway, and
/// carries segments between the node and peers at any address.
struct Internet {
    /// The node has been told where the gateway is.
    introduced: bool,
}

impl Internet {
    fn new() -> Internet {
        Internet { introduced: false }
    }

    /// What the node's card sent: the ARP requests it made are answered, and its TCP
    /// segments are returned.
    fn take(&mut self, st: &mut Stack) -> Vec<Segment> {
        if !self.introduced {
            // The gateway asks for the node, so the node knows where the gateway is
            // before its first answer has to go through it.
            st.card_mut(0).port.deliver(arp(
                ArpOperation::Request,
                (GATEWAY_MAC, GATEWAY),
                (EthernetAddress([0; 6]), NODE),
                EthernetAddress::BROADCAST,
            ));
            self.introduced = true;
        }
        let mut segments = Vec::new();
        for f in std::mem::take(&mut st.card_mut(0).port.tx) {
            let Ok(eth) = EthernetFrame::new_checked(&f[..]) else {
                continue;
            };
            match eth.ethertype() {
                EthernetProtocol::Arp => {
                    if let Ok(ArpRepr::EthernetIpv4 {
                        operation: ArpOperation::Request,
                        source_hardware_addr,
                        source_protocol_addr,
                        target_protocol_addr,
                        ..
                    }) = ArpPacket::new_checked(eth.payload()).and_then(|p| ArpRepr::parse(&p))
                        && target_protocol_addr == GATEWAY
                    {
                        st.card_mut(0).port.deliver(arp(
                            ArpOperation::Reply,
                            (GATEWAY_MAC, GATEWAY),
                            (source_hardware_addr, source_protocol_addr),
                            source_hardware_addr,
                        ));
                    }
                }
                EthernetProtocol::Ipv4 => {
                    if let Some(s) = segment_of(eth.payload()) {
                        segments.push(s);
                    }
                }
                _ => {}
            }
        }
        segments
    }
}

fn arp(
    operation: ArpOperation,
    from: (EthernetAddress, Ipv4Address),
    to: (EthernetAddress, Ipv4Address),
    dst: EthernetAddress,
) -> Vec<u8> {
    let repr = ArpRepr::EthernetIpv4 {
        operation,
        source_hardware_addr: from.0,
        source_protocol_addr: from.1,
        target_hardware_addr: to.0,
        target_protocol_addr: to.1,
    };
    let mut buf = vec![0u8; 14 + repr.buffer_len()];
    let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
    EthernetRepr {
        src_addr: from.0,
        dst_addr: dst,
        ethertype: EthernetProtocol::Arp,
    }
    .emit(&mut frame);
    repr.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
    buf
}

fn segment_of(ip: &[u8]) -> Option<Segment> {
    let caps = ChecksumCapabilities::default();
    let packet = Ipv4Packet::new_checked(ip).ok()?;
    let repr = Ipv4Repr::parse(&packet, &caps).ok()?;
    if repr.next_header != IpProtocol::Tcp || repr.src_addr != NODE {
        return None;
    }
    let tcp = TcpPacket::new_checked(packet.payload()).ok()?;
    let t = TcpRepr::parse(&tcp, &repr.src_addr.into(), &repr.dst_addr.into(), &caps).ok()?;
    Some(Segment {
        to: (repr.dst_addr, t.dst_port),
        control: t.control,
        seq: t.seq_number.0 as u32,
        ack: t.ack_number.map(|a| a.0 as u32),
        window: t.window_len,
        scale: t.window_scale,
    })
}

/// A segment from the peer at `from` to the node's port; a SYN asks for no window
/// scaling.
fn to_node(
    from: (Ipv4Address, u16),
    control: TcpControl,
    seq: u32,
    ack: Option<u32>,
    payload: &[u8],
) -> Vec<u8> {
    to_node_scaled(from, control, seq, ack, payload, None)
}

/// The same, with a SYN's window scale.
fn to_node_scaled(
    from: (Ipv4Address, u16),
    control: TcpControl,
    seq: u32,
    ack: Option<u32>,
    payload: &[u8],
    window_scale: Option<u8>,
) -> Vec<u8> {
    let caps = ChecksumCapabilities::default();
    let tcp = TcpRepr {
        src_port: from.1,
        dst_port: PORT,
        control,
        seq_number: TcpSeqNumber(seq as i32),
        ack_number: ack.map(|a| TcpSeqNumber(a as i32)),
        window_len: 65535,
        window_scale,
        max_seg_size: (control == TcpControl::Syn).then_some(MSS as u16),
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload,
    };
    let ip = Ipv4Repr {
        src_addr: from.0,
        dst_addr: NODE,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let mut buf = vec![0u8; 14 + ip.buffer_len() + tcp.buffer_len()];
    let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
    EthernetRepr {
        src_addr: GATEWAY_MAC,
        dst_addr: node_mac(),
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut frame);
    let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
    ip.emit(&mut packet, &caps);
    tcp.emit(
        &mut TcpPacket::new_unchecked(packet.payload_mut()),
        &from.0.into(),
        &NODE.into(),
        &caps,
    );
    buf
}

/// `n` bytes from the peer at `from`, starting at `seq`, in segments of at most [`MSS`].
fn bytes_to_node(from: (Ipv4Address, u16), seq: u32, ack: u32, n: usize) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut sent = 0;
    while sent < n {
        let k = MSS.min(n - sent);
        frames.push(to_node(
            from,
            TcpControl::Psh,
            seq.wrapping_add(sent as u32),
            Some(ack),
            &vec![0x5a; k],
        ));
        sent += k;
    }
    frames
}

/// A node with one interface behind a gateway, its `settings`, and `grant` for `api`.
fn node(grant: &str, settings: &str) -> String {
    format!(
        "moruna-netstack 1\n{settings}interface wan card=net0 ipv4=10.0.2.15/24 gateway4=10.0.2.2\n{grant}\n"
    )
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

/// Whether `done` holds within `within`, asked every 10 ms.
fn until(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

/// What a peer saw of the node's side of one connection.
#[derive(Default)]
struct Seen {
    /// The node's first sequence number, plus one: where its bytes start.
    theirs: Option<u32>,
    /// The node's window scale, when the peer offered scaling and the node took it.
    shift: u8,
    /// The furthest right edge of a window the node advertised (RFC 9293, 3.8.6).
    edge: Option<u32>,
    /// The furthest the node acknowledged.
    acked: Option<u32>,
    /// The node acknowledged a byte past every window it had advertised.
    past_edge: bool,
    reset: bool,
}

impl Seen {
    fn observe(&mut self, s: &Segment, offered_scale: bool) {
        if s.control == TcpControl::Rst {
            self.reset = true;
            return;
        }
        let Some(ack) = s.ack else { return };
        if s.control == TcpControl::Syn {
            self.theirs = Some(s.seq.wrapping_add(1));
            if offered_scale {
                self.shift = s.scale.unwrap_or(0);
            }
        } else if self.edge.is_some_and(|e| ack > e) {
            self.past_edge = true;
        }
        // A SYN's window is never scaled (RFC 7323, 2.2).
        let shift = if s.control == TcpControl::Syn {
            0
        } else {
            self.shift
        };
        let edge = ack.wrapping_add(u32::from(s.window) << shift);
        self.edge = Some(self.edge.map_or(edge, |e| e.max(edge)));
        self.acked = Some(self.acked.map_or(ack, |a| a.max(ack)));
    }
}

/// A node with a listener on 443 and, on its wire, one peer at `from`: it sends `first`
/// once the listener is up, then, for each segment the node sends it, what `peer`
/// answers. `check` runs while the node serves; then the node must still answer.
fn against(
    config: &str,
    from: (Ipv4Address, u16),
    offered_scale: bool,
    first: Vec<u8>,
    mut peer: impl FnMut(&Segment, &Seen) -> Vec<Vec<u8>> + Send + 'static,
    check: impl FnOnce(&Mutex<Seen>),
) {
    let listening = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Seen::default()));
    let wire: Wire = {
        let (listening, seen) = (listening.clone(), seen.clone());
        let mut internet = Internet::new();
        let mut first = Some(first);
        Box::new(move |st: &mut Stack, _: &mut Rng| {
            let segments = internet.take(st);
            if !listening.load(Ordering::SeqCst) {
                return;
            }
            if let Some(f) = first.take() {
                st.card_mut(0).port.deliver(f);
            }
            let mut seen = seen.lock().unwrap();
            for s in segments.iter().filter(|s| s.to == from) {
                seen.observe(s, offered_scale);
                for f in peer(s, &seen) {
                    st.card_mut(0).port.deliver(f);
                }
            }
        })
    };
    let (mut slots, join) = serve(config, &[MAC_A], wire);
    let listener = slots.remove("listen-api").expect("slot");
    let mut api = client(&listener);
    api.listen(Endpoint::new(Addr::v4(10, 0, 2, 15), PORT), 8)
        .expect("the listener on 443");
    listening.store(true, Ordering::SeqCst);
    check(&seen);
    // The node is alive: a panic would have ended its loop and closed this channel.
    api.info().expect("the node serves on");
    drop(api);
    drop(listener);
    drop(slots);
    join.join().expect("the server");
}

/// The work item's reproduction. smoltcp 0.12.0 took every byte its buffer had room
/// for, advertised or not, and then worked out its last window as `last_ack +
/// last_win - next_ack`, past zero: "attempt to subtract sequence numbers with
/// underflow" (`socket/tcp.rs:698`), a panic in a debug build and a wrong window in a
/// release one. Here the buffer is filled, within every window the node offers, to 5
/// bytes short, which a scale of 8 rounds to a window of 0; then 5 bytes come anyway,
/// in one segment, so that the delayed acknowledgement leaves the window update to be
/// worked out first.
#[test]
fn a_window_overrun_by_a_few_bytes_is_not_an_underflow() {
    const ISN: u32 = 7000;
    const FROM: (Ipv4Address, u16) = (Ipv4Address::new(203, 0, 113, 9), 40001);
    const FULL: u32 = ISN + 1 + 262_144 - 5;
    let mut next = None;
    let mut overrun = false;
    against(
        &node(
            "grant listen-api rights=listen local=0.0.0.0/0@443 sockets=81",
            "setting tcp-receive-buffer 262144\n",
        ),
        FROM,
        true,
        to_node_scaled(FROM, TcpControl::Syn, ISN, None, &[], Some(0)),
        move |s, seen| {
            let (Some(theirs), Some(edge)) = (seen.theirs, seen.edge) else {
                return vec![];
            };
            let mut f = Vec::new();
            let at = *next.get_or_insert_with(|| {
                assert_eq!(seen.shift, 3, "a 256 KiB buffer is advertised in eighths");
                f.push(to_node(FROM, TcpControl::None, ISN + 1, Some(theirs), &[]));
                ISN + 1
            });
            // Within the window the node last offered, up to 5 bytes short of full.
            let upto = edge.min(FULL);
            if upto > at {
                f.extend(bytes_to_node(FROM, at, theirs, (upto - at) as usize));
                next = Some(upto);
            }
            if !overrun && s.ack == Some(FULL) && s.window == 0 {
                overrun = true;
                f.extend(bytes_to_node(FROM, FULL, theirs, 5));
            }
            f
        },
        |seen| {
            let full = until(Duration::from_secs(10), || {
                seen.lock().unwrap().acked == Some(FULL)
            });
            // Time for the 5 bytes to arrive and be answered.
            thread::sleep(Duration::from_millis(500));
            let seen = seen.lock().unwrap();
            assert!(full, "the node acknowledged {:?}", seen.acked);
            assert!(!seen.reset, "the connection was reset");
            assert!(
                !seen.past_edge,
                "the node acknowledged {:?}, past the window it advertised",
                seen.acked.map(|a| a.wrapping_sub(ISN + 1))
            );
        },
    );
}

/// A peer that sends a whole window and four segments more at once, without window
/// scaling, is acknowledged for the window and not a byte past it (smoltcp 0.12.0
/// acknowledged all of it).
#[test]
fn a_peer_that_sends_past_its_window_is_acknowledged_for_the_window_only() {
    const ISN: u32 = 1000;
    const FROM: (Ipv4Address, u16) = (Ipv4Address::new(203, 0, 113, 7), 40000);
    let mut sent = false;
    against(
        &node(
            "grant listen-api rights=listen local=0.0.0.0/0@443 sockets=81",
            "",
        ),
        FROM,
        false,
        to_node(FROM, TcpControl::Syn, ISN, None, &[]),
        move |s, seen| {
            if sent || s.control != TcpControl::Syn {
                return vec![];
            }
            sent = true;
            let theirs = seen.theirs.unwrap_or(0);
            let mut f = vec![to_node(FROM, TcpControl::None, ISN + 1, Some(theirs), &[])];
            f.extend(bytes_to_node(
                FROM,
                ISN + 1,
                theirs,
                usize::from(s.window) + 4 * MSS,
            ));
            f
        },
        |seen| {
            until(Duration::from_secs(10), || {
                seen.lock().unwrap().acked > Some(ISN + 1)
            });
            thread::sleep(Duration::from_millis(500));
            let seen = seen.lock().unwrap();
            assert!(!seen.reset, "the connection was reset");
            assert!(seen.acked > Some(ISN + 1), "nothing was acknowledged");
            assert!(
                !seen.past_edge,
                "the node acknowledged {:?} bytes, past the window it advertised",
                seen.acked.map(|a| a.wrapping_sub(ISN + 1))
            );
        },
    );
}

/// `api`'s grant in the cloud profile (work item 1.3b): 80 connections and the listener,
/// each socket with 64 KiB to receive and 64 KiB to send.
const API_GRANT: &str =
    "grant listen-api rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536";
const API_SOCKETS: usize = 81;
const API_BUFFERS: usize = 65536 + 65536;

/// What the crowd of abandoning peers did and saw, and the most the node held.
#[derive(Default)]
struct Crowd {
    /// SYNs sent; handshakes the node finished, each then abandoned; SYNs it reset.
    sent: u32,
    opened: u32,
    refused: u32,
    /// SYNs neither answered nor reset within a second.
    unanswered: u32,
    /// The most sockets, and bytes of their buffers, the node held after any poll.
    most_sockets: usize,
    most_buffered: usize,
    /// The well-behaved peer that comes afterwards: its handshake finished, and the
    /// node's FIN, which is `api` closing it, seen.
    valid_opened: bool,
    valid_closed: bool,
}

/// The work item's success check. 10,000 connections are opened against `api`'s grant
/// and abandoned: each peer finishes its handshake and then says nothing again, not
/// even to the FIN `api`'s close sends. The node never holds more than the grant's 81
/// sockets, nor more than 81 times 128 KiB of their buffers; a SYN that finds the grant
/// full is answered with a reset; and a well-behaved connection afterwards is served.
/// The linger is a tenth of a second here (30 s by default), so that the 10,000 pass
/// in seconds: an abandoned connection is reset, and freed, once it has passed.
#[test]
fn ten_thousand_abandoned_connections_never_exceed_the_grant_and_the_next_is_served() {
    const TARGET: u32 = 10_000;
    const BURST: u32 = 100;
    const VALID: (Ipv4Address, u16) = (Ipv4Address::new(192, 0, 2, 10), 50000);
    const VALID_ISN: u32 = 5000;
    let attacking = Arc::new(AtomicBool::new(false));
    let crowd = Arc::new(Mutex::new(Crowd::default()));
    let wire: Wire = {
        let (attacking, crowd) = (attacking.clone(), crowd.clone());
        let mut internet = Internet::new();
        // Each SYN not yet answered, and when it went.
        let mut waiting: BTreeMap<(Ipv4Address, u16), Instant> = BTreeMap::new();
        let mut last_burst = Instant::now();
        let mut valid_syn: Option<Instant> = None;
        Box::new(move |st: &mut Stack, _: &mut Rng| {
            let segments = internet.take(st);
            let mut crowd = crowd.lock().unwrap();
            crowd.most_sockets = crowd.most_sockets.max(st.sockets());
            crowd.most_buffered = crowd.most_buffered.max(st.buffered());
            for s in &segments {
                if s.to == VALID {
                    match s.control {
                        TcpControl::Syn if !crowd.valid_opened => {
                            crowd.valid_opened = true;
                            st.card_mut(0).port.deliver(to_node(
                                VALID,
                                TcpControl::None,
                                VALID_ISN + 1,
                                Some(s.seq.wrapping_add(1)),
                                &[],
                            ));
                        }
                        TcpControl::Fin if crowd.valid_opened && !crowd.valid_closed => {
                            // `api` closed it: the peer closes its half too.
                            crowd.valid_closed = true;
                            st.card_mut(0).port.deliver(to_node(
                                VALID,
                                TcpControl::Fin,
                                VALID_ISN + 1,
                                Some(s.seq.wrapping_add(1)),
                                &[],
                            ));
                        }
                        TcpControl::Rst if !crowd.valid_opened => valid_syn = None,
                        _ => {}
                    }
                    continue;
                }
                if waiting.remove(&s.to).is_none() {
                    // An abandoned connection's FIN, its retransmissions, its reset:
                    // nobody answers.
                    continue;
                }
                match s.control {
                    TcpControl::Syn => {
                        crowd.opened += 1;
                        // The handshake's end, and then nothing, ever.
                        st.card_mut(0).port.deliver(to_node(
                            s.to,
                            TcpControl::None,
                            1,
                            Some(s.seq.wrapping_add(1)),
                            &[],
                        ));
                    }
                    TcpControl::Rst => crowd.refused += 1,
                    _ => {
                        waiting.insert(s.to, Instant::now());
                    }
                }
            }
            let before = waiting.len();
            waiting.retain(|_, at| at.elapsed() < Duration::from_secs(1));
            crowd.unanswered += (before - waiting.len()) as u32;
            if !attacking.load(Ordering::SeqCst) {
                return;
            }
            if crowd.opened < TARGET {
                if waiting.is_empty() && last_burst.elapsed() >= Duration::from_millis(10) {
                    last_burst = Instant::now();
                    for _ in 0..BURST {
                        // Every peer from an address of its own, as the internet's are.
                        let n = crowd.sent;
                        let from = (
                            Ipv4Address::from(0xc612_0000u32 + n),
                            40000 + (n % 20000) as u16,
                        );
                        waiting.insert(from, Instant::now());
                        st.card_mut(0)
                            .port
                            .deliver(to_node(from, TcpControl::Syn, 0, None, &[]));
                        crowd.sent += 1;
                    }
                }
            } else if !crowd.valid_opened
                && valid_syn.is_none_or(|at| at.elapsed() >= Duration::from_millis(100))
            {
                // Afterwards, a well-behaved peer, which tries again after a reset.
                valid_syn = Some(Instant::now());
                st.card_mut(0)
                    .port
                    .deliver(to_node(VALID, TcpControl::Syn, VALID_ISN, None, &[]));
            }
        })
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (mut slots, server) = serve_until(
        &node(API_GRANT, "setting tcp-linger-ms 100\n"),
        &[MAC_A],
        wire,
        None,
        stop.clone(),
    );
    let listener = slots.remove("listen-api").expect("slot");
    // `api` in step 1: every connection accepted and closed at once.
    let accepted = Arc::new(AtomicU32::new(0));
    let valid_accepted = Arc::new(AtomicBool::new(false));
    let api = {
        let mut api = client(&listener);
        let (accepted, valid_accepted) = (accepted.clone(), valid_accepted.clone());
        let (l, _) = api
            .listen(Endpoint::new(Addr::v4(10, 0, 2, 15), PORT), 64)
            .expect("the listener on 443");
        thread::spawn(move || {
            while let Ok(c) = api.accept(l) {
                accepted.fetch_add(1, Ordering::SeqCst);
                if c.remote == Endpoint::new(Addr::v4(192, 0, 2, 10), VALID.1) {
                    valid_accepted.store(true, Ordering::SeqCst);
                }
                let _ = api.close(c.socket);
            }
        })
    };
    attacking.store(true, Ordering::SeqCst);
    let served = until(Duration::from_secs(600), || {
        crowd.lock().unwrap().valid_closed && valid_accepted.load(Ordering::SeqCst)
    });
    stop.store(true, Ordering::SeqCst);
    server.join().expect("the server did not panic");
    api.join().expect("the api thread");
    let crowd = crowd.lock().unwrap();
    eprintln!(
        "{} SYNs: {} opened and abandoned, {} reset at the bound, {} unanswered; {} accepted; \
         at most {} sockets and {} bytes of buffers",
        crowd.sent,
        crowd.opened,
        crowd.refused,
        crowd.unanswered,
        accepted.load(Ordering::SeqCst),
        crowd.most_sockets,
        crowd.most_buffered
    );
    assert!(crowd.opened >= TARGET, "only {} opened", crowd.opened);
    assert!(
        crowd.most_sockets <= API_SOCKETS,
        "{} sockets held",
        crowd.most_sockets
    );
    assert!(
        crowd.most_buffered <= API_SOCKETS * API_BUFFERS,
        "{} bytes of buffers held",
        crowd.most_buffered
    );
    // The bound was reached, and what found it full was told so.
    assert!(crowd.refused > 0, "no SYN found the grant full");
    assert_eq!(crowd.unanswered, 0, "SYNs left unanswered");
    assert!(
        served,
        "the well-behaved connection afterwards was not served"
    );
}
