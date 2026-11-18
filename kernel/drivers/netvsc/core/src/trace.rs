//! The boot's network path, frame by frame, with times, for diagnosis only (work item 1.11c):
//! a build with the feature `trace-first-frames`, which a node's normal image never turns on.
//!
//! Azure drops what the guest sends for a while after its card starts. The trace shows each
//! boot's window, and which message it took: of the frames the boot's network path is made
//! of ([`traced`]: DHCP, ARP, and TCP's SYN and RST), the first [`FIRST`] out each get a line
//! when put on the ring to the host (its section, its length, its addresses and what it is),
//! a line when the commit that hands it to the host is made (and whether the host was
//! signalled, by VMBus's rule), and a line when the host completes it (with the host's
//! status); the first [`FIRST`] in, a line when each is read. Other frames are not traced, so
//! the console stays light. The first [`FIRST`] indications the host sends each get a line
//! with their status and what they did to the link: the card takes the link to be up from
//! its start, so an indication that the link came up later is otherwise passed over unseen.
//!
//! Every line starts with the time on the clock the caller gives, in milliseconds to the
//! microsecond: for the node's netvsc, its own clock, since its process started, the clock
//! of its console's other lines.

use alloc::{boxed::Box, format, string::String, vec::Vec};

use netvsp_protocol::rndisprot;

/// How many frames are traced each way, and how many indications.
pub const FIRST: u32 = 40;

/// The trace of a card's first frames: what the card noted, as lines for the console.
pub struct Trace {
    /// Nanoseconds on the caller's clock.
    clock: Box<dyn FnMut() -> u64>,
    /// Frames out traced so far.
    out: u32,
    /// Frames in traced so far.
    into: u32,
    /// Indications traced so far.
    indications: u32,
    /// The traced frames out the host has not completed: each one's transaction and number.
    flying: Vec<(u64, u32)>,
    /// The traced frames out written to the ring and not yet committed, by number.
    uncommitted: Vec<u32>,
    /// The lines not yet taken.
    lines: Vec<String>,
    /// The first frames out, each timed from its commit to its completion's read (work item
    /// 1.11d).
    latency: crate::latency::Latency,
}

impl Trace {
    /// A trace whose times are `clock`'s, in nanoseconds.
    pub fn new(clock: Box<dyn FnMut() -> u64>) -> Trace {
        Trace {
            clock,
            out: 0,
            into: 0,
            indications: 0,
            flying: Vec::new(),
            uncommitted: Vec::new(),
            lines: Vec::new(),
            latency: crate::latency::Latency::default(),
        }
    }

    /// The lines noted since the last take, oldest first; and once every timed frame out
    /// is complete, their timings (`latency`), said together.
    pub fn take(&mut self) -> Vec<String> {
        for line in self.latency.finished().unwrap_or_default() {
            self.say(line);
        }
        core::mem::take(&mut self.lines)
    }

    /// netvsc begins to wait for the host's signal (work item 1.11d).
    pub fn waiting(&mut self) {
        let at = (self.clock)();
        self.latency.waiting(at);
    }

    /// The host's signal woke netvsc (work item 1.11d).
    pub fn woken(&mut self) {
        let at = (self.clock)();
        self.latency.woken(at);
    }

    /// A frame of `bytes` written to the ring to the host under `transaction`, which the
    /// timings follow whether or not the trace says it (work item 1.11d).
    pub(crate) fn timed(&mut self, transaction: u64, bytes: usize) {
        self.latency.sent(transaction, bytes);
    }

    fn say(&mut self, what: String) {
        let ns = (self.clock)();
        self.lines.push(format!(
            "{}.{:03} ms: trace: {what}",
            ns / 1_000_000,
            ns / 1_000 % 1_000
        ));
    }

    /// A frame written to the ring to the host under `transaction`, in send section
    /// `section`, not yet committed.
    pub(crate) fn sent(&mut self, transaction: u64, section: u32, frame: &[u8]) {
        if self.out >= FIRST || !traced(frame) {
            return;
        }
        self.out += 1;
        let n = self.out;
        self.flying.push((transaction, n));
        self.uncommitted.push(n);
        self.say(format!(
            "out {n}: {} bytes in section {section}, {}",
            frame.len(),
            describe(frame)
        ));
    }

    /// The ring to the host committed: the host now sees what was written to it. The host
    /// was signalled, or by VMBus's rule did not need to be.
    pub(crate) fn committed(&mut self, signalled: bool) {
        let at = (self.clock)();
        self.latency.committed(at, signalled);
        if self.uncommitted.is_empty() {
            return;
        }
        let numbers: Vec<String> = self.uncommitted.drain(..).map(|n| format!("{n}")).collect();
        let signal = if signalled {
            "the host signalled"
        } else {
            "no signal: the ring was not empty, or the host masked it"
        };
        self.say(format!("out {} committed; {signal}", numbers.join(", ")));
    }

    /// The host's completion of the frame sent under `transaction`, with its NVSP status.
    pub(crate) fn completed(&mut self, transaction: u64, status: u32) {
        let read = (self.clock)();
        self.latency.completed(read, transaction);
        let Some(at) = self.flying.iter().position(|(t, _)| *t == transaction) else {
            return;
        };
        let (_, n) = self.flying.swap_remove(at);
        let what = match status {
            1 => "success",
            2 => "failure",
            5 => "an invalid RNDIS packet",
            6 => "busy",
            _ => "a status NVSP does not name",
        };
        self.say(format!("out {n} completed: status {status} ({what})"));
    }

    /// A frame read from the receive buffer.
    pub(crate) fn received(&mut self, frame: &[u8]) {
        if self.into >= FIRST || !traced(frame) {
            return;
        }
        self.into += 1;
        let n = self.into;
        self.say(format!(
            "in {n}: {} bytes, {}",
            frame.len(),
            describe(frame)
        ));
    }

    /// An indication of the host's, with the link as it was before it (`was`) and after.
    pub(crate) fn indicated(&mut self, status: u32, was: bool, now: bool) {
        if self.indications >= FIRST {
            return;
        }
        self.indications += 1;
        let what = match status {
            rndisprot::STATUS_MEDIA_CONNECT => "media connect",
            rndisprot::STATUS_MEDIA_DISCONNECT => "media disconnect",
            rndisprot::STATUS_LINK_SPEED_CHANGE => "link speed change",
            // Linux's RNDIS_STATUS_NETWORK_CHANGE (`include/linux/rndis.h`): the host moved
            // the card to another network. netvsp_protocol does not name it.
            0x4001_0018 => "network change",
            _ => "another status",
        };
        let link = |up: bool| if up { "up" } else { "down" };
        let change = if was == now {
            format!("the link stays {}", link(now))
        } else {
            format!("the link goes {}", link(now))
        };
        self.say(format!("indication {status:#010x} ({what}): {change}"));
    }
}

/// Whether the trace follows `frame`: it is one the boot's network path is made of, DHCP
/// (UDP between ports 67 and 68), ARP, or TCP opening or refusing a connection (SYN or RST
/// set).
pub fn traced(frame: &[u8]) -> bool {
    match ethertype(frame) {
        Some(ARP) => true,
        Some(IPV4) => match transport(&frame[14..]) {
            Some((UDP, segment)) => ports(segment).is_some_and(|(from, to)| dhcp_ports(from, to)),
            Some((TCP, segment)) => segment
                .get(13)
                .is_some_and(|flags| flags & (SYN | RST) != 0),
            _ => false,
        },
        _ => false,
    }
}

const IPV4: u16 = 0x0800;
const ARP: u16 = 0x0806;
const UDP: u8 = 17;
const TCP: u8 = 6;
const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const ACK: u8 = 0x10;

/// The frame's EtherType, if it has an Ethernet header.
fn ethertype(frame: &[u8]) -> Option<u16> {
    frame.get(12..14).map(|t| u16::from_be_bytes([t[0], t[1]]))
}

/// An IPv4 packet's protocol, and what follows its header.
fn transport(packet: &[u8]) -> Option<(u8, &[u8])> {
    let header = usize::from(packet.first()? & 0x0f) * 4;
    let protocol = *packet.get(9)?;
    Some((protocol, packet.get(header..)?))
}

/// The source and destination ports of a UDP or TCP segment.
fn ports(segment: &[u8]) -> Option<(u16, u16)> {
    let p = segment.get(..4)?;
    Some((
        u16::from_be_bytes([p[0], p[1]]),
        u16::from_be_bytes([p[2], p[3]]),
    ))
}

fn dhcp_ports(from: u16, to: u16) -> bool {
    matches!(from, 67 | 68) && matches!(to, 67 | 68)
}

fn ip(a: &[u8]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn mac(m: &[u8]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

/// A frame as the trace says it: its destination, what it carries, the addresses and ports
/// of IPv4, TCP's flags, ARP's question or answer, and for DHCP its message type and its
/// transaction ID.
pub fn describe(frame: &[u8]) -> String {
    let Some(kind) = ethertype(frame) else {
        return String::from("shorter than an Ethernet header");
    };
    let what = match kind {
        IPV4 => ipv4(&frame[14..]),
        ARP => arp(&frame[14..]),
        0x86dd => String::from("IPv6"),
        other => format!("EtherType {other:#06x}"),
    };
    format!("to {}, {what}", mac(&frame[..6]))
}

/// An ARP packet for IPv4 over Ethernet: a request asks who has the target's address; a
/// reply says the sender's address is at the sender's MAC.
fn arp(packet: &[u8]) -> String {
    let Some(p) = packet.get(..28) else {
        return String::from("ARP, truncated");
    };
    match u16::from_be_bytes([p[6], p[7]]) {
        1 => format!("ARP who has {} tell {}", ip(&p[24..28]), ip(&p[14..18])),
        2 => format!("ARP {} is at {}", ip(&p[14..18]), mac(&p[8..14])),
        op => format!("ARP operation {op}"),
    }
}

/// An IPv4 packet: its addresses, UDP's ports and DHCP's message when the ports are 67 and
/// 68, TCP's ports and flags.
fn ipv4(packet: &[u8]) -> String {
    let (Some(addresses), Some((protocol, segment))) = (packet.get(12..20), transport(packet))
    else {
        return String::from("IPv4, truncated");
    };
    let ips = format!("IPv4 {}>{}", ip(&addresses[..4]), ip(&addresses[4..]));
    let name = match protocol {
        UDP => "UDP",
        TCP => "TCP",
        other => return format!("{ips} protocol {other}"),
    };
    let Some((from, to)) = ports(segment) else {
        return format!("{ips} {name}, truncated");
    };
    let said = format!("{ips} {name} {from}>{to}");
    match protocol {
        UDP if dhcp_ports(from, to) => {
            format!("{said}, DHCP {}", dhcp(segment.get(8..).unwrap_or(&[])))
        }
        TCP => match segment.get(13) {
            Some(&flags) => {
                let names: Vec<&str> = [(SYN, "SYN"), (ACK, "ACK"), (RST, "RST"), (FIN, "FIN")]
                    .into_iter()
                    .filter(|(bit, _)| flags & bit != 0)
                    .map(|(_, name)| name)
                    .collect();
                format!("{said} [{}]", names.join(" "))
            }
            None => format!("{said}, truncated"),
        },
        _ => said,
    }
}

/// A DHCP message (RFC 2131): its type, from option 53, and its transaction ID.
fn dhcp(message: &[u8]) -> String {
    let Some(xid) = message.get(4..8) else {
        return String::from("truncated");
    };
    let xid = u32::from_be_bytes([xid[0], xid[1], xid[2], xid[3]]);
    let kind = match message_type(message) {
        Some(1) => "DISCOVER",
        Some(2) => "OFFER",
        Some(3) => "REQUEST",
        Some(4) => "DECLINE",
        Some(5) => "ACK",
        Some(6) => "NAK",
        Some(7) => "RELEASE",
        Some(8) => "INFORM",
        Some(_) => "of another type",
        None => "with no message type",
    };
    format!("{kind} xid {xid:#010x}")
}

/// Option 53 of a DHCP message: after the fixed fields and the magic cookie, options are
/// a code and a length, except pad (0) and end (255).
fn message_type(message: &[u8]) -> Option<u8> {
    const OPTIONS: usize = 240;
    if message.get(236..OPTIONS)? != [99, 130, 83, 99] {
        return None;
    }
    let mut at = OPTIONS;
    loop {
        match *message.get(at)? {
            0 => at += 1,
            255 => return None,
            code => {
                let len = usize::from(*message.get(at + 1)?);
                if code == 53 && len == 1 {
                    return message.get(at + 2).copied();
                }
                at += 2 + len;
            }
        }
    }
}
