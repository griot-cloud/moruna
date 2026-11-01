//! The protocol's values and their wire forms (SDD 31, e.1).

use alloc::{string::String, vec::Vec};
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use moruna_svc::{Decoder, Encoder, Wire, WireError};

/// An IPv4 or IPv6 address: a `u8` family (4 or 6), then its 4 or 16 octets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Addr(pub IpAddr);

impl Addr {
    pub const fn v4(a: u8, b: u8, c: u8, d: u8) -> Addr {
        Addr(IpAddr::V4(Ipv4Addr::new(a, b, c, d)))
    }

    pub fn is_v4(&self) -> bool {
        self.0.is_ipv4()
    }

    /// The address's bits: 32 or 128.
    pub fn bits(&self) -> u8 {
        if self.is_v4() { 32 } else { 128 }
    }

    /// The address as a 128-bit number, an IPv4 address in the low 32 bits.
    pub fn value(&self) -> u128 {
        match self.0 {
            IpAddr::V4(a) => u32::from(a) as u128,
            IpAddr::V6(a) => u128::from(a),
        }
    }

    /// Whether the address names one host: not unspecified, multicast or the IPv4
    /// broadcast address.
    pub fn is_host(&self) -> bool {
        match self.0 {
            IpAddr::V4(a) => !a.is_unspecified() && !a.is_multicast() && !a.is_broadcast(),
            IpAddr::V6(a) => !a.is_unspecified() && !a.is_multicast(),
        }
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Wire for Addr {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        match self.0 {
            IpAddr::V4(a) => {
                e.put_raw(&[4]);
                e.put_raw(&a.octets());
            }
            IpAddr::V6(a) => {
                e.put_raw(&[6]);
                e.put_raw(&a.octets());
            }
        }
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        match d.take_raw(1)?[0] {
            4 => {
                let b = d.take_raw(4)?;
                Ok(Addr(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3]))))
            }
            6 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(d.take_raw(16)?);
                Ok(Addr(IpAddr::V6(Ipv6Addr::from(o))))
            }
            _ => Err(WireError::Invalid),
        }
    }
}

/// An address and a port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Endpoint {
    pub addr: Addr,
    pub port: u16,
}

impl Endpoint {
    pub const fn new(addr: Addr, port: u16) -> Endpoint {
        Endpoint { addr, port }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.addr.0 {
            IpAddr::V4(a) => write!(f, "{a}:{}", self.port),
            IpAddr::V6(a) => write!(f, "[{a}]:{}", self.port),
        }
    }
}

impl Wire for Endpoint {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.addr.encode(e)?;
        self.port.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Endpoint {
            addr: Addr::decode(d)?,
            port: u16::decode(d)?,
        })
    }
}

/// An address and a prefix length: a network, or an interface's address and its
/// network. A prefix past the family's bits does not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    pub addr: Addr,
    pub prefix: u8,
}

impl Cidr {
    pub const fn new(addr: Addr, prefix: u8) -> Cidr {
        Cidr { addr, prefix }
    }

    /// The mask of the prefix, over [`Addr::value`]'s bits.
    fn mask(&self) -> u128 {
        let bits = self.addr.bits() as u32;
        let p = (self.prefix as u32).min(bits);
        if p == 0 {
            return 0;
        }
        // The top `p` bits of a `bits`-wide number.
        let ones = if p == 128 {
            u128::MAX
        } else {
            (1u128 << p) - 1
        };
        ones << (bits - p)
    }

    /// Whether `addr` lies in this network.
    pub fn contains(&self, addr: &Addr) -> bool {
        addr.is_v4() == self.addr.is_v4()
            && addr.value() & self.mask() == self.addr.value() & self.mask()
    }

    /// Whether every address of `other` lies in this network.
    pub fn covers(&self, other: &Cidr) -> bool {
        other.prefix >= self.prefix && self.contains(&other.addr)
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl Wire for Cidr {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.addr.encode(e)?;
        self.prefix.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let addr = Addr::decode(d)?;
        let prefix = u8::decode(d)?;
        if prefix > addr.bits() {
            return Err(WireError::Invalid);
        }
        Ok(Cidr { addr, prefix })
    }
}

/// A network and a range of ports, `first` to `last` inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    pub net: Cidr,
    pub first: u16,
    pub last: u16,
}

impl Rule {
    /// Every port of `net`.
    pub const fn any_port(net: Cidr) -> Rule {
        Rule {
            net,
            first: 0,
            last: u16::MAX,
        }
    }

    /// Whether the rule permits `endpoint`.
    pub fn permits(&self, endpoint: &Endpoint) -> bool {
        self.net.contains(&endpoint.addr) && (self.first..=self.last).contains(&endpoint.port)
    }

    /// Whether everything `other` permits, this permits.
    pub fn covers(&self, other: &Rule) -> bool {
        self.net.covers(&other.net) && self.first <= other.first && other.last <= self.last
    }
}

impl Wire for Rule {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.net.encode(e)?;
        self.first.encode(e)?;
        self.last.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Rule {
            net: Cidr::decode(d)?,
            first: u16::decode(d)?,
            last: u16::decode(d)?,
        })
    }
}

/// A name, or every name below a domain (`*.example.com`), and a range of ports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameRule {
    pub pattern: String,
    pub first: u16,
    pub last: u16,
}

impl Wire for NameRule {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.pattern.encode(e)?;
        self.first.encode(e)?;
        self.last.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(NameRule {
            pattern: String::decode(d)?,
            first: u16::decode(d)?,
            last: u16::decode(d)?,
        })
    }
}

/// What a connection may do (SDD 31, f.6): its rights, where it may reach (`remote`),
/// which names it may resolve and reach by name (`names`), which local endpoints it
/// may listen and bind on and send from (`local`), and how many sockets it may hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    pub rights: u32,
    pub remote: Vec<Rule>,
    pub local: Vec<Rule>,
    pub names: Vec<NameRule>,
    pub sockets: u32,
}

impl Wire for Policy {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.rights.encode(e)?;
        self.remote.encode(e)?;
        self.local.encode(e)?;
        self.names.encode(e)?;
        self.sockets.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Policy {
            rights: u32::decode(d)?,
            remote: Vec::decode(d)?,
            local: Vec::decode(d)?,
            names: Vec::decode(d)?,
            sockets: u32::decode(d)?,
        })
    }
}

/// Where a connect goes: an endpoint, or a name and a port (resolved by the server).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Endpoint(Endpoint),
    Name(String, u16),
}

impl Wire for Target {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        match self {
            Target::Endpoint(ep) => {
                e.put_raw(&[0]);
                ep.encode(e)
            }
            Target::Name(name, port) => {
                e.put_raw(&[1]);
                name.encode(e)?;
                port.encode(e)
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        match d.take_raw(1)?[0] {
            0 => Ok(Target::Endpoint(Endpoint::decode(d)?)),
            1 => Ok(Target::Name(String::decode(d)?, u16::decode(d)?)),
            _ => Err(WireError::Invalid),
        }
    }
}

/// One interface of the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    /// The card's address, the first octet in bits 40 to 47; zero for loopback.
    pub mac: u64,
    pub mtu: u32,
    pub up: bool,
    /// Whether its IPv4 address comes from DHCP.
    pub dhcp: bool,
    /// Its addresses and their networks; a DHCP interface has none until bound.
    pub addrs: Vec<Cidr>,
    pub gateway4: Option<Addr>,
    pub gateway6: Option<Addr>,
}

impl Wire for Interface {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.name.encode(e)?;
        self.mac.encode(e)?;
        self.mtu.encode(e)?;
        self.up.encode(e)?;
        self.dhcp.encode(e)?;
        self.addrs.encode(e)?;
        self.gateway4.encode(e)?;
        self.gateway6.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Interface {
            name: String::decode(d)?,
            mac: u64::decode(d)?,
            mtu: u32::decode(d)?,
            up: bool::decode(d)?,
            dhcp: bool::decode(d)?,
            addrs: Vec::decode(d)?,
            gateway4: Option::decode(d)?,
            gateway6: Option::decode(d)?,
        })
    }
}

/// What `info` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub interfaces: Vec<Interface>,
    /// The resolver's servers, configured first, then those DHCP gave.
    pub dns: Vec<Addr>,
    /// This connection's policy.
    pub policy: Policy,
}

impl Wire for Info {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.interfaces.encode(e)?;
        self.dns.encode(e)?;
        self.policy.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Info {
            interfaces: Vec::decode(d)?,
            dns: Vec::decode(d)?,
            policy: Policy::decode(d)?,
        })
    }
}

impl Info {
    /// The interface whose network holds `addr` as one of its own addresses.
    pub fn interface_of(&self, addr: &Addr) -> Option<&Interface> {
        self.interfaces
            .iter()
            .find(|i| i.addrs.iter().any(|c| c.addr == *addr))
    }
}

/// A TCP connection a connect or an accept opened: its socket and its two ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    pub socket: u32,
    pub local: Endpoint,
    pub remote: Endpoint,
}

impl Wire for Opened {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.socket.encode(e)?;
        self.local.encode(e)?;
        self.remote.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Opened {
            socket: u32::decode(d)?,
            local: Endpoint::decode(d)?,
            remote: Endpoint::decode(d)?,
        })
    }
}

/// A socket's state: TCP's (RFC 9293), a listener's, or a bound UDP socket's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketState {
    Closed = 0,
    Listen = 1,
    SynSent = 2,
    SynReceived = 3,
    Established = 4,
    FinWait1 = 5,
    FinWait2 = 6,
    CloseWait = 7,
    Closing = 8,
    LastAck = 9,
    TimeWait = 10,
    Bound = 11,
}

impl SocketState {
    const ALL: [SocketState; 12] = [
        SocketState::Closed,
        SocketState::Listen,
        SocketState::SynSent,
        SocketState::SynReceived,
        SocketState::Established,
        SocketState::FinWait1,
        SocketState::FinWait2,
        SocketState::CloseWait,
        SocketState::Closing,
        SocketState::LastAck,
        SocketState::TimeWait,
        SocketState::Bound,
    ];
}

impl Wire for SocketState {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        (self as u8).encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let v = u8::decode(d)?;
        SocketState::ALL
            .iter()
            .copied()
            .find(|s| *s as u8 == v)
            .ok_or(WireError::Invalid)
    }
}

/// What `status` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketStatus {
    pub state: SocketState,
    pub local: Endpoint,
    pub remote: Option<Endpoint>,
    /// Bytes (TCP) or datagrams (UDP) queued to send, and the room for them.
    pub send_queued: u32,
    pub send_capacity: u32,
    /// Bytes (TCP) or datagrams (UDP) received and not yet read, and the room for them.
    pub receive_queued: u32,
    pub receive_capacity: u32,
    /// A listener's established connections not yet accepted.
    pub backlog: u32,
}

impl Wire for SocketStatus {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.state.encode(e)?;
        self.local.encode(e)?;
        self.remote.encode(e)?;
        self.send_queued.encode(e)?;
        self.send_capacity.encode(e)?;
        self.receive_queued.encode(e)?;
        self.receive_capacity.encode(e)?;
        self.backlog.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(SocketStatus {
            state: SocketState::decode(d)?,
            local: Endpoint::decode(d)?,
            remote: Option::decode(d)?,
            send_queued: u32::decode(d)?,
            send_capacity: u32::decode(d)?,
            receive_queued: u32::decode(d)?,
            receive_capacity: u32::decode(d)?,
            backlog: u32::decode(d)?,
        })
    }
}
