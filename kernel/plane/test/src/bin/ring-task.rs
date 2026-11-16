//! DS-P's task (`architecture/data-os.md` §5, stream P): a native task in ring mode that runs a
//! batch function. It maps its region and its rings object once, reads each batch in place, and
//! writes `y = 3 * x + 7` of its one `Int64` column into a window as a record of the same
//! layout, which it posts; it sleeps on its bell only when it has nothing to do.
//!
//! At the end of its input it tells the engine (port 2) the bytes the kernel copied for it and
//! its calls on the bells, then asks (port 1) whether to write the window of its last record.
//! Told to, it writes it: the engine took write access to it back before it checked the
//! record, so the write faults and the kernel ends this task. Told not to, it exits 0.
//!
//! It writes one line to its log (`moruna_sys::plane::Log`), which the engine reads back.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write;
use moruna_ipc_check::record::{Need, check_record, framing_need};

use moruna_sys::plane::{Log, Plane, Rings, Step, copied};

moruna_sys::main!(run);

/// The batch function.
fn f(x: i64) -> i64 {
    x.wrapping_mul(3).wrapping_add(7)
}

/// Writes `f` of the record `input` (one `Int64` column) into `window` as a record of the same
/// layout: its framing, then each output value where the input's is. Its length.
fn apply(input: &[u8], window: &mut [u8]) -> Option<usize> {
    let Ok(Need::Framing(end)) = framing_need(input) else {
        return None;
    };
    let record = check_record(input.get(..end)?, input.len() as u64).ok()?;
    let rows = usize::try_from(record.rows).ok()?;
    let (at, len) = *record
        .buffers
        .iter()
        .find(|(_, len)| *len == record.rows * 8)?;
    let (at, len) = (usize::try_from(at).ok()?, usize::try_from(len).ok()?);
    if input.len() > window.len() || at + len > input.len() || len != rows * 8 {
        return None;
    }
    // Everything up to the values, as the input lays it out, then the values computed.
    window[..at].copy_from_slice(&input[..at]);
    let (outs, _) = window[at..at + len].as_chunks_mut::<8>();
    let (xs, _) = input[at..at + len].as_chunks::<8>();
    for (out, x) in outs.iter_mut().zip(xs) {
        *out = f(i64::from_le_bytes(*x)).to_le_bytes();
    }
    window[at + len..input.len()].copy_from_slice(&input[at + len..]);
    Some(input.len())
}

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
    let mut last = None;
    let mut answered = 0u64;
    loop {
        match rings.step(apply) {
            Ok(Step::Answered(w)) => {
                last = Some(w);
                answered += 1;
            }
            Ok(Step::NoInput) => rings.wait_input(),
            Ok(Step::NoWindow) => rings.wait_window(),
            Ok(Step::Ended) => break,
            Err(_) => return 5,
        }
    }
    // The kernel's copies for this task, and its calls on the bells.
    let copies = copied().unwrap_or(u64::MAX).wrapping_sub(before);
    // Its log line, which the engine reads back from the ring it gave the task as `log`.
    if let Some(mut log) = Log::take() {
        let _ = writeln!(log, "ring-task: answered {answered} batches");
    }
    // Two notes follow: the report and the question are port calls.
    let (sleeps, wakes) = rings.bell_calls();
    let mut report = [0u8; 24];
    report[..8].copy_from_slice(&copies.to_le_bytes());
    report[8..16].copy_from_slice(&sleeps.to_le_bytes());
    report[16..].copy_from_slice(&wakes.to_le_bytes());
    rings.note();
    if port.call(2, &report).is_err() {
        return 6;
    }
    // Write the last record's window, if the engine says so.
    rings.note();
    match port.call(1, b"posted") {
        Ok(Some(reply)) if reply.bytes() == b"write" => {}
        // The task says its exit code on its rings as it ends (protocol 4).
        _ => return rings.exit(0),
    }
    let Some(w) = last else {
        return 7;
    };
    let address = core::hint::black_box(rings.window_addr(w));
    // SAFETY: none, on purpose. This is the test: the engine revoked the window before it
    // checked the record in it, the write must fault, and the kernel must end this process.
    unsafe { core::ptr::write_volatile(address as *mut u8, 0x5a) };
    8
}
