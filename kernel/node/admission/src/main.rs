//! Launches only the boot-granted engine; channel possession is admission authority.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]
extern crate alloc;
mod authority;
use alloc::{boxed::Box, collections::BTreeMap, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;
use moruna_svc::{
    Channel, Completions, Context, Decoder, Dispatcher, Handle, Handler, Outcome, Pending,
    ProtocolId, Signals, Status, Watch, Wire,
};
use moruna_sys::{Account, Console, ObjectType, Process, Rights, println, start};
use netstack_protocol::net;
use node_protocol::{CANCELLED, CHECK_BYTES, MAX_JOBS, Needs, RUNNING, TERMINATED, admission};
moruna_sys::main!(run);

/// The account of admission's client of the network server (item 3.4).
const NETWORK_BYTES: u64 = 4 << 20;

struct Job {
    owner: u64,
    process: Process,
    _account: Account,
    /// What the job needs of the node, as its check answered them: kept for the grants of
    /// items 3.3, 3.4 and 3.6, which this item does not make yet.
    _needs: Needs,
    state: u32,
    cancel_requested: bool,
    waiters: Vec<Pending>,
}
struct Shared {
    root: Handle,
    image: Handle,
    console: Console,
    next: u64,
    jobs: BTreeMap<u64, Job>,
    watches: Vec<(Box<dyn Watch>, moruna_svc::WatchPermit)>,
    watch_capacity: Option<moruna_svc::WatchCapacity>,
    guardian: moruna_node::Guardian,
    fault: bool,
    closing: bool,
    shutdown_done: bool,
    shutdown: Option<Pending>,
    backends: BTreeMap<String, Channel>,
    /// The node's memory, the machine account's limit, which the supervisor sends at start
    /// (item 1.4): a job's budget is at most this less 768 MiB (system design 3A).
    node_memory: u64,
    /// Where each check's account is carved from: `CHECK_BYTES` of the server's own account,
    /// set aside at start when it serves `api`. `None`: checks are unavailable.
    check_account: Option<Account>,
    /// The check that holds the check account, until it is released.
    check: Option<u64>,
    /// Admission's own client of the network server, on the slot `network`: `grant,connect`
    /// on every name at 443, from which each job's network is minted (system design 3D, item
    /// 3.4). It never connects itself. `None`: jobs that name hosts are refused.
    network: Option<net::Client>,
}
struct End {
    shared: Rc<RefCell<Shared>>,
    job: u64,
    handle: Handle,
}
impl Watch for End {
    fn handle(&self) -> &Handle {
        &self.handle
    }
    fn signals(&self) -> Signals {
        Signals::TERMINATED
    }
    fn signaled(&mut self, _: Signals, done: &mut Completions) -> bool {
        let mut shared = self.shared.borrow_mut();
        if let Some(job) = shared.jobs.get_mut(&self.job) {
            job.state = if job.cancel_requested {
                CANCELLED
            } else {
                TERMINATED
            };
            for pending in core::mem::take(&mut job.waiters) {
                done.complete(pending, Outcome::reply(job.state));
            }
        }
        if shared.closing
            && shared.jobs.values().all(|j| j.state != RUNNING)
            && let Some(pending) = shared.shutdown.take()
        {
            done.complete(pending, Outcome::reply(()));
            shared.shutdown_done = true;
        }
        false
    }
}
struct Session {
    shared: Rc<RefCell<Shared>>,
    owner: Option<u64>,
}
fn args<T: Wire>(d: &mut Decoder<'_>) -> Option<T> {
    let a = T::decode(d).ok()?;
    d.finish().ok()?;
    Some(a)
}
fn failure(e: admission::Error) -> Outcome {
    use moruna_svc::ProtocolError;
    Outcome::Failure(e.code())
}
struct Launch {
    account: Handle,
    budget: u64,
    config: Handle,
    names: Vec<String>,
    grants: Vec<Handle>,
    services: Vec<String>,
    needs: Needs,
}
fn launch_job(
    shared: &Rc<RefCell<Shared>>,
    owner: u64,
    launch: Launch,
) -> Result<u64, admission::Error> {
    let Launch {
        account,
        budget,
        config,
        mut names,
        mut grants,
        services,
        needs,
    } = launch;
    if !node_protocol::valid_needs(&needs) {
        return Err(admission::Error::Invalid);
    }
    if services.len() > node_protocol::MAX_GRANTS {
        return Err(admission::Error::Invalid);
    }
    {
        let state = shared.borrow();
        for service in services {
            let listener = state
                .backends
                .get(&service)
                .ok_or(admission::Error::Unavailable)?;
            let mut rights = Rights::READ | Rights::WRITE | Rights::WAIT | Rights::TRANSFER;
            // The engine gives its Linux child a narrowed personality listener.
            // Other retained services only need to establish their own connection.
            if service == "linux" {
                rights = rights | Rights::DUPLICATE;
            }
            let grant = listener
                .0
                .duplicate(rights)
                .map_err(|_| admission::Error::Unavailable)?;
            names.push(service);
            grants.push(grant);
        }
    }
    // The job's network (system design 3D, item 3.4): `connect` to the hosts its check named,
    // at 443, and nothing else, as the engine's slot `network`. A job that names none has none.
    if !needs.0.is_empty() {
        let mut state = shared.borrow_mut();
        let client = state
            .network
            .as_mut()
            .ok_or(admission::Error::Unavailable)?;
        let channel = moruna_node::egress::mint(client, &needs.0, moruna_node::egress::SOCKETS)
            .map_err(|_| admission::Error::Unavailable)?;
        names.push("network".into());
        grants.push(channel.0);
    }
    if !node_protocol::valid_grants(&names, grants.len()) || budget == 0 || budget % 4096 != 0 {
        return Err(admission::Error::Invalid);
    }
    if account.info().ok().map(|i| i.0) != Some(ObjectType::Account)
        || config.info().ok().map(|i| i.0) != Some(ObjectType::Memory)
    {
        return Err(admission::Error::Invalid);
    }
    let mut state = shared.borrow_mut();
    if state.closing {
        return Err(admission::Error::Unavailable);
    }
    if state.jobs.len() >= MAX_JOBS {
        return Err(admission::Error::Limit);
    }
    let Some(id) = state.next.checked_add(1) else {
        return Err(admission::Error::Limit);
    };
    let watch_permit = state
        .watch_capacity
        .as_ref()
        .ok_or(admission::Error::Unavailable)?
        .reserve()
        .map_err(|_| admission::Error::Limit)?;
    let account = match Account(account).carve_from(budget, "job") {
        Ok(a) => a,
        Err(_) => return Err(admission::Error::Quota),
    };
    let process = match Process::create_in(&state.root, "engine", &account.0) {
        Ok(p) => p,
        Err(_) => return Err(admission::Error::Unavailable),
    };
    let entry = match process.map_image(&state.image) {
        Ok(e) => e,
        Err(_) => return Err(admission::Error::Unavailable),
    };
    let console = match state.console.0.duplicate(Rights::WRITE | Rights::TRANSFER) {
        Ok(h) => h,
        Err(_) => return Err(admission::Error::Unavailable),
    };
    let account_grant = match account.0.info().and_then(|(_, r)| account.0.duplicate(r)) {
        Ok(h) => h,
        Err(_) => return Err(admission::Error::Unavailable),
    };
    let mut handles = alloc::vec![
        (Some(console), "console"),
        (Some(account_grant), "account"),
        (Some(config), "config")
    ];
    for (name, handle) in names.iter().zip(grants) {
        handles.push((Some(handle), name.as_str()));
    }
    let watch_handle = match process.0.duplicate(Rights::WAIT) {
        Ok(h) => h,
        Err(_) => return Err(admission::Error::Unavailable),
    };
    state.next = id;
    let escrow = match state.guardian.publish(&process) {
        Ok(id) => id,
        Err(_) => return Err(admission::Error::Unavailable),
    };
    if process.start_with(entry, &mut handles).is_err() {
        if state.guardian.retract(escrow).is_err() {
            state.fault = true;
        }
        return Err(admission::Error::Unavailable);
    }
    state.jobs.insert(
        id,
        Job {
            owner,
            process,
            _account: account,
            _needs: needs,
            state: RUNNING,
            cancel_requested: false,
            waiters: Vec::new(),
        },
    );
    state.watches.push((
        Box::new(End {
            shared: shared.clone(),
            job: id,
            handle: watch_handle,
        }),
        watch_permit,
    ));
    Ok(id)
}
impl Handler for Session {
    fn protocol(&self) -> ProtocolId {
        admission::ID
    }
    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        let owner = cx.connection();
        self.owner = Some(owner);
        if !(1..=6).contains(&ordinal) {
            return Outcome::Status(Status::UnknownMethod);
        }
        if ordinal == 6 {
            let Some((config, names, grants, services)) =
                args::<(Handle, Vec<String>, Vec<Handle>, Vec<String>)>(d)
            else {
                return Outcome::Status(Status::BadMessage);
            };
            return match check_job(&self.shared, owner, config, names, grants, services) {
                Ok(id) => Outcome::reply(id),
                Err(e) => failure(e),
            };
        }
        if ordinal == 1 {
            // The arguments in order: the first six, then the job's needs.
            let Some(((account, budget, config, names, grants, services), needs)) =
                args::<(
                    (Handle, u64, Handle, Vec<String>, Vec<Handle>, Vec<String>),
                    Needs,
                )>(d)
            else {
                return Outcome::Status(Status::BadMessage);
            };
            // A job's budget is at most the node's memory less 768 MiB (system design 3A).
            let node_memory = self.shared.borrow().node_memory;
            if let Err(e) = node_protocol::admits(budget, node_memory) {
                return failure(e);
            }
            return match launch_job(
                &self.shared,
                owner,
                Launch {
                    account,
                    budget,
                    config,
                    names,
                    grants,
                    services,
                    needs,
                },
            ) {
                Ok(id) => Outcome::reply(id),
                Err(e) => failure(e),
            };
        }
        let Some(id) = args::<u64>(d) else {
            return Outcome::Status(Status::BadMessage);
        };
        let mut state = self.shared.borrow_mut();
        let Some(job) = state.jobs.get_mut(&id).filter(|j| j.owner == owner) else {
            return failure(admission::Error::NotFound);
        };
        match ordinal {
            2 => Outcome::reply(job.state),
            3 => {
                if job.state == RUNNING {
                    if job.process.kill().is_err() {
                        return failure(admission::Error::Unavailable);
                    }
                    job.cancel_requested = true;
                }
                Outcome::reply(())
            }
            4 => {
                if job.state != RUNNING {
                    Outcome::reply(job.state)
                } else if job.waiters.len() >= 8 {
                    failure(admission::Error::Limit)
                } else {
                    job.waiters.push(cx.defer());
                    Outcome::Deferred
                }
            }
            5 => {
                if job.state == RUNNING {
                    return failure(admission::Error::Invalid);
                }
                state.jobs.remove(&id);
                Outcome::reply(())
            }
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }
    fn closed(&mut self) {
        let mut shared = self.shared.borrow_mut();
        if let Some(owner) = self.owner {
            shared.jobs.retain(|_, job| {
                if job.owner != owner {
                    return true;
                }
                if job.state == RUNNING {
                    let _ = job.process.kill();
                }
                false
            });
        }
    }
}
/// Starts a check (method 6): the engine in a `CHECK_BYTES` account carved from the check
/// account, one at a time.
fn check_job(
    shared: &Rc<RefCell<Shared>>,
    owner: u64,
    config: Handle,
    names: Vec<String>,
    grants: Vec<Handle>,
    services: Vec<String>,
) -> Result<u64, admission::Error> {
    let account = {
        let state = shared.borrow();
        if state.check.is_some_and(|id| state.jobs.contains_key(&id)) {
            return Err(admission::Error::Limit);
        }
        let check = state
            .check_account
            .as_ref()
            .ok_or(admission::Error::Unavailable)?;
        let rights = check.0.info().map_err(|_| admission::Error::Unavailable)?.1;
        check
            .0
            .duplicate(rights)
            .map_err(|_| admission::Error::Unavailable)?
    };
    let id = launch_job(
        shared,
        owner,
        Launch {
            account,
            budget: CHECK_BYTES,
            config,
            names,
            grants,
            services,
            needs: Default::default(),
        },
    )?;
    shared.borrow_mut().check = Some(id);
    Ok(id)
}

/// The node's memory, as the supervisor's message on `supervisor` carries it.
fn read_node_memory(supervisor: Handle) -> Option<u64> {
    let mut bytes = [0u8; 16];
    let mut handles = [None];
    let (count, carried) = Channel(supervisor).read(&mut bytes, &mut handles).ok()?;
    moruna_node::Bootstrap::platform_memory(&bytes[..count], carried)
}

struct ControlSession(Rc<RefCell<Shared>>, Rc<authority::Authority>);
impl Handler for ControlSession {
    fn protocol(&self) -> ProtocolId {
        node_protocol::control::ID
    }
    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        if ordinal == 2 {
            let Some(id) = args::<u64>(d) else {
                return Outcome::Status(Status::BadMessage);
            };
            return match self.1.revoke(id) {
                Ok(()) => Outcome::reply(()),
                Err(()) => Outcome::Failure(1),
            };
        }
        if ordinal != 1 {
            return Outcome::Status(Status::UnknownMethod);
        }
        if args::<()>(d).is_none() {
            return Outcome::Status(Status::BadMessage);
        }
        let mut state = self.0.borrow_mut();
        if state.closing {
            return Outcome::Failure(1);
        }
        state.closing = true;
        let mut failed = false;
        for job in state.jobs.values_mut() {
            if job.state == RUNNING {
                if job.process.kill().is_err() {
                    failed = true;
                    break;
                }
                job.cancel_requested = true;
            }
        }
        if failed {
            state.fault = true;
            return Outcome::Failure(1);
        }
        if state.jobs.values().all(|j| j.state != RUNNING) {
            state.shutdown_done = true;
            Outcome::reply(())
        } else {
            state.shutdown = Some(cx.defer());
            Outcome::Deferred
        }
    }
    fn closed(&mut self) {
        let mut state = self.0.borrow_mut();
        if !state.closing {
            state.fault = true;
        }
    }
}

fn initial_job(
    shared: &Rc<RefCell<Shared>>,
    memory: Handle,
    authority: &Rc<authority::Authority>,
) -> Result<u64, admission::Error> {
    let memory = moruna_sys::Memory(memory);
    let mapping = memory
        .map(0, 4096, moruna_sys::map::READ)
        .map_err(|_| admission::Error::Invalid)?;
    let mut bytes = alloc::vec![0; 4096];
    mapping
        .read(0, &mut bytes)
        .map_err(|_| admission::Error::Invalid)?;
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .ok_or(admission::Error::Invalid)?;
    let initial =
        moruna_node::InitialJob::parse(&bytes[..end]).map_err(|_| admission::Error::Invalid)?;
    let account = start::take("account").ok_or(admission::Error::Unavailable)?;
    let config = start::take("initial-config").ok_or(admission::Error::Unavailable)?;
    let mut names = Vec::new();
    let mut grants = Vec::new();
    let mut services = Vec::new();
    for grant in initial.grants {
        // stream P (architecture/data-os.md section 5): `root.process` is a duplicate of
        // admission's own process resource, from which the job's engine starts an image's
        // native program; the node gives each boot capability one owner.
        let handle = if grant.source == "root.process" {
            shared
                .borrow()
                .root
                .duplicate(Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
                .map_err(|_| admission::Error::Unavailable)?
        } else {
            start::take(&grant.source).ok_or(admission::Error::Unavailable)?
        };
        authority
            .register(&grant.slot, &handle)
            .map_err(|_| admission::Error::Invalid)?;
        if grant.listener {
            if handle.info().ok().map(|i| i.0) != Some(ObjectType::Channel) {
                return Err(admission::Error::Invalid);
            }
            services.push(grant.slot.clone());
            if shared
                .borrow_mut()
                .backends
                .insert(grant.slot, Channel(handle))
                .is_some()
            {
                return Err(admission::Error::Invalid);
            }
        } else {
            names.push(grant.slot);
            grants.push(handle);
        }
    }
    launch_job(
        shared,
        0,
        Launch {
            account,
            budget: initial.budget,
            config,
            names,
            grants,
            services,
            needs: Default::default(),
        },
    )
}

fn run() -> i64 {
    let Some(console) = start::take("console").map(Console) else {
        return 1;
    };
    let (Some(root), Some(image)) = (start::take("root.process"), start::take("engine-image"))
    else {
        return 1;
    };
    let listener = start::take("listen");
    let initial = start::take("initial-job");
    if listener.is_none() && initial.is_none() {
        return 1;
    }
    let Some(guardian) = start::take("guardian").map(Channel) else {
        return 1;
    };
    let Some(control) = start::take("control").map(Channel) else {
        return 1;
    };
    let Some(node_memory) = start::take("supervisor").and_then(read_node_memory) else {
        println!(
            console,
            "moruna: fatal node-admission: the node's memory is not known: the supervisor sent \
             none on `supervisor`"
        );
        return 1;
    };
    let own = start::take("account").map(Account);
    // The job network's client (item 3.4), on its own small account: the memory it lends the
    // network server, and the clients of the channels it mints.
    let network = start::take("network").map(Channel).and_then(|listener| {
        let account = own.as_ref()?.carve_from(NETWORK_BYTES, "network").ok()?;
        let channel = moruna_svc::connect(&listener).ok()?;
        net::Client::connect(channel, moruna_svc::deadline_in(30_000_000_000), account).ok()
    });
    // The check account, set aside when the server serves `api` and runs no initial job: a
    // check runs beside a job (system design 3A).
    let check_account = if listener.is_some() && initial.is_none() {
        match own.as_ref().map(|own| own.carve_from(CHECK_BYTES, "check")) {
            Some(Ok(account)) => Some(account),
            Some(Err(e)) => {
                println!(
                    console,
                    "node-admission: no check account: {} bytes could not be carved from its \
                     own: {}",
                    CHECK_BYTES,
                    e.name()
                );
                None
            }
            None => None,
        }
    } else {
        None
    };
    let shared = Rc::new(RefCell::new(Shared {
        root,
        image,
        console,
        next: 0,
        jobs: BTreeMap::new(),
        watches: Vec::new(),
        watch_capacity: None,
        guardian: moruna_node::Guardian::new(guardian),
        fault: false,
        closing: false,
        shutdown_done: false,
        shutdown: None,
        backends: BTreeMap::new(),
        node_memory,
        check_account,
        check: None,
        network,
    }));
    let Some(policy) = start::take("storage.grants") else {
        return 1;
    };
    let (authority_channel, authority_kind) = match (
        start::take("storage.authority"),
        start::take("local-home.authority"),
    ) {
        (Some(channel), None) => (Channel(channel), "home"),
        (None, Some(channel)) => (Channel(channel), "local-home"),
        _ => return 1,
    };
    let authority = match authority::Authority::load(shared.clone(), policy) {
        Ok(authority) => authority,
        Err(()) => return 1,
    };
    let factory = shared.clone();
    let mut dispatcher = Dispatcher::new();
    shared.borrow_mut().watch_capacity = Some(dispatcher.watch_capacity());
    if let Some(listener) = listener
        && dispatcher
            .listen(
                Channel(listener),
                Box::new(move || {
                    Box::new(Session {
                        shared: factory.clone(),
                        owner: None,
                    })
                }),
            )
            .is_err()
    {
        return 1;
    }
    let mut masters = alloc::vec![(authority_channel, authority_kind)];
    for (slot, kind) in [("local.authority", "local")] {
        if let Some(channel) = start::take(slot) {
            masters.push((Channel(channel), kind));
        }
    }
    for (channel, kind) in masters {
        let handler = match authority.handler(&channel, kind) {
            Ok(handler) => handler,
            Err(()) => return 1,
        };
        if dispatcher.serve(channel, handler).is_err() {
            return 1;
        }
    }
    if dispatcher
        .serve(
            control,
            Box::new(ControlSession(shared.clone(), authority.clone())),
        )
        .is_err()
    {
        return 1;
    }
    if let Some(initial) = initial {
        match initial_job(&shared, initial, &authority) {
            Ok(job) => authority.owner(job),
            Err(_) => return 1,
        }
        let watches = core::mem::take(&mut shared.borrow_mut().watches);
        for (watch, permit) in watches {
            if dispatcher.watch_reserved(watch, permit).is_err() {
                for job in shared.borrow().jobs.values() {
                    let _ = job.process.kill();
                }
                return 1;
            }
        }
    }
    loop {
        if dispatcher.turn(moruna_svc::DEADLINE_INFINITE).is_err() {
            break;
        }
        let watches = core::mem::take(&mut shared.borrow_mut().watches);
        for (watch, permit) in watches {
            if dispatcher.watch_reserved(watch, permit).is_err() {
                for job in shared.borrow().jobs.values() {
                    let _ = job.process.kill();
                }
                return 1;
            }
        }
        if shared.borrow().shutdown_done {
            return 0;
        }
        if shared.borrow().fault {
            break;
        }
        if dispatcher.is_empty() {
            break;
        }
    }
    for job in shared.borrow().jobs.values() {
        let _ = job.process.kill();
    }
    1
}
