//! The vsock device driven from the guest side through its virtqueues, and from the host side
//! through real Unix sockets.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use vm_memory::{Bytes, GuestAddress};

use super::muxer::{BUF_ALLOC, CREDIT_UPDATE_THRESHOLD, FIRST_HOST_PORT, MAX_PKT_PAYLOAD};
use super::packet::{HDR_BYTES, Header, SHUTDOWN_RCV, SHUTDOWN_SEND, TYPE_STREAM, op};
use super::*;
use crate::devices::Device;
use crate::devices::mmio::{MmioTransport, regs};
use crate::testing::{DriverQueue, bring_up, counting_irq, guest_memory, rd, scratch_dir, wr};

const CID: u32 = 3;
const TX_AREA: u64 = 0x100000;
const RX_AREA: u64 = 0x200000;
const RX_BUF: u32 = (HDR_BYTES + 4096) as u32;

struct Guest {
    t: MmioTransport<VirtioVsock>,
    mem: Mem,
    rxq: DriverQueue,
    txq: DriverQueue,
    rx_addr: HashMap<u16, u64>,
    rx_slot: u64,
    tx_slot: u64,
    uds: PathBuf,
    dir: PathBuf,
}

impl Guest {
    fn new() -> Self {
        let dir = scratch_dir("vsock");
        let uds = dir.join("v.sock");
        let dev = VirtioVsock::new(CID, &uds).unwrap();
        let mem = guest_memory(8 << 20);
        let (_, irq) = counting_irq();
        let mut t = MmioTransport::new(dev, mem.clone(), Interrupt::new(irq)).unwrap();
        let rxq = DriverQueue::new(0x10000, 64);
        let txq = DriverQueue::new(0x20000, 64);
        let evq = DriverQueue::new(0x30000, 16);
        bring_up(&mut t, &[&rxq, &txq, &evq]);
        Guest {
            t,
            mem,
            rxq,
            txq,
            rx_addr: HashMap::new(),
            rx_slot: 0,
            tx_slot: 0,
            uds,
            dir,
        }
    }

    fn hdr(&self, op_: u16, src_port: u32, dst_port: u32) -> Header {
        Header {
            src_cid: CID as u64,
            dst_cid: 2,
            src_port,
            dst_port,
            len: 0,
            type_: TYPE_STREAM,
            op: op_,
            flags: 0,
            buf_alloc: 1 << 20,
            fwd_cnt: 0,
        }
    }

    /// Put a packet on the transmit queue and notify it.
    fn send(&mut self, mut h: Header, payload: &[u8]) {
        h.len = payload.len() as u32;
        let base = TX_AREA + (self.tx_slot % 4) * 0x20000;
        self.tx_slot += 1;
        self.mem
            .write_slice(&h.encode(), GuestAddress(base))
            .unwrap();
        if payload.is_empty() {
            self.txq.add(&self.mem, &[(base, HDR_BYTES as u32, false)]);
        } else {
            self.mem
                .write_slice(payload, GuestAddress(base + 0x100))
                .unwrap();
            self.txq.add(
                &self.mem,
                &[
                    (base, HDR_BYTES as u32, false),
                    (base + 0x100, payload.len() as u32, false),
                ],
            );
        }
        wr(&mut self.t, regs::QUEUE_NOTIFY, TXQ as u32);
        self.txq.used(&self.mem);
    }

    /// Offer `n` receive buffers.
    fn post_rx(&mut self, n: usize) {
        for _ in 0..n {
            let addr = RX_AREA + (self.rx_slot % 48) * 0x2000;
            self.rx_slot += 1;
            let head = self.rxq.add(&self.mem, &[(addr, RX_BUF, true)]);
            self.rx_addr.insert(head, addr);
        }
        wr(&mut self.t, regs::QUEUE_NOTIFY, RXQ as u32);
    }

    /// Packets the device returned on the receive queue.
    fn recv(&mut self) -> Vec<(Header, Vec<u8>)> {
        let mut out = Vec::new();
        for (head, len) in self.rxq.used(&self.mem) {
            if len == 0 {
                continue;
            }
            let addr = self.rx_addr[&(head as u16)];
            let mut raw = [0u8; HDR_BYTES];
            self.mem.read_slice(&mut raw, GuestAddress(addr)).unwrap();
            let h = Header::decode(&raw);
            let mut data = vec![0u8; len as usize - HDR_BYTES];
            self.mem
                .read_slice(&mut data, GuestAddress(addr + HDR_BYTES as u64))
                .unwrap();
            assert_eq!(h.len as usize, data.len());
            out.push((h, data));
        }
        out
    }

    /// One backend iteration: poll the host side and serve it.
    fn pump(&mut self) {
        for _ in 0..3 {
            let set = self.t.device().poll_set();
            let fds: Vec<_> = set.iter().map(|(p, _)| *p).collect();
            let ready = crate::sys::poll(&fds, Duration::from_millis(50)).unwrap();
            let pairs: Vec<_> = set.iter().map(|(_, t)| *t).zip(ready).collect();
            self.t
                .with_active(|d, q, m| d.backend_ready(&pairs, q, m))
                .unwrap();
        }
    }

    /// Pump until a packet arrives on the receive queue.
    fn recv_one(&mut self) -> (Header, Vec<u8>) {
        for _ in 0..50 {
            let mut got = self.recv();
            if !got.is_empty() {
                assert_eq!(got.len(), 1, "{got:?}");
                return got.remove(0);
            }
            self.pump();
        }
        panic!("no packet arrived");
    }

    /// Host connects in and the guest accepts on `port`; returns the host stream and the
    /// host port the device chose.
    fn host_connects(&mut self, port: u32) -> (UnixStream, u32) {
        let mut s = UnixStream::connect(&self.uds).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(format!("CONNECT {port}\n").as_bytes()).unwrap();
        self.post_rx(8);
        let (h, _) = self.recv_one();
        assert_eq!(
            (h.op, h.dst_port, h.src_cid, h.dst_cid),
            (op::REQUEST, port, 2, CID as u64)
        );
        assert!(h.src_port >= FIRST_HOST_PORT);
        self.send(self.hdr(op::RESPONSE, port, h.src_port), &[]);
        let mut line = Vec::new();
        let mut b = [0u8];
        while b[0] != b'\n' {
            s.read_exact(&mut b).unwrap();
            line.push(b[0]);
        }
        assert_eq!(
            String::from_utf8(line).unwrap(),
            format!("OK {}\n", h.src_port)
        );
        (s, h.src_port)
    }
}

fn read_n(s: &mut UnixStream, n: usize) -> Vec<u8> {
    // Best effort: macOS refuses SO_RCVTIMEO on a socket whose peer already closed.
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut v = vec![0u8; n];
    s.read_exact(&mut v).unwrap();
    v
}

#[test]
fn vm_t11_identity_and_config() {
    let mut g = Guest::new();
    assert_eq!(rd(&mut g.t, regs::DEVICE_ID), ID_VSOCK);
    let mut cid = [0u8; 8];
    g.t.read(regs::CONFIG, &mut cid);
    assert_eq!(u64::from_le_bytes(cid), CID as u64);
    assert_eq!(g.t.device().cid(), CID);
    assert!(g.uds.exists());
    assert!(g.t.device().waker().is_ok());
    // An event-queue notify does nothing; a stale socket file is replaced on the next boot.
    wr(&mut g.t, regs::QUEUE_NOTIFY, EVQ as u32);
    let again = VirtioVsock::new(CID, &g.dir.join("v2.sock")).unwrap();
    drop(again);
    assert!(
        !g.dir.join("v2.sock").exists(),
        "the socket goes with the device"
    );
    drop(std::os::unix::net::UnixListener::bind(g.dir.join("v3.sock")).unwrap());
    assert!(VirtioVsock::new(CID, &g.dir.join("v3.sock")).is_ok());
    std::fs::write(g.dir.join("file"), b"x").unwrap();
    assert!(VirtioVsock::new(CID, &g.dir.join("file")).is_err());
}

#[test]
fn vm_t11_host_initiated_connection_carries_data_both_ways() {
    let mut g = Guest::new();
    let (mut host, hp) = g.host_connects(5000);
    assert_eq!(g.t.device().muxer().connections(), 1);

    // Guest to host.
    g.send(g.hdr(op::RW, 5000, hp), b"hello from the guest");
    assert_eq!(read_n(&mut host, 20), b"hello from the guest");

    // Host to guest.
    host.write_all(b"spec").unwrap();
    g.post_rx(4);
    let (h, data) = g.recv_one();
    assert_eq!((h.op, h.src_port, h.dst_port), (op::RW, hp, 5000));
    assert_eq!(data, b"spec");
    assert_eq!(h.buf_alloc, BUF_ALLOC);
    assert_eq!(h.fwd_cnt, 20, "every packet carries the host's consumption");
}

#[test]
fn vm_t12_guest_initiated_connections() {
    let mut g = Guest::new();
    g.post_rx(8);
    // A host listener on <uds>_5001.
    let mut p = g.uds.as_os_str().to_owned();
    p.push("_5001");
    let listener = UnixListener::bind(PathBuf::from(p)).unwrap();
    g.send(g.hdr(op::REQUEST, 1234, 5001), &[]);
    let got = g.recv();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].0.op, got[0].0.src_port, got[0].0.dst_port),
        (op::RESPONSE, 5001, 1234)
    );
    let (mut host, _) = listener.accept().unwrap();
    g.send(g.hdr(op::RW, 1234, 5001), b"exit 0\n");
    assert_eq!(read_n(&mut host, 7), b"exit 0\n");
    host.write_all(b"ack").unwrap();
    let (h, data) = g.recv_one();
    assert_eq!((h.op, data.as_slice()), (op::RW, &b"ack"[..]));

    // No listener: refused.
    g.send(g.hdr(op::REQUEST, 1235, 6000), &[]);
    let got = g.recv();
    assert_eq!(got[0].0.op, op::RST);

    // A duplicate REQUEST for a live connection is refused too.
    g.send(g.hdr(op::REQUEST, 1234, 5001), &[]);
    assert_eq!(g.recv()[0].0.op, op::RST);

    // An in-process endpoint.
    let (mut mine, theirs) = UnixStream::pair().unwrap();
    let slot = std::sync::Mutex::new(Some(theirs));
    g.t.device_mut().add_connector(
        7000,
        Box::new(move || {
            slot.lock()
                .unwrap()
                .take()
                .ok_or_else(|| std::io::Error::other("used"))
        }),
    );
    g.send(g.hdr(op::REQUEST, 1300, 7000), &[]);
    assert_eq!(g.recv()[0].0.op, op::RESPONSE);
    g.send(g.hdr(op::RW, 1300, 7000), b"x");
    assert_eq!(read_n(&mut mine, 1), b"x");
    // The connector is single-use here: a second connect is refused.
    g.send(g.hdr(op::REQUEST, 1301, 7000), &[]);
    assert_eq!(g.recv()[0].0.op, op::RST);
}

#[test]
fn vm_t12_monitor_connects_to_a_guest_port() {
    let mut g = Guest::new();
    let (mut mine, theirs) = UnixStream::pair().unwrap();
    g.t.device_mut().connect_to_guest(theirs, 5002).unwrap();
    g.post_rx(4);
    let (h, _) = g.recv_one();
    assert_eq!((h.op, h.dst_port), (op::REQUEST, 5002));
    g.send(g.hdr(op::RESPONSE, 5002, h.src_port), &[]);
    let line = read_n(&mut mine, format!("OK {}\n", h.src_port).len());
    assert!(line.starts_with(b"OK "));
}

#[test]
fn vm_t13_host_to_guest_respects_guest_credit() {
    let mut g = Guest::new();
    let (mut host, hp) = g.host_connects(5000);
    // The guest advertises an 8-byte buffer.
    let mut credit = g.hdr(op::CREDIT_UPDATE, 5000, hp);
    credit.buf_alloc = 8;
    g.send(credit, &[]);
    host.write_all(&[7u8; 20]).unwrap();
    g.post_rx(8);
    let (_, data) = g.recv_one();
    assert_eq!(data.len(), 8);
    g.pump();
    assert!(g.recv().is_empty(), "no credit, no data");
    // The guest consumed 8: 8 more may come.
    credit.fwd_cnt = 8;
    g.send(credit, &[]);
    let (_, data) = g.recv_one();
    assert_eq!(data.len(), 8);
    // A credit request is answered.
    g.send(g.hdr(op::CREDIT_REQUEST, 5000, hp), &[]);
    let got = g.recv();
    assert_eq!(got[0].0.op, op::CREDIT_UPDATE);
}

#[test]
fn vm_t13_guest_to_host_credit_updates_and_overrun() {
    let mut g = Guest::new();
    g.post_rx(16);
    let (mut host, hp) = g.host_connects(5000);
    // Enough data to pass the update threshold: an update comes back once it reached the host.
    let chunk = vec![1u8; MAX_PKT_PAYLOAD];
    let total = (CREDIT_UPDATE_THRESHOLD as usize / MAX_PKT_PAYLOAD) + 1;
    let reader = std::thread::spawn(move || {
        let n = read_n(&mut host, total * MAX_PKT_PAYLOAD).len();
        (n, host)
    });
    for _ in 0..total {
        g.send(g.hdr(op::RW, 5000, hp), &chunk);
        g.pump();
    }
    // The host stream's socket buffer is small: keep serving it until the reader has it all.
    for _ in 0..200 {
        if reader.is_finished() {
            break;
        }
        g.pump();
    }
    let (n, _host) = reader.join().unwrap();
    assert_eq!(n, total * MAX_PKT_PAYLOAD);
    g.pump();
    let updates: Vec<_> = g
        .recv()
        .into_iter()
        .filter(|(h, _)| h.op == op::CREDIT_UPDATE)
        .collect();
    assert!(!updates.is_empty());
    assert!(updates.last().unwrap().0.fwd_cnt >= CREDIT_UPDATE_THRESHOLD);

    // A guest that sends past its credit to a host that is not reading is reset.
    let mut g = Guest::new();
    let (_host, hp) = g.host_connects(5000);
    g.post_rx(8);
    let mut sent = 0;
    while g.t.device().muxer().connections() == 1 && sent < 64 {
        g.send(g.hdr(op::RW, 5000, hp), &chunk);
        sent += 1;
    }
    assert_eq!(g.t.device().muxer().connections(), 0);
    assert!(sent as u64 * MAX_PKT_PAYLOAD as u64 > BUF_ALLOC as u64);
    assert!(g.recv().iter().any(|(h, _)| h.op == op::RST));
}

#[test]
fn vm_t14_shutdown_and_reset_lifecycle() {
    // Host closes: the guest is told the host sends no more.
    let mut g = Guest::new();
    let (host, hp) = g.host_connects(5000);
    drop(host);
    g.post_rx(4);
    let (h, _) = g.recv_one();
    assert_eq!((h.op, h.flags), (op::SHUTDOWN, SHUTDOWN_SEND));
    // The guest closes both ways: the device answers RST and forgets the connection.
    let mut s = g.hdr(op::SHUTDOWN, 5000, hp);
    s.flags = SHUTDOWN_SEND | SHUTDOWN_RCV;
    g.send(s, &[]);
    assert_eq!(g.recv()[0].0.op, op::RST);
    assert_eq!(g.t.device().muxer().connections(), 0);

    // The guest half-closes: data already sent still reaches the host, then EOF.
    let mut g = Guest::new();
    let (mut host, hp) = g.host_connects(5000);
    g.send(g.hdr(op::RW, 5000, hp), b"report");
    let mut s = g.hdr(op::SHUTDOWN, 5000, hp);
    s.flags = SHUTDOWN_SEND;
    g.send(s, &[]);
    let mut all = Vec::new();
    host.read_to_end(&mut all).unwrap();
    assert_eq!(all, b"report");
    assert_eq!(g.t.device().muxer().connections(), 1);

    // The guest resets: gone.
    g.send(g.hdr(op::RST, 5000, hp), &[]);
    assert_eq!(g.t.device().muxer().connections(), 0);
}

#[test]
fn vm_t14_invalid_packets() {
    let mut g = Guest::new();
    g.post_rx(16);
    // Data for a connection that does not exist: RST.
    g.send(g.hdr(op::RW, 1, 2), b"x");
    assert_eq!(g.recv()[0].0.op, op::RST);
    // An RST for one: silence.
    g.send(g.hdr(op::RST, 1, 2), &[]);
    // A spoofed source CID or a packet not for the host: dropped without a reply.
    let mut h = g.hdr(op::REQUEST, 1, 2);
    h.src_cid = 99;
    g.send(h, &[]);
    let mut h = g.hdr(op::REQUEST, 1, 2);
    h.dst_cid = 7;
    g.send(h, &[]);
    assert!(g.recv().is_empty());
    // Not a stream socket: RST (but not in answer to an RST).
    let mut h = g.hdr(op::REQUEST, 1, 2);
    h.type_ = 2;
    g.send(h, &[]);
    assert_eq!(g.recv()[0].0.op, op::RST);
    h.op = op::RST;
    g.send(h, &[]);
    assert!(g.recv().is_empty());
    // A packet claiming more payload than it carries is dropped.
    let mut h = g.hdr(op::RW, 1, 2);
    h.len = 100;
    let base = TX_AREA;
    g.mem.write_slice(&h.encode(), GuestAddress(base)).unwrap();
    g.txq.add(&g.mem, &[(base, HDR_BYTES as u32, false)]);
    wr(&mut g.t, regs::QUEUE_NOTIFY, TXQ as u32);
    assert!(g.recv().is_empty());
    // A short header too.
    g.txq.add(&g.mem, &[(base, 10, false)]);
    wr(&mut g.t, regs::QUEUE_NOTIFY, TXQ as u32);
    assert!(g.recv().is_empty());

    // An unknown operation or a RESPONSE to an established connection tears it down.
    let (_host, hp) = g.host_connects(5000);
    g.send(g.hdr(op::RESPONSE, 5000, hp), &[]);
    assert_eq!(g.recv()[0].0.op, op::RST);
    assert_eq!(g.t.device().muxer().connections(), 0);
    let (_host, hp) = g.host_connects(5000);
    g.send(g.hdr(42, 5000, hp), &[]);
    assert_eq!(g.recv()[0].0.op, op::RST);
    // The guest refuses a host connection: the host stream closes.
    let mut s = UnixStream::connect(&g.uds).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"CONNECT 9\n").unwrap();
    let (h, _) = g.recv_one();
    g.send(g.hdr(op::RST, 9, h.src_port), &[]);
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    assert_eq!(g.t.device().muxer().pending_control(), 0);
}

#[test]
fn vm_t14_bad_connect_lines_are_closed() {
    let mut g = Guest::new();
    for line in [&b"HELLO 5\n"[..], b"CONNECT x\n", &[b'C'; 40][..]] {
        let mut s = UnixStream::connect(&g.uds).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(line).unwrap();
        g.pump();
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }
    // A host that connects and hangs up before its line.
    drop(UnixStream::connect(&g.uds).unwrap());
    g.pump();
    assert_eq!(g.t.device().muxer().connections(), 0);
    // A CONNECT line with a carriage return is accepted.
    let mut s = UnixStream::connect(&g.uds).unwrap();
    s.write_all(b"CONNECT 5000\r\n").unwrap();
    g.post_rx(2);
    assert_eq!(g.recv_one().0.op, op::REQUEST);
    // Receive buffers too small for a header are returned empty and skipped.
    g.rxq.add(&g.mem, &[(RX_AREA, 8, true)]);
    wr(&mut g.t, regs::QUEUE_NOTIFY, RXQ as u32);
}
