//! SV-T13, the driver: started by the supervisor holding only a device memory resource
//! narrowed to one page and an interrupt resource narrowed to one line, it claims both
//! and is refused the pages either side, the next line, a wider resource, and each
//! resource used for the other's kind.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Channel, DeviceMemory, Error, Handle, Interrupt, Signals, map, now, println, wait_one,
};

moruna_sys::main!(run);

fn run() -> i64 {
    let out = console();
    let device = slot("device");
    let interrupt = slot("interrupt");
    let control = Channel(slot("control"));
    if let Err(e) = wait_one(&control.0, Signals::READABLE, now() + ms(10_000)) {
        return fail(&out, "wait for the window", e);
    }
    let mut message = [0u8; 16];
    match control.read(&mut message, &mut []) {
        Ok((16, 0)) => {}
        Ok(_) => return fail(&out, "the window message", Error::BadState),
        Err(e) => return fail(&out, "channel_read", e),
    }
    let mut word = [0u8; 8];
    word.copy_from_slice(&message[..8]);
    let window = u64::from_le_bytes(word);
    word.copy_from_slice(&message[8..]);
    let gsi = u64::from_le_bytes(word) as u32;

    match DeviceMemory::create(&device, window, 4096) {
        Ok(region) => match region.map(0, 4096, map::WRITE) {
            Ok(mapping) => drop(mapping),
            Err(e) => return fail(&out, "mapping its page", e),
        },
        Err(e) => return fail(&out, "claiming its page", e),
    }
    for (what, page) in [
        ("the page after", window + 4096),
        ("the page before", window - 4096),
    ] {
        if let Err(code) = refused(
            &out,
            what,
            DeviceMemory::create(&device, page, 4096).map(|_| ()),
        ) {
            return code;
        }
    }
    if let Err(code) = refused(
        &out,
        "a wider device resource",
        device.narrow(window, 8192).map(|_| ()),
    ) {
        return code;
    }
    match device.narrow(window, 4096) {
        Ok(same) => drop::<Handle>(same),
        Err(e) => return fail(&out, "narrowing to its own window", e),
    }
    if let Err(code) = refused(
        &out,
        "the next line",
        Interrupt::create(&interrupt, gsi + 1).map(|_| ()),
    ) {
        return code;
    }
    if let Err(code) = refused(
        &out,
        "a wider interrupt resource",
        interrupt.narrow(gsi as u64, 2).map(|_| ()),
    ) {
        return code;
    }
    if let Err(code) = refused(
        &out,
        "an interrupt from the device resource",
        Interrupt::create(&device, gsi).map(|_| ()),
    ) {
        return code;
    }
    if let Err(code) = refused(
        &out,
        "device memory from the interrupt resource",
        DeviceMemory::create(&interrupt, window, 4096).map(|_| ()),
    ) {
        return code;
    }
    match Interrupt::create(&interrupt, gsi) {
        Ok(line) => drop(line),
        Err(e) => return fail(&out, "claiming its line", e),
    }
    println!(
        out,
        "KB2 narrowing: ok: a driver given one device page and one interrupt line claimed both and was refused the pages either side, the next line, wider resources and each resource for the other kind"
    );
    0
}

fn refused(out: &moruna_sys::Console, what: &str, result: Result<(), Error>) -> Result<(), i64> {
    match result {
        Err(Error::AccessDenied) => Ok(()),
        Ok(()) => Err(fail(out, what, Error::BadState)),
        Err(e) => Err(fail(out, what, e)),
    }
}

fn fail(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "KB2 narrowing: FAIL: {}: {}", what, e.name());
    1
}
