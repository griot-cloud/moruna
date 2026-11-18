//! When an interface's DHCP client sends a DISCOVER or a REQUEST again (system design 1C;
//! work item 1.11c): a randomized exponential backoff, as RFC 2131 4.1 says a client MUST
//! use, whose first delay is 1 s, doubling to 64 s, each delay randomized by up to a tenth
//! either way (RFC 8415's way, `RT = IRT + RAND*IRT`, `RAND` in [-0.1, 0.1]). RFC 2131 has one
//! rule for both messages, so a REQUEST, which follows an OFFER, waits as a DISCOVER does.
//! A REQUEST that renews a lease is timed by smoltcp's renewal, not by this.
//!
//! A cloud's network may drop what the guest sends for a while after its card starts, and
//! the guest has no signal when the path opens: on Azure the host completed the first
//! DISCOVER with success and dropped it, sent no indication, and the first frame came in
//! 2.7 s later (work item 1.11c, console `console-trace-1.log`). The host answers a DISCOVER
//! in about 15 ms, so 1 s, DHCPv6's first Solicit timeout and the shortest first interval
//! dhcpcd allows, is ample for its reply; RFC 2131's 4 s is its example for a 10 Mb/s
//! Ethernet.
//!
//! smoltcp 0.13.1 sends a DISCOVER again after one fixed `discover_timeout`, and its n-th
//! REQUEST after `initial_request_timeout << (n / 2)`. Before each poll that may send one,
//! the stack sets both from [`Client`]'s schedules ([`Client::configure`]), and counts the
//! DISCOVERs and REQUESTs the poll made ([`Client::sent`]), so smoltcp itself is unchanged.

use smoltcp::{
    socket::dhcpv4::RetryConfig,
    time::Duration,
    wire::{
        DhcpMessageType, DhcpPacket, DhcpRepr, EthernetFrame, EthernetProtocol, IpProtocol,
        Ipv4Packet, UdpPacket,
    },
};

/// The delay after a client's first message of a kind, before randomizing, in nanoseconds.
pub const FIRST_NS: u64 = 1_000_000_000;
/// The longest delay before randomizing: RFC 2131 4.1's 64 s.
pub const CAP_NS: u64 = 64_000_000_000;
/// How far a delay is randomized either way: a tenth of it.
pub const SPREAD: u64 = 10;

/// One kind of message a client sends again, since it last started over: how many it has
/// sent, and the delay after the next, drawn once.
pub struct Schedule {
    sent: u32,
    next_ns: u64,
    random: u64,
}

impl Schedule {
    /// A schedule of no message sent yet, whose delays are randomized from `seed`.
    pub fn new(seed: u64) -> Schedule {
        let mut s = Schedule {
            sent: 0,
            next_ns: 0,
            random: seed,
        };
        s.next_ns = s.draw();
        s
    }

    /// The delay, in nanoseconds, after the next message.
    pub fn next_ns(&self) -> u64 {
        self.next_ns
    }

    /// How many messages have been sent since the schedule last started over.
    pub fn sent(&self) -> u32 {
        self.sent
    }

    /// One more message has gone: the next waits twice as long, up to the cap.
    pub fn went(&mut self) {
        self.sent = self.sent.saturating_add(1);
        self.next_ns = self.draw();
    }

    /// The client starts over: the next message is followed by the first delay again.
    pub fn restart(&mut self) {
        self.sent = 0;
        self.next_ns = self.draw();
    }

    /// The delay after message number `sent` (from 0): the first delay doubled `sent`
    /// times, at most the cap, then moved by a uniform amount of up to a tenth either way.
    fn draw(&mut self) -> u64 {
        // 1 s doubled six times is the cap.
        let base = if self.sent < 6 {
            FIRST_NS << self.sent
        } else {
            CAP_NS
        };
        let spread = base / SPREAD;
        base - spread + self.below(2 * spread + 1)
    }

    /// A uniform number below `bound` (SplitMix64).
    fn below(&mut self, bound: u64) -> u64 {
        self.random = self.random.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.random;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % bound
    }
}

/// A DHCP client's two schedules: its DISCOVERs, and the REQUESTs that follow an OFFER.
pub struct Client {
    pub discover: Schedule,
    pub request: Schedule,
}

impl Client {
    /// A client that has sent nothing yet, whose delays are randomized from `seed`.
    pub fn new(seed: u64) -> Client {
        Client {
            discover: Schedule::new(seed),
            request: Schedule::new(seed.rotate_left(32) ^ 0x5bd1_e995),
        }
    }

    /// Sets smoltcp's timeouts for the next DISCOVER and the next REQUEST. smoltcp waits
    /// `initial_request_timeout << (n / 2)` after REQUEST number `n`, so the schedule's delay
    /// is shifted back by as much, to the microsecond.
    pub fn configure(&self, retry: &mut RetryConfig) {
        retry.discover_timeout = Duration::from_micros(self.discover.next_ns() / 1000);
        let shift = (self.request.sent() / 2).min(63);
        retry.initial_request_timeout =
            Duration::from_micros((self.request.next_ns() / 1000) >> shift);
    }

    /// Counts `frame` if it is a DISCOVER or a REQUEST the client sent. A DISCOVER starts the
    /// REQUESTs over: the next OFFER begins smoltcp's count of them again.
    pub fn sent(&mut self, frame: &[u8]) {
        match client_message(frame) {
            Some(DhcpMessageType::Discover) => {
                self.discover.went();
                self.request.restart();
            }
            Some(DhcpMessageType::Request) => self.request.went(),
            _ => {}
        }
    }

    /// The client starts over, its link come up or its lease taken: both schedules begin
    /// again.
    pub fn restart(&mut self) {
        self.discover.restart();
        self.request.restart();
    }
}

/// The type of the DHCP message `frame` carries from a client: an Ethernet frame of IPv4,
/// UDP from port 68 to 67.
pub fn client_message(frame: &[u8]) -> Option<DhcpMessageType> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    if ethernet.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp {
        return None;
    }
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    if udp.src_port() != 68 || udp.dst_port() != 67 {
        return None;
    }
    let dhcp = DhcpPacket::new_checked(udp.payload()).ok()?;
    DhcpRepr::parse(&dhcp).ok().map(|repr| repr.message_type)
}

/// Whether `frame` is a DHCP DISCOVER a client sent.
pub fn is_discover(frame: &[u8]) -> bool {
    client_message(frame) == Some(DhcpMessageType::Discover)
}

#[cfg(test)]
mod tests;
