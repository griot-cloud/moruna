//! A task of the data plane's boot tests (SDD 29 k): a process with no handle at all,
//! whose only ways out are its three calls. Its first port call asks the engine what to
//! do; what it measures goes back with another.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use moruna_sys::{
    Error, RawHandle, now,
    plane::{self, Input, Output, Plane, Port},
    raw,
};
use plane_test::{Layout, Role, layout, least_median, port, report, role};

moruna_sys::main!(run);

const PAGE: u64 = 4096;

/// The layout of the record a batch or a framing begins, from the shared check.
fn layout_of(bytes: &[u8], code: u64) -> Result<Layout, Stop> {
    layout(&bytes[..bytes.len().min(PAGE as usize)]).map_err(|_| Stop(code))
}

/// The `u64` at `at` of `bytes`.
fn word(bytes: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(w)
}

/// Why a task stopped: a code it reports and exits with.
#[derive(Debug)]
struct Stop(u64);

fn fail<T>(code: u64) -> Result<T, Stop> {
    Err(Stop(code))
}

fn kernel(code: u64) -> impl FnOnce(Error) -> Stop {
    move |_| Stop(code)
}

fn run() -> i64 {
    let Some(plane) = Plane::take() else {
        return 2;
    };
    let Some(mut port) = plane.port() else {
        return 3;
    };
    let role = match port.call(port::ROLE, &[]) {
        Ok(Some(reply)) => reply.bytes().to_vec(),
        _ => return 4,
    };
    let Some(role) = Role::decode(&role) else {
        return 5;
    };
    let done = match role.role {
        role::PRODUCE_BIG => produce_big(&plane, &mut port, &role),
        role::CONSUME_BIG => consume_big(&plane, &mut port),
        role::PUT => put(&plane, &mut port, &role),
        role::CONSUME_ONE => consume_one(&plane, &mut port),
        role::ISOLATE => isolate(&plane, &mut port),
        role::ECHO => echo(&mut port, role.a),
        role::PRODUCE_MANY => produce_many(&plane, &mut port, &role),
        role::CONSUME_MANY => consume_many(&plane, &mut port, role.a),
        _ => fail(6),
    };
    match done {
        Ok(()) => 0,
        Err(Stop(code)) => {
            let _ = port.call(port::REPORT, &report(&[code]));
            code as i64
        }
    }
}

fn send(port: &mut Port<'_>, values: &[u64]) -> Result<(), Stop> {
    port.call(port::REPORT, &report(values))
        .map(|_| ())
        .map_err(kernel(7))
}

fn output<'p>(plane: &'p Plane) -> Result<Output<'p>, Stop> {
    plane.output(0).ok_or(Stop(8))
}

fn input<'p>(plane: &'p Plane) -> Result<Input<'p>, Stop> {
    plane.input(0).ok_or(Stop(9))
}

/// DP-T10: a record laid out as the framing given says, its validity all set and the
/// values `0, 1, ..` of its one column, written in the window and put. Reported: status, the
/// frames of the window's first, middle and last pages, the bytes the kernel copied for
/// this process from before the window to after the put, and how long the window's
/// mapping and the put took.
fn produce_big(plane: &Plane, port: &mut Port<'_>, role: &Role<'_>) -> Result<(), Stop> {
    let mut out = output(plane)?;
    let shape = layout_of(role.bytes, 16)?;
    let len = shape.record_len;
    let copied = plane::copied().map_err(kernel(10))?;
    let t0 = now();
    let mut window = out.window().map_err(kernel(11))?.ok_or(Stop(12))?;
    let t1 = now();
    if window.len() < len {
        return fail(13);
    }
    let bytes = window.bytes();
    bytes[..role.bytes.len()].copy_from_slice(role.bytes);
    let (at, n) = shape.validity;
    bytes[at as usize..(at + n) as usize].fill(0xff);
    let (at, n) = shape.values;
    let values = &mut bytes[at as usize..(at + n) as usize];
    for (i, v) in values.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        *v = (i as u64).to_le_bytes();
    }
    let last = (len - 1) & !(PAGE - 1);
    let frames = [
        plane::frame_at(window.addr()).map_err(kernel(14))?,
        plane::frame_at(window.addr() + ((len / 2) & !(PAGE - 1))).map_err(kernel(14))?,
        plane::frame_at(window.addr() + last).map_err(kernel(14))?,
    ];
    let t2 = now();
    window.put(0, len).map_err(|(e, _)| kernel(15)(e))?;
    let t3 = now();
    let copies = plane::copied().map_err(kernel(10))? - copied;
    send(
        port,
        &[0, frames[0], frames[1], frames[2], copies, t1 - t0, t3 - t2],
    )
}

/// DP-T10: once the engine has offered the batch, takes it, and reports status, the
/// frames of its first, middle and last pages, the bytes copied for this process across
/// `batch_get`, how long it took, and whether the values at the start, the middle and the
/// end are their indices. Then it reads the end, which releases the batch.
fn consume_big(plane: &Plane, port: &mut Port<'_>) -> Result<(), Stop> {
    let mut input = input(plane)?;
    port.call(port::WAIT, &[]).map_err(kernel(20))?;
    let copied = plane::copied().map_err(kernel(21))?;
    let t0 = now();
    let batch = input.get().map_err(kernel(22))?.ok_or(Stop(23))?;
    let t1 = now();
    let copies = plane::copied().map_err(kernel(21))? - copied;
    let len = batch.len();
    let last = (len - 1) & !(PAGE - 1);
    let frames = [
        plane::frame_at(batch.addr()).map_err(kernel(24))?,
        plane::frame_at(batch.addr() + ((len / 2) & !(PAGE - 1))).map_err(kernel(24))?,
        plane::frame_at(batch.addr() + last).map_err(kernel(24))?,
    ];
    let bytes = batch.bytes();
    let shape = layout_of(bytes, 26)?;
    let (at, n) = (shape.values.0 as usize, shape.values.1 as usize);
    let rows = n / 8;
    let value = |i: usize| word(bytes, at + i * 8);
    let right = [0, rows / 2, rows - 1]
        .iter()
        .all(|i| value(*i) == *i as u64);
    let checked = u64::from(right);
    send(
        port,
        &[0, frames[0], frames[1], frames[2], copies, t1 - t0, checked],
    )?;
    match input.get() {
        Ok(None) => Ok(()),
        _ => fail(25),
    }
}

/// DP-T11, DP-T12: the record given, `role.a` bytes of it put. A refused record ends
/// the task here, in its wait for an answer the engine never gives.
fn put(plane: &Plane, port: &mut Port<'_>, role: &Role<'_>) -> Result<(), Stop> {
    let mut out = output(plane)?;
    let mut window = out.window().map_err(kernel(30))?.ok_or(Stop(31))?;
    if (window.len() as usize) < role.bytes.len() {
        return fail(32);
    }
    window.bytes()[..role.bytes.len()].copy_from_slice(role.bytes);
    window.put(0, role.a).map_err(|(e, _)| kernel(33)(e))?;
    port.call(port::WAIT, &[]).map_err(kernel(34))?;
    Ok(())
}

/// DP-T11: one batch: status, the frame of its first page and its first value; then the
/// end, which releases it.
fn consume_one(plane: &Plane, port: &mut Port<'_>) -> Result<(), Stop> {
    let mut input = input(plane)?;
    let batch = input.get().map_err(kernel(40))?.ok_or(Stop(41))?;
    let frame = plane::frame_at(batch.addr()).map_err(kernel(42))?;
    let bytes = batch.bytes();
    let first = word(bytes, layout_of(bytes, 44)?.values.0 as usize);
    send(port, &[0, frame, first])?;
    match input.get() {
        Ok(None) => Ok(()),
        _ => fail(43),
    }
}

/// DP-T13: the plane's bounds. Its plane has input slot 0 and nothing else, and it holds
/// no handle. Reported: status, then how many probes answered other than they must.
fn isolate(plane: &Plane, port: &mut Port<'_>) -> Result<(), Stop> {
    let mut wrong = 0u64;
    let mut must = |r: u64, want: Error| {
        if Error::from_raw(r) != Err(want) {
            wrong += 1;
        }
    };
    // Slots it was not given name nothing; its own input takes no put.
    for n in 1..=4096u64 {
        must(raw::batch_get(n).0, Error::BadHandle);
        must(raw::batch_put(n, 0x10_0000_0000, PAGE), Error::BadHandle);
    }
    must(raw::batch_put(0, 0x10_0000_0000, PAGE), Error::InvalidArgs);
    // It holds no handle: another process's plane, a slot, a process are out of reach.
    for v in 0..=4096 as RawHandle {
        must(raw::plane_get(v, 0).0, Error::BadHandle);
        must(raw::plane_put(v, 0, 0x10_0000_0000, PAGE), Error::BadHandle);
        must(raw::slot_push(v, v, 0, PAGE, 0), Error::BadHandle);
        must(raw::slot_close(v), Error::BadHandle);
    }
    // A port the engine does not answer for is refused, and the task goes on.
    match port.call(port::NONE, b"anyone there?") {
        Err(Error::AccessDenied) => {}
        _ => wrong += 1,
    }
    // Its batch, which the engine then tries to unmap, protect and map over through its
    // handle to this process, while the task waits.
    let mut input = input(plane)?;
    let batch = input.get().map_err(kernel(50))?.ok_or(Stop(51))?;
    let first = batch.bytes().first().copied().unwrap_or(0);
    send(
        port,
        &[0, wrong, u64::from(first), batch.addr(), batch.len()],
    )?;
    port.call(port::WAIT, &[]).map_err(kernel(52))?;
    match input.get() {
        Ok(None) => Ok(()),
        _ => fail(53),
    }
}

/// DP-T14: `count` calls of 64 bytes to the echo port, each reply checked. Reported:
/// status, wrong replies, the least and the median round trip.
fn echo(port: &mut Port<'_>, count: u64) -> Result<(), Stop> {
    let mut samples = Vec::with_capacity(count as usize);
    let mut wrong = 0u64;
    let mut request = [0u8; 64];
    for i in 0..count {
        request[..8].copy_from_slice(&i.to_le_bytes());
        let t0 = now();
        let reply = port.call(port::ECHO, &request).map_err(kernel(60))?;
        let t1 = now();
        if reply.as_ref().map(|r| r.bytes()) != Some(&request[..]) {
            wrong += 1;
        }
        samples.push(t1 - t0);
    }
    let (least, median) = least_median(&mut samples);
    send(port, &[0, wrong, least, median])
}

/// DP-T15: `role.a` records of the framing given (one page), laid out as it says, one a
/// window. Reported: status, then the least and the median of the window's `batch_get`
/// and of `batch_put`.
fn produce_many(plane: &Plane, port: &mut Port<'_>, role: &Role<'_>) -> Result<(), Stop> {
    let mut out = output(plane)?;
    let shape = layout_of(role.bytes, 73)?;
    let len = shape.record_len;
    let mut gets = Vec::with_capacity(role.a as usize);
    let mut puts = Vec::with_capacity(role.a as usize);
    for i in 0..role.a {
        let t0 = now();
        let mut window = out.window().map_err(kernel(70))?.ok_or(Stop(71))?;
        let t1 = now();
        let bytes = window.bytes();
        bytes[..role.bytes.len()].copy_from_slice(role.bytes);
        let (at, n) = shape.validity;
        bytes[at as usize..(at + n) as usize].fill(0xff);
        let at = shape.values.0 as usize;
        bytes[at..at + 8].copy_from_slice(&i.to_le_bytes());
        let t2 = now();
        window.put(0, len).map_err(|(e, _)| kernel(72)(e))?;
        let t3 = now();
        gets.push(t1 - t0);
        puts.push(t3 - t2);
    }
    let (get_least, get_median) = least_median(&mut gets);
    let (put_least, put_median) = least_median(&mut puts);
    send(port, &[0, get_least, get_median, put_least, put_median])
}

/// DP-T15: `count` batches, each one's first value its number. Reported: status, out of
/// order batches, and the least and the median of `batch_get`.
fn consume_many(plane: &Plane, port: &mut Port<'_>, count: u64) -> Result<(), Stop> {
    let mut input = input(plane)?;
    let mut gets = Vec::with_capacity(count as usize);
    let mut wrong = 0u64;
    for i in 0..count {
        let t0 = now();
        let batch = input.get().map_err(kernel(80))?.ok_or(Stop(81))?;
        let t1 = now();
        let bytes = batch.bytes();
        let at = layout_of(bytes, 83)?.values.0 as usize;
        if word(bytes, at) != i {
            wrong += 1;
        }
        gets.push(t1 - t0);
    }
    match input.get() {
        Ok(None) => {}
        _ => return fail(82),
    }
    let (least, median) = least_median(&mut gets);
    send(port, &[0, wrong, least, median])
}
