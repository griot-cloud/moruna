//! KK-T8, holder: owns live objects and tells the prober their handle values.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{Channel, Memory, Signals, Timer, now, wait_one};

moruna_sys::main!(run);

fn run() -> i64 {
    let out = console();
    let peer = Channel(slot("peer"));
    let (Ok(memory), Ok(timer), Ok((a, b))) =
        (Memory::create(4096), Timer::create(), Channel::create())
    else {
        return 1;
    };
    let mut values = [0u8; 24];
    for (i, v) in [
        memory.0.raw(),
        timer.0.raw(),
        a.0.raw(),
        b.0.raw(),
        peer.0.raw(),
        out.0.raw(),
    ]
    .iter()
    .enumerate()
    {
        values[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    if peer.write(&values, []).is_err() {
        return 2;
    }
    // Hold everything live until the prober has finished with the values.
    match wait_one(&peer.0, Signals::PEER_CLOSED, now() + ms(60_000)) {
        Ok(_) => 0,
        Err(_) => 3,
    }
}
