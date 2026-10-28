//! The virtio vsock driver (SDD 25, f.11): serves `moruna.driver.vsock`, stream
//! sockets between this machine and its host, on the listener its supervisor gave it.
//!
//! Every packet is a 44-byte header and its payload in one page of a pinned buffer:
//! sixteen pages stay posted on the receive queue, sixteen carry what is sent. A
//! stream's flow is the device's credit scheme (virtio 1.2, 5.10.6.3): the driver
//! sends no more than the peer says it has room for, tells the peer how much room it
//! has (64 KiB a stream) and how much its client has read, and sends a credit update
//! when its client has read half of that since the last one.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    format,
    rc::Rc,
    string::String,
    vec,
    vec::Vec,
};
use core::cell::RefCell;

use moruna_mmio::Region;
use moruna_svc::{
    Channel, Completions, ConnectionId, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher,
    Handle, Handler, Interrupt, Outcome, Pending, ProtocolError, ProtocolId, Signals, Status,
    Watch, Wire, println, start,
};
use moruna_virtio::{
    Buffer,
    device::{Device, DmaBuffer, VirtQueue},
    kind,
};
use vsock_protocol::{CHUNK_MAX, vsock};

moruna_svc::main!(run);

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const EVENT_QUEUE: u16 = 2;
const PAGES: usize = 16;
const HEADER: usize = 44;
const PAYLOAD_MAX: usize = 4096 - HEADER;
/// The room each stream offers its peer.
const BUF_ALLOC: u32 = 64 * 1024;
const FIRST_EPHEMERAL: u32 = 49_152;

/// Header fields (virtio 1.2, 5.10.6).
mod op {
    pub const REQUEST: u16 = 1;
    pub const RESPONSE: u16 = 2;
    pub const RST: u16 = 3;
    pub const SHUTDOWN: u16 = 4;
    pub const RW: u16 = 5;
    pub const CREDIT_UPDATE: u16 = 6;
    pub const CREDIT_REQUEST: u16 = 7;
}
const TYPE_STREAM: u16 = 1;
const SHUTDOWN_SEND: u32 = 2;
const SHUTDOWN_BOTH: u32 = 3;
const EVENT_TRANSPORT_RESET: u32 = 0;

#[derive(Clone, Copy, Debug, Default)]
struct Header {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    kind: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Header {
    fn write(&self, m: &dyn Region, at: usize) -> Result<(), moruna_svc::Error> {
        m.write64(at, self.src_cid)?;
        m.write64(at + 8, self.dst_cid)?;
        m.write32(at + 16, self.src_port)?;
        m.write32(at + 20, self.dst_port)?;
        m.write32(at + 24, self.len)?;
        m.write16(at + 28, self.kind)?;
        m.write16(at + 30, self.op)?;
        m.write32(at + 32, self.flags)?;
        m.write32(at + 36, self.buf_alloc)?;
        m.write32(at + 40, self.fwd_cnt)
    }

    fn read(m: &dyn Region, at: usize) -> Result<Header, moruna_svc::Error> {
        Ok(Header {
            src_cid: m.read64(at)?,
            dst_cid: m.read64(at + 8)?,
            src_port: m.read32(at + 16)?,
            dst_port: m.read32(at + 20)?,
            len: m.read32(at + 24)?,
            kind: m.read16(at + 28)?,
            op: m.read16(at + 30)?,
            flags: m.read32(at + 32)?,
            buf_alloc: m.read32(at + 36)?,
            fwd_cnt: m.read32(at + 40)?,
        })
    }
}

enum Life {
    Connecting(Pending),
    Open,
    /// The peer reset it, or it was never accepted.
    Reset,
}

struct Stream {
    owner: ConnectionId,
    local_port: u32,
    peer_cid: u64,
    peer_port: u32,
    life: Life,
    received: VecDeque<u8>,
    /// Bytes the client has read, all told (what `fwd_cnt` reports).
    forwarded: u32,
    /// `forwarded` as last told to the peer.
    told: u32,
    peer_eof: bool,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Bytes sent, all told.
    sent: u32,
    reader: Option<(Pending, u32)>,
    writers: VecDeque<(Pending, Vec<u8>, usize)>,
}

impl Stream {
    fn credit(&self) -> u32 {
        self.peer_buf_alloc
            .saturating_sub(self.sent.wrapping_sub(self.peer_fwd_cnt))
    }
}

struct Listener {
    owner: ConnectionId,
    backlog: VecDeque<u32>,
    acceptors: VecDeque<Pending>,
}

struct Vsock {
    device: Device,
    rx: VirtQueue,
    tx: VirtQueue,
    events: VirtQueue,
    rx_memory: DmaBuffer,
    tx_memory: DmaBuffer,
    event_memory: DmaBuffer,
    rx_heads: [Option<u16>; PAGES],
    tx_heads: [Option<u16>; PAGES],
    event_heads: [Option<u16>; 4],
    /// Packets waiting for a free transmit page.
    outbox: VecDeque<(Header, Vec<u8>)>,
    cid: u64,
    streams: BTreeMap<u32, Stream>,
    listeners: BTreeMap<u32, Listener>,
    next_stream: u32,
    next_port: u32,
    answers: Vec<(Pending, Outcome)>,
    packets: u64,
    failed: Option<String>,
}

fn fail(e: vsock::Error) -> Outcome {
    Outcome::Failure(e.code())
}

impl Vsock {
    fn post_rx(&mut self, page: usize) -> Result<(), String> {
        let head = self
            .rx
            .queue
            .add(&[Buffer {
                addr: self.rx_memory.addr(page * 4096),
                len: 4096,
                device_writes: true,
            }])
            .map_err(|e| format!("receive queue: {e:?}"))?;
        self.rx_heads[page] = Some(head);
        Ok(())
    }

    fn post_event(&mut self, slot: usize) -> Result<(), String> {
        let head = self
            .events
            .queue
            .add(&[Buffer {
                addr: self.event_memory.addr(slot * 8),
                len: 8,
                device_writes: true,
            }])
            .map_err(|e| format!("event queue: {e:?}"))?;
        self.event_heads[slot] = Some(head);
        Ok(())
    }

    /// A header from `local_port` to the peer, with this side's credit.
    fn header(&self, id: Option<u32>, local: u32, cid: u64, port: u32, op: u16) -> Header {
        let fwd_cnt = id
            .and_then(|i| self.streams.get(&i))
            .map_or(0, |s| s.forwarded);
        Header {
            src_cid: self.cid,
            dst_cid: cid,
            src_port: local,
            dst_port: port,
            len: 0,
            kind: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: BUF_ALLOC,
            fwd_cnt,
        }
    }

    fn send_packet(&mut self, header: Header, payload: Vec<u8>) {
        self.outbox.push_back((header, payload));
    }

    /// Hands the device every waiting packet a free transmit page holds.
    fn flush_outbox(&mut self) -> Result<(), String> {
        let mut sent = false;
        while let Some(page) = self.tx_heads.iter().position(Option::is_none) {
            let Some((mut header, payload)) = self.outbox.pop_front() else {
                break;
            };
            header.len = payload.len() as u32;
            let at = page * 4096;
            let w = |e: moruna_svc::Error| format!("transmit page: {}", e.name());
            header.write(&self.tx_memory.map, at).map_err(w)?;
            self.tx_memory
                .map
                .write_bytes(at + HEADER, &payload)
                .map_err(w)?;
            let head = self
                .tx
                .queue
                .add(&[Buffer {
                    addr: self.tx_memory.addr(at),
                    len: (HEADER + payload.len()) as u32,
                    device_writes: false,
                }])
                .map_err(|e| format!("transmit queue: {e:?}"))?;
            self.tx_heads[page] = Some(head);
            self.packets += 1;
            sent = true;
        }
        if sent {
            self.device.notify(&self.tx)?;
        }
        Ok(())
    }

    fn reset_to(&mut self, header: &Header) {
        let mut rst = self.header(
            None,
            header.dst_port,
            header.src_cid,
            header.src_port,
            op::RST,
        );
        rst.src_cid = self.cid;
        self.send_packet(rst, Vec::new());
    }

    /// Sends as much of each waiting write as the peer's credit allows, and answers
    /// the writes now wholly queued.
    fn pump(&mut self, id: u32) {
        let Some(s) = self.streams.get(&id) else {
            return;
        };
        let (local, cid, port) = (s.local_port, s.peer_cid, s.peer_port);
        loop {
            let Some(s) = self.streams.get_mut(&id) else {
                return;
            };
            if !matches!(s.life, Life::Open) {
                return;
            }
            let credit = s.credit() as usize;
            let Some((_, data, done)) = s.writers.front_mut() else {
                return;
            };
            if *done == data.len() {
                if let Some((pending, data, _)) = s.writers.pop_front() {
                    self.answers
                        .push((pending, Outcome::reply(data.len() as u32)));
                }
                continue;
            }
            if credit == 0 {
                return;
            }
            let n = (data.len() - *done).min(credit).min(PAYLOAD_MAX);
            let chunk = data[*done..*done + n].to_vec();
            *done += n;
            s.sent = s.sent.wrapping_add(n as u32);
            let header = self.header(Some(id), local, cid, port, op::RW);
            self.send_packet(header, chunk);
        }
    }

    /// Answers a waiting `recv` from what has arrived, and tells the peer of room made.
    fn feed_reader(&mut self, id: u32) {
        let Some(s) = self.streams.get_mut(&id) else {
            return;
        };
        let ready = !s.received.is_empty() || s.peer_eof || matches!(s.life, Life::Reset);
        if !ready {
            return;
        }
        let Some((pending, max)) = s.reader.take() else {
            return;
        };
        if s.received.is_empty() && matches!(s.life, Life::Reset) && !s.peer_eof {
            self.answers.push((pending, fail(vsock::Error::Reset)));
            return;
        }
        let n = s.received.len().min(max as usize);
        let bytes: Vec<u8> = s.received.drain(..n).collect();
        s.forwarded = s.forwarded.wrapping_add(n as u32);
        let update = s.forwarded.wrapping_sub(s.told) >= BUF_ALLOC / 2;
        let (local, cid, port) = (s.local_port, s.peer_cid, s.peer_port);
        if update {
            s.told = s.forwarded;
        }
        self.answers.push((pending, Outcome::reply(bytes)));
        if update {
            let header = self.header(Some(id), local, cid, port, op::CREDIT_UPDATE);
            self.send_packet(header, Vec::new());
        }
    }

    /// Ends a stream the peer reset: its waiting requests are answered.
    fn reset_stream(&mut self, id: u32) {
        let Some(s) = self.streams.get_mut(&id) else {
            return;
        };
        match core::mem::replace(&mut s.life, Life::Reset) {
            Life::Connecting(pending) => {
                self.answers.push((pending, fail(vsock::Error::Refused)));
                self.streams.remove(&id);
                return;
            }
            Life::Open | Life::Reset => {}
        }
        for (pending, _, _) in s.writers.drain(..) {
            self.answers.push((pending, fail(vsock::Error::Reset)));
        }
        self.feed_reader(id);
    }

    fn stream_for(&self, h: &Header) -> Option<u32> {
        self.streams
            .iter()
            .find(|(_, s)| {
                s.local_port == h.dst_port && s.peer_port == h.src_port && s.peer_cid == h.src_cid
            })
            .map(|(id, _)| *id)
    }

    fn new_stream(
        &mut self,
        owner: ConnectionId,
        local: u32,
        cid: u64,
        port: u32,
        life: Life,
    ) -> u32 {
        let id = self.next_stream;
        self.next_stream = self.next_stream.wrapping_add(1).max(1);
        self.streams.insert(
            id,
            Stream {
                owner,
                local_port: local,
                peer_cid: cid,
                peer_port: port,
                life,
                received: VecDeque::new(),
                forwarded: 0,
                told: 0,
                peer_eof: false,
                peer_buf_alloc: 0,
                peer_fwd_cnt: 0,
                sent: 0,
                reader: None,
                writers: VecDeque::new(),
            },
        );
        id
    }

    /// One packet from the device.
    fn packet(&mut self, h: Header, payload: Vec<u8>) {
        if h.kind != TYPE_STREAM || h.dst_cid != self.cid {
            if h.op != op::RST {
                self.reset_to(&h);
            }
            return;
        }
        let Some(id) = self.stream_for(&h) else {
            if h.op == op::REQUEST
                && let Some(l) = self.listeners.get(&h.dst_port)
            {
                let owner = l.owner;
                let id = self.new_stream(owner, h.dst_port, h.src_cid, h.src_port, Life::Open);
                if let Some(s) = self.streams.get_mut(&id) {
                    s.peer_buf_alloc = h.buf_alloc;
                    s.peer_fwd_cnt = h.fwd_cnt;
                }
                let response =
                    self.header(Some(id), h.dst_port, h.src_cid, h.src_port, op::RESPONSE);
                self.send_packet(response, Vec::new());
                if let Some(l) = self.listeners.get_mut(&h.dst_port) {
                    match l.acceptors.pop_front() {
                        Some(pending) => self
                            .answers
                            .push((pending, Outcome::reply((id, h.src_cid, h.src_port)))),
                        None => l.backlog.push_back(id),
                    }
                }
            } else if h.op != op::RST {
                self.reset_to(&h);
            }
            return;
        };
        if let Some(s) = self.streams.get_mut(&id) {
            s.peer_buf_alloc = h.buf_alloc;
            s.peer_fwd_cnt = h.fwd_cnt;
        }
        match h.op {
            op::RESPONSE => {
                if let Some(s) = self.streams.get_mut(&id)
                    && let Life::Connecting(pending) = core::mem::replace(&mut s.life, Life::Open)
                {
                    self.answers.push((pending, Outcome::reply(id)));
                }
                self.pump(id);
            }
            op::RW => {
                let over = self
                    .streams
                    .get(&id)
                    .is_none_or(|s| s.received.len() + payload.len() > BUF_ALLOC as usize);
                if over {
                    // The peer sent past the room it was given.
                    self.reset_to(&h);
                    self.reset_stream(id);
                    return;
                }
                if let Some(s) = self.streams.get_mut(&id) {
                    s.received.extend(payload);
                }
                self.feed_reader(id);
            }
            op::CREDIT_UPDATE => self.pump(id),
            op::CREDIT_REQUEST => {
                if let Some(s) = self.streams.get(&id) {
                    let (local, cid, port) = (s.local_port, s.peer_cid, s.peer_port);
                    let header = self.header(Some(id), local, cid, port, op::CREDIT_UPDATE);
                    self.send_packet(header, Vec::new());
                }
            }
            op::SHUTDOWN => {
                if let Some(s) = self.streams.get_mut(&id) {
                    if h.flags & SHUTDOWN_SEND != 0 {
                        s.peer_eof = true;
                    }
                }
                if h.flags & SHUTDOWN_BOTH == SHUTDOWN_BOTH {
                    self.reset_to(&h);
                    self.reset_stream(id);
                } else {
                    self.feed_reader(id);
                }
            }
            op::RST => self.reset_stream(id),
            _ => {
                self.reset_to(&h);
                self.reset_stream(id);
            }
        }
    }

    fn service(&mut self) -> Result<(), String> {
        let mut reposted = false;
        while let Some(used) = self
            .rx
            .queue
            .pop_used()
            .map_err(|e| format!("receive ring: {e:?}"))?
        {
            let page = self
                .rx_heads
                .iter()
                .position(|h| *h == Some(used.head))
                .ok_or("the device returned a receive page not posted")?;
            let at = page * 4096;
            let w = |e: moruna_svc::Error| format!("receive page: {}", e.name());
            let header = Header::read(&self.rx_memory.map, at).map_err(w)?;
            let len = (header.len as usize)
                .min(PAYLOAD_MAX)
                .min((used.len as usize).saturating_sub(HEADER));
            let mut payload = vec![0u8; len];
            self.rx_memory
                .map
                .read_bytes(at + HEADER, &mut payload)
                .map_err(w)?;
            self.post_rx(page)?;
            reposted = true;
            self.packet(header, payload);
        }
        if reposted {
            self.device.notify(&self.rx)?;
        }
        while let Some(used) = self
            .tx
            .queue
            .pop_used()
            .map_err(|e| format!("transmit ring: {e:?}"))?
        {
            let page = self
                .tx_heads
                .iter()
                .position(|h| *h == Some(used.head))
                .ok_or("the device returned a transmit page not in flight")?;
            self.tx_heads[page] = None;
        }
        let mut events = false;
        while let Some(used) = self
            .events
            .queue
            .pop_used()
            .map_err(|e| format!("event ring: {e:?}"))?
        {
            let slot = self
                .event_heads
                .iter()
                .position(|h| *h == Some(used.head))
                .ok_or("the device returned an event buffer not posted")?;
            let id = self
                .event_memory
                .map
                .read32(slot * 8)
                .map_err(|e| format!("event: {}", e.name()))?;
            if id == EVENT_TRANSPORT_RESET {
                // Every stream is gone, and the cid may have changed.
                let ids: Vec<u32> = self.streams.keys().copied().collect();
                for id in ids {
                    self.reset_stream(id);
                }
                self.cid = read_cid(&self.device)?;
            }
            self.post_event(slot)?;
            events = true;
        }
        if events {
            self.device.notify(&self.events)?;
        }
        self.flush_outbox()
    }

    /// A connection went: its streams are reset and its listeners closed.
    fn forget(&mut self, owner: ConnectionId) {
        let ids: Vec<u32> = self
            .streams
            .iter()
            .filter(|(_, s)| s.owner == owner)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(s) = self.streams.remove(&id)
                && matches!(s.life, Life::Open)
            {
                let rst = self.header(None, s.local_port, s.peer_cid, s.peer_port, op::RST);
                self.send_packet(rst, Vec::new());
            }
        }
        self.listeners.retain(|_, l| l.owner != owner);
        if let Err(e) = self.flush_outbox() {
            self.failed.get_or_insert(e);
        }
    }
}

fn read_cid(device: &Device) -> Result<u64, String> {
    device
        .transport
        .device_config(|c| c.read64(0))
        .map_err(|e| format!("guest cid: {e}"))
}

struct Connection(Rc<RefCell<Vsock>>, Option<ConnectionId>);

fn decode<T: Wire>(d: &mut Decoder<'_>) -> Option<T> {
    T::decode(d).ok()
}

impl Connection {
    fn request(v: &mut Vsock, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        let me = cx.connection();
        let bad = Outcome::Status(Status::BadMessage);
        let owned = |v: &Vsock, id: u32| v.streams.get(&id).is_some_and(|s| s.owner == me);
        match ordinal {
            1 => {
                if d.finish().is_err() {
                    return bad;
                }
                Outcome::reply(v.cid)
            }
            2 => {
                let (Some(cid), Some(port)) = (decode::<u64>(d), decode::<u32>(d)) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                let local = v.next_port;
                v.next_port = if local == u32::MAX {
                    FIRST_EPHEMERAL
                } else {
                    local + 1
                };
                let pending = cx.defer();
                let id = v.new_stream(me, local, cid, port, Life::Connecting(pending));
                let header = v.header(Some(id), local, cid, port, op::REQUEST);
                v.send_packet(header, Vec::new());
                Outcome::Deferred
            }
            3 => {
                let Some(port) = decode::<u32>(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                if v.listeners.contains_key(&port) {
                    return fail(vsock::Error::InUse);
                }
                v.listeners.insert(
                    port,
                    Listener {
                        owner: me,
                        backlog: VecDeque::new(),
                        acceptors: VecDeque::new(),
                    },
                );
                Outcome::reply(())
            }
            4 => {
                let Some(port) = decode::<u32>(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                let Some(l) = v.listeners.get_mut(&port).filter(|l| l.owner == me) else {
                    return fail(vsock::Error::NotListening);
                };
                match l.backlog.pop_front() {
                    Some(id) => match v.streams.get(&id) {
                        Some(s) => Outcome::reply((id, s.peer_cid, s.peer_port)),
                        None => fail(vsock::Error::Reset),
                    },
                    None => {
                        l.acceptors.push_back(cx.defer());
                        Outcome::Deferred
                    }
                }
            }
            5 => {
                let (Some(id), Some(data)) = (decode::<u32>(d), decode::<Vec<u8>>(d)) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                if !owned(v, id) {
                    return fail(vsock::Error::NoStream);
                }
                if data.is_empty() {
                    return fail(vsock::Error::Empty);
                }
                let Some(s) = v.streams.get_mut(&id) else {
                    return fail(vsock::Error::NoStream);
                };
                if !matches!(s.life, Life::Open) {
                    return fail(vsock::Error::Reset);
                }
                s.writers.push_back((cx.defer(), data, 0));
                v.pump(id);
                Outcome::Deferred
            }
            6 => {
                let (Some(id), Some(max)) = (decode::<u32>(d), decode::<u32>(d)) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                if !owned(v, id) {
                    return fail(vsock::Error::NoStream);
                }
                if max == 0 {
                    return fail(vsock::Error::Empty);
                }
                let Some(s) = v.streams.get_mut(&id) else {
                    return fail(vsock::Error::NoStream);
                };
                if s.reader.is_some() {
                    return fail(vsock::Error::InUse);
                }
                s.reader = Some((cx.defer(), max.min(CHUNK_MAX)));
                v.feed_reader(id);
                Outcome::Deferred
            }
            7 => {
                let Some(id) = decode::<u32>(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                if !owned(v, id) {
                    return fail(vsock::Error::NoStream);
                }
                if let Some(s) = v.streams.remove(&id) {
                    if let Life::Connecting(pending) = s.life {
                        v.answers.push((pending, fail(vsock::Error::Reset)));
                    }
                    if let Some((pending, _)) = s.reader {
                        v.answers.push((pending, fail(vsock::Error::Reset)));
                    }
                    for (pending, _, _) in s.writers {
                        v.answers.push((pending, fail(vsock::Error::Reset)));
                    }
                    let mut shutdown =
                        v.header(None, s.local_port, s.peer_cid, s.peer_port, op::SHUTDOWN);
                    shutdown.flags = SHUTDOWN_BOTH;
                    v.send_packet(shutdown, Vec::new());
                    let rst = v.header(None, s.local_port, s.peer_cid, s.peer_port, op::RST);
                    v.send_packet(rst, Vec::new());
                }
                Outcome::reply(())
            }
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }
}

impl Handler for Connection {
    fn protocol(&self) -> ProtocolId {
        vsock::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        self.1 = Some(cx.connection());
        let mut v = self.0.borrow_mut();
        let outcome = Connection::request(&mut v, cx, ordinal, d);
        if let Err(e) = v.flush_outbox() {
            v.failed.get_or_insert(e);
        }
        for (pending, answer) in v.answers.drain(..) {
            cx.complete(pending, answer);
        }
        outcome
    }

    fn closed(&mut self) {
        if let Some(me) = self.1 {
            let mut v = self.0.borrow_mut();
            v.forget(me);
            v.answers.clear();
        }
    }
}

struct Irq {
    line: Interrupt,
    vsock: Rc<RefCell<Vsock>>,
}

impl Watch for Irq {
    fn handle(&self) -> &Handle {
        &self.line.0
    }
    fn signals(&self) -> Signals {
        Signals::SIGNALED
    }
    fn signaled(&mut self, _: Signals, done: &mut Completions) -> bool {
        let mut v = self.vsock.borrow_mut();
        let result = self
            .line
            .ack()
            .map_err(|e| format!("interrupt_ack: {}", e.name()))
            .and_then(|()| v.service());
        for (pending, answer) in v.answers.drain(..) {
            done.complete(pending, answer);
        }
        match result {
            Ok(()) => true,
            Err(e) => {
                v.failed.get_or_insert(e);
                false
            }
        }
    }
}

fn open() -> Result<(Vsock, Interrupt), String> {
    let (device, line) = Device::open(kind::VSOCK, 0, 0)?;
    let cid = read_cid(&device)?;
    let rx = device.queue(RX_QUEUE, 64)?;
    let tx = device.queue(TX_QUEUE, 64)?;
    let events = device.queue(EVENT_QUEUE, 4)?;
    let rx_memory = DmaBuffer::new(&device.dma, (PAGES * 4096) as u64)?;
    let tx_memory = DmaBuffer::new(&device.dma, (PAGES * 4096) as u64)?;
    let event_memory = DmaBuffer::new(&device.dma, 4096)?;
    device.ready()?;
    let mut v = Vsock {
        device,
        rx,
        tx,
        events,
        rx_memory,
        tx_memory,
        event_memory,
        rx_heads: [None; PAGES],
        tx_heads: [None; PAGES],
        event_heads: [None; 4],
        outbox: VecDeque::new(),
        cid,
        streams: BTreeMap::new(),
        listeners: BTreeMap::new(),
        next_stream: 1,
        next_port: FIRST_EPHEMERAL,
        answers: Vec::new(),
        packets: 0,
        failed: None,
    };
    for page in 0..PAGES {
        v.post_rx(page)?;
    }
    for slot in 0..v.event_heads.len().min(v.events.queue.size() as usize) {
        v.post_event(slot)?;
    }
    v.device.notify(&v.rx)?;
    v.device.notify(&v.events)?;
    Ok((v, line))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let name = start::process_name();
    let Some(listener) = start::take("listen").map(Channel) else {
        println!(out, "KB3 {}: FAIL: no listener", name);
        return 1;
    };
    let (vsock, line) = match open() {
        Ok(v) => v,
        Err(e) => {
            println!(out, "KB3 {}: FAIL: {}", name, e);
            return 1;
        }
    };
    let cid = vsock.cid;
    let vsock = Rc::new(RefCell::new(vsock));
    let mut dispatcher = Dispatcher::new();
    let shared = vsock.clone();
    let factory: moruna_svc::Factory =
        Box::new(move || Box::new(Connection(shared.clone(), None)) as Box<dyn Handler>);
    let setup = dispatcher.listen(listener, factory).and_then(|()| {
        dispatcher.watch(Box::new(Irq {
            line,
            vsock: vsock.clone(),
        }))
    });
    if let Err(e) = setup {
        println!(out, "KB3 {}: FAIL: dispatcher: {}", name, e.name());
        return 1;
    }
    println!(out, "KB3 {}: virtio-vsock up as cid {}", name, cid);
    while dispatcher.channels() > 0 {
        if let Some(e) = vsock.borrow_mut().failed.take() {
            println!(out, "KB3 {}: FAIL: the device: {}", name, e);
            return 1;
        }
        if let Err(e) = dispatcher.turn(DEADLINE_INFINITE) {
            println!(out, "KB3 {}: FAIL: wait: {}", name, e.name());
            return 1;
        }
    }
    println!(
        out,
        "KB3 {}: sent {} packets until every client left",
        name,
        vsock.borrow().packets
    );
    0
}
