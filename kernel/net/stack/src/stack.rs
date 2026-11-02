//! The stack (SDD 31, f.1 to f.5): smoltcp's interfaces, one a card and loopback, the
//! sockets of every connection, and the requests that wait.
//!
//! Each interface has its own smoltcp `Interface` and socket set, so a socket's
//! packets leave by the interface its local address is on. A poll takes every frame
//! the cards delivered one at a time (so a listener that took a SYN is given a new
//! listening socket before the next frame), answers the requests that can now be
//! answered, and sends what the sockets have to send; loopback's frames go round
//! again within the poll.
//!
//! Every value from a frame is smoltcp's to check: it parses each header and length
//! before it believes it, and a frame it cannot parse is dropped. What this module
//! takes from the wire beyond smoltcp is an echo reply's identifier and sequence
//! number, read only through smoltcp's checked packet views. Every handle into a
//! socket set is held in exactly one place (a connection's socket, a listener's
//! queues, an orphan, an interface's own sockets) and removed from it when the socket
//! is removed, since smoltcp panics on a stale handle.

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::String,
    vec,
    vec::Vec,
};
use core::net::{IpAddr, Ipv6Addr};

use moruna_svc::{Context, Outcome, Pending, ProtocolError};
use netstack_protocol::{
    Addr, BACKLOG_MAX, Cidr, Endpoint, Info, Interface as IfaceInfo, Opened, PING_MAX, Policy,
    RECEIVE_MAX, SEND_MAX, SocketState, SocketStatus, Target, net, normalise_name, rights,
};
use smoltcp::{
    iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet},
    socket::{dhcpv4, dns, icmp, tcp, udp},
    time::{Duration, Instant},
    wire::{
        DnsQueryType, EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, Icmpv6Packet,
        Icmpv6Repr, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint,
    },
};

use crate::{
    config::{Config, Congestion, Settings},
    device::Port,
};

/// The first port a connect or a port-0 listener may be given (RFC 6335).
const EPHEMERAL_FIRST: u16 = 49152;
/// Rounds of loopback a poll takes before it leaves the rest for the next.
const LOOPBACK_ROUNDS: usize = 32;

fn fail(e: net::Error) -> Outcome {
    Outcome::Failure(e.code())
}

fn ip(a: &Addr) -> IpAddress {
    IpAddress::from(a.0)
}

fn addr(a: IpAddress) -> Addr {
    Addr(IpAddr::from(a))
}

fn endpoint(e: IpEndpoint) -> Endpoint {
    Endpoint::new(addr(e.addr), e.port)
}

fn instant(ns: u64) -> Instant {
    Instant::from_micros((ns / 1000) as i64)
}

fn ns(i: Instant) -> u64 {
    (i.total_micros().max(0) as u64).saturating_mul(1000)
}

fn ms(v: u64) -> Option<Duration> {
    (v != 0).then(|| Duration::from_millis(v))
}

/// The IPv6 link-local address of a card's MAC (RFC 4291, appendix A).
fn link_local(mac: u64) -> Addr {
    let m = netstack_mac(mac);
    Addr(IpAddr::V6(Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([m[0] ^ 2, m[1]]),
        u16::from_be_bytes([m[2], 0xff]),
        u16::from_be_bytes([0xfe, m[3]]),
        u16::from_be_bytes([m[4], m[5]]),
    )))
}

fn netstack_mac(mac: u64) -> [u8; 6] {
    let b = mac.to_be_bytes();
    [b[2], b[3], b[4], b[5], b[6], b[7]]
}

fn is_link_local(a: &Addr) -> bool {
    matches!(a.0, IpAddr::V6(v) if v.segments()[0] & 0xffc0 == 0xfe80)
}

/// What a card's driver says of it when the server opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Card {
    pub mac: u64,
    pub mtu: u32,
    pub up: bool,
}

/// One interface.
pub struct Iface {
    pub name: String,
    /// The card's address; zero for loopback.
    pub mac: u64,
    pub up: bool,
    pub port: Port,
    iface: Interface,
    set: SocketSet<'static>,
    dhcp: Option<SocketHandle>,
    dns: Option<SocketHandle>,
    /// The resolver's servers this interface reaches, as last given to its socket.
    dns_servers: Vec<Addr>,
    icmp: SocketHandle,
    icmp_ident: u16,
    gateway4: Option<Addr>,
    gateway6: Option<Addr>,
    dhcp_dns: Vec<Addr>,
}

impl Iface {
    fn addrs(&self) -> Vec<Cidr> {
        self.iface
            .ip_addrs()
            .iter()
            .map(|c| Cidr::new(addr(c.address()), c.prefix_len()))
            .collect()
    }

    fn has(&self, a: &Addr) -> bool {
        self.iface.has_ip_addr(ip(a))
    }

    /// What `info` says of the interface.
    pub fn info(&self) -> IfaceInfo {
        IfaceInfo {
            name: self.name.clone(),
            mac: self.mac,
            mtu: self.port.mtu() as u32,
            up: self.up,
            dhcp: self.dhcp.is_some(),
            addrs: self.addrs(),
            gateway4: self.gateway4,
            gateway6: self.gateway6,
        }
    }

    /// The largest IP packet the interface carries.
    fn ip_mtu(&self) -> usize {
        self.port.mtu()
    }
}

/// Owned loan of caller-charged storage, retained only until completion/cancel.
struct StreamLoan {
    memory: moruna_svc::Memory,
    offset: u64,
    max: u32,
}
impl StreamLoan {
    fn mapping(&self, writable: bool) -> Result<(moruna_svc::Mapping<'_>, usize), net::Error> {
        let base = self.offset / 4096 * 4096;
        let within = (self.offset - base) as usize;
        let length = (within as u64)
            .checked_add(u64::from(self.max))
            .and_then(|n| n.checked_add(4095))
            .ok_or(net::Error::BadArgument)?
            / 4096
            * 4096;
        self.memory
            .map(
                base,
                length,
                if writable {
                    moruna_svc::map::WRITE
                } else {
                    moruna_svc::map::READ
                },
            )
            .map(|mapping| (mapping, within))
            .map_err(|_| net::Error::BadArgument)
    }
}

struct Tcp {
    tls: Option<crate::tls::Tls>,
    server_identity: Option<u32>,
    tls_required: bool,
    upgrading: Option<Pending>,
    shutdown_pending: Option<Pending>,
    plaintext_used: bool,
    outbound: bool,
    iface: usize,
    handle: SocketHandle,
    local: Endpoint,
    remote: Endpoint,
    /// A connect waiting to be established, and when it gives up.
    opening: Option<(Pending, u64)>,
    receive: Option<(Pending, StreamLoan)>,
    send: Option<(Pending, StreamLoan)>,
    shut: bool,
    established: bool,
    saw_fin: bool,
}

struct Listener {
    iface: usize,
    at: Endpoint,
    backlog: usize,
    /// The one socket listening, when there is room for another connection.
    listening: Option<SocketHandle>,
    /// Sockets that took a SYN and are not yet established.
    half: Vec<SocketHandle>,
    /// Established, waiting for an accept.
    queue: VecDeque<SocketHandle>,
    accept: Option<Pending>,
}

struct Udp {
    iface: usize,
    handle: SocketHandle,
    at: Endpoint,
    receive: Option<Pending>,
    send: Option<(Pending, Endpoint, Vec<u8>)>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "Socket state stays inline in the per-connection map without a separate TCP allocation"
)]
enum Sock {
    Tcp(Tcp),
    Listener(Listener),
    Udp(Udp),
}

struct ServerContext {
    identity: crate::tls::ServerIdentity,
    expires: u64,
}
struct Conn {
    policy: Policy,
    account: Option<moruna_svc::Account>,
    identities: BTreeMap<u32, ServerContext>,
    next_identity: u32,
    next: u32,
    sockets: BTreeMap<u32, Sock>,
    /// Connects by name still resolving, which count against the policy's bound.
    resolving: u32,
}

impl Conn {
    fn held(&self) -> u32 {
        self.sockets.len() as u32 + self.resolving
    }
}

enum Purpose {
    Resolve(Pending),
    Open {
        pending: Pending,
        port: u16,
        from: Option<Addr>,
        /// A name rule permitted the connect, so the address needs no address rule.
        by_rule: bool,
    },
}

struct Resolve {
    conn: u64,
    iface: usize,
    queries: Vec<dns::QueryHandle>,
    addrs: Vec<Addr>,
    deadline: u64,
    purpose: Purpose,
}

struct Ping {
    conn: u64,
    seq: u16,
    sent: u64,
    deadline: u64,
    pending: Pending,
}

/// A TCP socket whose client closed it, or whose connection went, finishing its
/// close.
struct Orphan {
    iface: usize,
    handle: SocketHandle,
    local: Endpoint,
    deadline: u64,
    /// Whether an egress has run since it was closed or reset: a reset smoltcp owes
    /// the peer goes out with the next egress, and only while the socket is in its
    /// set, so a closed orphan is removed only after one.
    sent: bool,
}

pub struct Stack {
    pub settings: Settings,
    trust: Option<crate::tls::Trust>,
    /// Loopback first, then each card's interface in the configuration's order.
    pub ifaces: Vec<Iface>,
    conns: BTreeMap<u64, Conn>,
    dns_static: Vec<Addr>,
    resolves: Vec<Resolve>,
    pings: Vec<Ping>,
    orphans: Vec<Orphan>,
    /// TCP and UDP sockets in every set, the interfaces' own aside.
    smol: usize,
    /// Local endpoints of TCP listeners, of UDP sockets, and of TCP connections (with
    /// how many share one, as accepted connections share their listener's).
    listening: BTreeSet<Endpoint>,
    udp_bound: BTreeSet<Endpoint>,
    tcp_local: BTreeMap<Endpoint, usize>,
    /// Answers ready to send, for the server's loop.
    pub ready: Vec<(Pending, Outcome)>,
    /// Grants to make: a listener for each policy, its endpoint the answer.
    pub grants: Vec<(Pending, Policy)>,
    /// What changed that the server's log should say: a lease taken or lost.
    pub notes: Vec<String>,
    now: u64,
    next_port: u16,
    next_seq: u16,
}

impl Stack {
    /// The stack the configuration describes, over `cards` (one for each of its
    /// interfaces, in order); `seed` seeds smoltcp's choices (sequence numbers,
    /// DHCP transaction ids).
    pub fn new(config: &Config, cards: &[Card], seed: u64, now: u64) -> Result<Stack, String> {
        if cards.len() != config.interfaces.len() {
            return Err(format!(
                "{} cards for {} interfaces",
                cards.len(),
                config.interfaces.len()
            ));
        }
        let s = config.settings.clone();
        let mut ifaces = Vec::new();
        let t = instant(now);
        let mut seeds = seed;
        let mut next_seed = || {
            seeds = seeds
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seeds
        };
        {
            let mut port = Port::loopback(s.loopback_mtu);
            let mut ic = IfaceConfig::new(HardwareAddress::Ip);
            ic.random_seed = next_seed();
            let mut iface = Interface::new(ic, &mut port, t);
            iface.update_ip_addrs(|a| {
                let _ = a.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
                let _ = a.push(IpCidr::new(IpAddress::Ipv6(Ipv6Addr::LOCALHOST), 128));
            });
            ifaces.push(Iface::with(
                String::from("lo"),
                0,
                true,
                port,
                iface,
                None,
                Some(dns::Socket::new(
                    &[],
                    (0..s.dns_queries).map(|_| None).collect::<Vec<_>>(),
                )),
                next_seed() as u16,
                &s,
            ));
        }
        for (c, card) in config.interfaces.iter().zip(cards) {
            if card.mtu < 1280 || card.mtu as usize + 14 > net_protocol_frame_max() {
                return Err(format!(
                    "card {} has an MTU of {}, outside 1280 to {}",
                    c.card,
                    card.mtu,
                    net_protocol_frame_max() - 14
                ));
            }
            let mut port = Port::ethernet(
                card.mtu as usize,
                s.card_queue_frames,
                s.card_sends_in_flight * 64,
            );
            let mut ic = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(
                netstack_mac(card.mac),
            )));
            ic.random_seed = next_seed();
            let mut iface = Interface::new(ic, &mut port, t);
            let mut full = false;
            iface.update_ip_addrs(|a| {
                for cidr in c
                    .addrs
                    .iter()
                    .copied()
                    .chain([Cidr::new(link_local(card.mac), 64)])
                {
                    full |= a.push(IpCidr::new(ip(&cidr.addr), cidr.prefix)).is_err();
                }
            });
            if full {
                return Err(format!(
                    "interface {} has more addresses than smoltcp holds",
                    c.name
                ));
            }
            if let Some(g) = c.gateway4
                && let IpAddr::V4(v) = g.0
            {
                iface
                    .routes_mut()
                    .add_default_ipv4_route(v)
                    .map_err(|_| "no room for a route")?;
            }
            if let Some(g) = c.gateway6
                && let IpAddr::V6(v) = g.0
            {
                iface
                    .routes_mut()
                    .add_default_ipv6_route(v)
                    .map_err(|_| "no room for a route")?;
            }
            let mut x = Iface::with(
                c.name.clone(),
                card.mac,
                card.up,
                port,
                iface,
                c.dhcp.then(dhcpv4::Socket::new),
                Some(dns::Socket::new(
                    &[],
                    (0..s.dns_queries).map(|_| None).collect::<Vec<_>>(),
                )),
                next_seed() as u16,
                &s,
            );
            x.gateway4 = c.gateway4;
            x.gateway6 = c.gateway6;
            ifaces.push(x);
        }
        let mut stack = Stack {
            settings: s,
            trust: None,
            ifaces,
            conns: BTreeMap::new(),
            dns_static: config.dns.clone(),
            resolves: Vec::new(),
            pings: Vec::new(),
            orphans: Vec::new(),
            smol: 0,
            listening: BTreeSet::new(),
            udp_bound: BTreeSet::new(),
            tcp_local: BTreeMap::new(),
            ready: Vec::new(),
            grants: Vec::new(),
            notes: Vec::new(),
            now,
            next_port: EPHEMERAL_FIRST + (seed % 16384) as u16,
            next_seq: 1,
        };
        stack.update_resolvers();
        Ok(stack)
    }

    /// The card interface `card` (0 for the configuration's first interface).
    pub fn card_mut(&mut self, card: usize) -> &mut Iface {
        &mut self.ifaces[card + 1]
    }

    pub fn cards(&self) -> usize {
        self.ifaces.len() - 1
    }

    /// Records a card's link; a DHCP interface whose link comes up asks again.
    pub fn set_link(&mut self, card: usize, up: bool) {
        let i = &mut self.ifaces[card + 1];
        if up
            && !i.up
            && let Some(h) = i.dhcp
        {
            i.set.get_mut::<dhcpv4::Socket>(h).reset();
        }
        i.up = up;
        if !up {
            i.port.rx.clear();
            i.port.tx.clear();
        }
        // A down card cannot provide a resolver or transmit while its link is
        // absent. A new DHCP lease is requested when it comes back.
        self.dhcp(card + 1);
        self.update_resolvers();
    }

    /// Whether nothing is left to finish: no socket closing on its own.
    pub fn idle(&self) -> bool {
        self.orphans.is_empty()
    }

    /// Sockets in the stack's sets, its interfaces' own aside.
    pub fn sockets(&self) -> usize {
        self.smol
    }

    /// When the stack must next be polled, on the clock `poll` is given.
    pub fn poll_at(&mut self) -> Option<u64> {
        let t = instant(self.now);
        let mut at: Option<u64> = None;
        let mut soon = |v: u64| at = Some(at.map_or(v, |a| a.min(v)));
        for i in self.ifaces.iter_mut() {
            if !i.up {
                continue;
            }
            if !i.port.rx.is_empty() {
                soon(self.now);
            }
            if let Some(p) = i.iface.poll_at(t, &i.set) {
                soon(ns(p).max(self.now));
            }
        }
        for c in self.conns.values() {
            for identity in c.identities.values() {
                soon(identity.expires);
            }
            for s in c.sockets.values() {
                if let Sock::Tcp(t) = s
                    && t.tls.as_ref().is_some_and(crate::tls::Tls::needs_poll)
                {
                    soon(self.now);
                }
                if let Sock::Tcp(t) = s
                    && let Some(deadline) = t.tls.as_ref().and_then(crate::tls::Tls::deadline)
                {
                    soon(deadline);
                }
                if let Sock::Tcp(Tcp {
                    opening: Some((_, d)),
                    ..
                }) = s
                {
                    soon(*d);
                }
            }
        }
        for r in &self.resolves {
            soon(r.deadline);
        }
        for p in &self.pings {
            soon(p.deadline);
        }
        for o in &self.orphans {
            soon(o.deadline);
        }
        at
    }

    /// Takes the frames delivered, answers what can be answered, and sends.
    pub fn poll(&mut self, now: u64) {
        self.now = now;
        if let Some(trust) = &self.trust {
            trust.time.update(now);
        }
        self.expire_server_identities();
        let t = instant(now);
        for _ in 0..LOOPBACK_ROUNDS {
            for i in 0..self.ifaces.len() {
                if !self.ifaces[i].up {
                    continue;
                }
                loop {
                    let x = &mut self.ifaces[i];
                    if x.port.rx.is_empty() {
                        break;
                    }
                    let before = x.port.rx.len();
                    let _ = x.iface.poll_ingress_single(t, &mut x.port, &mut x.set);
                    if x.port.rx.len() == before {
                        // No room to answer: the frame waits for the card.
                        break;
                    }
                    self.tend_listeners(Some(i));
                }
                self.dhcp(i);
            }
            self.service();
            for x in self.ifaces.iter_mut() {
                if !x.up {
                    continue;
                }
                let _ = x.iface.poll_egress(t, &mut x.port, &mut x.set);
            }
            for o in self.orphans.iter_mut() {
                o.sent = true;
            }
            let lo = &mut self.ifaces[0].port;
            if lo.tx.is_empty() {
                break;
            }
            for f in core::mem::take(&mut lo.tx) {
                lo.deliver(f);
            }
        }
    }

    // ---- interfaces: DHCP, the resolver's servers, routes ----

    fn dhcp(&mut self, i: usize) {
        let x = &mut self.ifaces[i];
        let Some(h) = x.dhcp else {
            return;
        };
        let event = match x.set.get_mut::<dhcpv4::Socket>(h).poll() {
            None => return,
            Some(dhcpv4::Event::Configured(c)) => Some((
                c.address,
                c.router,
                c.dns_servers.iter().copied().collect::<Vec<_>>(),
            )),
            Some(dhcpv4::Event::Deconfigured) => None,
        };
        x.iface
            .update_ip_addrs(|a| a.retain(|c| !matches!(c.address(), IpAddress::Ipv4(_))));
        x.iface.routes_mut().remove_default_ipv4_route();
        x.gateway4 = None;
        x.dhcp_dns.clear();
        let note = match event {
            Some((cidr, router, servers)) => {
                x.iface.update_ip_addrs(|a| {
                    let _ = a.push(IpCidr::Ipv4(cidr));
                });
                if let Some(r) = router {
                    let _ = x.iface.routes_mut().add_default_ipv4_route(r);
                    x.gateway4 = Some(Addr(IpAddr::V4(r)));
                }
                x.dhcp_dns = servers.into_iter().map(|s| Addr(IpAddr::V4(s))).collect();
                let dns: Vec<String> = x.dhcp_dns.iter().map(|d| format!("{d}")).collect();
                format!(
                    "{}: DHCP bound {}/{}, router {}, dns {}",
                    x.name,
                    cidr.address(),
                    cidr.prefix_len(),
                    x.gateway4.map_or(String::from("none"), |g| format!("{g}")),
                    if dns.is_empty() {
                        String::from("none")
                    } else {
                        dns.join(" ")
                    }
                )
            }
            None => format!("{}: DHCP lease lost", x.name),
        };
        self.notes.push(note);
        self.update_resolvers();
    }

    /// Every resolver server, configured first.
    fn dns_servers(&self) -> Vec<Addr> {
        let mut all = self.dns_static.clone();
        for x in &self.ifaces {
            for s in &x.dhcp_dns {
                if !all.contains(s) {
                    all.push(*s);
                }
            }
        }
        all.truncate(crate::config::DNS_MAX);
        all
    }

    /// Gives each interface's resolver the servers it reaches.
    fn update_resolvers(&mut self) {
        let servers = self.dns_servers();
        for i in 0..self.ifaces.len() {
            let reach: Vec<Addr> = servers
                .iter()
                .copied()
                .filter(|s| self.reaches(i, s))
                .collect();
            let x = &mut self.ifaces[i];
            if let Some(h) = x.dns
                && reach != x.dns_servers
            {
                let list: Vec<IpAddress> = reach.iter().map(ip).collect();
                x.set.get_mut::<dns::Socket>(h).update_servers(&list);
                x.dns_servers = reach;
            }
        }
    }

    /// Whether interface `i` reaches `to`: on one of its networks, or through its
    /// gateway of that family.
    fn reaches(&self, i: usize, to: &Addr) -> bool {
        let x = &self.ifaces[i];
        if !x.up {
            return false;
        }
        x.addrs()
            .iter()
            .any(|c| c.contains(to) && (c.prefix < c.addr.bits() || c.addr == *to))
            || (to.is_v4() && x.gateway4.is_some())
            || (!to.is_v4() && x.gateway6.is_some() && !is_link_local(to))
    }

    /// The interface with `a` as one of its addresses.
    fn iface_of(&self, a: &Addr) -> Option<usize> {
        self.ifaces.iter().position(|x| x.has(a))
    }

    /// The interface and source address for `to`: from `from` if given, else the
    /// first interface on whose network `to` lies with an address the policy may
    /// send from, else the first with a gateway of its family and such an address.
    fn route(
        &self,
        to: &Addr,
        from: Option<&Addr>,
        policy: &Policy,
    ) -> Result<(usize, Addr), net::Error> {
        if let Some(f) = from {
            let i = self.iface_of(f).ok_or(net::Error::BadAddress)?;
            return if self.reaches(i, to) {
                Ok((i, *f))
            } else {
                Err(net::Error::NoRoute)
            };
        }
        let mut refused = false;
        for gateway in [false, true] {
            for (i, x) in self.ifaces.iter().enumerate() {
                if !x.up {
                    continue;
                }
                if gateway && !self.reaches(i, to) {
                    continue;
                }
                for c in x.addrs() {
                    let fits = c.addr.is_v4() == to.is_v4()
                        && is_link_local(&c.addr) == is_link_local(to)
                        && (gateway || c.contains(to));
                    if !fits {
                        continue;
                    }
                    if policy.may_use_source(&c.addr) {
                        return Ok((i, c.addr));
                    }
                    refused = true;
                }
            }
        }
        Err(if refused {
            net::Error::NotPermitted
        } else {
            net::Error::NoRoute
        })
    }

    // ---- connections and their sockets ----

    fn conn(&mut self, id: u64) -> Option<&mut Conn> {
        self.conns.get_mut(&id)
    }

    /// Registers connection `id` holding `policy`, if it is not yet.
    pub fn connection(&mut self, id: u64, policy: &Policy) {
        self.conns.entry(id).or_insert_with(|| Conn {
            policy: policy.clone(),
            account: None,
            identities: BTreeMap::new(),
            next_identity: 1,
            next: 1,
            sockets: BTreeMap::new(),
            resolving: 0,
        });
    }

    pub fn account_bound(&self, conn: u64) -> bool {
        self.conns.get(&conn).is_some_and(|c| c.account.is_some())
    }
    pub fn bind_account(&mut self, conn: u64, handle: moruna_svc::Handle) -> Outcome {
        let Ok((kind, rights)) = handle.info() else {
            return fail(net::Error::BadArgument);
        };
        if kind != moruna_svc::ObjectType::Account
            || !rights.contains(moruna_svc::Rights::READ | moruna_svc::Rights::WRITE)
        {
            return fail(net::Error::BadArgument);
        }
        let Some(c) = self.conns.get_mut(&conn) else {
            return fail(net::Error::NotConnected);
        };
        if c.account.is_some() || !c.sockets.is_empty() || c.resolving != 0 {
            return fail(net::Error::InUse);
        }
        c.account = Some(moruna_svc::Account(handle));
        Outcome::reply(())
    }
    fn memory_matches(&self, conn: u64, memory: &moruna_svc::Memory) -> bool {
        self.conns
            .get(&conn)
            .and_then(|c| c.account.as_ref())
            .is_some_and(|a| a.matches(&memory.0) == Ok(true))
    }
    pub fn install_server_identity(
        &mut self,
        conn: u64,
        control: moruna_svc::Memory,
        control_bytes: u32,
        key: moruna_svc::Memory,
        key_bytes: u32,
    ) -> Outcome {
        if self.policy(conn).rights & rights::LISTEN == 0 {
            return fail(net::Error::NotPermitted);
        }
        if control_bytes == 0 || control_bytes > 65536 || key_bytes == 0 || key_bytes > 16384 {
            return fail(net::Error::BadArgument);
        }
        if self
            .conns
            .values()
            .map(|c| c.identities.len())
            .sum::<usize>()
            >= 8
        {
            return fail(net::Error::TooMany);
        }
        if !self.memory_matches(conn, &control) || !self.memory_matches(conn, &key) {
            return fail(net::Error::BadArgument);
        }
        let Some(trust) = &self.trust else {
            return fail(net::Error::TlsUnavailable);
        };
        let read = |memory: &moruna_svc::Memory,
                    length: u32,
                    destination: &mut [u8]|
         -> Result<(), net::Error> {
            memory
                .map(
                    0,
                    u64::from(length).div_ceil(4096) * 4096,
                    moruna_svc::map::READ,
                )
                .and_then(|map| map.read(0, destination))
                .map_err(|_| net::Error::BadArgument)
        };
        let mut public = Vec::new();
        if public.try_reserve_exact(control_bytes as usize).is_err() {
            return fail(net::Error::TooMany);
        }
        public.resize(control_bytes as usize, 0);
        if let Err(error) = read(&control, control_bytes, &mut public) {
            return fail(error);
        }
        let spec = match netstack_protocol::ServerIdentitySpec::decode(&public) {
            Ok(spec) => spec,
            Err(_) => return fail(net::Error::BadArgument),
        };
        let mut secret = zeroize::Zeroizing::new(Vec::new());
        if secret.try_reserve_exact(key_bytes as usize).is_err() {
            return fail(net::Error::TooMany);
        }
        secret.resize(key_bytes as usize, 0);
        if let Err(error) = read(&key, key_bytes, &mut secret) {
            return fail(error);
        }
        let identity = match crate::tls::ServerIdentity::new(
            spec.certificates,
            &secret,
            spec.alpn,
            trust.time.clone(),
        ) {
            Ok(identity) => identity,
            Err(error) => return fail(tls_error(error)),
        };
        let Some(c) = self.conns.get_mut(&conn) else {
            return fail(net::Error::NotConnected);
        };
        let id = c.next_identity;
        let Some(next) = id.checked_add(1) else {
            return fail(net::Error::TooMany);
        };
        let Some(expires) = self.now.checked_add(spec.lifetime_ns) else {
            return fail(net::Error::BadArgument);
        };
        c.identities.insert(id, ServerContext { identity, expires });
        c.next_identity = next;
        Outcome::reply(id)
    }
    fn expire_server_identities(&mut self) {
        let now = self.now;
        let expired = self
            .conns
            .iter()
            .flat_map(|(conn, c)| {
                c.identities.iter().filter_map(move |(id, context)| {
                    (now >= context.expires || !context.identity.clock_usable())
                        .then_some((*conn, *id))
                })
            })
            .collect::<Vec<_>>();
        for (conn, identity) in expired {
            let _ = self.release_server_identity(conn, identity);
        }
    }
    pub fn release_server_identity(&mut self, conn: u64, identity: u32) -> Outcome {
        let Some(c) = self.conns.get_mut(&conn) else {
            return fail(net::Error::NotConnected);
        };
        if c.identities.remove(&identity).is_none() {
            return fail(net::Error::BadArgument);
        }
        let sockets = c
            .sockets
            .iter()
            .filter_map(|(id, s)| {
                matches!(s,Sock::Tcp(t) if t.server_identity==Some(identity)).then_some(*id)
            })
            .collect::<Vec<_>>();
        for socket in sockets {
            let _ = self.close(conn, socket);
        }
        Outcome::reply(())
    }
    pub fn tls_alpn(&self, conn: u64, socket: u32) -> Outcome {
        let Some(Sock::Tcp(t)) = self.conns.get(&conn).and_then(|c| c.sockets.get(&socket)) else {
            return fail(net::Error::NoSocket);
        };
        let Some(tls) = &t.tls else {
            return fail(net::Error::WrongKind);
        };
        if let Some(error) = tls.error() {
            return fail(tls_error(error));
        }
        if !tls.ready() {
            return fail(net::Error::NotConnected);
        }
        Outcome::reply(tls.alpn().map(|bytes| bytes.to_vec()))
    }
    fn require_tls(&mut self, conn: u64, socket: u32, outbound: bool) {
        if let Some(Sock::Tcp(t)) = self
            .conns
            .get_mut(&conn)
            .and_then(|c| c.sockets.get_mut(&socket))
            && t.outbound == outbound
            && !t.plaintext_used
            && t.receive.is_none()
            && t.send.is_none()
        {
            t.tls_required = true;
        }
    }
    pub fn start_tls_server(
        &mut self,
        cx: &mut Context<'_>,
        conn: u64,
        socket: u32,
        identity: u32,
    ) -> Outcome {
        self.require_tls(conn, socket, false);
        let policy = self.policy(conn);
        if policy.rights & rights::LISTEN == 0 {
            return fail(net::Error::NotPermitted);
        }
        let active = self
            .conns
            .values()
            .flat_map(|c| c.sockets.values())
            .filter(|s| matches!(s,Sock::Tcp(t) if t.tls.is_some()))
            .count();
        if active >= self.settings.tls_sockets_max {
            return fail(net::Error::TooMany);
        }
        let Some(c) = self.conns.get_mut(&conn) else {
            return fail(net::Error::NotConnected);
        };
        let Some(identity_config) = c.identities.get(&identity) else {
            return fail(net::Error::BadArgument);
        };
        if self.now >= identity_config.expires {
            return fail(net::Error::TlsUnavailable);
        }
        let t = match c.sockets.get_mut(&socket) {
            Some(Sock::Tcp(t)) => t,
            Some(_) => return fail(net::Error::WrongKind),
            None => return fail(net::Error::NoSocket),
        };
        if !policy.may_listen(&t.local) {
            return fail(net::Error::NotPermitted);
        }
        let tcp = self.ifaces[t.iface].set.get_mut::<tcp::Socket>(t.handle);
        if t.tls.is_some()
            || t.upgrading.is_some()
            || t.plaintext_used
            || t.receive.is_some()
            || t.send.is_some()
            || tcp.send_queue() > 0
        {
            return fail(net::Error::InUse);
        }
        if t.outbound || !t.established || t.shut || tcp.state() != tcp::State::Established {
            return fail(net::Error::NotConnected);
        }
        let tls = match crate::tls::Tls::server(
            &identity_config.identity,
            self.settings.tls_buffer_bytes,
            self.now
                .saturating_add(self.settings.tls_handshake_timeout_ms * 1_000_000),
        ) {
            Ok(tls) => tls,
            Err(error) => return fail(tls_error(error)),
        };
        t.tls = Some(tls);
        t.server_identity = Some(identity);
        t.upgrading = Some(cx.defer());
        Outcome::Deferred
    }

    /// Connection `id` has gone: its sockets close as their client's close would, its
    /// waiting requests go with it.
    pub fn connection_closed(&mut self, id: u64) {
        let Some(c) = self.conns.remove(&id) else {
            return;
        };
        for (_, s) in c.sockets {
            self.retire(s, None);
        }
        // Its queries are cancelled, so their slots are free for others.
        let ifaces = &mut self.ifaces;
        self.resolves.retain(|r| {
            if r.conn != id {
                return true;
            }
            let x = &mut ifaces[r.iface];
            if let Some(h) = x.dns {
                let socket = x.set.get_mut::<dns::Socket>(h);
                for q in &r.queries {
                    socket.cancel_query(*q);
                }
            }
            false
        });
        self.pings.retain(|p| p.conn != id);
    }

    fn new_tcp(&self) -> tcp::Socket<'static> {
        let s = &self.settings;
        let mut t = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; s.tcp_receive_buffer]),
            tcp::SocketBuffer::new(vec![0; s.tcp_send_buffer]),
        );
        t.set_nagle_enabled(s.tcp_nagle);
        t.set_ack_delay(ms(s.tcp_ack_delay_ms));
        t.set_keep_alive(ms(s.tcp_keepalive_ms));
        t.set_timeout(ms(s.tcp_idle_timeout_ms));
        t.set_congestion_control(match s.tcp_congestion {
            Congestion::Reno => tcp::CongestionControl::Reno,
            Congestion::Cubic => tcp::CongestionControl::Cubic,
            Congestion::None => tcp::CongestionControl::None,
        });
        t
    }

    fn room(&self) -> Result<(), net::Error> {
        if self.smol >= self.settings.sockets_max {
            return Err(net::Error::TooMany);
        }
        Ok(())
    }

    fn remove_socket(&mut self, iface: usize, handle: SocketHandle) {
        self.ifaces[iface].set.remove(handle);
        self.smol -= 1;
    }

    fn unbind_tcp(&mut self, local: &Endpoint) {
        if let Some(n) = self.tcp_local.get_mut(local) {
            *n -= 1;
            if *n == 0 {
                self.tcp_local.remove(local);
            }
        }
    }

    /// A port for `addr` none of its TCP listeners and connections (or UDP sockets,
    /// for `udp`) uses, inside the policy's local rules when given.
    fn free_port(&mut self, addr: &Addr, udp: bool, policy: Option<&Policy>) -> Option<u16> {
        let ranges: Vec<(u16, u16)> = match policy {
            Some(p) => p
                .local_rules_for(addr)
                .map(|r| (r.first.max(1), r.last))
                .collect(),
            None => vec![(EPHEMERAL_FIRST, u16::MAX)],
        };
        let used = |s: &Stack, port: u16| {
            let e = Endpoint::new(*addr, port);
            if udp {
                s.udp_bound.contains(&e)
            } else {
                s.listening.contains(&e) || s.tcp_local.contains_key(&e)
            }
        };
        // The ephemeral range first, where a range reaches it; then the rest of it.
        for (first, last) in ranges {
            let (lo, hi) = if last >= EPHEMERAL_FIRST {
                (first.max(EPHEMERAL_FIRST), last)
            } else {
                (first, last)
            };
            for range in [(lo, hi), (first, last)] {
                if range.0 > range.1 {
                    continue;
                }
                let span = (range.1 - range.0) as u32 + 1;
                let start = (self.next_port as u32) % span;
                for k in 0..span.min(65536) {
                    let port = range.0 + ((start + k) % span) as u16;
                    if !used(self, port) {
                        self.next_port = self.next_port.wrapping_add(1).max(1);
                        return Some(port);
                    }
                }
            }
        }
        None
    }

    fn insert(&mut self, conn: u64, sock: Sock) -> u32 {
        let Some(c) = self.conn(conn) else {
            return 0;
        };
        let id = c.next;
        c.next = c.next.checked_add(1).unwrap_or(1);
        c.sockets.insert(id, sock);
        id
    }

    /// Opens a TCP socket from `src` on interface `iface` to `to`, answering
    /// `pending` once it is established or refused.
    fn start_tcp(
        &mut self,
        conn: u64,
        iface: usize,
        src: Addr,
        to: Endpoint,
        pending: Pending,
    ) -> Result<(), net::Error> {
        self.room()?;
        let port = self
            .free_port(&src, false, None)
            .ok_or(net::Error::TooMany)?;
        let mut sock = self.new_tcp();
        let x = &mut self.ifaces[iface];
        sock.connect(
            x.iface.context(),
            IpEndpoint::new(ip(&to.addr), to.port),
            IpListenEndpoint {
                addr: Some(ip(&src)),
                port,
            },
        )
        .map_err(|_| net::Error::BadAddress)?;
        let handle = x.set.add(sock);
        self.smol += 1;
        let local = Endpoint::new(src, port);
        *self.tcp_local.entry(local).or_insert(0) += 1;
        let deadline = self
            .now
            .saturating_add(self.settings.connect_timeout_ms * 1_000_000);
        self.insert(
            conn,
            Sock::Tcp(Tcp {
                tls: None,
                server_identity: None,
                tls_required: false,
                upgrading: None,
                shutdown_pending: None,
                plaintext_used: false,
                outbound: true,
                iface,
                handle,
                local,
                remote: to,
                opening: Some((pending, deadline)),
                receive: None,
                send: None,
                shut: false,
                established: false,
                saw_fin: false,
            }),
        );
        Ok(())
    }

    /// A socket its client closed, or whose connection went: requests waiting on it are
    /// answered `Closed` (when `ready` is given), and its smoltcp sockets close.
    fn retire(&mut self, sock: Sock, mut ready: Option<&mut Vec<(Pending, Outcome)>>) {
        let mut answer = |p: Pending| {
            if let Some(r) = ready.as_mut() {
                r.push((p, fail(net::Error::Closed)));
            }
        };
        match sock {
            Sock::Tcp(t) => {
                for p in [
                    t.upgrading,
                    t.shutdown_pending,
                    t.opening.map(|o| o.0),
                    t.receive.map(|r| r.0),
                    t.send.map(|s| s.0),
                ]
                .into_iter()
                .flatten()
                {
                    answer(p);
                }
                self.orphan(t.iface, t.handle, t.local);
            }
            Sock::Listener(l) => {
                if let Some(p) = l.accept {
                    answer(p);
                }
                self.listening.remove(&l.at);
                if let Some(h) = l.listening {
                    self.remove_socket(l.iface, h);
                }
                for h in l.half.into_iter().chain(l.queue) {
                    self.ifaces[l.iface].set.get_mut::<tcp::Socket>(h).abort();
                    *self.tcp_local.entry(l.at).or_insert(0) += 1;
                    self.orphans.push(Orphan {
                        iface: l.iface,
                        handle: h,
                        local: l.at,
                        deadline: self.now,
                        sent: false,
                    });
                }
            }
            Sock::Udp(u) => {
                for p in [u.receive, u.send.map(|s| s.0)].into_iter().flatten() {
                    answer(p);
                }
                self.udp_bound.remove(&u.at);
                self.remove_socket(u.iface, u.handle);
            }
        }
    }

    /// A TCP socket that finishes its close on its own: with its FIN after what it
    /// has queued, or at once with a reset when bytes it received were never read.
    fn orphan(&mut self, iface: usize, handle: SocketHandle, local: Endpoint) {
        let s = self.ifaces[iface].set.get_mut::<tcp::Socket>(handle);
        if s.recv_queue() > 0 {
            s.abort();
        } else {
            s.close();
        }
        self.orphans.push(Orphan {
            iface,
            handle,
            local,
            deadline: self
                .now
                .saturating_add(self.settings.tcp_linger_ms * 1_000_000),
            sent: false,
        });
    }

    // ---- the requests (SDD 31, f.4) ----

    fn policy(&self, conn: u64) -> Policy {
        self.conns
            .get(&conn)
            .map(|c| c.policy.clone())
            .unwrap_or_default()
    }

    fn held(&self, conn: u64) -> Result<(), net::Error> {
        let c = self.conns.get(&conn).ok_or(net::Error::NoSocket)?;
        if c.held() >= c.policy.sockets {
            return Err(net::Error::TooMany);
        }
        Ok(())
    }

    pub fn info(&mut self, conn: u64) -> Outcome {
        Outcome::reply(Info {
            interfaces: self.ifaces.iter().map(Iface::info).collect(),
            dns: self.dns_servers(),
            policy: self.policy(conn),
        })
    }

    pub fn open(
        &mut self,
        cx: &mut Context<'_>,
        conn: u64,
        to: Target,
        from: Option<Addr>,
    ) -> Outcome {
        let policy = self.policy(conn);
        let checked = |s: &Stack, family_v4: bool| -> Result<(), net::Error> {
            if let Some(f) = from {
                if !f.is_host() || f.is_v4() != family_v4 {
                    return Err(net::Error::BadAddress);
                }
                if !policy.may_use_source(&f) {
                    return Err(net::Error::NotPermitted);
                }
                if s.iface_of(&f).is_none() {
                    return Err(net::Error::BadAddress);
                }
            }
            Ok(())
        };
        match to {
            Target::Endpoint(ep) => {
                if !ep.addr.is_host() || ep.port == 0 {
                    return fail(net::Error::BadAddress);
                }
                let route = checked(self, ep.addr.is_v4())
                    .and_then(|()| {
                        if policy.may_connect(&ep) {
                            Ok(())
                        } else {
                            Err(net::Error::NotPermitted)
                        }
                    })
                    .and_then(|()| self.held(conn))
                    .and_then(|()| self.route(&ep.addr, from.as_ref(), &policy));
                match route {
                    Ok((iface, src)) => {
                        let pending = cx.defer();
                        if let Err(e) = self.start_tcp(conn, iface, src, ep, pending.clone()) {
                            // Deferred already, so answered through the dispatcher.
                            self.ready.push((pending, fail(e)));
                        }
                        Outcome::Deferred
                    }
                    Err(e) => fail(e),
                }
            }
            Target::Name(name, port) => {
                let Some(name) = normalise_name(&name).filter(|n| n.parse::<IpAddr>().is_err())
                else {
                    return fail(net::Error::BadAddress);
                };
                if port == 0 {
                    return fail(net::Error::BadAddress);
                }
                let by_rule = policy.may_connect_name(&name, port);
                if !by_rule && !(policy.has(rights::CONNECT) && policy.has(rights::RESOLVE)) {
                    return fail(net::Error::NotPermitted);
                }
                if let Some(f) = from
                    && let Err(e) = checked(self, f.is_v4())
                {
                    return fail(e);
                }
                if let Err(e) = self.held(conn) {
                    return fail(e);
                }
                match self.start_resolve(conn, &name) {
                    Ok((iface, queries)) => {
                        if let Some(c) = self.conn(conn) {
                            c.resolving += 1;
                        }
                        self.resolves.push(Resolve {
                            conn,
                            iface,
                            queries,
                            addrs: Vec::new(),
                            deadline: self
                                .now
                                .saturating_add(self.settings.resolve_timeout_ms * 1_000_000),
                            purpose: Purpose::Open {
                                pending: cx.defer(),
                                port,
                                from,
                                by_rule,
                            },
                        });
                        Outcome::Deferred
                    }
                    Err(e) => fail(e),
                }
            }
        }
    }

    pub fn listen(&mut self, conn: u64, at: Endpoint, backlog: u32) -> Outcome {
        if backlog == 0 || backlog > BACKLOG_MAX {
            return fail(net::Error::BadArgument);
        }
        if !at.addr.is_host() {
            return fail(net::Error::BadAddress);
        }
        let policy = self.policy(conn);
        if !policy.has(rights::LISTEN)
            || (at.port != 0 && !policy.may_listen(&at))
            || !policy.may_use_source(&at.addr)
        {
            return fail(net::Error::NotPermitted);
        }
        let Some(iface) = self.iface_of(&at.addr) else {
            return fail(net::Error::BadAddress);
        };
        if let Err(e) = self.held(conn).and_then(|()| self.room()) {
            return fail(e);
        }
        let port = if at.port == 0 {
            match self.free_port(&at.addr, false, Some(&policy)) {
                Some(p) => p,
                None => return fail(net::Error::AddressInUse),
            }
        } else {
            at.port
        };
        let at = Endpoint::new(at.addr, port);
        if self.listening.contains(&at) {
            return fail(net::Error::AddressInUse);
        }
        let Some(h) = self.listening_socket(iface, &at) else {
            return fail(net::Error::BadAddress);
        };
        self.listening.insert(at);
        let id = self.insert(
            conn,
            Sock::Listener(Listener {
                iface,
                at,
                backlog: backlog as usize,
                listening: Some(h),
                half: Vec::new(),
                queue: VecDeque::new(),
                accept: None,
            }),
        );
        Outcome::reply((id, at))
    }

    fn listening_socket(&mut self, iface: usize, at: &Endpoint) -> Option<SocketHandle> {
        let mut s = self.new_tcp();
        s.listen(IpListenEndpoint {
            addr: Some(ip(&at.addr)),
            port: at.port,
        })
        .ok()?;
        self.smol += 1;
        Some(self.ifaces[iface].set.add(s))
    }

    pub fn accept(&mut self, cx: &mut Context<'_>, conn: u64, listener: u32) -> Outcome {
        let full = self.held(conn).is_err();
        let Some(c) = self.conns.get_mut(&conn) else {
            return fail(net::Error::NoSocket);
        };
        let l = match c.sockets.get_mut(&listener) {
            None => return fail(net::Error::NoSocket),
            Some(Sock::Listener(l)) => l,
            Some(_) => return fail(net::Error::WrongKind),
        };
        if l.accept.is_some() {
            return fail(net::Error::InUse);
        }
        if full {
            return fail(net::Error::TooMany);
        }
        match l.queue.pop_front() {
            Some(h) => {
                let iface = l.iface;
                Outcome::reply(self.accepted(conn, iface, h))
            }
            None => {
                l.accept = Some(cx.defer());
                Outcome::Deferred
            }
        }
    }

    /// An established socket from a listener's queue, as a connection's socket.
    fn accepted(&mut self, conn: u64, iface: usize, h: SocketHandle) -> Opened {
        let s = self.ifaces[iface].set.get::<tcp::Socket>(h);
        let unset = Endpoint::new(Addr::v4(0, 0, 0, 0), 0);
        let local = s.local_endpoint().map(endpoint).unwrap_or(unset);
        let remote = s.remote_endpoint().map(endpoint).unwrap_or(unset);
        *self.tcp_local.entry(local).or_insert(0) += 1;
        let socket = self.insert(
            conn,
            Sock::Tcp(Tcp {
                tls: None,
                server_identity: None,
                tls_required: false,
                upgrading: None,
                shutdown_pending: None,
                plaintext_used: false,
                outbound: false,
                iface,
                handle: h,
                local,
                remote,
                opening: None,
                receive: None,
                send: None,
                shut: false,
                established: true,
                saw_fin: false,
            }),
        );
        Opened {
            socket,
            local,
            remote,
        }
    }

    /// The boot's roots and UTC anchor, installed before serving any connection.
    pub fn install_tls(&mut self, trust: crate::tls::Trust) -> Result<(), String> {
        if !self.conns.is_empty() || self.trust.is_some() {
            return Err(String::from("TLS trust is immutable after startup"));
        }
        self.trust = Some(trust);
        Ok(())
    }

    pub fn start_tls(
        &mut self,
        cx: &mut Context<'_>,
        conn: u64,
        socket: u32,
        name: String,
    ) -> Outcome {
        let Some(name) = normalise_name(&name).filter(|n| n.parse::<IpAddr>().is_err()) else {
            return fail(net::Error::BadAddress);
        };
        self.require_tls(conn, socket, true);
        let policy = self.policy(conn);
        let active = self
            .conns
            .values()
            .flat_map(|c| c.sockets.values())
            .filter(|s| matches!(s, Sock::Tcp(t) if t.tls.is_some()))
            .count();
        if active >= self.settings.tls_sockets_max {
            return fail(net::Error::TooMany);
        }
        let Some(trust) = &self.trust else {
            return fail(net::Error::TlsUnavailable);
        };
        let Some(sock) = self
            .conns
            .get_mut(&conn)
            .and_then(|c| c.sockets.get_mut(&socket))
        else {
            return fail(net::Error::NoSocket);
        };
        let Sock::Tcp(t) = sock else {
            return fail(net::Error::WrongKind);
        };
        if !(policy.may_connect_name(&name, t.remote.port)
            || (policy.may_connect(&t.remote) && policy.may_resolve(&name)))
        {
            return fail(net::Error::NotPermitted);
        }
        let s = self.ifaces[t.iface].set.get_mut::<tcp::Socket>(t.handle);
        if t.tls.is_some()
            || t.upgrading.is_some()
            || t.plaintext_used
            || t.receive.is_some()
            || t.send.is_some()
            || s.recv_queue() > 0
            || s.send_queue() > 0
        {
            return fail(net::Error::InUse);
        }
        if !t.outbound || !t.established || t.shut || s.state() != tcp::State::Established {
            return fail(net::Error::NotConnected);
        }
        match crate::tls::Tls::new(
            trust,
            &name,
            self.settings.tls_buffer_bytes,
            self.now
                .saturating_add(self.settings.tls_handshake_timeout_ms * 1_000_000),
        ) {
            Ok(tls) => {
                t.tls = Some(tls);
                t.upgrading = Some(cx.defer());
                Outcome::Deferred
            }
            Err(e) => fail(tls_error(e)),
        }
    }

    pub fn send_from(
        &mut self,
        cx: &mut Context<'_>,
        conn: u64,
        socket: u32,
        memory: moruna_svc::Memory,
        offset: u64,
        length: u32,
    ) -> Outcome {
        if length == 0 || length as usize > SEND_MAX {
            return fail(net::Error::BadArgument);
        }
        if !self.memory_matches(conn, &memory) {
            return fail(net::Error::BadArgument);
        }
        let source = StreamLoan {
            memory,
            offset,
            max: length,
        };
        if source.mapping(false).is_err() {
            return fail(net::Error::BadArgument);
        }
        let Stack { conns, ifaces, .. } = self;
        let t = match conns
            .get_mut(&conn)
            .and_then(|c| c.sockets.get_mut(&socket))
        {
            None => return fail(net::Error::NoSocket),
            Some(Sock::Tcp(t)) => t,
            _ => return fail(net::Error::WrongKind),
        };
        if t.send.is_some() {
            return fail(net::Error::InUse);
        }
        let socket = ifaces[t.iface].set.get_mut::<tcp::Socket>(t.handle);
        match try_send_from(t, socket, &source) {
            Some(Ok(n)) => Outcome::reply(n),
            Some(Err(error)) => fail(error),
            None => {
                t.send = Some((cx.defer(), source));
                Outcome::Deferred
            }
        }
    }

    pub fn receive_into(
        &mut self,
        cx: &mut Context<'_>,
        conn: u64,
        socket: u32,
        memory: moruna_svc::Memory,
        offset: u64,
        max: u32,
    ) -> Outcome {
        if !self.memory_matches(conn, &memory) {
            return fail(net::Error::BadArgument);
        }
        let Stack { conns, ifaces, .. } = self;
        let t = match conns
            .get_mut(&conn)
            .and_then(|c| c.sockets.get_mut(&socket))
        {
            None => return fail(net::Error::NoSocket),
            Some(Sock::Tcp(t)) => t,
            Some(_) => return fail(net::Error::WrongKind),
        };
        if max == 0 || max as usize > RECEIVE_MAX {
            return fail(net::Error::BadArgument);
        }
        let destination = StreamLoan {
            memory,
            offset,
            max,
        };
        if destination.mapping(true).is_err() {
            return fail(net::Error::BadArgument);
        }
        if t.receive.is_some() {
            return fail(net::Error::InUse);
        }
        let s = ifaces[t.iface].set.get_mut::<tcp::Socket>(t.handle);
        match try_receive_into(t, s, &destination) {
            Some(Ok(b)) => Outcome::reply(b),
            Some(Err(e)) => fail(e),
            None => {
                t.receive = Some((cx.defer(), destination));
                Outcome::Deferred
            }
        }
    }

    pub fn shutdown(&mut self, cx: &mut Context<'_>, conn: u64, socket: u32) -> Outcome {
        let now = self.now;
        let close_deadline = now.saturating_add(self.settings.tcp_linger_ms * 1_000_000);
        let Stack {
            conns,
            ifaces,
            ready,
            ..
        } = self;
        let t = match conns
            .get_mut(&conn)
            .and_then(|c| c.sockets.get_mut(&socket))
        {
            None => return fail(net::Error::NoSocket),
            Some(Sock::Tcp(t)) => t,
            Some(_) => return fail(net::Error::WrongKind),
        };
        if t.shutdown_pending.is_some() {
            return fail(net::Error::InUse);
        }
        if let Some(tls) = &t.tls {
            if let Some(e) = tls.error() {
                return fail(tls_error(e));
            }
            if !tls.ready() {
                return fail(net::Error::NotConnected);
            }
        }
        if let Some((p, _)) = t.send.take() {
            ready.push((p, fail(net::Error::NotConnected)));
        }
        t.shut = true;
        if let Some(tls) = &mut t.tls {
            tls.shutdown(close_deadline);
            tls.poll(ifaces[t.iface].set.get_mut::<tcp::Socket>(t.handle), now);
            if let Some(e) = tls.error() {
                return fail(tls_error(e));
            }
            if !tls.close_sent() {
                t.shutdown_pending = Some(cx.defer());
                return Outcome::Deferred;
            }
        } else {
            ifaces[t.iface].set.get_mut::<tcp::Socket>(t.handle).close();
        }
        Outcome::reply(())
    }

    pub fn close(&mut self, conn: u64, socket: u32) -> Outcome {
        let Some(sock) = self.conn(conn).and_then(|c| c.sockets.remove(&socket)) else {
            return fail(net::Error::NoSocket);
        };
        let mut ready = core::mem::take(&mut self.ready);
        self.retire(sock, Some(&mut ready));
        self.ready = ready;
        Outcome::reply(())
    }

    pub fn bind(&mut self, conn: u64, at: Endpoint) -> Outcome {
        if !at.addr.is_host() {
            return fail(net::Error::BadAddress);
        }
        let policy = self.policy(conn);
        if !policy.has(rights::UDP)
            || (at.port != 0 && !policy.may_bind(&at))
            || !policy.may_use_source(&at.addr)
        {
            return fail(net::Error::NotPermitted);
        }
        let Some(iface) = self.iface_of(&at.addr) else {
            return fail(net::Error::BadAddress);
        };
        if let Err(e) = self.held(conn).and_then(|()| self.room()) {
            return fail(e);
        }
        let port = if at.port == 0 {
            match self.free_port(&at.addr, true, Some(&policy)) {
                Some(p) => p,
                None => return fail(net::Error::AddressInUse),
            }
        } else {
            at.port
        };
        let at = Endpoint::new(at.addr, port);
        if self.udp_bound.contains(&at) {
            return fail(net::Error::AddressInUse);
        }
        let s = &self.settings;
        let mut u = udp::Socket::new(
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; s.udp_receive_packets],
                vec![0; s.udp_receive_bytes],
            ),
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; s.udp_send_packets],
                vec![0; s.udp_send_bytes],
            ),
        );
        if u.bind(IpListenEndpoint {
            addr: Some(ip(&at.addr)),
            port,
        })
        .is_err()
        {
            return fail(net::Error::BadAddress);
        }
        let handle = self.ifaces[iface].set.add(u);
        self.smol += 1;
        self.udp_bound.insert(at);
        let id = self.insert(
            conn,
            Sock::Udp(Udp {
                iface,
                handle,
                at,
                receive: None,
                send: None,
            }),
        );
        Outcome::reply((id, at))
    }

    pub fn send_to(
        &mut self,
        cx: &mut Context<'_>,
        conn: u64,
        socket: u32,
        to: Endpoint,
        bytes: Vec<u8>,
    ) -> Outcome {
        let policy = self.policy(conn);
        let (iface, from, waiting) =
            match self.conns.get(&conn).and_then(|c| c.sockets.get(&socket)) {
                None => return fail(net::Error::NoSocket),
                Some(Sock::Udp(u)) => (u.iface, u.at, u.send.is_some()),
                Some(_) => return fail(net::Error::WrongKind),
            };
        if waiting {
            return fail(net::Error::InUse);
        }
        if !to.addr.is_host() || to.port == 0 || to.addr.is_v4() != from.addr.is_v4() {
            return fail(net::Error::BadAddress);
        }
        if !policy.may_send_to(&to) {
            return fail(net::Error::NotPermitted);
        }
        let header = if to.addr.is_v4() { 28 } else { 48 };
        let largest = (self.ifaces[iface].ip_mtu() - header).min(netstack_protocol::DATAGRAM_MAX);
        if bytes.len() > largest || bytes.len() > self.settings.udp_send_bytes {
            return fail(net::Error::TooLarge);
        }
        if !self.reaches(iface, &to.addr) {
            return fail(net::Error::NoRoute);
        }
        let Stack { conns, ifaces, .. } = self;
        let Some(Sock::Udp(u)) = conns
            .get_mut(&conn)
            .and_then(|c| c.sockets.get_mut(&socket))
        else {
            return fail(net::Error::NoSocket);
        };
        let s = ifaces[iface].set.get_mut::<udp::Socket>(u.handle);
        if try_send_to(s, &u.at, &to, &bytes) {
            Outcome::reply(())
        } else {
            u.send = Some((cx.defer(), to, bytes));
            Outcome::Deferred
        }
    }

    pub fn receive_from(&mut self, cx: &mut Context<'_>, conn: u64, socket: u32) -> Outcome {
        let Stack { conns, ifaces, .. } = self;
        let u = match conns
            .get_mut(&conn)
            .and_then(|c| c.sockets.get_mut(&socket))
        {
            None => return fail(net::Error::NoSocket),
            Some(Sock::Udp(u)) => u,
            Some(_) => return fail(net::Error::WrongKind),
        };
        if u.receive.is_some() {
            return fail(net::Error::InUse);
        }
        match try_receive_from(ifaces[u.iface].set.get_mut::<udp::Socket>(u.handle)) {
            Some(v) => Outcome::reply(v),
            None => {
                u.receive = Some(cx.defer());
                Outcome::Deferred
            }
        }
    }

    /// Starts an A and an AAAA query for `name` on the first interface whose resolver
    /// has a server.
    fn start_resolve(
        &mut self,
        conn: u64,
        name: &str,
    ) -> Result<(usize, Vec<dns::QueryHandle>), net::Error> {
        let _ = conn;
        let iface = (0..self.ifaces.len())
            .find(|&i| !self.ifaces[i].dns_servers.is_empty())
            .ok_or(net::Error::NoResolver)?;
        let x = &mut self.ifaces[iface];
        let Some(h) = x.dns else {
            return Err(net::Error::NoResolver);
        };
        let mut queries = Vec::new();
        for kind in [DnsQueryType::A, DnsQueryType::Aaaa] {
            let socket = x.set.get_mut::<dns::Socket>(h);
            match socket.start_query(x.iface.context(), name, kind) {
                Ok(q) => queries.push(q),
                Err(e) => {
                    for q in queries {
                        socket.cancel_query(q);
                    }
                    return Err(match e {
                        dns::StartQueryError::NoFreeSlot => net::Error::TooMany,
                        dns::StartQueryError::InvalidName | dns::StartQueryError::NameTooLong => {
                            net::Error::BadAddress
                        }
                    });
                }
            }
        }
        Ok((iface, queries))
    }

    pub fn resolve(&mut self, cx: &mut Context<'_>, conn: u64, name: String) -> Outcome {
        let Some(name) = normalise_name(&name).filter(|n| n.parse::<IpAddr>().is_err()) else {
            return fail(net::Error::BadAddress);
        };
        if !self.policy(conn).may_resolve(&name) {
            return fail(net::Error::NotPermitted);
        }
        match self.start_resolve(conn, &name) {
            Ok((iface, queries)) => {
                self.resolves.push(Resolve {
                    conn,
                    iface,
                    queries,
                    addrs: Vec::new(),
                    deadline: self
                        .now
                        .saturating_add(self.settings.resolve_timeout_ms * 1_000_000),
                    purpose: Purpose::Resolve(cx.defer()),
                });
                Outcome::Deferred
            }
            Err(e) => fail(e),
        }
    }

    pub fn ping(&mut self, cx: &mut Context<'_>, conn: u64, to: Addr, size: u32) -> Outcome {
        if size > PING_MAX {
            return fail(net::Error::BadArgument);
        }
        if !to.is_host() {
            return fail(net::Error::BadAddress);
        }
        let policy = self.policy(conn);
        if !policy.may_ping(&to) {
            return fail(net::Error::NotPermitted);
        }
        let (iface, _) = match self.route(&to, None, &policy) {
            Ok(r) => r,
            Err(e) => return fail(e),
        };
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        let x = &mut self.ifaces[iface];
        let ident = x.icmp_ident;
        let data: Vec<u8> = (0..size).map(|i| (i as u8) ^ (seq as u8)).collect();
        let socket = x.set.get_mut::<icmp::Socket>(x.icmp);
        let sent = if to.is_v4() {
            let repr = Icmpv4Repr::EchoRequest {
                ident,
                seq_no: seq,
                data: &data,
            };
            socket.send(repr.buffer_len(), ip(&to)).map(|buf| {
                repr.emit(
                    &mut Icmpv4Packet::new_unchecked(buf),
                    &smoltcp::phy::ChecksumCapabilities::ignored(),
                )
            })
        } else {
            let repr = Icmpv6Repr::EchoRequest {
                ident,
                seq_no: seq,
                data: &data,
            };
            // The interface emits the packet again with its own source and checksum.
            let dst = match to.0 {
                IpAddr::V6(v) => v,
                IpAddr::V4(_) => Ipv6Addr::UNSPECIFIED,
            };
            socket.send(repr.buffer_len(), ip(&to)).map(|buf| {
                repr.emit(
                    &Ipv6Addr::UNSPECIFIED,
                    &dst,
                    &mut Icmpv6Packet::new_unchecked(buf),
                    &smoltcp::phy::ChecksumCapabilities::ignored(),
                )
            })
        };
        if sent.is_err() {
            return fail(net::Error::TooMany);
        }
        self.pings.push(Ping {
            conn,
            seq,
            sent: self.now,
            deadline: self
                .now
                .saturating_add(self.settings.ping_timeout_ms * 1_000_000),
            pending: cx.defer(),
        });
        Outcome::Deferred
    }

    pub fn grant(&mut self, cx: &mut Context<'_>, conn: u64, child: Policy) -> Outcome {
        let policy = self.policy(conn);
        if !policy.has(rights::GRANT) {
            return fail(net::Error::NotPermitted);
        }
        if let Err(e) = child.validate() {
            return fail(e);
        }
        if !policy.covers(&child) {
            return fail(net::Error::NotPermitted);
        }
        self.grants.push((cx.defer(), child));
        Outcome::Deferred
    }

    pub fn status(&mut self, conn: u64, socket: u32) -> Outcome {
        let Some(sock) = self.conns.get(&conn).and_then(|c| c.sockets.get(&socket)) else {
            return fail(net::Error::NoSocket);
        };
        let st = match sock {
            Sock::Tcp(t) => {
                let s = self.ifaces[t.iface].set.get::<tcp::Socket>(t.handle);
                SocketStatus {
                    state: state_of(s.state()),
                    local: t.local,
                    remote: Some(t.remote),
                    send_queued: s.send_queue() as u32,
                    send_capacity: s.send_capacity() as u32,
                    receive_queued: s.recv_queue() as u32,
                    receive_capacity: s.recv_capacity() as u32,
                    backlog: 0,
                }
            }
            Sock::Listener(l) => SocketStatus {
                state: SocketState::Listen,
                local: l.at,
                remote: None,
                send_queued: 0,
                send_capacity: 0,
                receive_queued: 0,
                receive_capacity: 0,
                backlog: l.queue.len() as u32,
            },
            Sock::Udp(u) => {
                let s = self.ifaces[u.iface].set.get::<udp::Socket>(u.handle);
                SocketStatus {
                    state: SocketState::Bound,
                    local: u.at,
                    remote: None,
                    send_queued: s.send_queue() as u32,
                    send_capacity: s.packet_send_capacity() as u32,
                    receive_queued: s.recv_queue() as u32,
                    receive_capacity: s.packet_recv_capacity() as u32,
                    backlog: 0,
                }
            }
        };
        Outcome::reply(st)
    }

    // ---- answering what waits (SDD 31, f.5) ----

    /// Moves a listener's sockets along: a listening socket that took a SYN to the
    /// half-open ones, an established one to the queue, a closed one out; and gives
    /// it a listening socket again while it has room. `iface` limits it to one
    /// interface's listeners.
    fn tend_listeners(&mut self, iface: Option<usize>) {
        let room = self.settings.sockets_max.saturating_sub(self.smol);
        let mut wanted: Vec<(u64, u32, usize, Endpoint)> = Vec::new();
        let mut removed: Vec<(usize, SocketHandle)> = Vec::new();
        let Stack { conns, ifaces, .. } = self;
        for (cid, c) in conns.iter_mut() {
            for (sid, sock) in c.sockets.iter_mut() {
                let Sock::Listener(l) = sock else { continue };
                if iface.is_some_and(|i| i != l.iface) {
                    continue;
                }
                let set = &mut ifaces[l.iface].set;
                if let Some(h) = l.listening
                    && set.get::<tcp::Socket>(h).state() != tcp::State::Listen
                {
                    l.half.push(h);
                    l.listening = None;
                }
                let mut k = 0;
                while k < l.half.len() {
                    let h = l.half[k];
                    match set.get::<tcp::Socket>(h).state() {
                        tcp::State::SynReceived => k += 1,
                        tcp::State::Closed | tcp::State::Listen => {
                            removed.push((l.iface, h));
                            l.half.swap_remove(k);
                        }
                        _ => {
                            l.queue.push_back(h);
                            l.half.swap_remove(k);
                        }
                    }
                }
                l.queue.retain(|&h| {
                    let closed = set.get::<tcp::Socket>(h).state() == tcp::State::Closed;
                    if closed {
                        removed.push((l.iface, h));
                    }
                    !closed
                });
                if l.listening.is_none() && 1 + l.half.len() + l.queue.len() <= l.backlog {
                    wanted.push((*cid, *sid, l.iface, l.at));
                }
            }
        }
        for (i, h) in removed {
            self.remove_socket(i, h);
        }
        for (cid, sid, i, at) in wanted.into_iter().take(room) {
            let Some(h) = self.listening_socket(i, &at) else {
                continue;
            };
            if let Some(Sock::Listener(l)) = self
                .conns
                .get_mut(&cid)
                .and_then(|c| c.sockets.get_mut(&sid))
            {
                l.listening = Some(h);
            } else {
                self.remove_socket(i, h);
            }
        }
    }

    fn service(&mut self) {
        self.tend_listeners(None);
        self.service_orphans();
        let now = self.now;
        let mut accepts: Vec<(u64, u32)> = Vec::new();
        let mut failed_opens: Vec<(u64, u32)> = Vec::new();
        {
            let Stack {
                conns,
                ifaces,
                ready,
                ..
            } = self;
            for (cid, c) in conns.iter_mut() {
                for (sid, sock) in c.sockets.iter_mut() {
                    match sock {
                        Sock::Tcp(t) => {
                            let s = ifaces[t.iface].set.get_mut::<tcp::Socket>(t.handle);
                            if let Some(tls) = &mut t.tls {
                                tls.poll(s, now);
                                if let Some(p) = t.shutdown_pending.take() {
                                    if let Some(e) = tls.error() {
                                        ready.push((p, fail(tls_error(e))));
                                    } else if tls.close_sent() {
                                        ready.push((p, Outcome::reply(())));
                                    } else {
                                        t.shutdown_pending = Some(p);
                                    }
                                }
                                if let Some(p) = t.upgrading.take() {
                                    if let Some(e) = tls.error() {
                                        ready.push((p, fail(tls_error(e))));
                                    } else if tls.ready() {
                                        ready.push((p, Outcome::reply(())));
                                    } else {
                                        t.upgrading = Some(p);
                                    }
                                }
                            }
                            let state = s.state();
                            if matches!(
                                state,
                                tcp::State::Established
                                    | tcp::State::CloseWait
                                    | tcp::State::FinWait1
                                    | tcp::State::FinWait2
                            ) {
                                t.established = true;
                            }
                            if matches!(
                                state,
                                tcp::State::CloseWait
                                    | tcp::State::LastAck
                                    | tcp::State::Closing
                                    | tcp::State::TimeWait
                            ) {
                                t.saw_fin = true;
                            }
                            if let Some((p, deadline)) = t.opening.take() {
                                if t.established {
                                    ready.push((
                                        p,
                                        Outcome::reply(Opened {
                                            socket: *sid,
                                            local: t.local,
                                            remote: t.remote,
                                        }),
                                    ));
                                } else if state == tcp::State::Closed {
                                    ready.push((p, fail(net::Error::Refused)));
                                    failed_opens.push((*cid, *sid));
                                } else if now >= deadline {
                                    s.abort();
                                    ready.push((p, fail(net::Error::TimedOut)));
                                    failed_opens.push((*cid, *sid));
                                } else {
                                    t.opening = Some((p, deadline));
                                }
                                continue;
                            }
                            if let Some((p, destination)) = t.receive.take() {
                                match try_receive_into(t, s, &destination) {
                                    Some(Ok(b)) => ready.push((p, Outcome::reply(b))),
                                    Some(Err(e)) => ready.push((p, fail(e))),
                                    None => t.receive = Some((p, destination)),
                                }
                            }
                            if let Some((p, bytes)) = t.send.take() {
                                match try_send_from(t, s, &bytes) {
                                    Some(Ok(n)) => ready.push((p, Outcome::reply(n))),
                                    Some(Err(e)) => ready.push((p, fail(e))),
                                    None => t.send = Some((p, bytes)),
                                }
                            }
                        }
                        Sock::Listener(l) => {
                            if l.accept.is_some() && !l.queue.is_empty() {
                                accepts.push((*cid, *sid));
                            }
                        }
                        Sock::Udp(u) => {
                            let s = ifaces[u.iface].set.get_mut::<udp::Socket>(u.handle);
                            if let Some(p) = u.receive.take() {
                                match try_receive_from(s) {
                                    Some(v) => ready.push((p, Outcome::reply(v))),
                                    None => u.receive = Some(p),
                                }
                            }
                            if let Some((p, to, bytes)) = u.send.take() {
                                if try_send_to(s, &u.at, &to, &bytes) {
                                    ready.push((p, Outcome::reply(())));
                                } else {
                                    u.send = Some((p, to, bytes));
                                }
                            }
                        }
                    }
                }
            }
        }
        for (cid, sid) in failed_opens {
            if let Some(Sock::Tcp(t)) = self.conn(cid).and_then(|c| c.sockets.remove(&sid)) {
                // Closed, or just aborted: the reset goes out with the next egress, so
                // the socket finishes as an orphan.
                let local = t.local;
                self.orphans.push(Orphan {
                    iface: t.iface,
                    handle: t.handle,
                    local,
                    deadline: now,
                    sent: false,
                });
            }
        }
        for (cid, sid) in accepts {
            let full = self.held(cid).is_err();
            let Some(Sock::Listener(l)) = self.conn(cid).and_then(|c| c.sockets.get_mut(&sid))
            else {
                continue;
            };
            let Some(p) = l.accept.take() else { continue };
            if full {
                self.ready.push((p, fail(net::Error::TooMany)));
                continue;
            }
            let Some(h) = l.queue.pop_front() else {
                continue;
            };
            let iface = l.iface;
            let opened = self.accepted(cid, iface, h);
            self.ready.push((p, Outcome::reply(opened)));
        }
        self.service_resolves();
        self.service_pings();
    }

    fn service_orphans(&mut self) {
        let now = self.now;
        let mut k = 0;
        while k < self.orphans.len() {
            let o = &mut self.orphans[k];
            let s = self.ifaces[o.iface].set.get_mut::<tcp::Socket>(o.handle);
            if s.state() == tcp::State::Closed {
                if o.sent {
                    let o = self.orphans.swap_remove(k);
                    self.remove_socket(o.iface, o.handle);
                    self.unbind_tcp(&o.local);
                    continue;
                }
            } else if now >= o.deadline {
                // Past its time: reset, and removed once the reset has gone.
                s.abort();
                o.sent = false;
            }
            k += 1;
        }
    }

    fn service_resolves(&mut self) {
        let now = self.now;
        let mut done = Vec::new();
        for (k, r) in self.resolves.iter_mut().enumerate() {
            let x = &mut self.ifaces[r.iface];
            let Some(h) = x.dns else { continue };
            let socket = x.set.get_mut::<dns::Socket>(h);
            r.queries.retain(|q| match socket.get_query_result(*q) {
                Ok(found) => {
                    for a in found {
                        let a = addr(a);
                        if !r.addrs.contains(&a) {
                            r.addrs.push(a);
                        }
                    }
                    false
                }
                Err(dns::GetQueryResultError::Failed) => false,
                Err(dns::GetQueryResultError::Pending) => true,
            });
            if r.queries.is_empty() {
                done.push((k, None));
            } else if now >= r.deadline {
                for q in r.queries.drain(..) {
                    socket.cancel_query(q);
                }
                done.push((k, Some(net::Error::TimedOut)));
            }
        }
        for (k, timed_out) in done.into_iter().rev() {
            let r = self.resolves.swap_remove(k);
            self.finish_resolve(r, timed_out);
        }
    }

    fn finish_resolve(&mut self, mut r: Resolve, failed: Option<net::Error>) {
        r.addrs.sort_by_key(|a| !a.is_v4());
        match r.purpose {
            Purpose::Resolve(p) => {
                let answer = match failed {
                    Some(e) => fail(e),
                    None if r.addrs.is_empty() => fail(net::Error::NotFound),
                    None => Outcome::reply(r.addrs),
                };
                self.ready.push((p, answer));
            }
            Purpose::Open {
                pending,
                port,
                from,
                by_rule,
            } => {
                if let Some(c) = self.conn(r.conn) {
                    c.resolving = c.resolving.saturating_sub(1);
                }
                let policy = self.policy(r.conn);
                let mut why = failed.unwrap_or(net::Error::NotFound);
                let mut chosen = None;
                for a in &r.addrs {
                    if from.is_some_and(|f| f.is_v4() != a.is_v4()) {
                        continue;
                    }
                    let ep = Endpoint::new(*a, port);
                    if !by_rule && !policy.may_connect(&ep) {
                        why = net::Error::NotPermitted;
                        continue;
                    }
                    match self.route(a, from.as_ref(), &policy) {
                        Ok((iface, src)) => {
                            chosen = Some((iface, src, ep));
                            break;
                        }
                        Err(e) => why = e,
                    }
                }
                let result = match chosen {
                    Some((iface, src, ep)) => self
                        .held(r.conn)
                        .and_then(|()| self.start_tcp(r.conn, iface, src, ep, pending.clone())),
                    None => Err(why),
                };
                if let Err(e) = result {
                    self.ready.push((pending, fail(e)));
                }
            }
        }
    }

    fn service_pings(&mut self) {
        let now = self.now;
        for x in self.ifaces.iter_mut() {
            let ident = x.icmp_ident;
            let socket = x.set.get_mut::<icmp::Socket>(x.icmp);
            while let Ok((payload, _from)) = socket.recv() {
                let echo = if let Ok(p) = Icmpv4Packet::new_checked(payload) {
                    (p.msg_type() == smoltcp::wire::Icmpv4Message::EchoReply)
                        .then(|| (p.echo_ident(), p.echo_seq_no()))
                } else {
                    None
                }
                .or_else(|| {
                    Icmpv6Packet::new_checked(payload)
                        .ok()
                        .filter(|p| p.msg_type() == smoltcp::wire::Icmpv6Message::EchoReply)
                        .map(|p| (p.echo_ident(), p.echo_seq_no()))
                });
                let Some((id, seq)) = echo else { continue };
                if id != ident {
                    continue;
                }
                if let Some(k) = self.pings.iter().position(|p| p.seq == seq) {
                    let p = self.pings.swap_remove(k);
                    self.ready
                        .push((p.pending, Outcome::reply(now.saturating_sub(p.sent).max(1))));
                }
            }
        }
        let mut k = 0;
        while k < self.pings.len() {
            if now >= self.pings[k].deadline {
                let p = self.pings.swap_remove(k);
                self.ready.push((p.pending, fail(net::Error::TimedOut)));
            } else {
                k += 1;
            }
        }
    }
}

impl Iface {
    #[allow(clippy::too_many_arguments)]
    fn with(
        name: String,
        mac: u64,
        up: bool,
        port: Port,
        iface: Interface,
        dhcp: Option<dhcpv4::Socket<'static>>,
        dns: Option<dns::Socket<'static>>,
        icmp_ident: u16,
        s: &Settings,
    ) -> Iface {
        let mut set = SocketSet::new(Vec::new());
        // Sixteen echoes of the largest a ping carries, each way: what pings in flight
        // need, so sized from the protocol's bound rather than a setting.
        let packets = 16;
        let bytes = packets * (PING_MAX as usize + 64);
        let _ = s;
        let mut ping = icmp::Socket::new(
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; packets], vec![0; bytes]),
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; packets], vec![0; bytes]),
        );
        let ident = icmp_ident.max(1);
        let _ = ping.bind(icmp::Endpoint::Ident(ident));
        let icmp = set.add(ping);
        let dhcp = dhcp.map(|d| set.add(d));
        let dns = dns.map(|d| set.add(d));
        Iface {
            name,
            mac,
            up,
            port,
            iface,
            set,
            dhcp,
            dns,
            dns_servers: Vec::new(),
            icmp,
            icmp_ident: ident,
            gateway4: None,
            gateway6: None,
            dhcp_dns: Vec::new(),
        }
    }
}

/// The largest frame `moruna.driver.net` carries.
fn net_protocol_frame_max() -> usize {
    16 * 1024
}

fn state_of(s: tcp::State) -> SocketState {
    match s {
        tcp::State::Closed => SocketState::Closed,
        tcp::State::Listen => SocketState::Listen,
        tcp::State::SynSent => SocketState::SynSent,
        tcp::State::SynReceived => SocketState::SynReceived,
        tcp::State::Established => SocketState::Established,
        tcp::State::FinWait1 => SocketState::FinWait1,
        tcp::State::FinWait2 => SocketState::FinWait2,
        tcp::State::CloseWait => SocketState::CloseWait,
        tcp::State::Closing => SocketState::Closing,
        tcp::State::LastAck => SocketState::LastAck,
        tcp::State::TimeWait => SocketState::TimeWait,
    }
}

/// What a send can be answered with now: how many bytes were taken, or why none can
/// be; `None` while it must wait for room.
fn try_send_from(
    t: &mut Tcp,
    s: &mut tcp::Socket<'static>,
    source: &StreamLoan,
) -> Option<Result<u32, net::Error>> {
    if t.shut {
        return Some(Err(net::Error::NotConnected));
    }
    let (mapping, within) = match source.mapping(false) {
        Ok(mapping) => mapping,
        Err(error) => return Some(Err(error)),
    };
    if let Some(tls) = &mut t.tls {
        return tls
            .send_from(source.max as usize, |bytes| mapping.read(within, bytes))
            .map(|result| {
                result
                    .map_err(tls_error)?
                    .map_err(|_| net::Error::BadArgument)
            });
    }
    if t.tls_required {
        return Some(Err(net::Error::TlsFailed));
    }
    if !s.may_send() {
        return Some(Err(net::Error::NotConnected));
    }
    if !s.can_send() {
        return None;
    }
    Some(
        s.send(|bytes| {
            let count = bytes.len().min(source.max as usize);
            match mapping.read(within, &mut bytes[..count]) {
                Ok(()) => {
                    t.plaintext_used = true;
                    (count, Ok(count as u32))
                }
                Err(_) => (0, Err(net::Error::BadArgument)),
            }
        })
        .unwrap_or(Err(net::Error::NotConnected)),
    )
}

/// What a receive can be answered with now: bytes, the end of the stream (none), or
/// a reset; `None` while it must wait.
fn try_receive_into(
    t: &mut Tcp,
    s: &mut tcp::Socket<'static>,
    destination: &StreamLoan,
) -> Option<Result<u32, net::Error>> {
    // Map the owned capability before consuming any TCP or TLS plaintext.
    let (mapping, within) = match destination.mapping(true) {
        Ok(m) => m,
        Err(e) => return Some(Err(e)),
    };
    if let Some(tls) = &mut t.tls {
        // rustls' bounded server-charged plaintext staging is not a wire reply.
        return tls
            .receive_to(destination.max as usize, |bytes| {
                mapping.write(within, bytes)
            })
            .map(|result| {
                result
                    .map_err(tls_error)?
                    .map_err(|_| net::Error::BadArgument)
            });
    }
    if t.tls_required {
        return Some(Err(net::Error::TlsFailed));
    }
    if s.can_recv() {
        return Some(
            s.recv(|bytes| {
                let count = bytes.len().min(destination.max as usize);
                match mapping.write(within, &bytes[..count]) {
                    Ok(()) => {
                        t.plaintext_used = true;
                        (count, Ok(count as u32))
                    }
                    Err(_) => (0, Err(net::Error::BadArgument)),
                }
            })
            .unwrap_or(Err(net::Error::NotConnected)),
        );
    }
    if !s.may_recv() {
        if s.state() == tcp::State::Closed && t.established && !t.saw_fin {
            return Some(Err(net::Error::Reset));
        }
        return Some(Ok(0));
    }
    None
}

fn try_send_to(s: &mut udp::Socket<'static>, from: &Endpoint, to: &Endpoint, bytes: &[u8]) -> bool {
    let meta = udp::UdpMetadata {
        endpoint: IpEndpoint::new(ip(&to.addr), to.port),
        local_address: Some(ip(&from.addr)),
        meta: Default::default(),
    };
    s.send_slice(bytes, meta).is_ok()
}

fn try_receive_from(s: &mut udp::Socket<'static>) -> Option<(Endpoint, Vec<u8>)> {
    let (payload, meta) = s.recv().ok()?;
    Some((endpoint(meta.endpoint), payload.to_vec()))
}

fn tls_error(error: crate::tls::Error) -> net::Error {
    match error {
        crate::tls::Error::Unavailable => net::Error::TlsUnavailable,
        crate::tls::Error::Protocol => net::Error::TlsFailed,
        crate::tls::Error::Limit => net::Error::TooLarge,
        crate::tls::Error::TimedOut => net::Error::TimedOut,
    }
}
