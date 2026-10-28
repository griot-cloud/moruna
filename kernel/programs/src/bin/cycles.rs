//! SV-T14: the cycle rule. An endpoint whose queue carries an endpoint cannot be sent;
//! once drained it travels; a channel's own endpoint never travels through it; and
//! closing the endpoint that holds a queued endpoint closes that one too, so nothing
//! in flight outlives its holder.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms};
use moruna_sys::{Channel, Error, Handle, Signals, now, println, wait_one};

moruna_sys::main!(run);

fn run() -> i64 {
    let out = console();
    let ((a0, a1), (b0, b1)) = match (Channel::create(), Channel::create()) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return fail(&out, "channel_create", e),
    };
    // a1 through its own channel.
    let a1 = match a0.write(b"self", [a1.0]) {
        Err((Error::InvalidArgs, [back])) => Channel(back),
        Err((e, _)) => return fail(&out, "sending an endpoint through its own channel", e),
        Ok(()) => {
            return fail(
                &out,
                "an endpoint travelled through its own channel",
                Error::BadState,
            );
        }
    };
    // b1 into a1's queue: allowed, b1's queue is empty.
    if let Err((e, _)) = a0.write(b"b1", [b1.0]) {
        return fail(&out, "sending b1", e);
    }
    // a1 into b1's queue: refused, a1's queue carries b1.
    let a1 = match b0.write(b"a1", [a1.0]) {
        Err((Error::BadState, [back])) if back.info().is_ok() => Channel(back),
        Err((e, _)) => return fail(&out, "sending a1 while its queue carries b1", e),
        Ok(()) => {
            return fail(
                &out,
                "a1 travelled while its queue carried b1",
                Error::BadState,
            );
        }
    };
    // Drain a1: b1 comes back, and a1 may travel.
    let mut bytes = [0u8; 8];
    let mut handles: [Option<Handle>; 1] = [None];
    let b1 = match a1.read(&mut bytes, &mut handles) {
        Ok((2, 1)) => match handles[0].take() {
            Some(h) => Channel(h),
            None => return fail(&out, "b1 did not come back", Error::BadState),
        },
        Ok(_) => return fail(&out, "the message carrying b1", Error::BadState),
        Err(e) => return fail(&out, "reading b1 back", e),
    };
    if let Err((e, _)) = b0.write(b"a1", [a1.0]) {
        return fail(&out, "sending a1 once drained", e);
    }
    // b1's queue holds a1; closing b1 closes a1, which a0 sees.
    drop(b1);
    match wait_one(&a0.0, Signals::PEER_CLOSED, now() + ms(5_000)) {
        Ok(s) if s.contains(Signals::PEER_CLOSED) => {}
        Ok(_) => return fail(&out, "a1 outlived b1", Error::BadState),
        Err(e) => return fail(&out, "waiting for a1 to close", e),
    }
    println!(
        out,
        "KB2 cycles: ok: an endpoint was refused passage through its own channel and, while its queue carried an endpoint, through another; drained, it travelled; closing its holder closed it"
    );
    0
}

fn fail(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "KB2 cycles: FAIL: {}: {}", what, e.name());
    1
}
