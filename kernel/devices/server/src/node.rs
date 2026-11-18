//! The device manager's process on the node (SDD 25, f.12): the supervisor's work for
//! devices, in a process of its own that the supervisor hands its root resources.
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
//! 4. It serves the directory on every `directory*` slot, registration on every
//!    `register*` slot (the library's `Directory`; a lookup of a name nothing has waits
//!    while a held `register*` slot's driver has not finished registering), and control
//!    on every `control*` slot, and watches each driver: one that ends is started again with
//!    the same resources (its pages in flight wait in its DMA resource's quarantine
//!    until the new driver has reset the device). Lookups that arrive meanwhile are
//!    answered once it runs.
//! 5. When the last directory and control client has gone, it closes the drivers'
//!    listeners, waits for each driver to end, and exits. Unless the supervisor's
//!    message says it serves until it is stopped, as a node's supervisor does (system
//!    design 1C: it lives as long as the node, while its clients come and go): then it
//!    serves on, with or without clients, until the supervisor stops it.
//!
//! A table line whose program is `-` starts no driver: its devices are data disks the engine
//! drives itself. Each is handed, DMA right included, to the one connection that asks through
//! the listener the boot description links to the engine's supervisor
//! (`devices_protocol::HAND_OVER_SLOT`), once (`architecture/data-os.md` §3, decision 2).
//! Nothing else may reach such a device: it has no service to look up, and `sandbox` refuses it.

use alloc::{
    boxed::Box,
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};
use core::cell::RefCell;

use devices_protocol::{Entry, HAND_OVER_SLOT, Windows, control, directory, hand_over_names};
use moruna_devices::{Access, Directory, Drivers, Registry};
use moruna_driver_table::Table;
use moruna_mmio::Sub;
use moruna_pci::{Address, Bar, CONFIG_SIZE, Config, DEVICES, FUNCTIONS};
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, DeviceMemory, Dispatcher, Error,
    Handle, Handler, Memory, Outcome, Pending, Process, ProtocolError, Rights, Signals, Timer,
    Watch, connect, deadline_in, map, now, println, start, wait_one,
};

/// How long the device manager waits for the supervisor's resources, and for its
/// drivers to end once it has closed their listeners.
const WAIT_NS: u64 = 120_000_000_000;
/// The most MSI slots one function gets: an NVMe controller's driver takes one for
/// each I/O queue pair, one per CPU (SDD 25, f.15). A slot holds no vector until its
/// driver makes an interrupt in it.
const MSI_PER_FUNCTION: u64 = 16;

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
    /// A data disk with no driver: whether it has been handed to the supervisor.
    handed: bool,
}

impl Device {
    /// Whether the table line starts a driver for it.
    fn driven(&self) -> bool {
        self.program != moruna_driver_table::NO_DRIVER
    }
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
    guardian: moruna_node::Guardian,
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
        let escrow = self
            .guardian
            .publish(&process)
            .map_err(|e| format!("driver escrow: {}", e.name()))?;
        if let Err(e) = process.start_with(entry, &mut handles) {
            self.guardian
                .retract(escrow)
                .map_err(|e| format!("driver retract: {}", e.name()))?;
            return Err(format!("process_start: {}", e.name()));
        }
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

/// Sends the answers of lookups that waited for a held slot's driver that has since
/// finished (its connection closed, outside any request). An answer whose client has gone
/// is dropped.
fn answer_waiting(dispatcher: &mut Dispatcher, registry: &RefCell<Registry>) {
    let ready = registry.borrow_mut().take_ready();
    for (pending, outcome) in ready {
        let _ = dispatcher.complete(pending, outcome);
    }
}

/// The services the device manager's drivers publish, for the directory. A lookup for a
/// driver that has ended waits until its watch has started it again.
struct Published(Rc<RefCell<State>>);

impl Drivers for Published {
    fn lookup(&mut self, cx: &mut Context<'_>, name: &str) -> Option<Outcome> {
        let mut s = self.0.borrow_mut();
        let index = s.find(name)?;
        if s.shutting {
            return Some(Outcome::Failure(directory::Error::Unavailable.code()));
        }
        let device = &mut s.devices[index];
        if !device.driven() {
            // A data disk is the engine's, and has no service.
            return Some(Outcome::Failure(directory::Error::NotFound.code()));
        }
        Some(match connection(device) {
            Some(c) => Outcome::reply(c),
            None => {
                // The driver has ended and its watch has yet to start it again.
                device.lookups.push(cx.defer());
                Outcome::Deferred
            }
        })
    }

    fn list(&self) -> Vec<Entry> {
        self.0
            .borrow()
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
            .collect()
    }
}

/// Control, through one listener: `hand_over` only when it is the supervisor's.
struct Control {
    state: Rc<RefCell<State>>,
    supervisor: bool,
}

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
        let mut s = self.state.borrow_mut();
        let Some(index) = s.find(&name) else {
            return Outcome::Failure(control::Error::NotFound.code());
        };
        let failed = Outcome::Failure(control::Error::Failed.code());
        let refused = Outcome::Failure(control::Error::Refused.code());
        let device = &mut s.devices[index];
        match ordinal {
            4 => {
                if !self.supervisor || device.driven() || device.handed {
                    return refused;
                }
                let mut handles = Vec::new();
                let all = [&device.config, &device.msi, &device.dma]
                    .into_iter()
                    .chain(device.bars.iter().map(|(_, h)| h));
                for h in all {
                    match dup(h, "a resource") {
                        Ok(h) => handles.push(h),
                        Err(_) => return failed,
                    }
                }
                device.handed = true;
                let bars: Vec<usize> = device.bars.iter().map(|(i, _)| *i).collect();
                let name = device.name.clone();
                println!(s.out, "KB3 devices: {} handed to the supervisor", name);
                Outcome::reply((hand_over_names(&bars), handles))
            }
            2 if !device.driven() => refused,
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

/// Reads the supervisor's message: the five root resources, in order, and whether the
/// device manager serves until it is stopped.
fn roots(supervisor: &Channel) -> Result<(Roots, bool), String> {
    wait_one(&supervisor.0, Signals::READABLE, deadline_in(WAIT_NS))
        .map_err(|e| format!("waiting for the supervisor's resources: {}", e.name()))?;
    let mut bytes = [0u8; 16];
    let mut handles: [Option<Handle>; 5] = Default::default();
    let until_stopped = match supervisor.read(&mut bytes, &mut handles) {
        Ok((n, count)) => match moruna_node::Bootstrap::decode(&bytes[..n], count) {
            Some(moruna_node::Bootstrap::Devices { until_stopped }) => until_stopped,
            _ => return Err("invalid versioned device root bootstrap".to_string()),
        },
        Err(e) => return Err(format!("reading the supervisor's resources: {}", e.name())),
    };
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
    Ok((
        Roots {
            process,
            device,
            ecam,
            msi,
            dma,
        },
        until_stopped,
    ))
}

/// A timer never set: a watch that never fires, so a device manager that serves until
/// it is stopped has a wait to sleep in when it holds no client and no driver.
struct Never(Handle);

impl Watch for Never {
    fn handle(&self) -> &Handle {
        &self.0
    }
    fn signals(&self) -> Signals {
        Signals::SIGNALED
    }
    fn signaled(&mut self, _: Signals, _: &mut Completions) -> bool {
        true
    }
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
/// An empty window, which the core gives when the firmware describes no PCI
/// configuration space (`architecture/moruna-os-system.md`, section 1F), holds none: the
/// device manager says so in one line and serves with no driver.
fn enumerate(out: &Console, ecam: &Handle, table: &Table) -> Result<Vec<Function>, String> {
    let info = ecam
        .resource_info()
        .map_err(|e| format!("resource_info of the ECAM: {}", e.name()))?;
    if info.len == 0 {
        println!(out, "{}", moruna_abi::boot_line::NO_PCI_BUS);
        return Ok(Vec::new());
    }
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

pub fn run() -> i64 {
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
            // The node's console line for a service that cannot go on (the system design,
            // section 1B): the stage is the service's own name.
            println!(
                out,
                "{}",
                moruna_abi::boot_line::Fatal {
                    stage: "devices",
                    reason: &e
                }
            );
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
    let (roots, until_stopped) = roots(&supervisor)?;
    let functions = enumerate(out, &roots.ecam, &table)?;
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
            handed: false,
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
        guardian: moruna_node::Guardian::new(supervisor),
    }));
    let mut dispatcher = Dispatcher::new();
    let count = state.borrow().devices.len();
    let mut started = Vec::new();
    let mut handed = Vec::new();
    for index in 0..count {
        if !state.borrow().devices[index].driven() {
            let s = state.borrow();
            let d = &s.devices[index];
            handed.push(format!("{} at {}", d.name, d.function.address));
            continue;
        }
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
    if !handed.is_empty() {
        println!(
            out,
            "KB3 devices: {} data disks for the supervisor: {}",
            handed.len(),
            handed.join(", ")
        );
    }
    let registry = Rc::new(RefCell::new(Registry::new()));
    let mut listeners = 0;
    for i in 0..start::count() {
        let Some((_, _, slot)) = start::entry(i) else {
            continue;
        };
        let factory: moruna_svc::Factory = if let Some(access) = Access::of(slot) {
            let s = state.clone();
            let registry = registry.clone();
            // A held register slot: its driver is on this machine, and lookups of a name
            // nothing has wait until it has finished, connected or not.
            let held = (access == Access::Register).then(|| registry.borrow_mut().expect());
            Box::new(move || {
                let directory = Directory::new(Published(s.clone()), registry.clone(), access);
                Box::new(match held {
                    Some(number) => directory.through(number),
                    None => directory,
                }) as Box<dyn Handler>
            })
        } else if slot.starts_with("control") {
            let s = state.clone();
            let supervisor = slot == HAND_OVER_SLOT;
            Box::new(move || {
                Box::new(Control {
                    state: s.clone(),
                    supervisor,
                }) as Box<dyn Handler>
            })
        } else {
            continue;
        };
        let channel = Channel(start::take(slot).ok_or_else(|| format!("slot {slot} taken"))?);
        dispatcher
            .listen(channel, factory)
            .map_err(|e| format!("listening on {slot}: {}", e.name()))?;
        listeners += 1;
    }
    if until_stopped {
        // System design 1C: on a node the device manager lives as long as the node.
        // Clients come and go, and the supervisor stops it.
        println!(out, "KB3 devices: serving until the supervisor stops it");
        let never = Timer::create().map_err(|e| format!("timer_create: {}", e.name()))?;
        dispatcher
            .watch(Box::new(Never(never.0)))
            .map_err(|e| format!("watching the idle timer: {}", e.name()))?;
        loop {
            dispatcher
                .turn(DEADLINE_INFINITE)
                .map_err(|e| format!("wait: {}", e.name()))?;
            answer_waiting(&mut dispatcher, &registry);
        }
    }
    while dispatcher.channels() > 0 {
        dispatcher
            .turn(DEADLINE_INFINITE)
            .map_err(|e| format!("wait: {}", e.name()))?;
        answer_waiting(&mut dispatcher, &registry);
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
