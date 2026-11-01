//! NT-T7 to NT-T9 (SDD 29, k), a client of `net0` and `net1` found through the
//! directory: two virtio-net cards the boot joins through QEMU's socket backend, so
//! every frame out of one leaves QEMU as a UDP datagram on the host and comes back in
//! at the other.
//!
//! - NT-T7: the protocol's suite for one card, against each;
//! - NT-T8: the suite for two cards on one link, `net0` and `net1`;
//! - NT-T9: frames a second from `net0` to `net1` and back, 64-byte and 1514-byte
//!   frames in batches bounded by 64 and the channel wire size, with at most 128 frames between the sender and the
//!   receiver (what one receive queue's buffers hold, so the host's socket buffer is
//!   not what is measured), printed with how many arrived; at least one must. These are not judged
//!   further: under TCG they measure the emulator.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

mod batch;

use alloc::{format, string::String, vec::Vec};

use devices_protocol::directory;
use moruna_svc::{CallError, Channel, Console, conformance, deadline_in, now, println, start};
use net_protocol::{Frame, SEND_FRAMES_MAX, net};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;
/// The most frames between the sender and the receiver: a receive queue's buffers.
const WINDOW: u64 = 128;

struct Lookup<'a> {
    directory: &'a RefDirectory,
    name: &'static str,
}

/// The directory, shared by the connectors of a pair suite.
struct RefDirectory(core::cell::RefCell<directory::Client>);

impl conformance::Connector for Lookup<'_> {
    fn connect(&mut self) -> Result<Channel, conformance::Fail> {
        self.directory
            .0
            .borrow_mut()
            .lookup(String::from(self.name))
            .map_err(|e| format!("looking up {}: {e:?}", self.name))
    }
}

fn open(directory: &RefDirectory, name: &str) -> Result<net::Client, String> {
    let channel = directory
        .0
        .borrow_mut()
        .lookup(String::from(name))
        .map_err(|e| format!("looking up {name}: {e:?}"))?;
    let mut client = net::Client::connect(channel, deadline_in(60 * SECONDS))
        .map_err(|e| format!("hello to {name}: {e:?}"))?;
    client.core().set_timeout(60 * SECONDS);
    Ok(client)
}

/// Frames of `len` bytes from `from` to `to`, `total` of them, as fast as the window
/// lets them go: the frames that arrived, and the nanoseconds it took.
fn stream(
    directory: &RefDirectory,
    from: &str,
    to: &str,
    len: usize,
    total: u64,
) -> Result<(u64, u64, u64), String> {
    let batch_limit = batch::frames_per_send(len, SEND_FRAMES_MAX)
        .ok_or_else(|| format!("frame of {len} bytes cannot fit a send message"))?;
    let mut tx = open(directory, from)?;
    let mut rx = open(directory, to)?;
    let (src, dst) = (
        tx.info().map_err(|e| format!("info: {e:?}"))?.mac,
        rx.info().map_err(|e| format!("info: {e:?}"))?.mac,
    );
    rx.subscribe().map_err(|e| format!("subscribe: {e:?}"))?;
    let dropped_before = rx.stats().map_err(|e| format!("stats: {e:?}"))?.rx_dropped;
    // A receive that finds nothing for two seconds ends the stream: the rest were lost.
    rx.core().set_timeout(2 * SECONDS);
    let (mut sent, mut arrived) = (0u64, 0u64);
    let begin = now();
    while arrived < total {
        while sent < total && sent - arrived < WINDOW {
            let n = (total - sent).min(batch_limit as u64);
            let batch: Vec<Frame> = (0..n)
                .map(|i| net_protocol::frame(dst, src, (sent + i) as u32, len))
                .collect();
            tx.send(batch).map_err(|e| format!("send: {e:?}"))?;
            sent += n;
        }
        match rx.receive(256) {
            Ok(frames) => {
                arrived += frames
                    .iter()
                    .filter(|f| f.0.get(12..14) == Some(&net_protocol::ETHERTYPE.to_be_bytes()[..]))
                    .count() as u64;
            }
            Err(CallError::TimedOut) => break,
            Err(e) => return Err(format!("receive: {e:?}")),
        }
    }
    let took = now() - begin;
    let dropped = rx.stats().map_err(|e| format!("stats: {e:?}"))?.rx_dropped - dropped_before;
    if arrived == 0 {
        return Err(format!("none of {sent} frames from {from} arrived at {to}"));
    }
    Ok((arrived, took, dropped))
}

fn report(
    out: &Console,
    directory: &RefDirectory,
    from: &str,
    to: &str,
    len: usize,
    total: u64,
) -> Result<(), String> {
    let (arrived, took, dropped) = stream(directory, from, to, len, total)?;
    let per_second = arrived * SECONDS / took.max(1);
    let mbits = per_second * len as u64 * 8 / 1_000_000;
    println!(
        out,
        "KE1 net performance: {} to {}: {} of {} frames of {} bytes in {} ms: {} frames a second, {} Mbit/s; {} lost on the way, {} dropped by the receiver's queue",
        from,
        to,
        arrived,
        total,
        len,
        took / 1_000_000,
        per_second,
        mbits,
        total - arrived,
        dropped
    );
    Ok(())
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(d) = start::take("directory") else {
        println!(out, "KE1 net: FAIL: no directory");
        return 1;
    };
    let dir = match devices_protocol::open_directory(Channel(d), deadline_in(300 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KE1 net: FAIL: the directory: {:?}", e);
            return 1;
        }
    };
    let dir = RefDirectory(core::cell::RefCell::new(dir));
    dir.0.borrow_mut().core().set_timeout(300 * SECONDS);
    let mut failed = 0;
    for name in ["net0", "net1"] {
        let suite = net_protocol::suite();
        let summary = conformance::run(
            &suite,
            &mut Lookup {
                directory: &dir,
                name,
            },
            &mut |check| {
                if let Err(why) = check.result {
                    println!(
                        out,
                        "KE1 conformance over {}: FAIL: {}: {}", name, check.name, why
                    );
                }
            },
        );
        if summary.ok() {
            println!(
                out,
                "KE1 conformance {} over {}: ok: {} checks passed",
                suite.id.name,
                name,
                summary.passed
            );
        } else {
            failed += 1;
        }
    }
    let pair = net_protocol::pair_suite();
    let summary = conformance::run_pair(
        &pair,
        &mut Lookup {
            directory: &dir,
            name: "net0",
        },
        &mut Lookup {
            directory: &dir,
            name: "net1",
        },
        &mut |check| {
            if let Err(why) = check.result {
                println!(
                    out,
                    "KE1 conformance over net0 and net1: FAIL: {}: {}", check.name, why
                );
            }
        },
    );
    if summary.ok() {
        println!(
            out,
            "KE1 conformance {} over net0 and net1 on one link: ok: {} checks passed",
            pair.id.name,
            summary.passed
        );
    } else {
        failed += 1;
    }
    for (from, to, len, total) in [
        ("net0", "net1", 64, 20_000),
        ("net1", "net0", 64, 20_000),
        ("net0", "net1", 1514, 5_000),
    ] {
        if let Err(e) = report(&out, &dir, from, to, len, total) {
            println!(out, "KE1 net performance: FAIL: {}", e);
            failed += 1;
        }
    }
    failed
}
