//! Owns service process handles and their carved accounts; no spare channel ends.
//!
//! A node that fails says why and stops (`architecture/moruna-os-system.md`, section 1B):
//! - the supervisor that cannot go on writes `moruna: fatal services: <reason>`;
//! - a service not started within 5 s of the supervisor's own start is
//!   `moruna: fatal services: <name> did not start within 5 s`;
//! - a service that ends is `moruna: fatal <name>: ended with code <n>`, or `faulted`, or
//!   `was killed`, as the kernel says it ended, whether the supervisor sees its process end
//!   or the channel it watches it by (a bootstrap's, or admission's guardian) close first;
//!   a service that closes that channel and runs on is
//!   `moruna: fatal <name>: closed its channel to the supervisor`.
//!
//! After any of them it kills every child and exits with failure, and the core, with no
//! process left, powers the machine off.
//!
//! One service may be started again when it dies (node.conf's `restart`, system design 2B, item
//! 2.2c): at each of its starts, the first included, the supervisor mints its listener from the
//! network server through the granting slot it keeps the end of, and makes it a fresh channel to
//! `platform`, whose other end it sends `platform` as `api-started` on `platform`'s bootstrap
//! channel, which it keeps for that. Its sixth death in 10 minutes is
//! `moruna: fatal <name>: ended 6 times in 10 minutes`; every other death stops the node.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use moruna_node::{
    Bootstrap, Config, Gone, Lifecycle, Restart, StartError, line,
    restart::{self, Deaths, Verdict},
};
use moruna_sys::{
    Account, Channel, Console, Handle, Memory, Process, Rights, Signals, Usage, WaitItem, map, now,
    println, start, wait,
};
use netstack_protocol::net;

moruna_sys::main!(run);

/// How long the network server has to answer the supervisor's client: within 1B's 5 s for the
/// services to start.
const CLIENT_DEADLINE_NS: u64 = 5_000_000_000;
/// The account of the supervisor's own client of the network server, which opens no socket.
const CLIENT_ACCOUNT_BYTES: u64 = 64 * 1024;

struct Child {
    name: String,
    process: Process,
    account: Account,
    entry: u64,
    handles: Vec<(Option<Handle>, String)>,
}

/// The service that is started again when it dies, and what each of its starts needs.
struct Restarting {
    index: usize,
    /// The network server's service.
    server: usize,
    record: Restart,
    /// Its program, kept for each new start.
    image: Handle,
    /// The supervisor's end of the network server's granting slot.
    granting: Channel,
    /// The supervisor's client end of the listener node.conf links it to
    /// (`link SERVER SLOT supervisor CONNECT`), which each start is given a fresh connection
    /// through on the record's `CONNECT` slot (item 3.2).
    connector: Option<Channel>,
    /// The network server's client the listeners are minted with, once the first start has
    /// connected it.
    client: Option<net::Client>,
    deaths: Deaths,
}

/// What the supervisor keeps from its start for the service that restarts.
struct Kept {
    process_root: Handle,
    /// `platform`'s bootstrap channel, kept after the node's memory: each start of the service
    /// that restarts is sent on it.
    platform: Option<Channel>,
    restarting: Option<Restarting>,
}

fn take(name: &str) -> Result<Handle, String> {
    start::take(name).ok_or_else(|| format!("missing boot capability {name}"))
}
/// The services, their lifecycle, the channels the supervisor watches them by with the
/// service that holds each, admission's control channel, and what it keeps for the service
/// that restarts.
type Prepared = (
    Vec<Child>,
    Lifecycle,
    Vec<Channel>,
    Vec<usize>,
    Option<Channel>,
    Kept,
);

/// A start's console and account, as every service gets them.
fn own_handles(
    console: &Console,
    account: &Account,
) -> Result<Vec<(Option<Handle>, String)>, String> {
    // A configured service owns its output capability: it may retain a
    // narrowed console for shared state or delegate output to a real child.
    // Descendant grants remain narrowed by that owner at the actual handoff.
    let console_rights = Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER;
    Ok(alloc::vec![
        (
            Some(
                console
                    .0
                    .duplicate(console_rights)
                    .map_err(|e| format!("console: {}", e.name()))?
            ),
            "console".into()
        ),
        (
            Some(
                account
                    .0
                    .duplicate(
                        account
                            .0
                            .info()
                            .map_err(|e| format!("account rights: {}", e.name()))?
                            .1
                    )
                    .map_err(|e| format!("account: {}", e.name()))?
            ),
            "account".into()
        ),
    ])
}

fn prepare(console: &Console) -> Result<Prepared, String> {
    let config = Memory(take("config")?);
    // Image data objects are zero padded. Never accept an unterminated oversized
    // config or silently truncate it to the first mapped page.
    let page = config
        .map(0, moruna_node::MAX_CONFIG as u64, map::READ)
        .map_err(|e| format!("config mapping: {}", e.name()))?;
    let mut bytes = alloc::vec![0; moruna_node::MAX_CONFIG];
    page.read(0, &mut bytes)
        .map_err(|e| format!("config read: {}", e.name()))?;
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .ok_or("unterminated config")?;
    let usage = Usage::own().map_err(|e| format!("node usage: {}", e.name()))?;
    // The core makes `root.hyperv` only on Hyper-V (ABI 2.7): elsewhere a service granted
    // it, `vmbus`, is not started (system design 1F).
    let hyperv = start::peek(moruna_node::HYPERV).is_some();
    let config = Config::parse(&bytes[..end], usage.room())
        .map_err(|e| format!("config: {e:?}"))?
        .on_machine(hyperv);
    let process_root = take("root.process")?;
    let mut guardians = Vec::new();
    let mut control = None;
    let mut platform = None;
    let mut children = Vec::new();
    // Reuse only the explicitly granted immutable ELF; each instance gets a fresh
    // actual Process, Account and separately configured service channel graph.
    let mut images = BTreeMap::new();
    for service in &config.services {
        let account = Account::carve(service.account_bytes, &service.name)
            .map_err(|e| format!("{} account: {}", service.name, e.name()))?;
        let process = Process::create_in(&process_root, &service.name, &account.0)
            .map_err(|e| format!("{} process: {}", service.name, e.name()))?;
        if let alloc::collections::btree_map::Entry::Vacant(entry) =
            images.entry(service.image.clone())
        {
            let image = take(&service.image)?;
            let (kind, rights) = image
                .info()
                .map_err(|e| format!("image type: {}", e.name()))?;
            if kind != moruna_sys::ObjectType::Memory
                || !rights.contains(Rights::READ)
                || rights.contains(Rights::WRITE)
            {
                return Err(format!("{} must be readonly ELF memory", service.image));
            }
            entry.insert(image);
        }
        let image = images.get(&service.image).ok_or("missing retained image")?;
        let entry = process
            .map_image(image)
            .map_err(|e| format!("{} ELF: {}", service.name, e.name()))?;
        let handles = own_handles(console, &account)?;
        children.push(Child {
            name: service.name.clone(),
            process,
            account,
            entry,
            handles,
        });
    }
    for grant in &config.grants {
        let index = config
            .services
            .iter()
            .position(|s| s.name == grant.service)
            .ok_or("unknown grant service")?;
        let handle = if grant.source == "root.process" {
            process_root
                .duplicate(Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
                .map_err(|e| format!("process grant: {}", e.name()))?
        } else {
            take(&grant.source)?
        };
        children[index]
            .handles
            .push((Some(handle), grant.slot.clone()));
    }
    let mut connector = None;
    for link in &config.links {
        let left = config
            .services
            .iter()
            .position(|s| s.name == link.left)
            .ok_or("unknown link service")?;
        let (a, b) = Channel::create().map_err(|e| format!("link: {}", e.name()))?;
        children[left]
            .handles
            .push((Some(a.0), link.left_slot.clone()));
        // The supervisor's own link (item 3.2): it keeps the client's end.
        if link.right == restart::SUPERVISOR {
            connector = Some(b);
            continue;
        }
        let right = config
            .services
            .iter()
            .position(|s| s.name == link.right)
            .ok_or("unknown link service")?;
        children[right]
            .handles
            .push((Some(b.0), link.right_slot.clone()));
    }
    for (kind, service) in &config.bootstraps {
        let index = config
            .services
            .iter()
            .position(|s| s.name == *service)
            .ok_or("unknown bootstrap service")?;
        let (sender, receiver) =
            Channel::create().map_err(|e| format!("bootstrap channel: {}", e.name()))?;
        let root = || -> Result<Handle, String> {
            process_root
                .duplicate(
                    process_root
                        .info()
                        .map_err(|e| format!("process root rights: {}", e.name()))?
                        .1,
                )
                .map_err(|e| format!("process root: {}", e.name()))
        };
        let roots = match kind {
            // No root for `platform`: its message is the node's memory, the machine
            // account's limit that every service and job is carved from (item 1.4).
            Bootstrap::Platform => Vec::new(),
            Bootstrap::Linux => alloc::vec![root()?],
            Bootstrap::Devices { .. } => alloc::vec![
                root()?,
                take("root.device")?,
                take("root.ecam")?,
                take("root.msi")?,
                take("root.dma")?
            ],
        };
        let message = match kind {
            Bootstrap::Platform => Bootstrap::platform_message(usage.limit).to_vec(),
            _ => kind.header().to_vec(),
        };
        // The bounded bootstrap kinds carry different handle counts. This
        // SDK seam consumes every slot only on success; refusal retains owners.
        let mut roots: Vec<Option<Handle>> = roots.into_iter().map(Some).collect();
        sender
            .write_with(&message, &mut roots)
            .map_err(|e| format!("bootstrap transfer: {}", e.name()))?;
        children[index]
            .handles
            .push((Some(receiver.0), "supervisor".into()));
        // `platform` makes no process, so its channel is no guardian. Its end is kept: each
        // start of the service that restarts is sent on it, after the memory (item 2.2c).
        if *kind == Bootstrap::Platform {
            platform = Some(sender);
        } else {
            guardians.push(sender);
        }
    }
    if let Some(service) = &config.admission {
        let index = config
            .services
            .iter()
            .position(|s| s.name == *service)
            .ok_or("unknown admission service")?;
        let (parent, child) =
            Channel::create().map_err(|e| format!("guardian channel: {}", e.name()))?;
        children[index]
            .handles
            .push((Some(child.0), "guardian".into()));
        guardians.push(parent);
        let (parent, child) =
            Channel::create().map_err(|e| format!("control channel: {}", e.name()))?;
        children[index]
            .handles
            .push((Some(child.0), "control".into()));
        control = Some(parent);
        // The node's memory, as the platform bootstrap carries it (item 1.4): the machine
        // account's limit, which a job's budget is measured against (system design 3A, item
        // 3.2). This end closes here; the message stays queued for admission.
        let (sender, receiver) =
            Channel::create().map_err(|e| format!("memory channel: {}", e.name()))?;
        sender
            .write_with(&Bootstrap::platform_message(usage.limit), &mut [])
            .map_err(|e| format!("memory transfer: {}", e.name()))?;
        children[index]
            .handles
            .push((Some(receiver.0), "supervisor".into()));
    }
    // The service at each guardian's place, so that a closed one names its service.
    let owners = config
        .guardians()
        .iter()
        .map(|name| config.services.iter().position(|s| s.name == *name))
        .collect::<Option<Vec<usize>>>()
        .ok_or("unknown guardian service")?;
    if owners.len() != guardians.len() {
        return Err("a guardian channel has no service".into());
    }
    let mut restarting = None;
    if let Some(record) = &config.restart {
        let position = |name: &str| {
            config
                .services
                .iter()
                .position(|s| s.name == name)
                .ok_or("unknown restart service")
        };
        let index = position(&record.service)?;
        let server = position(&record.server)?;
        // The network server's granting slot: its end to the server, ours kept.
        let (ours, theirs) =
            Channel::create().map_err(|e| format!("granting channel: {}", e.name()))?;
        children[server]
            .handles
            .push((Some(theirs.0), record.granting.clone()));
        let image = images
            .remove(&config.services[index].image)
            .ok_or("missing retained image")?;
        restarting = Some(Restarting {
            index,
            server,
            record: record.clone(),
            image,
            granting: ours,
            connector: connector.take(),
            client: None,
            deaths: Deaths::new(),
        });
    }
    let lifecycle = Lifecycle::new(children.len());
    let kept = Kept {
        process_root,
        platform,
        restarting,
    };
    Ok((children, lifecycle, guardians, owners, control, kept))
}

/// What a start of the service that restarts gets besides its console and account: its
/// listener, minted from the network server; when `platform` is bootstrapped, a fresh channel to
/// `platform`, whose other end `platform` is sent as `api-started`; and when the record names a
/// `CONNECT` slot, a fresh connection to the server the supervisor is linked to (item 3.2), which
/// ends with this start.
fn fresh(
    restarting: &mut Restarting,
    platform: Option<&Channel>,
) -> Result<Vec<(Option<Handle>, String)>, String> {
    if restarting.client.is_none() {
        let deadline = now().saturating_add(CLIENT_DEADLINE_NS);
        let channel = moruna_svc::connect(&restarting.granting)
            .map_err(|e| format!("the network server's granting slot: {e:?}"))?;
        let account = Account::carve(CLIENT_ACCOUNT_BYTES, "supervisor-network")
            .map_err(|e| format!("the supervisor's network account: {}", e.name()))?;
        let mut client = net::Client::connect(channel, deadline, account)
            .map_err(|e| format!("the network server did not welcome the supervisor: {e:?}"))?;
        client.core().set_timeout(CLIENT_DEADLINE_NS);
        restarting.client = Some(client);
    }
    let client = restarting
        .client
        .as_mut()
        .ok_or("the network server's client is gone")?;
    let listener = client
        .grant(restarting.record.policy.clone())
        .map_err(|e| format!("its listener was not granted: {e:?}"))?;
    let mut handles = alloc::vec![(Some(listener.0), restarting.record.slot.clone())];
    if let Some(platform) = platform {
        let (ours, theirs) =
            Channel::create().map_err(|e| format!("platform channel: {}", e.name()))?;
        platform
            .write_with(restart::API_STARTED, &mut [Some(theirs.0)])
            .map_err(|e| format!("api-started to platform: {}", e.name()))?;
        handles.push((Some(ours.0), restart::PLATFORM_SLOT.into()));
    }
    if let (Some(slot), Some(connector)) = (&restarting.record.connect, &restarting.connector) {
        let channel = moruna_svc::connect(connector)
            .map_err(|e| format!("a connection on its {slot} slot: {e:?}"))?;
        handles.push((Some(channel.0), slot.clone()));
    }
    Ok(handles)
}

/// A new process for the service that restarts, in its account, with its program and a fresh
/// start's handles.
fn renew(child: &mut Child, kept: &mut Kept, console: &Console) -> Result<(), String> {
    let restarting = kept.restarting.as_mut().ok_or("no service restarts")?;
    let process = Process::create_in(&kept.process_root, &child.name, &child.account.0)
        .map_err(|e| format!("process: {}", e.name()))?;
    let entry = process
        .map_image(&restarting.image)
        .map_err(|e| format!("ELF: {}", e.name()))?;
    let mut handles = own_handles(console, &child.account)?;
    handles.extend(fresh(restarting, kept.platform.as_ref())?);
    child.process = process;
    child.entry = entry;
    child.handles = handles;
    Ok(())
}

/// `node: account <name>: <used> bytes held, peak <peak>, limit <limit> (<when>)`: what the
/// accounts of the service that restarts and of the network server hold at each of its starts,
/// before the start, and at each of its deaths, so that a restart is seen to leave nothing
/// behind in either (item 2.2c).
fn account_lines(console: &Console, children: &[Child], restarting: &Restarting, when: &str) {
    for child in [&children[restarting.index], &children[restarting.server]] {
        match child.account.usage() {
            Ok(usage) => println!(
                console,
                "node: account {}: {} bytes held, peak {}, limit {} ({})",
                child.name,
                usage.used,
                usage.peak,
                usage.limit,
                when
            ),
            Err(e) => println!(
                console,
                "node: account {}: {} ({})",
                child.name,
                e.name(),
                when
            ),
        }
    }
}

/// Starts `child` with its handles.
fn launch(child: &mut Child) -> Result<(), String> {
    let mut handles: Vec<_> = child
        .handles
        .iter_mut()
        .map(|(h, name)| (h.take(), name.as_str()))
        .collect();
    child
        .process
        .start_with(child.entry, &mut handles)
        .map_err(|e| e.name().into())
}

/// Why a service's start failed.
enum Unstarted {
    /// What a start of the service that restarts needs could not be made.
    Fresh(String),
    /// The kernel refused the start.
    Refused(String),
}

type Descendants = Vec<(usize, u64, Process)>;
/// Why the services' children could not be collected.
enum Uncollected {
    /// The guardian channel at this place closed: the service that held it has gone.
    Closed(usize),
    Failed(String),
}
impl From<String> for Uncollected {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}
impl From<&str> for Uncollected {
    fn from(reason: &str) -> Self {
        Self::Failed(reason.into())
    }
}
impl core::fmt::Display for Uncollected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Closed(at) => write!(f, "guardian {at} closed"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}
fn collect(
    guardians: &[Channel],
    children: &mut Descendants,
    closing: bool,
) -> Result<(), Uncollected> {
    for (index, guardian) in guardians.iter().enumerate() {
        loop {
            let mut bytes = [0; 16];
            let mut handles = [None];
            let (count, carried) = match guardian.read(&mut bytes, &mut handles) {
                Ok(message) => message,
                Err(moruna_sys::Error::ShouldWait) => break,
                Err(moruna_sys::Error::PeerClosed) if closing => break,
                Err(moruna_sys::Error::PeerClosed) => return Err(Uncollected::Closed(index)),
                Err(e) => return Err(format!("guardian read: {}", e.name()).into()),
            };
            let record = moruna_node::ChildRecord::decode(&bytes[..count], carried)
                .ok_or("invalid child escrow record")?;
            if record.retain {
                if children
                    .iter()
                    .any(|(g, id, _)| *g == index && *id == record.id)
                    || children.iter().filter(|(g, _, _)| *g == index).count() >= 64
                {
                    return Err("duplicate or excess child escrow".into());
                }
                let handle = handles[0].take().ok_or("missing escrow process handle")?;
                let (kind, rights) = handle
                    .info()
                    .map_err(|e| format!("child handle: {}", e.name()))?;
                if kind != moruna_sys::ObjectType::Process
                    || !rights.contains(Rights::WRITE | Rights::WAIT)
                {
                    return Err("invalid child escrow authority".into());
                }
                children.push((index, record.id, Process(handle)));
            } else {
                children.retain(|(g, id, _)| *g != index || *id != record.id);
            }
        }
    }
    children.retain(|(_, _, child)| !matches!(moruna_sys::wait_one(&child.0, Signals::TERMINATED, now()), Ok(seen) if seen.contains(Signals::TERMINATED)));
    Ok(())
}
/// How `process`, which has ended, ended: its exit code, a fault or a kill (ABI 2.7).
/// The kernel answers for every process the supervisor holds; were it not to, the line
/// would still name the service, as ending with code 0.
fn end(process: &Process) -> moruna_sys::process_end::End {
    process
        .end()
        .unwrap_or(moruna_sys::process_end::End::Code(0))
}

/// Whether `process` has ended, waiting up to a second: a service that ends closes its
/// channels a moment before its process is seen to have ended, so its closed guardian
/// channel can reach the supervisor first.
fn ended(process: &Process) -> bool {
    matches!(
        moruna_sys::wait_one(&process.0, Signals::TERMINATED, now().saturating_add(1_000_000_000)),
        Ok(seen) if seen.contains(Signals::TERMINATED)
    )
}
fn shutdown(
    children: &[Child],
    lifecycle: &mut Lifecycle,
    guardians: &[Channel],
    descendants: &mut Descendants,
    control: &mut Option<Channel>,
) -> Result<(), String> {
    lifecycle.stop();
    let graceful_deadline = now().saturating_add(5_000_000_000);
    if let Some(channel) = control.take()
        && let Ok(mut client) = node_protocol::control::Client::connect(channel, graceful_deadline)
    {
        client
            .core()
            .set_timeout(graceful_deadline.saturating_sub(now()));
        let _ = client.shutdown();
    }
    // Stop creators and observe quiescence before declaring their escrow set final.
    // A bounded timeout is an explicit incomplete teardown, never a false success.
    let deadline = now().saturating_add(5_000_000_000);
    for (index, child) in children.iter().enumerate() {
        if lifecycle.state(index) != Some(moruna_node::State::Pending) {
            let _ = child.process.kill();
        }
    }
    let mut complete = true;
    for (index, child) in children.iter().enumerate() {
        if lifecycle.state(index) != Some(moruna_node::State::Pending)
            && !matches!(moruna_sys::wait_one(&child.process.0, Signals::TERMINATED, deadline), Ok(seen) if seen.contains(Signals::TERMINATED))
        {
            complete = false;
        }
    }
    let collect_error = collect(guardians, descendants, true)
        .err()
        .map(|e| format!("{e}"));
    if collect_error.is_some() {
        complete = false;
    }
    for (_, _, process) in descendants.iter() {
        match process.kill() {
            Ok(()) => {
                if !matches!(moruna_sys::wait_one(&process.0, Signals::TERMINATED, deadline), Ok(seen) if seen.contains(Signals::TERMINATED))
                {
                    complete = false;
                }
            }
            // After all creators terminated, an unstarted escrow can be safely
            // dropped: no creator remains that could race its process_start.
            Err(moruna_sys::Error::BadState) if complete => {}
            Err(_) => complete = false,
        }
    }
    if complete {
        Ok(())
    } else {
        Err(collect_error.unwrap_or_else(|| {
            "creator/descendant termination deadline expired; teardown incomplete".into()
        }))
    }
}
fn stop_node(
    console: &Console,
    children: &[Child],
    lifecycle: &mut Lifecycle,
    guardians: &[Channel],
    descendants: &mut Descendants,
    control: &mut Option<Channel>,
) -> i64 {
    if let Err(e) = shutdown(children, lifecycle, guardians, descendants, control) {
        println!(console, "node: shutdown incomplete: {}", e);
    }
    1
}

fn run() -> i64 {
    // The services limit counts from here, the supervisor's own start.
    let began = now();
    let console = match take("console") {
        Ok(h) => Console(h),
        Err(_) => return 1,
    };
    let (mut children, mut lifecycle, guardians, owners, mut control, mut kept) =
        match prepare(&console) {
            Ok(c) => c,
            Err(e) => {
                println!(console, "node: startup refused: {}", e);
                println!(console, "{}", line::fatal(&e));
                return 1;
            }
        };
    let mut descendants = Vec::new();
    let started = moruna_node::start_services(&mut lifecycle, began, now, |index| {
        // The service that restarts takes its first start's listener and channel to `platform`
        // as every later start does.
        if let Some(restarting) = kept.restarting.as_mut()
            && restarting.index == index
        {
            let handles = fresh(restarting, kept.platform.as_ref()).map_err(Unstarted::Fresh)?;
            children[index].handles.extend(handles);
            let when = format!("{} starting", restarting.record.service);
            account_lines(&console, &children, restarting, &when);
        }
        launch(&mut children[index]).map_err(Unstarted::Refused)?;
        println!(console, "node: service {} started", index);
        Ok(())
    });
    if let Err(failure) = started {
        match failure {
            StartError::Refused {
                index,
                error: Unstarted::Fresh(e),
            } => {
                println!(
                    console,
                    "{}",
                    restart::fatal_line(&children[index].name, &e)
                );
            }
            StartError::Refused {
                index,
                error: Unstarted::Refused(e),
            } => {
                println!(console, "node: service {} start failed: {}", index, e);
                println!(
                    console,
                    "{}",
                    line::fatal(format_args!(
                        "{} could not start: {}",
                        children[index].name, e
                    ))
                );
            }
            StartError::Late { index } => {
                println!(console, "{}", line::Late(&children[index].name));
            }
        }
        return stop_node(
            &console,
            &children,
            &mut lifecycle,
            &guardians,
            &mut descendants,
            &mut control,
        );
    }
    // System design 1B: the boot line once every service in node.conf has started.
    println!(console, "moruna: services started");
    loop {
        if let Err(e) = collect(&guardians, &mut descendants, false) {
            match e {
                // A closed guardian channel is its service gone, seen before its process's
                // end or instead of it: the line names the service (1B). The service that
                // restarts holds none.
                Uncollected::Closed(at) => {
                    let index = owners[at];
                    let process = &children[index].process;
                    let gone = Gone::closed_channel(ended(process).then(|| end(process)));
                    lifecycle.terminated(index);
                    println!(
                        console,
                        "node: critical service {} terminated; stopping node", index
                    );
                    println!(console, "{}", line::Gone(&children[index].name, gone));
                }
                Uncollected::Failed(reason) => {
                    println!(console, "node: child supervision failed: {}", reason);
                    println!(
                        console,
                        "{}",
                        line::fatal(format_args!("child supervision failed: {reason}"))
                    );
                }
            }
            return stop_node(
                &console,
                &children,
                &mut lifecycle,
                &guardians,
                &mut descendants,
                &mut control,
            );
        }
        let ended: Vec<usize> = {
            let mut items: Vec<_> = children
                .iter()
                .map(|c| WaitItem::new(&c.process.0, Signals::TERMINATED))
                .collect();
            items.extend(
                guardians
                    .iter()
                    .map(|g| WaitItem::new(&g.0, Signals::READABLE | Signals::PEER_CLOSED)),
            );
            items.extend(
                descendants
                    .iter()
                    .map(|(_, _, p)| WaitItem::new(&p.0, Signals::TERMINATED)),
            );
            if let Err(e) = wait(&mut items, moruna_sys::DEADLINE_INFINITE) {
                println!(console, "node: service wait failed: {}", e.name());
                println!(
                    console,
                    "{}",
                    line::fatal(format_args!("service wait failed: {}", e.name()))
                );
                drop(items);
                return stop_node(
                    &console,
                    &children,
                    &mut lifecycle,
                    &guardians,
                    &mut descendants,
                    &mut control,
                );
            }
            items[..children.len()]
                .iter()
                .enumerate()
                .filter(|(_, item)| item.observed().contains(Signals::TERMINATED))
                .map(|(index, _)| index)
                .collect()
        };
        for index in ended {
            // Every death but the restarting service's stops the node.
            let Some(restarting) = kept
                .restarting
                .as_mut()
                .filter(|restarting| restarting.index == index)
            else {
                lifecycle.terminated(index);
                println!(
                    console,
                    "node: critical service {} terminated; stopping node", index
                );
                let gone = Gone::ended(end(&children[index].process));
                println!(console, "{}", line::Gone(&children[index].name, gone));
                return stop_node(
                    &console,
                    &children,
                    &mut lifecycle,
                    &guardians,
                    &mut descendants,
                    &mut control,
                );
            };
            // System design 2B: started again, at most 5 times in 10 minutes.
            let service = restarting.record.service.clone();
            account_lines(&console, &children, restarting, &format!("{service} ended"));
            let n = match restarting.deaths.died(now()) {
                Verdict::Restart(n) => n,
                Verdict::Fatal => {
                    println!(console, "{}", restart::ended_line(&service));
                    lifecycle.terminated(index);
                    return stop_node(
                        &console,
                        &children,
                        &mut lifecycle,
                        &guardians,
                        &mut descendants,
                        &mut control,
                    );
                }
            };
            lifecycle.restarting(index);
            let started = renew(&mut children[index], &mut kept, &console).and_then(|()| {
                // Before the start, so the line comes before any line the new start prints.
                println!(console, "{}", restart::restarted_line(&service, n));
                if let Some(restarting) = kept.restarting.as_ref() {
                    account_lines(
                        &console,
                        &children,
                        restarting,
                        &format!("{service} starting"),
                    );
                }
                launch(&mut children[index])
            });
            if let Err(e) = started {
                println!(console, "{}", restart::fatal_line(&service, &e));
                return stop_node(
                    &console,
                    &children,
                    &mut lifecycle,
                    &guardians,
                    &mut descendants,
                    &mut control,
                );
            }
            lifecycle.started(index);
        }
    }
}
