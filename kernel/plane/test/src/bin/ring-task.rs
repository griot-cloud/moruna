//! DS-P's task (`architecture/data-os.md` §5, stream P): a task in ring mode. It maps its
//! region and its rings object once, echoes every batch the engine posts into a window and
//! posts it, and sleeps on its bell only when it has nothing to do.
//!
//! At the end of its input it tells the engine (port 2) the bytes the kernel copied for it
//! and how many times it slept, waits (port 1) until the engine has taken every record, and
//! then writes the window of its last record: the engine took write access to it back before
//! it checked the record, so the write faults and the kernel ends this task.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use moruna_sys::plane::{Plane, Rings, copied};

moruna_sys::main!(run);

fn run() -> i64 {
    let Some(plane) = Plane::take() else {
        return 2;
    };
    let Some(mut port) = plane.port() else {
        return 3;
    };
    let Ok(mut rings) = Rings::attach(&plane) else {
        return 4;
    };
    let before = copied().unwrap_or(0);
    let mut waits = 0u64;
    let mut last = None;
    let mut record = Vec::new();
    loop {
        let cookie = match rings.batch() {
            Ok(Some((entry, bytes))) => {
                record.clear();
                record.extend_from_slice(bytes);
                Some(entry.cookie)
            }
            Ok(None) => None,
            Err(_) => return 5,
        };
        let Some(cookie) = cookie else {
            if rings.ended() {
                break;
            }
            waits += 1;
            rings.wait_input();
            continue;
        };
        let w = loop {
            match rings.window() {
                Ok(Some((w, window))) => {
                    if record.len() > window.len() {
                        return 6;
                    }
                    window[..record.len()].copy_from_slice(&record);
                    break w;
                }
                Ok(None) => {
                    waits += 1;
                    rings.wait_window();
                }
                Err(_) => return 7,
            }
        };
        if !rings.post(w, record.len() as u64, cookie) {
            return 8;
        }
        rings.release();
        last = Some(w);
    }
    // The kernel's copies for this task, and its sleeps: each sleep reads its bell's word.
    let copies = copied().unwrap_or(u64::MAX).wrapping_sub(before);
    let mut report = [0u8; 16];
    report[..8].copy_from_slice(&copies.to_le_bytes());
    report[8..].copy_from_slice(&waits.to_le_bytes());
    if port.call(2, &report).is_err() {
        return 9;
    }
    // Once the engine has taken every record, write the last one's window.
    if port.call(1, b"posted").is_err() {
        return 10;
    }
    let Some(w) = last else {
        return 11;
    };
    let address = core::hint::black_box(rings.window_addr(w));
    // SAFETY: none, on purpose. This is the test: the engine revoked the window before it
    // checked the record in it, the write must fault, and the kernel must end this process.
    unsafe { core::ptr::write_volatile(address as *mut u8, 0x5a) };
    12
}
