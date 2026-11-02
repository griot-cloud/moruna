//! An interface's device as smoltcp sees it (SDD 31, f.2): frames received and not yet
//! taken, frames made and not yet sent, and how many more may be made. The server
//! moves frames between a port and its card; the loopback's port hands what it makes
//! back to itself.

use alloc::{collections::VecDeque, vec::Vec};

use smoltcp::{
    phy::{self, Checksum, ChecksumCapabilities, DeviceCapabilities, Medium},
    time::Instant,
};

/// The frames of one interface.
pub struct Port {
    /// Frames received, oldest first.
    pub rx: VecDeque<Vec<u8>>,
    /// Frames made, to be sent, oldest first.
    pub tx: Vec<Vec<u8>>,
    /// The most frames `tx` may hold: what the card can take before it answers.
    pub tx_room: usize,
    /// The most frames `rx` holds; a frame that finds it full is dropped.
    pub rx_room: usize,
    medium: Medium,
    /// The largest IP packet: the card's MTU.
    mtu: usize,
    /// The largest frame a card carries (`mtu` and the Ethernet header).
    max_frame: usize,
    /// Frames dropped because `rx` was full, or because their length is not a frame's.
    pub dropped: u64,
    pub rx_frames: u64,
    pub tx_frames: u64,
}

impl Port {
    pub fn ethernet(mtu: usize, rx_room: usize, tx_room: usize) -> Port {
        Port::new(Medium::Ethernet, mtu, mtu + 14, rx_room, tx_room)
    }

    pub fn loopback(mtu: usize) -> Port {
        // Loopback frames go back into `rx` within one poll; a bound keeps one poll's
        // work bounded.
        Port::new(Medium::Ip, mtu, mtu, 4096, 4096)
    }

    fn new(medium: Medium, mtu: usize, max_frame: usize, rx_room: usize, tx_room: usize) -> Port {
        Port {
            rx: VecDeque::new(),
            tx: Vec::new(),
            tx_room,
            rx_room,
            medium,
            mtu,
            max_frame,
            dropped: 0,
            rx_frames: 0,
            tx_frames: 0,
        }
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }

    /// Queues a frame a card received: untrusted, so one shorter than a header or
    /// longer than the card's largest frame, or one that finds the queue full, is
    /// dropped and counted.
    pub fn deliver(&mut self, frame: Vec<u8>) {
        let least = if self.medium == Medium::Ethernet {
            14
        } else {
            1
        };
        if frame.len() < least || frame.len() > self.max_frame || self.rx.len() >= self.rx_room {
            self.dropped += 1;
            return;
        }
        self.rx_frames += 1;
        self.rx.push_back(frame);
    }
}

pub struct Rx(Vec<u8>);

pub struct Tx<'a> {
    tx: &'a mut Vec<Vec<u8>>,
    count: &'a mut u64,
}

impl phy::RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = alloc::vec![0u8; len];
        let r = f(&mut frame);
        *self.count += 1;
        self.tx.push(frame);
        r
    }
}

impl phy::Device for Port {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        if self.tx.len() >= self.tx_room {
            // smoltcp answers some frames at once (ARP, echo); with no room to answer,
            // the frame waits.
            return None;
        }
        let frame = self.rx.pop_front()?;
        Some((
            Rx(frame),
            Tx {
                tx: &mut self.tx,
                count: &mut self.tx_frames,
            },
        ))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        (self.tx.len() < self.tx_room).then_some(Tx {
            tx: &mut self.tx,
            count: &mut self.tx_frames,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = self.medium;
        caps.max_transmission_unit = self.max_frame;
        caps.max_burst_size = None;
        // The cards offload nothing (`moruna.driver.net` 1.0): every checksum is
        // computed and checked here. Loopback's frames never leave the server, but
        // they are checked all the same, so it behaves as a card does.
        let mut checksum = ChecksumCapabilities::default();
        checksum.ipv4 = Checksum::Both;
        checksum.udp = Checksum::Both;
        checksum.tcp = Checksum::Both;
        checksum.icmpv4 = Checksum::Both;
        checksum.icmpv6 = Checksum::Both;
        caps.checksum = checksum;
        caps
    }
}
