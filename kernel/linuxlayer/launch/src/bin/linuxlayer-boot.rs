// SPDX-License-Identifier: MPL-2.0

//! B2's booted test: launches the Linux personality into a new process with an initramfs
//! (and a command line, if given), and reports how the process ended. The process resource
//! is the slot `process`, or the supervisor sends it on the slot `supervisor`.

#![no_std]
#![no_main]

extern crate alloc;

use linuxlayer_launch::launcher::{LaunchError, launch, object_len};
use moruna_sys::{
    Channel, Console, Handle, Memory, Rights, Signals, now, println, start, wait_one,
};

moruna_sys::main!(run);

const SECOND: u64 = 1_000_000_000;

/// How long the personality may run, in seconds.
const WAIT: u64 = 4 * 3600;

fn slot(name: &str) -> Option<Handle> {
    start::take(name)
}

fn run() -> i64 {
    let Some(console) = slot("console") else {
        return 2;
    };
    let out = Console(console);
    match boot(&out) {
        Ok(code) => code,
        Err(what) => {
            println!(out, "B2 boot: FAIL: {what}");
            1
        }
    }
}

fn boot(out: &Console) -> Result<i64, alloc::string::String> {
    use alloc::format;
    let resource = match slot("process") {
        Some(resource) => resource,
        None => from_supervisor()?,
    };

    let image = Memory(slot("personality").ok_or("no personality image")?);
    let len = object_len(&image);
    let mapping = image
        .map(0, len, 0)
        .map_err(|e| format!("mapping the personality image: {e:?}"))?;
    let initramfs = slot("initramfs").ok_or("no initramfs")?;
    let processes = resource
        .duplicate(Rights::WRITE | Rights::TRANSFER)
        .map_err(|e| format!("duplicating the process resource: {e:?}"))?;
    let console = out
        .0
        .duplicate(Rights::WRITE | Rights::TRANSFER)
        .map_err(|e| format!("duplicating the console: {e:?}"))?;
    let process = launch(
        &resource,
        "linux",
        mapping.bytes(),
        &mut [
            (Some(console), "console"),
            (Some(initramfs), "initramfs"),
            (slot("cmdline"), "cmdline"),
            (Some(processes), linuxlayer_launch::slot::PROCESSES),
        ],
    )
    .map_err(|e| match e {
        LaunchError::Plan(p) => format!("planning the personality: {p:?}"),
        LaunchError::Call(what, e) => format!("{what}: {e:?}"),
    })?;
    println!(out, "B2 boot: ok: launched the personality ({len} bytes)");
    // A gVisor batch runs for long; the boot's own timeout bounds a boot that hangs.
    match wait_one(&process.0, Signals::TERMINATED, now() + WAIT * SECOND) {
        Ok(_) => println!(out, "B2 boot: ok: the personality's process ended"),
        Err(e) => {
            println!(
                out,
                "B2 boot: the personality still runs after {WAIT} s ({e:?}); ending it"
            );
            let _ = process.kill();
        }
    }
    Ok(0)
}

/// The process resource the supervisor sends.
fn from_supervisor() -> Result<Handle, alloc::string::String> {
    use alloc::format;
    let supervisor = Channel(slot("supervisor").ok_or("no process or supervisor slot")?);
    wait_one(&supervisor.0, Signals::READABLE, now() + 60 * SECOND)
        .map_err(|e| format!("waiting for the process resource: {e:?}"))?;
    let mut buf = [0u8; 16];
    let mut handles = [None];
    supervisor
        .read(&mut buf, &mut handles)
        .map_err(|e| format!("reading the process resource: {e:?}"))?;
    handles[0]
        .take()
        .ok_or_else(|| "the supervisor sent no process resource".into())
}
