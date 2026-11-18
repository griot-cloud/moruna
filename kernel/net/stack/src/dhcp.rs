//! When an interface's DHCP client sends its DISCOVER again (system design 1C; work item
//! 1.11c): a randomized exponential backoff, as RFC 2131 4.1 says a client MUST use, whose
//! first delay is 1 s, doubling to 64 s, each delay randomized by up to a tenth either way
//! (RFC 8415's way, `RT = IRT + RAND*IRT`, `RAND` in [-0.1, 0.1]).
//!
//! A cloud's network may drop what the guest sends for a while after its card starts, and
//! the guest has no signal when the path opens: on Azure the host completed the first
//! DISCOVER with success and dropped it, sent no indication, and the first frame came in
//! 2.7 s later (work item 1.11c, console `console-trace-1.log`). The host answers a DISCOVER
//! in about 15 ms, so 1 s, DHCPv6's first Solicit timeout and the shortest first interval
//! dhcpcd allows, is ample for its reply; RFC 2131's 4 s is its example for a 10 Mb/s
//! Ethernet. A REQUEST, which follows an OFFER over a path that works, waits 4 s as before.
//!
//! smoltcp 0.13.1 sends a DISCOVER again after one fixed `discover_timeout`. The stack sets
//! it, before each poll that may send one, to the delay [`Discovery`] has drawn for the next
//! DISCOVER, and counts the DISCOVERs the poll made ([`is_discover`]), so smoltcp itself is
//! unchanged.

use smoltcp::wire::{
    DhcpMessageType, DhcpPacket, DhcpRepr, EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet,
    UdpPacket,
};

/// The delay after a client's first DISCOVER, before randomizing, in nanoseconds.
pub const FIRST_NS: u64 = 1_000_000_000;
/// The longest delay before randomizing: RFC 2131 4.1's 64 s.
pub const CAP_NS: u64 = 64_000_000_000;
/// How far a delay is randomized either way: a tenth of it.
pub const SPREAD: u64 = 10;

/// One client's DISCOVERs since it last started over: how many it has sent, and the delay
/// after the next, drawn once.
pub struct Discovery {
    sent: u32,
    next_ns: u64,
    random: u64,
}

impl Discovery {
    /// A client that has sent no DISCOVER yet, whose delays are randomized from `seed`.
    pub fn new(seed: u64) -> Discovery {
        let mut d = Discovery {
            sent: 0,
            next_ns: 0,
            random: seed,
        };
        d.next_ns = d.draw();
        d
    }

    /// The delay, in nanoseconds, after the next DISCOVER the client sends.
    pub fn next_ns(&self) -> u64 {
        self.next_ns
    }

    /// How many DISCOVERs the client has sent since it last started over.
    pub fn sent(&self) -> u32 {
        self.sent
    }

    /// One more DISCOVER has gone: the next waits twice as long, up to the cap.
    pub fn discovered(&mut self) {
        self.sent = self.sent.saturating_add(1);
        self.next_ns = self.draw();
    }

    /// The client starts over, its link come up or its lease taken: the next DISCOVER is
    /// followed by the first delay again.
    pub fn restart(&mut self) {
        self.sent = 0;
        self.next_ns = self.draw();
    }

    /// The delay after DISCOVER number `sent` (from 0): the first delay doubled `sent`
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

/// Whether `frame` is a DHCP DISCOVER: an Ethernet frame of IPv4, UDP from port 68 to 67,
/// whose DHCP message type is DISCOVER.
pub fn is_discover(frame: &[u8]) -> bool {
    let Ok(ethernet) = EthernetFrame::new_checked(frame) else {
        return false;
    };
    if ethernet.ethertype() != EthernetProtocol::Ipv4 {
        return false;
    }
    let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
        return false;
    };
    if ip.next_header() != IpProtocol::Udp {
        return false;
    }
    let Ok(udp) = UdpPacket::new_checked(ip.payload()) else {
        return false;
    };
    if udp.src_port() != 68 || udp.dst_port() != 67 {
        return false;
    }
    let Ok(dhcp) = DhcpPacket::new_checked(udp.payload()) else {
        return false;
    };
    DhcpRepr::parse(&dhcp).is_ok_and(|repr| repr.message_type == DhcpMessageType::Discover)
}

#[cfg(test)]
mod tests;
