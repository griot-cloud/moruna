//! The edu driver (SDD 25, DR-T17): a third party's driver as the table sees it. It
//! opens its function from the windows the device manager gave it, maps BAR 0, and
//! serves `moruna.test.edu` by reading and writing the device's registers, each a
//! 32-bit access. It needs no interrupt: a factorial is waited for by polling the
//! status register, within a deadline.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::rc::Rc;

use edu_protocol::edu;
use moruna_pci::function::Function;
use moruna_svc::{Channel, Console, Context, Dispatcher, Mapping, now, println, start};

moruna_svc::main!(run);

const IDENTIFICATION: usize = 0x00;
const LIVENESS: usize = 0x04;
const FACTORIAL: usize = 0x08;
const STATUS: usize = 0x20;
const COMPUTING: u32 = 1;
const VENDOR: u16 = 0x1234;
const DEVICE: u16 = 0x11e8;
/// How long a factorial may take.
const COMPUTE_NS: u64 = 1_000_000_000;

/// BAR 0: its window, and where the BAR starts in it.
struct Bar {
    map: Mapping<'static>,
    delta: usize,
}

impl Bar {
    fn read(&self, register: usize) -> Result<u32, edu::Error> {
        self.map
            .read32(self.delta + register)
            .map_err(|_| edu::Error::TimedOut)
    }

    fn write(&self, register: usize, value: u32) -> Result<(), edu::Error> {
        self.map
            .write32(self.delta + register, value)
            .map_err(|_| edu::Error::TimedOut)
    }
}

struct Edu(Rc<Bar>);

impl edu::Server for Edu {
    fn identify(&mut self, _: &mut Context<'_>) -> Result<u32, edu::Error> {
        self.0.read(IDENTIFICATION)
    }

    fn liveness(&mut self, _: &mut Context<'_>, value: u32) -> Result<u32, edu::Error> {
        self.0.write(LIVENESS, value)?;
        self.0.read(LIVENESS)
    }

    fn factorial(&mut self, _: &mut Context<'_>, n: u32) -> Result<u32, edu::Error> {
        self.0.write(FACTORIAL, n)?;
        let deadline = now() + COMPUTE_NS;
        while self.0.read(STATUS)? & COMPUTING != 0 {
            if now() >= deadline {
                return Err(edu::Error::TimedOut);
            }
            core::hint::spin_loop();
        }
        self.0.read(FACTORIAL)
    }
}

fn open() -> Result<(Function, Bar), alloc::string::String> {
    let mut function = Function::open()?;
    match (function.config.vendor(), function.config.device()) {
        (Ok(VENDOR), Ok(DEVICE)) => {}
        other => return Err(alloc::format!("not an edu device: {other:?}")),
    }
    let [Some((map, delta)), ..] = function.take_bars() else {
        return Err(alloc::string::String::from("no window for bar0"));
    };
    if map.len() < delta + 0x100 {
        return Err(alloc::string::String::from("bar0's window is too small"));
    }
    function.enable_bus_master()?;
    Ok((
        function,
        Bar {
            map,
            delta: delta as usize,
        },
    ))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let name = start::process_name();
    let Some(listener) = start::take("listen").map(Channel) else {
        println!(out, "KB3 {}: FAIL: no listener", name);
        return 1;
    };
    // The function holds the claims on the device's windows for as long as it runs.
    let (_function, bar) = match open() {
        Ok(opened) => opened,
        Err(e) => {
            println!(out, "KB3 {}: FAIL: {}", name, e);
            return 1;
        }
    };
    let id = match bar.read(IDENTIFICATION) {
        Ok(id) => id,
        Err(e) => {
            println!(out, "KB3 {}: FAIL: identification: {:?}", name, e);
            return 1;
        }
    };
    let bar = Rc::new(bar);
    let mut dispatcher = Dispatcher::new();
    if let Err(e) = dispatcher.listen(listener, edu::factory(move || Edu(bar.clone()))) {
        println!(out, "KB3 {}: FAIL: dispatcher: {}", name, e.name());
        return 1;
    }
    println!(out, "KB3 {}: edu up: identification {:#x}", name, id);
    match dispatcher.run() {
        Ok(()) => 0,
        Err(e) => {
            println!(out, "KB3 {}: FAIL: wait: {}", name, e.name());
            1
        }
    }
}
