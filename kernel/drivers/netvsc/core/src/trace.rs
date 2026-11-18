//! The first frames each way, with their times, for diagnosis only (work item 1.11c): a
//! build with the feature `trace-first-frames`, which a node's normal image never turns on.
//!
//! The item asks whether the node's first DHCP DISCOVER left the guest and whether an OFFER
//! came back. So for the first [`FIRST`] frames out, a line when each is put on the ring to
//! the host (its section, its length, where it goes and whether it is DHCP), a line when the
//! commit that hands it to the host is made (and whether the host was signalled, by VMBus's
//! rule), and a line when the host completes it (with the host's status). For the first
//! [`FIRST`] frames in, a line when each is read. And for the first [`FIRST`] indications
//! the host sends, a line with its status and what it did to the link: the card takes the
//! link to be up from its start, so an indication that the link came up later is otherwise
//! passed over unseen.
//!
//! Every line starts with the time on the clock the caller gives, in milliseconds to the
//! microsecond: for the node's netvsc, its own clock, since its process started, the clock
//! of its console's other lines.

use alloc::{boxed::Box, format, string::String, vec::Vec};

use netvsp_protocol::rndisprot;

/// How many frames are traced each way, and how many indications.
pub const FIRST: u32 = 20;

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
        }
    }

    /// The lines noted since the last take, oldest first.
    pub fn take(&mut self) -> Vec<String> {
        core::mem::take(&mut self.lines)
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
        if self.out >= FIRST {
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
        if self.into >= FIRST {
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

/// A frame as the trace says it: its destination, what it carries, and for DHCP its
/// ports, its message type and its transaction ID.
pub fn describe(frame: &[u8]) -> String {
    let Some(header) = frame.get(..14) else {
        return String::from("shorter than an Ethernet header");
    };
    let d = &header[..6];
    let to = format!(
        "to {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        d[0], d[1], d[2], d[3], d[4], d[5]
    );
    let kind = u16::from_be_bytes([header[12], header[13]]);
    let what = match kind {
        0x0800 => ipv4(&frame[14..]),
        0x0806 => String::from("ARP"),
        0x86dd => String::from("IPv6"),
        other => format!("EtherType {other:#06x}"),
    };
    format!("{to}, {what}")
}

/// An IPv4 packet: UDP's ports, and DHCP's message when the ports are 67 and 68.
fn ipv4(packet: &[u8]) -> String {
    let Some(&first) = packet.first() else {
        return String::from("IPv4, truncated");
    };
    let header = usize::from(first & 0x0f) * 4;
    let Some(&protocol) = packet.get(9) else {
        return String::from("IPv4, truncated");
    };
    if protocol != 17 {
        return format!("IPv4 protocol {protocol}");
    }
    let Some(udp) = packet.get(header..).filter(|udp| udp.len() >= 8) else {
        return String::from("IPv4 UDP, truncated");
    };
    let from = u16::from_be_bytes([udp[0], udp[1]]);
    let to = u16::from_be_bytes([udp[2], udp[3]]);
    let ports = format!("IPv4 UDP {from}>{to}");
    if !matches!(from, 67 | 68) || !matches!(to, 67 | 68) {
        return ports;
    }
    format!("{ports}, DHCP {}", dhcp(&udp[8..]))
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
