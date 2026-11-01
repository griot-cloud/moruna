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
//! and each listener a `grant SLOT service FROM` explicitly names as `FROM`.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};

use devices_protocol::directory;
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

/// The handles one run starts with, made fresh for it.
fn grants(
    config: &Config,
    personality: &Channel,
    directory: &mut Option<directory::Client>,
    data: &BTreeMap<String, Handle>,
    services: &BTreeMap<String, Channel>,
    admissions: &mut BTreeMap<String, moruna_storage_service::protocol::admission::Client>,
    account: &Account,
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
fn drain_accounts(runtime: &Account, scratch: &Account) -> Result<(), String> {
    let timer = Timer::create().map_err(|e| format!("account drain timer: {}", e.name()))?;
    let deadline = now().saturating_add(5 * SECONDS);
    loop {
        let run = runtime
            .usage()
            .map_err(|e| format!("runtime usage: {}", e.name()))?;
        let temporary = scratch
            .usage()
            .map_err(|e| format!("scratch usage: {}", e.name()))?;
        if run.used == 0 && run.reserved == 0 && temporary.used == 0 && temporary.reserved == 0 {
            return Ok(());
        }
        if now() >= deadline {
            return Err(format!(
                "original accounts not drained: runtime used={} reserved={}, scratch used={} reserved={}",
                run.used, run.reserved, temporary.used, temporary.reserved
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
        let scratch = match Account::carve(config.scratch, "scratch") {
            Ok(account) => account,
            Err(e) => {
                println!(out, "KD2 engine: FAIL: scratch account: {}", e.name());
                return 1;
            }
        };
        let mut handles = match grants(
            &config,
            &personality,
            &mut directory,
            &data,
            &services,
            &mut admissions,
            &runtime,
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
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER)
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
            scratch: Some(scratch_charge),
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
        if let Err(e) = drain_accounts(&runtime, &scratch) {
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
