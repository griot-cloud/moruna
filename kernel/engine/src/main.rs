//! The engine's supervisor (SDD 30 d.3): the first process of the run after the
//! servers it needs, it starts Moruna's engine, an unmodified Linux program, through the
//! Linux personality's `run_granted`, with what its `config` names: the program, its
//! arguments and environment, its memory budget and scratch quota, and the handles it
//! starts with (a connection to the vsock driver, the personality's listener, the attached
//! image's record). It starts it `runs` times, one after another, each run serving one job,
//! and prints how each ended.
//!
//! Start slots: `console`; `config`, the configuration (`engine::Config`); `linux`, a
//! listener of `moruna.linux`; `directory`, the device manager's directory, when a grant
//! looks a service up; each data handle a `grant SLOT data FROM` names as `FROM`;
//! each listener a `grant SLOT service FROM` explicitly names as `FROM`; and `handover`, a
//! listener of the device manager's control linked to its `control.engine` slot, when a
//! `grant disk.NAME disk DEVICE` asks for a data disk (`architecture/data-os.md` §3). Each data
//! disk is handed over once, and each run is given duplicates of its handles. Each process
//! resource a `grant SLOT process FROM` names as `FROM` (stream P).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};

use devices_protocol::{control, directory};
use engine::{Config, Source};
use linux_protocol::{Bytes, Ended, Grant, how, linux};
use moruna_svc::{
    Account, CallError, Channel, Console, Handle, Memory, Rights, Signals, Timer, deadline_in, map,
    now, println, start, wait_one,
};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;
const PAGE: u64 = 4096;
/// How long one run of the engine may take, emulation included.
const RUN_SECONDS: u64 = 3600;

fn b(s: &str) -> Bytes {
    Bytes(s.as_bytes().to_vec())
}

/// The configuration: the text of the data handle `name`, a page at most, as the boot's
/// image holds a file (its bytes, then zeros to the page's end).
fn read_config(name: &str) -> Result<String, String> {
    let memory = Memory(start::take(name).ok_or_else(|| format!("no `{name}` slot"))?);
    let page = memory
        .map(0, PAGE, map::READ)
        .map_err(|e| format!("mapping {name}: {}", e.name()))?;
    let mut bytes = alloc::vec![0u8; PAGE as usize];
    page.read(0, &mut bytes)
        .map_err(|e| format!("reading {name}: {}", e.name()))?;
    let Some(len) = bytes.iter().position(|b| *b == 0) else {
        return Err(format!("{name} is larger than a page"));
    };
    bytes.truncate(len);
    String::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8"))
}

/// The data handles the grants name, taken from the supervisor's start page once: each
/// run is given a duplicate.
fn data_slots(config: &Config) -> Result<BTreeMap<String, Handle>, String> {
    let mut out = BTreeMap::new();
    for g in &config.grants {
        let from = match &g.source {
            Source::Data(from) => from,
            Source::Admission { control, .. } => control,
            _ => continue,
        };
        if !out.contains_key(from) {
            let handle = start::take(from).ok_or_else(|| {
                format!(
                    "`grant {} data {from}` names a data handle the supervisor was not given",
                    g.slot
                )
            })?;
            out.insert(from.clone(), handle);
        }
    }
    Ok(out)
}

/// The functions a `grant SLOT functions FROM` hands on: the index the supervisor was given as
/// `FROM`, and each function's file, `function.N`, taken from the start page once and checked
/// to be memory; `None` for no such grant, or no index (a job that uses no function).
struct Functions {
    slot: String,
    index: Handle,
    files: Vec<(String, Handle)>,
}

fn function_slots(config: &Config) -> Result<Option<Functions>, String> {
    let Some((slot, from)) = config.grants.iter().find_map(|g| match &g.source {
        Source::Functions(from) => Some((g.slot.clone(), from.clone())),
        _ => None,
    }) else {
        return Ok(None);
    };
    let Some(index) = start::take(&from) else {
        return Ok(None);
    };
    let text = page_text(&index, &from, node_protocol::functions::INDEX_MAX as u64)?;
    let mut files = Vec::new();
    for name in engine::function_slots(&text)? {
        let file = start::take(&name).ok_or_else(|| {
            format!("the functions' index lists `{name}`, and the supervisor was not given it")
        })?;
        match file.info() {
            Ok((moruna_svc::ObjectType::Memory, _)) => files.push((name, file)),
            _ => return Err(format!("`{name}` is not a memory object")),
        }
    }
    Ok(Some(Functions { slot, index, files }))
}

/// The text of the memory object `handle` (`name` in messages): its bytes up to its first zero,
/// within its first `most` bytes.
fn page_text(handle: &Handle, name: &str, most: u64) -> Result<String, String> {
    let memory = Memory(
        handle
            .duplicate(Rights::READ | Rights::MAP)
            .map_err(|e| format!("duplicating {name}: {}", e.name()))?,
    );
    let mapping = memory
        .map(0, most, map::READ)
        .map_err(|e| format!("mapping {name}: {}", e.name()))?;
    let mut bytes = alloc::vec![0u8; most as usize];
    mapping
        .read(0, &mut bytes)
        .map_err(|e| format!("reading {name}: {}", e.name()))?;
    let len = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    bytes.truncate(len);
    String::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8"))
}

/// A run's duplicates of the functions: the index read-only under its slot, and each file
/// read-only (it may be mapped and handed on, never written) under `function.N`.
fn function_grants(functions: &Option<Functions>) -> Result<Vec<(Bytes, Handle)>, String> {
    let Some(functions) = functions else {
        return Ok(Vec::new());
    };
    let mut out = alloc::vec![(
        b(&functions.slot),
        functions
            .index
            .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .map_err(|e| format!("duplicating the functions' index: {}", e.name()))?,
    )];
    for (name, file) in &functions.files {
        let handle = file
            .duplicate(Rights::READ | Rights::MAP | Rights::DUPLICATE | Rights::TRANSFER)
            .map_err(|e| format!("duplicating `{name}`: {}", e.name()))?;
        out.push((b(name), handle));
    }
    Ok(out)
}

// stream P (architecture/data-os.md section 5)
/// The process resources the grants name, taken from the supervisor's start page once and
/// checked to be process resources: each run is given a duplicate.
fn process_slots(config: &Config) -> Result<BTreeMap<String, Handle>, String> {
    let mut out = BTreeMap::new();
    for g in &config.grants {
        if let Source::Process(from) = &g.source
            && !out.contains_key(from)
        {
            let handle = start::take(from).ok_or_else(|| {
                format!(
                    "`grant {} process {from}` names a process resource the supervisor was not \
                     given",
                    g.slot
                )
            })?;
            match handle.resource_info() {
                Ok(info) if info.kind == moruna_svc::ResourceKind::Process => {}
                _ => return Err(format!("`{from}` is not a process resource")),
            }
            out.insert(from.clone(), handle);
        }
    }
    Ok(out)
}

/// Take each explicitly granted service listener once. Keep its owner here; each run
/// connects afresh rather than inheriting a previous run's channel or data rights.
fn service_slots(config: &Config) -> Result<BTreeMap<String, Channel>, String> {
    let mut out = BTreeMap::new();
    for grant in &config.grants {
        if let Source::Service(from) = &grant.source
            && !out.contains_key(from)
        {
            let listener = start::take(from).map(Channel).ok_or_else(|| {
                format!(
                    "`grant {} service {from}` names a listener the supervisor was not given",
                    grant.slot
                )
            })?;
            out.insert(from.clone(), listener);
        }
    }
    Ok(out)
}

/// Private admission endpoints are already connected, not public listeners.
fn admission_slots(
    config: &Config,
) -> Result<BTreeMap<String, moruna_storage_service::protocol::admission::Client>, String> {
    let mut clients = BTreeMap::new();
    for grant in &config.grants {
        if let Source::Admission { authority, .. } = &grant.source
            && !clients.contains_key(authority)
        {
            let channel = Channel(
                start::take(authority)
                    .ok_or_else(|| format!("no private admission `{authority}`"))?,
            );
            let client = moruna_storage_service::protocol::admission::Client::connect(
                channel,
                deadline_in(5 * SECONDS),
            )
            .map_err(|e| format!("private admission `{authority}` handshake: {e:?}"))?;
            clients.insert(authority.clone(), client);
        }
    }
    Ok(clients)
}

/// Validate sole canonical Admission framing, then charge the actual RPC control
/// loan to the runtime child. Boot data is an assertion, never admission authority.
fn canonical_control(handle: &Handle, padded: bool) -> Result<Vec<u8>, String> {
    let borrowed = Memory(
        handle
            .duplicate(Rights::READ | Rights::MAP)
            .map_err(|e| format!("admission data: {}", e.name()))?,
    );
    let first = borrowed
        .map(0, PAGE, map::READ)
        .map_err(|e| format!("admission prefix: {}", e.name()))?;
    let mut prefix = [0u8; moruna_storage_format::wire::PREFIX_BYTES];
    first
        .read(0, &mut prefix)
        .map_err(|e| format!("admission prefix read: {}", e.name()))?;
    drop(first);
    let length = moruna_storage_format::wire::header_bytes(&prefix)
        .map_err(|_| String::from("invalid canonical admission length"))?;
    let pages = (length as u64).div_ceil(PAGE) * PAGE;
    let mapping = borrowed
        .map(0, pages, map::READ)
        .map_err(|e| format!("admission data map: {}", e.name()))?;
    let mut bytes = alloc::vec![0u8; if padded { pages as usize } else { length }];
    mapping
        .read(0, &mut bytes)
        .map_err(|e| format!("admission data read: {}", e.name()))?;
    Ok(bytes)
}
fn admission_control(
    handle: &Handle,
    account: &Account,
    expected: &moruna_storage_format::Scope,
) -> Result<(Memory, u32), String> {
    let bytes = canonical_control(handle, false)?;
    let admission = moruna_storage_format::auth::Admission::decode(&bytes)
        .map_err(|_| String::from("invalid canonical admission"))?;
    if &admission.scope != expected {
        return Err(String::from(
            "private admission disagrees with engine.bindings scope",
        ));
    }
    let length = bytes.len();
    let pages = (length as u64).div_ceil(PAGE) * PAGE;
    let memory = Memory::create_in(pages, &account.0)
        .map_err(|e| format!("admission charged control: {}", e.name()))?;
    memory
        .map(0, pages, map::READ | map::WRITE)
        .and_then(|m| m.write(0, &bytes))
        .map_err(|e| format!("admission control write: {}", e.name()))?;
    let readonly = memory
        .0
        .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
        .map_err(|e| format!("admission control loan: {}", e.name()))?;
    Ok((Memory(readonly), length as u32))
}

/// The data disks the device manager has handed over: each device's handle names and handles,
/// kept for every run.
struct Disks {
    listener: Option<Channel>,
    control: Option<control::Client>,
    handed: BTreeMap<String, (String, Vec<Handle>)>,
}

impl Disks {
    fn new() -> Disks {
        Disks {
            listener: start::take("handover").map(Channel),
            control: None,
            handed: BTreeMap::new(),
        }
    }

    /// A channel whose one message carries duplicates of data disk `device`'s handles, named;
    /// the disk is handed over the first time a run asks for it.
    fn grant(&mut self, slot: &str, device: &str) -> Result<Handle, String> {
        if !self.handed.contains_key(device) {
            if self.control.is_none() {
                let listener = self.listener.take().ok_or_else(|| {
                    format!("`grant {slot} disk {device}` needs the `handover` slot")
                })?;
                self.control = Some(
                    devices_protocol::open_control(listener, deadline_in(120 * SECONDS))
                        .map_err(|e| format!("the device manager's control: {e:?}"))?,
                );
            }
            let control = self.control.as_mut().ok_or("no control connection")?;
            let handed = control
                .hand_over(String::from(device))
                .map_err(|e| format!("the device manager refused to hand {device} over: {e:?}"))?;
            self.handed.insert(String::from(device), handed);
        }
        let (names, handles) = self
            .handed
            .get(device)
            .ok_or_else(|| format!("no data disk {device}"))?;
        let mut copies = Vec::with_capacity(handles.len());
        for h in handles {
            copies.push(Some(
                h.duplicate(Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
                    .map_err(|e| format!("duplicating {device}'s handles: {}", e.name()))?,
            ));
        }
        let (ours, theirs) =
            Channel::create().map_err(|e| format!("channel_create: {}", e.name()))?;
        ours.write_with(names.as_bytes(), &mut copies)
            .map_err(|e| format!("granting {device}: {}", e.name()))?;
        Ok(theirs.0)
    }
}

/// The handles one run starts with, made fresh for it.
#[allow(clippy::too_many_arguments)]
fn grants(
    config: &Config,
    personality: &Channel,
    directory: &mut Option<directory::Client>,
    disks: &mut Disks,
    data: &BTreeMap<String, Handle>,
    services: &BTreeMap<String, Channel>,
    admissions: &mut BTreeMap<String, moruna_storage_service::protocol::admission::Client>,
    account: &Account,
    processes: &BTreeMap<String, Handle>,
) -> Result<Vec<(Bytes, Handle)>, String> {
    let bindings = if config
        .grants
        .iter()
        .any(|g| matches!(g.source, Source::Admission { .. }))
    {
        let source = config
            .grants
            .iter()
            .find_map(|grant| match &grant.source {
                Source::Data(from) if grant.slot == "engine.bindings" => Some(from),
                _ => None,
            })
            .ok_or_else(|| {
                String::from("private storage admission requires explicit engine.bindings data")
            })?;
        let bytes = canonical_control(
            data.get(source)
                .ok_or_else(|| String::from("missing engine.bindings data"))?,
            true,
        )?;
        Some(
            moruna_storage_format::bindings::EngineBindings::decode_page(&bytes)
                .map_err(|_| String::from("invalid canonical engine.bindings envelope"))?,
        )
    } else {
        None
    };
    let mut out = Vec::new();
    for g in &config.grants {
        let handle = match &g.source {
            Source::Personality => personality
                .0
                .duplicate(Rights::WRITE | Rights::TRANSFER)
                .map_err(|e| format!("duplicating the personality's listener: {}", e.name()))?,
            Source::Lookup(service) => {
                let dir = directory.as_mut().ok_or_else(|| {
                    format!(
                        "`grant {} lookup {service}` needs the `directory` slot",
                        g.slot
                    )
                })?;
                match dir.lookup(String::from(service.as_str())) {
                    Ok(channel) => channel.0,
                    Err(CallError::Remote(directory::Error::NotFound)) => {
                        return Err(format!("the device manager has no {service}"));
                    }
                    Err(e) => return Err(format!("looking up {service}: {e:?}")),
                }
            }
            Source::Service(from) => {
                let listener = services
                    .get(from)
                    .ok_or_else(|| format!("no service listener `{from}`"))?;
                moruna_svc::connect(listener)
                    .map_err(|error| {
                        format!(
                            "connecting explicit service `{from}` for slot `{}`: {error:?}",
                            g.slot
                        )
                    })?
                    .0
            }
            Source::Admission { authority, control } => {
                let source = data
                    .get(control)
                    .ok_or_else(|| format!("no admission data `{control}`"))?;
                let expected = &bindings
                    .as_ref()
                    .ok_or_else(|| String::from("missing binding identity"))?
                    .admission
                    .scope;
                let (memory, bytes) = admission_control(source, account, expected)?;
                let child = account
                    .0
                    .duplicate(Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
                    .map_err(|e| format!("admission child account: {}", e.name()))?;
                admissions
                    .get_mut(authority)
                    .ok_or_else(|| format!("no admission authority `{authority}`"))?
                    .bind(memory, bytes, child)
                    .map_err(|e| format!("admission refused for `{}`: {e:?}", g.slot))?
                    .0
            }
            Source::Data(from) => data
                .get(from)
                .ok_or_else(|| format!("no data handle `{from}`"))?
                .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                .map_err(|e| format!("duplicating the data handle `{from}`: {}", e.name()))?,
            Source::Disk(device) => disks.grant(&g.slot, device)?,
            // stream P: the engine makes processes from it, nothing more.
            // Handed on beside the grants, by `function_grants`.
            Source::Functions(_) => continue,
            Source::Process(from) => processes
                .get(from)
                .ok_or_else(|| format!("no process resource `{from}`"))?
                .duplicate(Rights::WRITE | Rights::TRANSFER)
                .map_err(|e| format!("duplicating the process resource `{from}`: {}", e.name()))?,
            // The configuration runs once (`Config::parse`): the channel moves to the program
            // as it is, taken from the start page here.
            Source::Channel(from) => {
                let channel = start::take(from).ok_or_else(|| {
                    format!(
                        "`grant {} channel {from}` names a channel the supervisor was not given",
                        g.slot
                    )
                })?;
                match channel.info() {
                    Ok((moruna_svc::ObjectType::Channel, _)) => channel,
                    _ => return Err(format!("`{from}` is not a channel")),
                }
            }
        };
        out.push((b(&g.slot), handle));
    }
    Ok(out)
}

fn ended_text(e: &Ended) -> String {
    let how_ = match e.how {
        how::EXITED => format!("exited with code {}", e.code),
        how::SIGNALED => format!("was ended by signal {}", e.code),
        _ => String::from("ended without exiting (the kernel's line says why)"),
    };
    format!(
        "{how_}; {} calls served; refused: {}",
        e.served,
        moruna_linux::refused::Refused::render(&e.refused)
    )
}

/// A reply is not permission to reset an account while original objects/loans
/// remain. Sequential runs wait for actual accounting release, within a bound.
fn drain_accounts(runtime: &Account, scratch: Option<&Account>) -> Result<(), String> {
    let timer = Timer::create().map_err(|e| format!("account drain timer: {}", e.name()))?;
    let deadline = now().saturating_add(5 * SECONDS);
    loop {
        let run = runtime
            .usage()
            .map_err(|e| format!("runtime usage: {}", e.name()))?;
        let (used, reserved) = match scratch {
            Some(scratch) => {
                let temporary = scratch
                    .usage()
                    .map_err(|e| format!("scratch usage: {}", e.name()))?;
                (temporary.used, temporary.reserved)
            }
            None => (0, 0),
        };
        if run.used == 0 && run.reserved == 0 && used == 0 && reserved == 0 {
            return Ok(());
        }
        if now() >= deadline {
            return Err(format!(
                "original accounts not drained: runtime used={} reserved={}, scratch used={} reserved={}",
                run.used, run.reserved, used, reserved
            ));
        }
        timer
            .set(now().saturating_add(1_000_000).min(deadline))
            .map_err(|e| format!("account drain arm: {}", e.name()))?;
        wait_one(&timer.0, Signals::SIGNALED, deadline)
            .map_err(|e| format!("account drain wait: {}", e.name()))?;
    }
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let config = match read_config("config").and_then(|t| Config::parse(&t)) {
        Ok(c) => c,
        Err(why) => {
            println!(out, "KD2 engine: FAIL: the configuration: {}", why);
            return 1;
        }
    };
    let Some(personality) = start::take("linux").map(Channel) else {
        println!(out, "KD2 engine: FAIL: no `linux` slot");
        return 1;
    };
    let mut directory = match start::take("directory").map(Channel) {
        None => None,
        Some(listener) => match moruna_svc::connect(&listener)
            .map_err(|e| format!("{e:?}"))
            .and_then(|c| {
                directory::Client::connect(c, deadline_in(120 * SECONDS))
                    .map_err(|e| format!("{e:?}"))
            }) {
            Ok(c) => Some(c),
            Err(why) => {
                println!(out, "KD2 engine: FAIL: the device directory: {}", why);
                return 1;
            }
        },
    };
    let mut client = match moruna_svc::connect(&personality)
        .map_err(|e| format!("{e:?}"))
        .and_then(|c| {
            linux::Client::connect(c, deadline_in(300 * SECONDS)).map_err(|e| format!("{e:?}"))
        }) {
        Ok(c) => c,
        Err(why) => {
            println!(out, "KD2 engine: FAIL: connecting to moruna.linux: {}", why);
            return 1;
        }
    };
    client.core().set_timeout(RUN_SECONDS * SECONDS);
    let data = match data_slots(&config) {
        Ok(d) => d,
        Err(why) => {
            println!(out, "KD2 engine: FAIL: {}", why);
            return 1;
        }
    };
    let services = match service_slots(&config) {
        Ok(services) => services,
        Err(why) => {
            println!(out, "KD2 engine: FAIL: {}", why);
            return 1;
        }
    };
    let mut admissions = match admission_slots(&config) {
        Ok(clients) => clients,
        Err(why) => {
            println!(out, "KD2 engine: FAIL: {}", why);
            return 1;
        }
    };
    // stream P
    let processes = match process_slots(&config) {
        Ok(p) => p,
        Err(why) => {
            println!(out, "KD2 engine: FAIL: {}", why);
            return 1;
        }
    };
    let functions = match function_slots(&config) {
        Ok(f) => f,
        Err(why) => {
            println!(out, "KD2 engine: FAIL: {}", why);
            return 1;
        }
    };
    let mut disks = Disks::new();
    let mut argv = Vec::with_capacity(config.args.len() + 1);
    argv.push(b(&config.program));
    argv.extend(config.args.iter().map(|a| b(a)));
    let env: Vec<Bytes> = config.env.iter().map(|e| b(e)).collect();
    let mut exited = 0;
    for n in 1..=config.runs {
        // Both accounts are carved from this native engine process's own account.
        // No node/admission/supervisor account is substituted into the runtime.
        let runtime = match Account::carve(config.memory, "runtime") {
            Ok(account) => account,
            Err(e) => {
                println!(out, "KD2 engine: FAIL: runtime account: {}", e.name());
                return 1;
            }
        };
        // `scratch 0`: no scratch account (a node's check).
        let scratch = match config.scratch {
            0 => None,
            bytes => match Account::carve(bytes, "scratch") {
                Ok(account) => Some(account),
                Err(e) => {
                    println!(out, "KD2 engine: FAIL: scratch account: {}", e.name());
                    return 1;
                }
            },
        };
        let mut handles = match grants(
            &config,
            &personality,
            &mut directory,
            &mut disks,
            &data,
            &services,
            &mut admissions,
            &runtime,
            &processes,
        ) {
            Ok(h) => h,
            Err(why) => {
                println!(
                    out,
                    "KD2 engine: FAIL: run {} of {}: {}", n, config.runs, why
                );
                return 1;
            }
        };
        match function_grants(&functions) {
            Ok(granted) => handles.extend(granted),
            Err(why) => {
                println!(
                    out,
                    "KD2 engine: FAIL: run {} of {}: {}", n, config.runs, why
                );
                return 1;
            }
        }
        let runtime_start = match runtime
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
        {
            Ok(handle) => handle,
            Err(e) => {
                println!(
                    out,
                    "KD2 engine: FAIL: runtime account start grant: {}",
                    e.name()
                );
                return 1;
            }
        };
        handles.push((b("account"), runtime_start));
        let runtime_charge = match runtime
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER)
        {
            Ok(handle) => handle,
            Err(e) => {
                println!(
                    out,
                    "KD2 engine: FAIL: runtime charging grant: {}",
                    e.name()
                );
                return 1;
            }
        };
        let scratch_charge = match scratch
            .as_ref()
            .map(|s| {
                s.0.duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER)
            })
            .transpose()
        {
            Ok(handle) => handle,
            Err(e) => {
                println!(
                    out,
                    "KD2 engine: FAIL: scratch charging grant: {}",
                    e.name()
                );
                return 1;
            }
        };
        let names: Vec<String> = handles
            .iter()
            .map(|(s, _)| String::from_utf8_lossy(&s.0).into_owned())
            .collect();
        println!(
            out,
            "KD2 engine: run {} of {}: starting {} with {:?}, {} MiB, scratch {} MiB",
            n,
            config.runs,
            config.program,
            names,
            config.memory >> 20,
            config.scratch >> 20
        );
        let grant = Grant {
            handles,
            account: runtime_charge,
            scratch: scratch_charge,
        };
        match client.run_granted(b(&config.program), argv.clone(), env.clone(), grant) {
            Ok(ended) => {
                println!(
                    out,
                    "KD2 engine: run {} of {}: the engine {}",
                    n,
                    config.runs,
                    ended_text(&ended)
                );
                if ended.how == how::EXITED {
                    exited += 1;
                }
            }
            Err(e) => {
                println!(
                    out,
                    "KD2 engine: FAIL: run {} of {}: run_granted: {:?}", n, config.runs, e
                );
                return 1;
            }
        }
        if let Err(e) = drain_accounts(&runtime, scratch.as_ref()) {
            println!(
                out,
                "KD2 engine: FAIL: run {} original account retirement: {}", n, e
            );
            return 1;
        }
    }
    drop(directory);
    if exited == config.runs {
        println!(
            out,
            "KD2 engine: ok: {} runs, each ended by its own exit", config.runs
        );
        0
    } else {
        println!(
            out,
            "KD2 engine: FAIL: {} of {} runs ended without exiting",
            config.runs - exited,
            config.runs
        );
        1
    }
}
