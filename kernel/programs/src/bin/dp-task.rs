//! DS-A's task (`architecture/data-os-streams.md`, stream A): the engine side is
//! `datapath`, which revokes this task's windows through its output slot
//! (`windows_protect`'s slot form).
//!
//! 1. It fills its first window, tells the engine, writes the pages the engine left
//!    writable, and puts the window while part of it is still revoked.
//! 2. It fills its second window, tells the engine, and once told to go writes a page
//!    the engine revoked: the write faults, and the kernel ends this task.
//!
//! Start slots: `peer`, a channel to the engine.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{ms, slot};
use moruna_sys::{Channel, Signals, now, plane::Plane, wait_one};

moruna_sys::main!(run);

const PAGE: usize = 4096;

/// Waits for the engine's next word.
fn heard(peer: &Channel, word: &[u8]) -> bool {
    if wait_one(&peer.0, Signals::READABLE, now() + ms(30_000)).is_err() {
        return false;
    }
    let mut buf = [0u8; 16];
    matches!(peer.read(&mut buf, &mut []), Ok((n, 0)) if &buf[..n] == word)
}

fn run() -> i64 {
    let peer = Channel(slot("peer"));
    let Some(plane) = Plane::take() else {
        return 2;
    };
    let Some(mut out) = plane.output(0) else {
        return 3;
    };
    // The first window: filled, then the pages left writable written again, then put.
    let Ok(Some(mut window)) = out.window() else {
        return 4;
    };
    for page in 0..4 {
        window.bytes()[page * PAGE..(page + 1) * PAGE].fill(0xa0 + page as u8);
    }
    if peer.write(b"mapped", []).is_err() || !heard(&peer, b"go") {
        return 5;
    }
    // Pages 1 and 2 are revoked now: 0 and 3 still take writes.
    window.bytes()[0] = 0xb0;
    window.bytes()[3 * PAGE] = 0xb3;
    core::hint::black_box(window.bytes());
    if window.put(0, 4 * PAGE as u64).is_err() {
        return 6;
    }
    if peer.write(b"put", []).is_err() {
        return 7;
    }
    // The second window: filled, then a write to the page the engine revoked.
    let Ok(Some(mut second)) = out.window() else {
        return 8;
    };
    for page in 0..4 {
        second.bytes()[page * PAGE..(page + 1) * PAGE].fill(0xc0 + page as u8);
    }
    if peer.write(b"mapped", []).is_err() || !heard(&peer, b"go") {
        return 9;
    }
    second.bytes()[PAGE] = 0xd1;
    core::hint::black_box(second.bytes());
    // Still here: the revoke did not hold.
    let _ = peer.write(b"survived", []);
    10
}
