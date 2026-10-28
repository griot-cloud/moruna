//! KK-T6: ping sends 10,000 numbered messages over one channel and checks each echo.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, slot};
use moruna_sys::{Channel, DEADLINE_INFINITE, Signals, now, println, wait_one};

moruna_sys::main!(run);

const ROUNDS: u64 = 10_000;

fn run() -> i64 {
    let out = console();
    let peer = Channel(slot("peer"));
    let started = now();
    let mut buf = [0u8; 8];
    for i in 0..ROUNDS {
        if let Err((e, _)) = peer.write(&i.to_le_bytes(), []) {
            println!(out, "KB1 ping-pong: FAIL: write {}: {}", i, e.name());
            return 1;
        }
        match wait_one(
            &peer.0,
            Signals::READABLE | Signals::PEER_CLOSED,
            DEADLINE_INFINITE,
        ) {
            Ok(s) if s.contains(Signals::READABLE) => {}
            Ok(_) => {
                println!(out, "KB1 ping-pong: FAIL: pong closed at {}", i);
                return 1;
            }
            Err(e) => {
                println!(out, "KB1 ping-pong: FAIL: wait {}: {}", i, e.name());
                return 1;
            }
        }
        match peer.read(&mut buf, &mut []) {
            Ok((8, 0)) if u64::from_le_bytes(buf) == i => {}
            Ok((n, h)) => {
                println!(
                    out,
                    "KB1 ping-pong: FAIL: round {} echoed {} bytes {} handles", i, n, h
                );
                return 1;
            }
            Err(e) => {
                println!(out, "KB1 ping-pong: FAIL: read {}: {}", i, e.name());
                return 1;
            }
        }
    }
    let us = (now() - started) / 1000;
    println!(
        out,
        "KB1 ping-pong: ok: {} round trips over one channel in {} us", ROUNDS, us
    );
    0
}
