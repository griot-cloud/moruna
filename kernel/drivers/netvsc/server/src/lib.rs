//! The `netvsc` process (`architecture/moruna-os-system.md`, section 1F; work item 1.11b):
//! Azure's synthetic network card, served as `moruna.driver.net`.
//!
//! The supervisor starts it on Hyper-V only, as a client of `vmbus` (a service whose server
//! is not on a machine is not on it either). It holds no DMA grant: `vmbus` pins the
//! buffers it shares, and a frame is copied into a send section, so netvsc pins nothing
//! (1F: a VMBus driver gets a grant only if it pins memory itself).
//!
//! 0. The device manager holds its `register-netvsc` slot, so a lookup of a name nothing
//!    has yet (the network server's of `net0`) waits until netvsc has finished
//!    registering, whether or not it has connected. It connects first thing, before the
//!    card is up, and finishes by closing that connection once `net0` is registered.
//! 1. Through `vmbus`'s `net` listener it lists the synthetic network cards' offers and
//!    opens the first card's channel; another card is said on the console and not served
//!    (1C: one interface, on the first card).
//! 2. It makes the card's two buffers, which `vmbus` shares with the host, and starts the
//!    card with `moruna-netvsc`: NVSP, then RNDIS, then the address and the MTU, which it
//!    says on the console ([`describe`]).
//! 3. It serves `moruna.driver.net` on a listener of its own, with `net_protocol::Server`
//!    as every network driver does ([`Driver`]), and registers that listener with the
//!    device manager as [`NAME`] through the link that carries only that right, so the
//!    network server finds the card as `net0` and `network.conf` is the same on every
//!    cloud (1C). Then it closes that connection: it has finished registering.
//!
//! A frame out is copied into a free section of the send buffer and named to the host; a
//! frame in is copied out of its receive section to each subscribed connection, and the
//! section goes back to the host. The frames a turn puts go to the host together, one
//! commit of the ring and at most one signal (VMBus's rule), not a signal a frame.
//! Between turns the process waits only on handles: its
//! clients' channels and the card's wake channel, after asking the host to signal
//! ([`moruna_netvsc::Netvsc::arm`]), as `storvsc` does. Everything it needs of the node is
//! in `node.rs`, so this library is tested on a host against `moruna-netvsc`'s fake netvsp.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{boxed::Box, format, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use moruna_netvsc::{ETHERNET_HEADER, Event, Netvsc, NetvscError, Shared};
use moruna_svc::{ConnectionId, Context, Decoder, Factory, Handler, Outcome, Pending, ProtocolId};
use moruna_vmbus::Channel;
use net_protocol::{Info, Ring, Server, net, octets};

/// The name the card is registered under, which the network server looks up (1C).
pub const NAME: &str = "net0";

/// The pages of the channel's rings: each ring a control page and 128 of data (512 KiB),
/// as Linux's netvsc sizes them (`ring_size` 128), the outgoing ring first. netvsp is
/// tuned for Linux's sizes; item 3.4's throughput measurement on Azure judges them.
pub const RING_PAGES: u32 = 2 * (1 + 128);

/// The outgoing ring's pages among [`RING_PAGES`].
pub const OUTGOING_PAGES: u32 = 1 + 128;

/// Bytes of the receive buffer, which the host cuts into sections of a frame each: 16 MiB,
/// Linux's netvsc's.
pub const RECEIVE_BUFFER: usize = 16 << 20;

/// Bytes of the send buffer: 6 MiB, Linux's netvsc's 1,024 sections of the host's 6,144
/// bytes, so 1,024 frames in flight.
pub const SEND_BUFFER: usize = 6 << 20;

/// The most refusals said on the console; the rest are only counted.
const REFUSAL_LINES: u64 = 16;

/// What `info` answers for `card`: its address and MTU, no offload, one queue.
pub fn info<C: Channel, S: Shared>(card: &Netvsc<C, S>) -> Info {
    Info {
        mac: net_protocol::mac(card.mac()),
        mtu: card.mtu(),
        max_frame: card.mtu() + ETHERNET_HEADER as u32,
        offloads: 0,
        queues: 1,
    }
}

/// The card, in the console's words: its address, MTU and link.
pub fn describe<C: Channel, S: Shared>(card: &Netvsc<C, S>) -> String {
    let m = octets(net_protocol::mac(card.mac()));
    format!(
        "address {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, MTU {}, link {}, no offloads",
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5],
        card.mtu(),
        if card.link_up() { "up" } else { "down" }
    )
}

/// The card's transmit side, as the server puts frames on it.
struct Transmit<'a, C: Channel, S: Shared>(&'a mut Netvsc<C, S>);

impl<C: Channel, S: Shared> Ring for Transmit<'_, C, S> {
    /// A frame into a free send section, written to the ring and not yet committed:
    /// `Ok(false)` while no section is free or the ring to the host is full, until the
    /// host's completions or its room arrive. The server refuses a frame no card of this
    /// MTU sends before it gets here, so an error is the channel's or the ring's.
    fn put(&mut self, frame: &[u8]) -> Result<bool, String> {
        self.0
            .send(frame)
            .map_err(|e| format!("sending a frame: {e}"))
    }

    /// The frames put since the last flush go to the host together: one commit, and a
    /// signal only if VMBus's rule asks for one.
    fn flush(&mut self) -> Result<(), String> {
        self.0.flush().map_err(|e| format!("sending frames: {e}"))
    }
}

/// The driver: the card, and the protocol's server over it.
pub struct Driver<C: Channel, S: Shared> {
    card: Netvsc<C, S>,
    server: Server,
    /// Lines for the console, which the process's loop writes.
    lines: Vec<String>,
    /// What the host sent that was refused.
    refused: u64,
}

impl<C: Channel, S: Shared> Driver<C, S> {
    /// Serves `card`, started: its address, MTU and link are the server's.
    pub fn new(card: Netvsc<C, S>) -> Driver<C, S> {
        let server = Server::new(info(&card), card.link_up(), 0);
        Driver {
            card,
            server,
            lines: Vec::new(),
            refused: 0,
        }
    }

    /// The card.
    pub fn card(&mut self) -> &mut Netvsc<C, S> {
        &mut self.card
    }

    /// The protocol's server: its hub's counters, the sends waiting.
    pub fn server(&self) -> &Server {
        &self.server
    }

    /// Why the card can no longer be used: the process ends with it.
    pub fn failed(&self) -> Option<&str> {
        self.server.failed.as_deref()
    }

    /// What the host sent that was refused.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// The answers ready to send.
    pub fn take_ready(&mut self) -> Vec<(Pending, Outcome)> {
        core::mem::take(&mut self.server.ready)
    }

    /// The lines to write on the console.
    pub fn take_lines(&mut self) -> Vec<String> {
        core::mem::take(&mut self.lines)
    }

    fn fail(&mut self, why: String) {
        self.server.failed.get_or_insert(why);
    }

    /// Reads everything the host has sent, hands each frame to the subscribed
    /// connections and answers the receives they satisfy, puts waiting frames on the card;
    /// then asks the host to signal, and does it all again while there is more. The
    /// process waits only once this has returned.
    pub fn service(&mut self) {
        loop {
            if self.server.failed.is_some() {
                return;
            }
            self.poll();
            self.server.delivered();
            if self.server.failed.is_some() {
                return;
            }
            let Driver { card, server, .. } = self;
            if let Err(e) = server.transmit(&mut Transmit(card)) {
                self.fail(e);
                return;
            }
            match self.card.arm() {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => {
                    self.fail(format!("the card: {e}"));
                    return;
                }
            }
        }
    }

    /// Everything the host has sent: frames to the hub, the link to the server. A packet
    /// the card refused is counted and said; the channel's or the ring's error ends the
    /// card.
    fn poll(&mut self) {
        loop {
            let Driver {
                card,
                server,
                lines,
                refused,
            } = self;
            match card.poll() {
                Ok(None) => return,
                Ok(Some(Event::Frame(frame))) => server.hub.deliver(frame),
                Ok(Some(Event::Link(up))) => {
                    server.link(up, 0);
                    lines.push(format!("netvsc: link {}", if up { "up" } else { "down" }));
                }
                // The host could not send a frame it had taken.
                Err(NetvscError::NotSent(_)) => server.hub.tx_errors += 1,
                Err(e @ (NetvscError::Channel(_) | NetvscError::Ring(_))) => {
                    server.failed.get_or_insert(format!("the card: {e}"));
                    return;
                }
                // One packet, which is gone: the card goes on.
                Err(e) => {
                    server.hub.rx_errors += 1;
                    *refused += 1;
                    if *refused <= REFUSAL_LINES {
                        lines.push(format!("netvsc: refused {e}"));
                    }
                }
            }
        }
    }

    /// Answers request `ordinal` of the connection `cx` is for.
    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        let Driver { card, server, .. } = self;
        server.handle(&mut Transmit(card), cx, ordinal, d)
    }
}

/// One client's connection to the card.
struct Connection<C: Channel, S: Shared> {
    driver: Rc<RefCell<Driver<C, S>>>,
    id: Option<ConnectionId>,
}

impl<C: Channel, S: Shared> Handler for Connection<C, S> {
    fn protocol(&self) -> ProtocolId {
        net::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        self.id = Some(cx.connection());
        let driver = self.driver.clone();
        let mut driver = driver.borrow_mut();
        let outcome = driver.handle(cx, ordinal, d);
        // What this request's turn finished, its own answer among them maybe.
        driver.service();
        for (pending, answer) in driver.take_ready() {
            cx.complete(pending, answer);
        }
        outcome
    }

    fn closed(&mut self) {
        if let (Some(id), Ok(mut driver)) = (self.id, self.driver.try_borrow_mut()) {
            driver.server.closed(id);
        }
    }
}

/// The factory of the card's listener: each client that connects there is served it.
pub fn factory<C: Channel + 'static, S: Shared + 'static>(
    driver: Rc<RefCell<Driver<C, S>>>,
) -> Factory {
    Box::new(move || {
        Box::new(Connection {
            driver: driver.clone(),
            id: None,
        }) as Box<dyn Handler>
    })
}
