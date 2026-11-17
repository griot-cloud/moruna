//! Owns service process handles and their carved accounts; no spare channel ends.
//!
//! A node that fails says why and stops (`architecture/moruna-os-system.md`, section 1B):
//! - the supervisor that cannot go on writes `moruna: fatal services: <reason>`;
//! - a service not started within 5 s of the supervisor's own start is
//!   `moruna: fatal services: <name> did not start within 5 s`;
//! - a service that ends is `moruna: fatal service <name>: ended`.
//!
//! After any of them it kills every child and exits with failure, and the core, with no
//! process left, powers the machine off.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use moruna_node::{Bootstrap, Config, Lifecycle, StartError, line};
use moruna_sys::{
    Account, Channel, Console, Handle, Memory, Process, Rights, Signals, Usage, WaitItem, map, now,
    println, start, wait,
};

moruna_sys::main!(run);

struct Child {
    name: String,
    process: Process,
    _account: Account,
    entry: u64,
    handles: Vec<(Option<Handle>, String)>,
}
fn take(name: &str) -> Result<Handle, String> {
    start::take(name).ok_or_else(|| format!("missing boot capability {name}"))
}
type Prepared = (Vec<Child>, Lifecycle, Vec<Channel>, Option<Channel>);

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
    let config =
        Config::parse(&bytes[..end], usage.room()).map_err(|e| format!("config: {e:?}"))?;
    let process_root = take("root.process")?;
    let mut guardians = Vec::new();
    let mut control = None;
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
        // A configured service owns its output capability: it may retain a
        // narrowed console for shared state or delegate output to a real child.
        // Descendant grants remain narrowed by that owner at the actual handoff.
        let console_rights = Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER;
        let handles = alloc::vec![
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
        ];
        children.push(Child {
            name: service.name.clone(),
            process,
            _account: account,
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
    for link in &config.links {
        let left = config
            .services
            .iter()
            .position(|s| s.name == link.left)
            .ok_or("unknown link service")?;
        let right = config
            .services
            .iter()
            .position(|s| s.name == link.right)
            .ok_or("unknown link service")?;
        let (a, b) = Channel::create().map_err(|e| format!("link: {}", e.name()))?;
        children[left]
            .handles
            .push((Some(a.0), link.left_slot.clone()));
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
        // `platform` makes no process, so its channel is no guardian, and this end is not
        // kept: the message stays queued for `platform` once the end has closed.
        if *kind != Bootstrap::Platform {
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
    }
    let lifecycle = Lifecycle::new(children.len());
    Ok((children, lifecycle, guardians, control))
}
type Descendants = Vec<(usize, u64, Process)>;
/// Why the services' children could not be collected.
enum Uncollected {
    /// A guardian channel's other end closed: the service that held it has ended, or is
    /// ending.
    Closed,
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
            Self::Closed => f.write_str("guardian read: peer closed"),
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
                Err(moruna_sys::Error::PeerClosed) => return Err(Uncollected::Closed),
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
/// The first started service whose process has ended, waiting up to a second for one. A
/// service that ends closes its channels a moment before its process is seen to have
/// ended, so its closed guardian channel can reach the supervisor first.
fn ended_service(children: &[Child], lifecycle: &Lifecycle) -> Option<usize> {
    let started: Vec<usize> = (0..children.len())
        .filter(|index| lifecycle.state(*index) != Some(moruna_node::State::Pending))
        .collect();
    let mut items: Vec<_> = started
        .iter()
        .map(|index| WaitItem::new(&children[*index].process.0, Signals::TERMINATED))
        .collect();
    wait(&mut items, now().saturating_add(1_000_000_000)).ok()?;
    items
        .iter()
        .position(|item| item.observed().contains(Signals::TERMINATED))
        .map(|at| started[at])
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
    let (mut children, mut lifecycle, guardians, mut control) = match prepare(&console) {
        Ok(c) => c,
        Err(e) => {
            println!(console, "node: startup refused: {}", e);
            println!(console, "{}", line::fatal(&e));
            return 1;
        }
    };
    let mut descendants = Vec::new();
    let started = moruna_node::start_services(&mut lifecycle, began, now, |index| {
        let child: &mut Child = &mut children[index];
        let mut handles: Vec<_> = child
            .handles
            .iter_mut()
            .map(|(h, name)| (h.take(), name.as_str()))
            .collect();
        child.process.start_with(child.entry, &mut handles)?;
        println!(console, "node: service {} started", index);
        Ok::<(), moruna_sys::Error>(())
    });
    if let Err(failure) = started {
        match failure {
            StartError::Refused { index, error } => {
                println!(
                    console,
                    "node: service {} start failed: {}",
                    index,
                    error.name()
                );
                println!(
                    console,
                    "{}",
                    line::fatal(format_args!(
                        "{} could not start: {}",
                        children[index].name,
                        error.name()
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
            // A closed guardian channel is a service ending, seen before its process's end
            // (as it is on 2 CPUs): then the line is the one for a service that ended.
            let ended = match e {
                Uncollected::Closed => ended_service(&children, &lifecycle),
                Uncollected::Failed(_) => None,
            };
            match ended {
                Some(index) => {
                    lifecycle.terminated(index);
                    println!(
                        console,
                        "node: critical service {} terminated; stopping node", index
                    );
                    println!(console, "{}", line::Ended(&children[index].name));
                }
                None => {
                    println!(console, "node: child supervision failed: {}", e);
                    println!(
                        console,
                        "{}",
                        line::fatal(format_args!("child supervision failed: {e}"))
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
            return stop_node(
                &console,
                &children,
                &mut lifecycle,
                &guardians,
                &mut descendants,
                &mut control,
            );
        }
        for (index, item) in items[..children.len()].iter().enumerate() {
            if item.observed().contains(Signals::TERMINATED) {
                lifecycle.terminated(index);
                println!(
                    console,
                    "node: critical service {} terminated; stopping node", index
                );
                println!(console, "{}", line::Ended(&children[index].name));
                return stop_node(
                    &console,
                    &children,
                    &mut lifecycle,
                    &guardians,
                    &mut descendants,
                    &mut control,
                );
            }
        }
    }
}
