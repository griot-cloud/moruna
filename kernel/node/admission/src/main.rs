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
use moruna_sys::{Account, Console, ObjectType, Process, Rights, start};
use node_protocol::{CANCELLED, MAX_JOBS, RUNNING, TERMINATED, admission};
moruna_sys::main!(run);

struct Job {
    owner: u64,
    process: Process,
    _account: Account,
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
        if shared.closing && shared.jobs.values().all(|j| j.state != RUNNING) {
            if let Some(pending) = shared.shutdown.take() {
                done.complete(pending, Outcome::reply(()));
                shared.shutdown_done = true;
            }
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
    } = launch;
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
            let grant = listener
                .0
                .duplicate(Rights::READ | Rights::WRITE | Rights::WAIT | Rights::TRANSFER)
                .map_err(|_| admission::Error::Unavailable)?;
            names.push(service);
            grants.push(grant);
        }
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
        if !(1..=5).contains(&ordinal) {
            return Outcome::Status(Status::UnknownMethod);
        }
        if ordinal == 1 {
            let Some((account, budget, config, names, grants, services)) =
                args::<(Handle, u64, Handle, Vec<String>, Vec<Handle>, Vec<String>)>(d)
            else {
                return Outcome::Status(Status::BadMessage);
            };
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
        let handle = start::take(&grant.source).ok_or(admission::Error::Unavailable)?;
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
    if let Some(listener) = listener {
        if dispatcher
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
