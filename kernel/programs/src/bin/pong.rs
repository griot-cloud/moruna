//! KK-T6: pong echoes every message until ping closes its end.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::slot;
use moruna_sys::{Channel, DEADLINE_INFINITE, Error, Signals, wait_one};

moruna_sys::main!(run);

fn run() -> i64 {
    let peer = Channel(slot("peer"));
    let mut buf = [0u8; 64];
    let mut echoed: i64 = 0;
    loop {
        match wait_one(
            &peer.0,
            Signals::READABLE | Signals::PEER_CLOSED,
            DEADLINE_INFINITE,
        ) {
            Ok(s) if s.contains(Signals::READABLE) => {}
            Ok(_) => return if echoed == 10_000 { 0 } else { 2 },
            Err(_) => return 3,
        }
        match peer.read(&mut buf, &mut []) {
            Ok((n, _)) => {
                if peer.write(&buf[..n], []).is_err() {
                    return 4;
                }
                echoed += 1;
            }
            Err(Error::PeerClosed) => return 5,
            Err(_) => return 6,
        }
    }
}
