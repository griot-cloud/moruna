//! The device manager (SDD 25, f.12): the supervisor's work for devices, in a process
//! of its own that the supervisor hands its root resources.
//!
//! 1. It receives from the supervisor the process, device memory, ECAM, MSI and DMA
//!    resources, and reads the driver table the boot image carries.
//! 2. It maps the ECAM window (the kernel gives its range with `resource_info`), walks
//!    every bus, and records each function; for those the table names it sizes the
//!    BARs. Then it lets the window go, so each driver can claim its own page of it.
//! 3. For each function the table names it narrows its resources to that function: a
//!    window over its 4 KiB of configuration space, one over each memory BAR, a range
//!    of MSI slots, and a DMA resource for its requester id alone. It starts the
//!    table's program with duplicates of those, a console and a listener, and
//!    publishes the listener as `SERVICEn` in the directory.
//! 4. It serves the directory on every `directory*` slot and control on every
//!    `control*` slot, and watches each driver: one that ends is started again with
//!    the same resources (its pages in flight wait in its DMA resource's quarantine
//!    until the new driver has reset the device). Lookups that arrive meanwhile are
//!    answered once it runs.
//! 5. When the last directory and control client has gone, it closes the drivers'
//!    listeners, waits for each driver to end, and exits.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{
    boxed::Box,
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};
use core::cell::RefCell;

use devices_protocol::{Entry, Windows, control, directory};
use moruna_driver_table::Table;
use moruna_mmio::Sub;
use moruna_pci::{Address, Bar, CONFIG_SIZE, Config, DEVICES, FUNCTIONS};
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, DeviceMemory, Dispatcher, Error,
    Handle, Handler, Memory, Outcome, Pending, Process, ProtocolError, Rights, Signals, Watch,
    connect, deadline_in, map, now, println, start, wait_one,
};

moruna_svc::main!(run);

/// How long the device manager waits for the supervisor's resources, and for its
/// drivers to end once it has closed their listeners.
const WAIT_NS: u64 = 120_000_000_000;
/// The most MSI slots one function gets.
const MSI_PER_FUNCTION: u64 = 4;

/// A function found on the bus.
struct Function {
    address: Address,
    config: u64,
    vendor: u16,
    device: u16,
    bars: [Option<Bar>; 6],
    msix_entries: u16,
}

/// A device the table names, its resources, and its driver.
struct Device {
    name: String,
    program: String,
    function: Function,
    config: Handle,
    bars: Vec<(usize, Handle)>,
    msi: Handle,
    dma: Handle,
    /// The running driver, for `restart`.
    process: Option<Process>,
    /// The device manager's end of the driver's listener.
    listener: Option<Channel>,
    starts: u32,
    lookups: Vec<Pending>,
    restarts: Vec<Pending>,
}

struct Roots {
    process: Handle,
    device: Handle,
    ecam: Handle,
    msi: Handle,
    dma: Handle,
}

struct State {
    out: Console,
    roots: Roots,
    images: Vec<(String, Handle)>,
    devices: Vec<Device>,
    shutting: bool,
    restarted: u32,
}

fn dup(h: &Handle, what: &str) -> Result<Handle, String> {
    h.duplicate(Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
        .map_err(|e| format!("duplicating {what}: {}", e.name()))
}

impl State {
    /// Starts `device`'s driver: a new process with the device's resources, a console
    /// and a fresh listener. Returns the handle the watch waits on.
    fn start(&mut self, index: usize) -> Result<Handle, String> {
        let console = self
            .out
            .0
            .duplicate(Rights::WRITE | Rights::TRANSFER)
            .map_err(|e| format!("duplicating the console: {}", e.name()))?;
        let d = &self.devices[index];
        let image = self
            .images
            .iter()
            .find(|(p, _)| *p == d.program)
            .map(|(_, h)| h)
            .ok_or_else(|| format!("no image for {}", d.program))?;
        let process = Process::create(&self.roots.process, &d.name)
            .map_err(|e| format!("process_create: {}", e.name()))?;
        let entry = process
            .map_image(image)
            .map_err(|e| format!("loading {}: {}", d.program, e.name()))?;
        let (ours, theirs) =
            Channel::create().map_err(|e| format!("channel_create: {}", e.name()))?;
        let bar_slots: Vec<String> = d.bars.iter().map(|(i, _)| format!("bar{i}")).collect();
        let mut handles: Vec<(Option<Handle>, &str)> = Vec::new();
        handles.push((Some(console), "console"));
        handles.push((Some(dup(&d.config, "config")?), "config"));
        for ((_, bar), slot) in d.bars.iter().zip(&bar_slots) {
            handles.push((Some(dup(bar, slot)?), slot));
        }
        handles.push((Some(dup(&d.msi, "msi")?), "msi"));
        handles.push((Some(dup(&d.dma, "dma")?), "dma"));
        handles.push((Some(theirs.0), "listen"));
        process
            .start_with(entry, &mut handles)
            .map_err(|e| format!("process_start: {}", e.name()))?;
        let watched = process
            .0
            .duplicate(Rights::READ | Rights::WAIT)
            .map_err(|e| format!("duplicating the process: {}", e.name()))?;
        let d = &mut self.devices[index];
        d.process = Some(process);
        d.listener = Some(ours);
        d.starts += 1;
        Ok(watched)
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.devices.iter().position(|d| d.name == name)
    }
}

/// A connection to `device`'s driver, or `None` while it is not running.
fn connection(device: &Device) -> Option<Channel> {
    device.listener.as_ref().and_then(|l| connect(l).ok())
}

/// Watches a driver, and starts it again when it ends.
struct Driver {
    process: Handle,
    index: usize,
    state: Rc<RefCell<State>>,
}

impl Watch for Driver {
    fn handle(&self) -> &Handle {
        &self.process
    }
    fn signals(&self) -> Signals {
        Signals::TERMINATED
    }
    fn signaled(&mut self, _: Signals, done: &mut Completions) -> bool {
        let mut s = self.state.borrow_mut();
        let d = &mut s.devices[self.index];
        d.process = None;
        d.listener = None;
        if s.shutting {
            return false;
        }
        let name = s.devices[self.index].name.clone();
        let started = s.start(self.index);
        let d = &mut s.devices[self.index];
        let lookups = core::mem::take(&mut d.lookups);
        let restarts = core::mem::take(&mut d.restarts);
        match started {
            Ok(process) => {
                let starts = d.starts;
                for pending in lookups {
                    let outcome = match connection(d) {
                        Some(c) => Outcome::reply(c),
                        None => Outcome::Failure(directory::Error::Unavailable.code()),
                    };
                    done.complete(pending, outcome);
                }
                for pending in restarts {
                    done.complete(pending, Outcome::reply(starts));
                }
                s.restarted += 1;
                println!(
                    s.out,
                    "KB3 devices: {} ended; started it again (start {})", name, starts
                );
                self.process = process;
                true
            }
            Err(e) => {
                println!(s.out, "KB3 devices: FAIL: restarting {}: {}", name, e);
                for pending in lookups {
                    done.complete(
                        pending,
                        Outcome::Failure(directory::Error::Unavailable.code()),
                    );
                }
                for pending in restarts {
                    done.complete(pending, Outcome::Failure(control::Error::Failed.code()));
                }
                false
            }
        }
    }
}

/// The directory. A lookup for a driver that has ended waits until its watch has
/// started it again.
struct Directory(Rc<RefCell<State>>);

impl Handler for Directory {
    fn protocol(&self) -> moruna_svc::ProtocolId {
        directory::ID
    }

    fn handle(
        &mut self,
        cx: &mut Context<'_>,
        ordinal: u16,
        d: &mut moruna_svc::Decoder<'_>,
    ) -> Outcome {
        let bad = Outcome::Status(moruna_svc::Status::BadMessage);
        let mut s = self.0.borrow_mut();
        match ordinal {
            1 => {
                let Ok(name) = <String as moruna_svc::Wire>::decode(d) else {
                    return bad;
                };
                if d.finish().is_err() {
                    return bad;
                }
                let Some(index) = s.find(&name) else {
                    return Outcome::Failure(directory::Error::NotFound.code());
                };
                if s.shutting {
                    return Outcome::Failure(directory::Error::Unavailable.code());
                }
                let device = &mut s.devices[index];
                match connection(device) {
                    Some(c) => Outcome::reply(c),
                    None => {
                        // The driver has ended and its watch has yet to start it again.
                        device.lookups.push(cx.defer());
                        Outcome::Deferred
                    }
                }
            }
            2 => {
                if d.finish().is_err() {
                    return bad;
                }
                let entries: Vec<Entry> = s
                    .devices
                    .iter()
                    .map(|d| {
                        (
                            d.name.clone(),
                            (d.function.vendor as u32) << 16 | d.function.device as u32,
                            d.program.clone(),
                            d.starts,
                        )
                    })
                    .collect();
                Outcome::reply(entries)
            }
            _ => Outcome::Status(moruna_svc::Status::UnknownMethod),
        }
    }
}

struct Control(Rc<RefCell<State>>);

impl Handler for Control {
    fn protocol(&self) -> moruna_svc::ProtocolId {
        control::ID
    }

    fn handle(
        &mut self,
        cx: &mut Context<'_>,
        ordinal: u16,
        d: &mut moruna_svc::Decoder<'_>,
    ) -> Outcome {
        if !control::REQUESTS.contains(&ordinal) {
            return Outcome::Status(moruna_svc::Status::UnknownMethod);
        }
        let Ok(name) = <String as moruna_svc::Wire>::decode(d) else {
            return Outcome::Status(moruna_svc::Status::BadMessage);
        };
        if d.finish().is_err() {
            return Outcome::Status(moruna_svc::Status::BadMessage);
        }
        let mut s = self.0.borrow_mut();
        let Some(index) = s.find(&name) else {
            return Outcome::Failure(control::Error::NotFound.code());
        };
        let failed = Outcome::Failure(control::Error::Failed.code());
        let device = &mut s.devices[index];
        match ordinal {
            1 => {
                let Some(process) = device.process.as_ref() else {
                    return failed;
                };
                match process.kill() {
                    Ok(()) | Err(Error::BadState) => {
                        device.restarts.push(cx.defer());
                        Outcome::Deferred
                    }
                    Err(_) => failed,
                }
            }
            2 => {
                let mut handles = Vec::new();
                let all = core::iter::once(&device.config)
                    .chain(device.bars.iter().map(|(_, h)| h))
                    .chain(core::iter::once(&device.msi));
                for h in all {
                    match dup(h, "a resource") {
                        Ok(h) => handles.push(h),
                        Err(_) => return failed,
                    }
                }
                Outcome::reply(handles)
            }
            _ => {
                let f = &device.function;
                let windows: Windows = (
                    f.config,
                    f.bars
                        .iter()
                        .enumerate()
                        .filter_map(|(i, b)| match b {
                            Some(Bar::Memory { base, size, .. }) => Some((i as u32, *base, *size)),
                            _ => None,
                        })
                        .collect(),
                );
                Outcome::reply(windows)
            }
        }
    }
}

/// Reads the supervisor's message: the five root resources, in order.
fn roots(supervisor: &Channel) -> Result<Roots, String> {
    wait_one(&supervisor.0, Signals::READABLE, deadline_in(WAIT_NS))
        .map_err(|e| format!("waiting for the supervisor's resources: {}", e.name()))?;
    let mut bytes = [0u8; 16];
    let mut handles: [Option<Handle>; 5] = Default::default();
    match supervisor.read(&mut bytes, &mut handles) {
        Ok((_, 5)) => {}
        Ok((_, n)) => return Err(format!("the supervisor sent {n} handles, not 5")),
        Err(e) => return Err(format!("reading the supervisor's resources: {}", e.name())),
    }
    let [
        Some(process),
        Some(device),
        Some(ecam),
        Some(msi),
        Some(dma),
    ] = handles
    else {
        return Err("the supervisor's message lacks a handle".to_string());
    };
    Ok(Roots {
        process,
        device,
        ecam,
        msi,
        dma,
    })
}

fn table() -> Result<Table, String> {
    let memory = Memory(start::take("drivers").ok_or("no driver table")?);
    let mapping = memory
        .map(0, 4096, 0)
        .map_err(|e| format!("mapping the driver table: {}", e.name()))?;
    let mut bytes = alloc::vec![0u8; 4096];
    mapping
        .read(0, &mut bytes)
        .map_err(|e| format!("reading the driver table: {}", e.name()))?;
    let len = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let text = core::str::from_utf8(&bytes[..len]).map_err(|_| "the driver table is not text")?;
    Table::parse(text).map_err(|e| e.to_string())
}

/// Walks the ECAM window: every function, with BARs sized for those the table names.
fn enumerate(ecam: &Handle, table: &Table) -> Result<Vec<Function>, String> {
    let info = ecam
        .resource_info()
        .map_err(|e| format!("resource_info of the ECAM: {}", e.name()))?;
    let window = DeviceMemory::create(ecam, info.base, info.len)
        .map_err(|e| format!("claiming the ECAM window: {}", e.name()))?;
    let mapping = window
        .map(0, info.len, map::WRITE)
        .map_err(|e| format!("mapping the ECAM window: {}", e.name()))?;
    let k = |e: Error| e.name().to_string();
    let mut found = Vec::new();
    let buses = (info.len / moruna_pci::BUS_BYTES).min(256) as u16;
    for bus in 0..buses {
        for device in 0..DEVICES {
            for function in 0..FUNCTIONS {
                let address = Address {
                    bus: bus as u8,
                    device,
                    function,
                };
                let offset = address.ecam_offset() as usize;
                let config =
                    Config::new(Sub::new(&mapping, offset, CONFIG_SIZE).map_err(k)?).map_err(k)?;
                if !config.present().map_err(k)? {
                    if function == 0 {
                        break;
                    }
                    continue;
                }
                let (vendor, id) = (config.vendor().map_err(k)?, config.device().map_err(k)?);
                let named =
                    table.find(vendor, id).is_some() && config.header_type().map_err(k)? == 0;
                let bars = if named {
                    config.size_bars().map_err(k)?
                } else {
                    [None; 6]
                };
                let msix_entries = if named {
                    config.msix().map_err(k)?.map_or(0, |m| m.entries)
                } else {
                    0
                };
                found.push(Function {
                    address,
                    config: info.base + offset as u64,
                    vendor,
                    device: id,
                    bars,
                    msix_entries,
                });
                if function == 0 && !config.multifunction().map_err(k)? {
                    break;
                }
            }
        }
    }
    Ok(found)
}

/// What a function's driver holds: its windows, its MSI slots, its DMA right.
struct Resources {
    config: Handle,
    bars: Vec<(usize, Handle)>,
    msi: Handle,
    dma: Handle,
}

/// The resources of a function the table names, narrowed to it.
fn assign(roots: &Roots, function: &Function, msi_next: &mut u64) -> Result<Resources, String> {
    let config = roots
        .ecam
        .narrow(function.config, CONFIG_SIZE as u64)
        .map_err(|e| format!("narrowing the ECAM: {}", e.name()))?;
    let mut bars = Vec::new();
    for (i, bar) in function.bars.iter().enumerate() {
        let Some(bar) = bar else { continue };
        let Some((base, len)) = bar.pages() else {
            continue;
        };
        if base == 0 {
            return Err(format!("bar{i} was given no address by the firmware"));
        }
        let window = roots
            .device
            .narrow(base, len)
            .map_err(|e| format!("narrowing bar{i}: {}", e.name()))?;
        bars.push((i, window));
    }
    let slots = (function.msix_entries as u64).clamp(1, MSI_PER_FUNCTION);
    let msi = roots
        .msi
        .narrow(*msi_next, slots)
        .map_err(|e| format!("narrowing MSI slots: {}", e.name()))?;
    *msi_next += slots;
    let dma = roots
        .dma
        .narrow(function.address.requester_id() as u64, 1)
        .map_err(|e| format!("narrowing DMA: {}", e.name()))?;
    Ok(Resources {
        config,
        bars,
        msi,
        dma,
    })
}

/// Whether two functions' BARs share a page: a driver of one could reach the other.
fn shares_a_page(a: &Function, b: &Function) -> bool {
    let pages = |f: &Function| -> Vec<(u64, u64)> {
        f.bars.iter().flatten().filter_map(Bar::pages).collect()
    };
    let theirs = pages(b);
    pages(a)
        .iter()
        .any(|(s, l)| theirs.iter().any(|(t, m)| *s < t + m && *t < s + l))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    match serve(&out) {
        Ok(summary) => {
            println!(out, "KB3 devices: ok: {}", summary);
            0
        }
        Err(e) => {
            println!(out, "KB3 devices: FAIL: {}", e);
            1
        }
    }
}

fn serve(out: &Console) -> Result<String, String> {
    let supervisor = Channel(start::take("supervisor").ok_or("no supervisor channel")?);
    let table = table()?;
    let mut images = Vec::new();
    for program in table.programs() {
        let slot = format!("{}{}", moruna_driver_table::IMAGE_SLOT, program);
        let image = start::take(&slot).ok_or_else(|| format!("no image in slot {slot}"))?;
        images.push((program.to_string(), image));
    }
    let roots = roots(&supervisor)?;
    let functions = enumerate(&roots.ecam, &table)?;
    let mut devices: Vec<Device> = Vec::new();
    let mut counts: Vec<(String, u32)> = Vec::new();
    let mut msi_next = 0;
    let mut unclaimed = Vec::new();
    for function in functions {
        let Some(driver) = table.find(function.vendor, function.device) else {
            unclaimed.push(format!(
                "{} {:04x}:{:04x}",
                function.address, function.vendor, function.device
            ));
            continue;
        };
        if let Some(other) = devices
            .iter()
            .find(|d| shares_a_page(&d.function, &function))
        {
            println!(
                out,
                "KB3 devices: FAIL: {} shares a BAR page with {}; it gets no driver",
                function.address,
                other.name
            );
            continue;
        }
        let n = match counts.iter_mut().find(|(s, _)| *s == driver.service) {
            Some((_, n)) => {
                *n += 1;
                *n - 1
            }
            None => {
                counts.push((driver.service.clone(), 1));
                0
            }
        };
        let name = format!("{}{}", driver.service, n);
        let Resources {
            config,
            bars,
            msi,
            dma,
        } = assign(&roots, &function, &mut msi_next)
            .map_err(|e| format!("{name} at {}: {e}", function.address))?;
        devices.push(Device {
            name,
            program: driver.program.clone(),
            function,
            config,
            bars,
            msi,
            dma,
            process: None,
            listener: None,
            starts: 0,
            lookups: Vec::new(),
            restarts: Vec::new(),
        });
    }
    let console = Console(
        out.0
            .duplicate(Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
            .map_err(|e| format!("duplicating the console: {}", e.name()))?,
    );
    let state = Rc::new(RefCell::new(State {
        out: console,
        roots,
        images,
        devices,
        shutting: false,
        restarted: 0,
    }));
    let mut dispatcher = Dispatcher::new();
    let count = state.borrow().devices.len();
    let mut started = Vec::new();
    for index in 0..count {
        let process = state.borrow_mut().start(index);
        let s = state.borrow();
        let d = &s.devices[index];
        let process = process.map_err(|e| format!("starting {} ({}): {e}", d.name, d.program))?;
        started.push(format!(
            "{} ({} at {})",
            d.name, d.program, d.function.address
        ));
        dispatcher
            .watch(Box::new(Driver {
                process,
                index,
                state: state.clone(),
            }))
            .map_err(|e| format!("watching {}: {}", d.name, e.name()))?;
    }
    println!(
        out,
        "KB3 devices: started {} drivers: {}; no driver for {}",
        started.len(),
        started.join(", "),
        if unclaimed.is_empty() {
            "none".to_string()
        } else {
            unclaimed.join(", ")
        }
    );
    let mut listeners = 0;
    for i in 0..start::count() {
        let Some((_, _, slot)) = start::entry(i) else {
            continue;
        };
        let factory: moruna_svc::Factory = if slot.starts_with("directory") {
            let s = state.clone();
            Box::new(move || Box::new(Directory(s.clone())) as Box<dyn Handler>)
        } else if slot.starts_with("control") {
            let s = state.clone();
            Box::new(move || Box::new(Control(s.clone())) as Box<dyn Handler>)
        } else {
            continue;
        };
        let channel = Channel(start::take(slot).ok_or_else(|| format!("slot {slot} taken"))?);
        dispatcher
            .listen(channel, factory)
            .map_err(|e| format!("listening on {slot}: {}", e.name()))?;
        listeners += 1;
    }
    while dispatcher.channels() > 0 {
        dispatcher
            .turn(DEADLINE_INFINITE)
            .map_err(|e| format!("wait: {}", e.name()))?;
    }
    // Every client has gone: the drivers' listeners close, and each driver ends when
    // its own clients have.
    {
        let mut s = state.borrow_mut();
        s.shutting = true;
        for d in s.devices.iter_mut() {
            d.listener = None;
        }
    }
    let deadline = now().saturating_add(WAIT_NS);
    while !dispatcher.is_empty() {
        if dispatcher
            .turn(deadline)
            .map_err(|e| format!("wait: {}", e.name()))?
            == moruna_svc::Turn::TimedOut
        {
            let s = state.borrow();
            let running: Vec<&str> = s
                .devices
                .iter()
                .filter(|d| d.process.is_some())
                .map(|d| d.name.as_str())
                .collect();
            return Err(format!(
                "drivers still running after their listeners closed: {}",
                running.join(", ")
            ));
        }
    }
    let s = state.borrow();
    Ok(format!(
        "{} drivers served {} listeners' clients; {} restarts; every driver ended",
        s.devices.len(),
        listeners,
        s.restarted
    ))
}
