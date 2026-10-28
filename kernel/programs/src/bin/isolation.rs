//! KK-T8, prober: every handle-taking call (21, ABI 1.2) on every value from 0 to 4096, and on the
//! holder's live values, must return bad handle. First with only the console, then,
//! after closing it, with no handle at all; the exit code counts the second pass's
//! failures and the kernel prints it.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{Channel, Error, RawHandle, Signals, WaitItem, map, now, println, raw, wait_one};

moruna_sys::main!(run);

const CALLS: usize = 21;
const SWEEP: RawHandle = 4096;

/// The number of the first call that did not answer bad handle for `v`, if any.
fn probe(v: RawHandle) -> Option<usize> {
    let mut items = [WaitItem::raw(v, Signals::ALL)];
    let results: [u64; CALLS] = [
        raw::handle_close(v),
        raw::handle_duplicate(v, 0),
        raw::handle_info(v),
        raw::channel_write(v, &[], &[]),
        raw::channel_read(v, &mut [], &mut []).0,
        raw::memory_map(v, 0, 4096, map::READ),
        raw::process_create(v, b"x"),
        raw::process_map_image(v, v),
        raw::process_start(v, 0x40_0000, &[]),
        raw::timer_set(v, 0),
        raw::wait(&mut items, 0),
        raw::debug_write(v, b""),
        raw::interrupt_create(v, 10),
        raw::interrupt_ack(v),
        raw::device_memory_create(v, 0x10_0000_0000, 4096),
        raw::resource_narrow(v, 0x10_0000_0000, 4096),
        raw::interrupt_create_msi(v, 0).0,
        raw::memory_pin(v, v, 0, &mut []).0,
        raw::dma_release(v),
        raw::resource_info(v, &mut [0; 24]),
        raw::process_kill(v),
    ];
    results
        .iter()
        .position(|r| Error::from_raw(*r) != Err(Error::BadHandle))
}

/// Probes 0..=4096 and `foreign`, skipping `own`; returns how many values it probed,
/// how many failed, and the first failure.
fn sweep(foreign: &[RawHandle], own: Option<RawHandle>) -> (u32, u32, Option<(RawHandle, usize)>) {
    let mut probed = 0;
    let mut failures = 0;
    let mut first = None;
    for v in (0..=SWEEP).chain(foreign.iter().copied()) {
        if Some(v) == own {
            continue;
        }
        probed += 1;
        if let Some(call) = probe(v) {
            failures += 1;
            first.get_or_insert((v, call));
        }
    }
    (probed, failures, first)
}

fn run() -> i64 {
    let out = console();
    let peer = Channel(slot("peer"));
    if wait_one(&peer.0, Signals::READABLE, now() + ms(10_000)).is_err() {
        println!(out, "KB1 isolation: FAIL: no values from the holder");
        return 1;
    }
    let mut bytes = [0u8; 24];
    if !matches!(peer.read(&mut bytes, &mut []), Ok((24, 0))) {
        println!(out, "KB1 isolation: FAIL: bad message from the holder");
        return 1;
    }
    let foreign: [RawHandle; 6] = core::array::from_fn(|i| {
        u32::from_le_bytes([
            bytes[i * 4],
            bytes[i * 4 + 1],
            bytes[i * 4 + 2],
            bytes[i * 4 + 3],
        ])
    });
    let peer_value = peer.0.raw();
    drop(peer);

    let own = out.0.raw();
    let (probed, failures, first) = sweep(&foreign, Some(own));
    if let Some((v, call)) = first {
        println!(
            out,
            "KB1 isolation: FAIL: {} values named something; first {:#x} on call {}",
            failures,
            v,
            call
        );
        return 1;
    }
    println!(
        out,
        "KB1 isolation: ok: {} values x {} calls answered bad handle, {} of them another process's live handles; probing again with no handle at all",
        probed,
        CALLS,
        foreign.len()
    );
    drop(out);
    let mut again = [0; 8];
    again[..6].copy_from_slice(&foreign);
    again[6] = own;
    again[7] = peer_value;
    let (_, failures, _) = sweep(&again, None);
    failures as i64
}
