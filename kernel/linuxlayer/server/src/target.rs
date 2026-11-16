// SPDX-License-Identifier: MPL-2.0

//! The drop-in `linux` server: `moruna.linux` 3.0 (`linux-protocol`), as
//! `kernel/linux/server` served it, with each program run on a Linux personality of its
//! own (`linuxlayer_launch`) instead of forwarded to this server call by call.
//!
//! For each request it makes the program's process with the personality in it, granted:
//!
//! - its own connection to the image filesystem (`filesystem`), the console, and the
//!   process resource (`processes`), so the personality can fork;
//! - the program's path, arguments and environment (`command`), and a page the
//!   personality writes how the task ended on (`report`);
//! - a log ring (`log`): the engine's for `run_plane`, else one this server makes and
//!   reads as the program runs, echoing each line to the console and keeping the first
//!   `OUTPUT_MAX` bytes for the answer;
//! - `run_plane`'s slots, attached to the process and granted (`input`, `output`), and its
//!   account; `run_granted`'s account and scratch account.
//!
//! The process is published to the supervisor's guardian before it starts, and is ended
//! when the connection that asked for it closes. At its end the request is answered from
//! the report and the output.
//!
//! Start slots, as `kernel/linux/server` took them: `console`; `supervisor` (the process
//! resource, `moruna_node::Bootstrap::Linux`); `fs`, a listener of the image filesystem;
//! `listen*`, listeners of `moruna.linux`. The personality's image is in the server
//! (`LINUXLAYER_PERSONALITY` at its build), so a boot or node image starts it as it
//! started the old one.

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    string::String,
    vec,
    vec::Vec,
};
use core::cell::{Cell, RefCell};

use linux_protocol::{Bytes, Grant, Plane, linux};
use linuxlayer_launch::{
    command,
    launcher::{self, LaunchError, Options},
    report, slot,
};
use linuxlayer_server::{Output, echo, ended, process_name, program_names, request_ok, ring_lines};
use moruna_record::ring::Ring;
use moruna_svc::{
    Channel, Completions, ConnectionId, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher,
    Handle, Handler, Outcome, Pending, ProtocolError, ProtocolId, Signals, Status, Watch, Wire,
    println, start,
};
use moruna_sys::{Mapping, Memory, Process, Rights, map, plane::Slot};

moruna_svc::main!(run);

/// The most programs running at once, as `kernel/linux/server` ran; past it a request is
/// refused `Busy`.
const TASKS_MAX: usize = 24;

/// How long `layers` waits for the image filesystem, which answers from what it mounted.
const LAYERS_WAIT: u64 = 60_000_000_000;

/// The log ring of a program run with `run` or `run_granted`.
const RING_LEN: u64 = 256 * 1024;

/// How often a running program's ring is read, in nanoseconds.
const READ_EVERY: u64 = 20_000_000;

/// Bytes aligned as an ELF image's headers want them.
#[repr(C, align(4096))]
struct Aligned<T: ?Sized>(T);

/// The personality's image.
static PERSONALITY: &Aligned<[u8]> = &Aligned(*include_bytes!(env!("LINUXLAYER_PERSONALITY")));

/// A request to run a program, waiting for the main loop to start it.
struct Job {
    pending: Pending,
    path: Vec<u8>,
    argv: Vec<Vec<u8>>,
    env: Vec<Vec<u8>>,
    plane: Option<Plane>,
    grant: Option<Grant>,
}

/// What every task shares.
struct Shared {
    out: Console,
    fs: Channel,
    resource: Handle,
    cpus: usize,
    jobs: VecDeque<Job>,
    running: usize,
    /// The rings of the programs that run, read as they run.
    rings: Vec<Rc<RefCell<TaskRing>>>,
    /// The processes each connection's requests started and that still run: closing the
    /// connection ends them.
    owned: BTreeMap<ConnectionId, Vec<(u64, Handle)>>,
    guardian: moruna_node::Guardian,
    next: u64,
}

impl Shared {
    /// The connection that asked for task `id` no longer needs it watched.
    fn disown(&mut self, connection: ConnectionId, id: u64) {
        if let Some(list) = self.owned.get_mut(&connection) {
            list.retain(|(t, _)| *t != id);
            if list.is_empty() {
                self.owned.remove(&connection);
            }
        }
    }
}

/// A program's log ring, which this server made: its lines are echoed and kept.
struct TaskRing {
    name: String,
    ring: Mapping<'static>,
    output: Output,
}

impl TaskRing {
    /// Reads every line written since the last read.
    fn read(&mut self, out: &Console) {
        let Ok(region) = self.ring.shared_region() else {
            return;
        };
        for (_, line) in ring_lines(region) {
            println!(out, "{}", echo(&self.name, &line));
            self.output.line(&line);
        }
    }
}

struct Connection {
    shared: Rc<RefCell<Shared>>,
    /// Its number, once it has made a request.
    id: Option<ConnectionId>,
}

impl Handler for Connection {
    fn protocol(&self) -> ProtocolId {
        linux::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        if ordinal == 4 {
            if d.finish().is_err() {
                return Outcome::Status(Status::BadMessage);
            }
            return match layers(&self.shared.borrow()) {
                Ok(seals) => Outcome::reply(seals),
                Err(e) => Outcome::Failure(e.code()),
            };
        }
        if !(1..=3).contains(&ordinal) {
            return Outcome::Status(Status::UnknownMethod);
        }
        self.id = Some(cx.connection());
        let (Ok(path), Ok(argv), Ok(env)) = (
            Bytes::decode(d),
            Vec::<Bytes>::decode(d),
            Vec::<Bytes>::decode(d),
        ) else {
            return Outcome::Status(Status::BadMessage);
        };
        let plane = match ordinal {
            2 => match Plane::decode(d) {
                Ok(plane) => Some(plane),
                Err(_) => return Outcome::Status(Status::BadMessage),
            },
            _ => None,
        };
        let grant = match ordinal {
            3 => match Grant::decode(d) {
                Ok(grant) => Some(grant),
                Err(_) => return Outcome::Status(Status::BadMessage),
            },
            _ => None,
        };
        if d.finish().is_err() {
            return Outcome::Status(Status::BadMessage);
        }
        let argv: Vec<Vec<u8>> = argv.into_iter().map(|b| b.0).collect();
        let env: Vec<Vec<u8>> = env.into_iter().map(|b| b.0).collect();
        if !request_ok(&path.0, &argv, &env) {
            return Outcome::Failure(linux::Error::BadRequest.code());
        }
        let mut shared = self.shared.borrow_mut();
        if shared.running + shared.jobs.len() >= TASKS_MAX {
            return Outcome::Failure(linux::Error::Busy.code());
        }
        shared.jobs.push_back(Job {
            pending: cx.defer(),
            path: path.0,
            argv,
            env,
            plane,
            grant,
        });
        Outcome::Deferred
    }

    /// The client has gone: what it started ends with it.
    fn closed(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        let owned = self.shared.borrow_mut().owned.remove(&id);
        for (task, process) in owned.unwrap_or_default() {
            if let Err(e) = Process(process).kill() {
                println!(
                    self.shared.borrow().out,
                    "linux: FAIL: ending task {} for its closed connection: {}",
                    task,
                    e.name()
                );
            }
        }
    }
}

/// The seals of the layers its programs start on: its image filesystem's `layers`, on a
/// connection of its own, as a program's would be.
fn layers(s: &Shared) -> Result<Vec<Bytes>, linux::Error> {
    let fail = |what: &str, e: &dyn core::fmt::Debug| {
        println!(s.out, "linux: FAIL: {} the image filesystem: {:?}", what, e);
        linux::Error::NoImage
    };
    let channel = moruna_svc::connect(&s.fs).map_err(|e| fail("connecting to", &e))?;
    let mut fs =
        imagefs_protocol::imagefs::Client::connect(channel, moruna_svc::deadline_in(LAYERS_WAIT))
            .map_err(|e| fail("hello to", &e))?;
    fs.core().set_timeout(LAYERS_WAIT);
    let seals = fs.layers().map_err(|e| fail("asking the layers of", &e))?;
    Ok(seals.into_iter().map(|b| Bytes(b.0)).collect())
}

/// A task's process: its end answers the request that started it.
struct End {
    id: u64,
    waiter: Handle,
    /// Kept while the task runs.
    _process: Process,
    report: Memory,
    ring: Option<Rc<RefCell<TaskRing>>>,
    shared: Rc<RefCell<Shared>>,
    pending: Option<Pending>,
}

impl Watch for End {
    fn handle(&self) -> &Handle {
        &self.waiter
    }

    fn signals(&self) -> Signals {
        Signals::TERMINATED
    }

    fn signaled(&mut self, _seen: Signals, done: &mut Completions) -> bool {
        let mut shared = self.shared.borrow_mut();
        let output = match self.ring.take() {
            Some(ring) => {
                shared.rings.retain(|r| !Rc::ptr_eq(r, &ring));
                let mut ring = ring.borrow_mut();
                ring.read(&shared.out);
                core::mem::take(&mut ring.output)
            }
            None => Output::default(),
        };
        let report = self
            .report
            .map(0, report::LEN as u64, 0)
            .ok()
            .and_then(|m| report::decode(m.bytes()));
        let answer = match ended(report, output) {
            Ok(ended) => Outcome::reply(ended),
            Err(e) => Outcome::Failure(e.code()),
        };
        shared.running -= 1;
        if let Some(pending) = self.pending.take() {
            shared.disown(pending.connection(), self.id);
            done.complete(pending, answer);
        }
        false
    }
}

/// What a started task hands the main loop.
struct Started {
    process: Process,
    report: Memory,
    ring: Option<Rc<RefCell<TaskRing>>>,
}

/// Makes `job`'s process with the personality in it and starts it.
fn launch_job(
    s: &mut Shared,
    path: Vec<u8>,
    argv: Vec<Vec<u8>>,
    env: Vec<Vec<u8>>,
    plane: Option<Plane>,
    grant: Option<Grant>,
) -> Result<Started, linux::Error> {
    let name = process_name(&path);
    let Shared {
        out,
        fs,
        resource,
        guardian,
        ..
    } = s;
    let out: &Console = out;
    let fail = |what: &str, e: moruna_svc::Error| {
        println!(out, "linux: {}: {}: {}", name, what, e.name());
        linux::Error::NoProcess
    };
    let command = command::encode(&command::Command { path, argv, env })
        .map_err(|_| linux::Error::BadRequest)?;
    let command = memory_with(&command).map_err(|e| fail("the command's page", e))?;
    let report = Memory::create(report::LEN as u64).map_err(|e| fail("the report's page", e))?;
    let report_grant = report
        .0
        .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
        .map_err(|e| fail("the report's page", e))?;
    let console = out
        .0
        .duplicate(Rights::WRITE | Rights::TRANSFER)
        .map_err(|e| fail("the console", e))?;
    let processes = resource
        .duplicate(Rights::WRITE | Rights::TRANSFER)
        .map_err(|e| fail("the process resource", e))?;
    let filesystem = moruna_svc::connect(fs).map_err(|e| {
        println!(
            out,
            "linux: FAIL: connecting to the image filesystem: {e:?}"
        );
        linux::Error::NoProcess
    })?;

    // The program's own handles, as the old server gave them: `run_granted`'s, and for
    // `run_plane` an observer of its account; with either, a read-only export of the image's
    // root as `filesystem`.
    let (account, scratch, log, launch_plane, mut program) = match (plane, grant) {
        (Some(p), _) => {
            if p.assets.is_some() {
                println!(
                    out,
                    "linux: {}: its assets are not mounted on the personality yet", name
                );
            }
            let observer = p
                .account
                .duplicate(Rights::READ | Rights::TRANSFER)
                .map_err(|_| linux::Error::BadGrant)?;
            let plane = launcher::Plane {
                input: p.input.map(Slot),
                output: p.output.map(Slot),
                port: p.port,
            };
            let program = vec![(String::from("account"), Some(observer))];
            (Some(p.account), None, Some(p.log), Some(plane), program)
        }
        (None, Some(g)) => {
            let (names, handles): (Vec<Vec<u8>>, Vec<Handle>) =
                g.handles.into_iter().map(|(n, h)| (n.0, h)).unzip();
            let names = program_names(&names).ok_or(linux::Error::BadGrant)?;
            let program = names
                .into_iter()
                .zip(handles.into_iter().map(Some))
                .collect();
            (Some(g.account), g.scratch, None, None, program)
        }
        (None, None) => (None, None, None, None, Vec::new()),
    };
    if account.is_some() {
        program.push((String::from("filesystem"), Some(export_root(fs, out)?)));
    }
    let (log, ring) = match log {
        Some(log) => (log, None),
        None => {
            let (log, ring) = new_ring().map_err(|e| fail("the log ring", e))?;
            let ring = TaskRing {
                name: name.clone(),
                ring,
                output: Output::default(),
            };
            (log, Some(Rc::new(RefCell::new(ring))))
        }
    };

    let mut grants = vec![
        (Some(console), slot::CONSOLE),
        (Some(processes), slot::PROCESSES),
        (Some(filesystem.0), slot::FILESYSTEM),
        (scratch, slot::SCRATCH),
        (Some(log), slot::LOG),
        (Some(command.0), slot::COMMAND),
        (Some(report_grant), slot::REPORT),
    ];
    grants.extend(program.iter_mut().map(|(n, h)| (h.take(), n.as_str())));
    let escrow = Cell::new(None);
    let mut publish = |process: &Process| {
        escrow.set(Some(guardian.publish(process)?));
        Ok(())
    };
    let options = Options {
        account: account.as_ref(),
        plane: launch_plane,
        before_start: Some(&mut publish),
    };
    let launched = launcher::launch_with(resource, &name, &PERSONALITY.0, &mut grants, options);
    let (process, _) = launched.map_err(|e| {
        if let Some(id) = escrow.get() {
            let _ = guardian.retract(id);
        }
        match e {
            LaunchError::Plan(p) => {
                println!(out, "linux: FAIL: the personality's image: {:?}", p);
                linux::Error::NoProcess
            }
            LaunchError::Call(what, e) => {
                println!(out, "linux: {}: {}: {}", name, what, e.name());
                match what {
                    "process_plane" | "process_grant" => linux::Error::BadGrant,
                    _ => linux::Error::NoProcess,
                }
            }
        }
    })?;
    Ok(Started {
        process,
        report,
        ring,
    })
}

/// A read-only export of the image's root, on a connection of its own to the image
/// filesystem: a program's `filesystem`.
fn export_root(fs: &Channel, out: &Console) -> Result<Handle, linux::Error> {
    let fail = |what: &str, e: &dyn core::fmt::Debug| {
        println!(out, "linux: FAIL: {} the image filesystem: {:?}", what, e);
        linux::Error::NoProcess
    };
    let channel = moruna_svc::connect(fs).map_err(|e| fail("connecting to", &e))?;
    let mut client =
        imagefs_protocol::imagefs::Client::connect(channel, moruna_svc::deadline_in(LAYERS_WAIT))
            .map_err(|e| fail("hello to", &e))?;
    client.core().set_timeout(LAYERS_WAIT);
    let root = client
        .export_directory(imagefs_protocol::Bytes(b"/".to_vec()))
        .map_err(|e| fail("exporting the root of", &e))?;
    Ok(root.0)
}

/// A memory object holding `bytes`, whole pages.
fn memory_with(bytes: &[u8]) -> Result<Memory, moruna_svc::Error> {
    let len = (bytes.len().max(1) as u64).next_multiple_of(4096);
    let memory = Memory::create(len)?;
    memory.map(0, len, map::WRITE)?.write(0, bytes)?;
    Ok(memory)
}

/// A new log ring: its memory object, and this server's mapping of it.
fn new_ring() -> Result<(Handle, Mapping<'static>), moruna_svc::Error> {
    let memory = Memory::create(RING_LEN)?;
    let mut mapping = memory.map_detached(0, RING_LEN, map::READ | map::WRITE)?;
    Ring::init(mapping.shared_region()?).map_err(|_| moruna_svc::Error::InvalidArgs)?;
    Ok((memory.0, mapping))
}

fn start_job(
    job: Job,
    shared: &Rc<RefCell<Shared>>,
    dispatcher: &mut Dispatcher,
) -> Result<(), (Pending, linux::Error)> {
    let Job {
        pending,
        path,
        argv,
        env,
        plane,
        grant,
    } = job;
    if dispatcher.len() + 1 > moruna_svc::limits::WAIT_MAX_ITEMS {
        return Err((pending, linux::Error::Busy));
    }
    let started = launch_job(&mut shared.borrow_mut(), path, argv, env, plane, grant);
    let started = match started {
        Ok(started) => started,
        Err(e) => return Err((pending, e)),
    };
    let waiter = started.process.0.duplicate(Rights::WAIT);
    let killer = started.process.0.duplicate(Rights::WRITE);
    let (waiter, killer) = match (waiter, killer) {
        (Ok(w), Ok(k)) => (w, k),
        (Err(e), _) | (_, Err(e)) => {
            let _ = started.process.kill();
            println!(
                shared.borrow().out,
                "linux: FAIL: a task cannot be watched: {}",
                e.name()
            );
            return Err((pending, linux::Error::NoProcess));
        }
    };
    let id = {
        let mut s = shared.borrow_mut();
        s.next += 1;
        s.running += 1;
        let id = s.next;
        s.owned
            .entry(pending.connection())
            .or_default()
            .push((id, killer));
        s.rings.extend(started.ring.clone());
        id
    };
    let end = End {
        id,
        waiter,
        _process: started.process,
        report: started.report,
        ring: started.ring,
        shared: shared.clone(),
        pending: Some(pending),
    };
    // Room was checked above.
    if dispatcher.watch(Box::new(end)).is_err() {
        println!(shared.borrow().out, "linux: FAIL: no room to watch a task");
    }
    Ok(())
}

fn take_root(out: &Console) -> Option<(Handle, Channel)> {
    let supervisor = Channel(start::take("supervisor")?);
    let mut buf = [0u8; 16];
    let mut handles = [None];
    loop {
        match supervisor.read(&mut buf, &mut handles) {
            Ok((n, count)) if moruna_node::Bootstrap::Linux.accepts(&buf[..n], count) => {
                return Some((handles[0].take()?, supervisor));
            }
            Ok(_) => return None,
            Err(moruna_svc::Error::ShouldWait) => {
                if moruna_svc::wait_one(
                    &supervisor.0,
                    Signals::READABLE | Signals::PEER_CLOSED,
                    DEADLINE_INFINITE,
                )
                .is_err()
                {
                    return None;
                }
            }
            Err(e) => {
                println!(
                    out,
                    "linux: FAIL: reading the supervisor's channel: {}",
                    e.name()
                );
                return None;
            }
        }
    }
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some((resource, supervisor)) = take_root(&out) else {
        println!(
            out,
            "linux: FAIL: no process resource, so no process can be made"
        );
        return 1;
    };
    let Some(fs) = start::take("fs").map(Channel) else {
        println!(out, "linux: FAIL: no image filesystem (the slot `fs`)");
        return 1;
    };
    let shared = Rc::new(RefCell::new(Shared {
        out,
        fs,
        resource,
        cpus: start::cpus().max(1),
        jobs: VecDeque::new(),
        running: 0,
        rings: Vec::new(),
        owned: BTreeMap::new(),
        guardian: moruna_node::Guardian::new(supervisor),
        next: 0,
    }));
    let mut dispatcher = Dispatcher::new();
    let mut listeners = 0;
    for i in 0..start::count() {
        let Some((_, _, slot)) = start::entry(i) else {
            continue;
        };
        if !slot.starts_with("listen") {
            continue;
        }
        let Some(handle) = start::take(slot) else {
            continue;
        };
        let s = shared.clone();
        let factory: moruna_svc::Factory = Box::new(move || {
            Box::new(Connection {
                shared: s.clone(),
                id: None,
            }) as Box<dyn Handler>
        });
        if let Err(e) = dispatcher.listen(Channel(handle), factory) {
            println!(
                shared.borrow().out,
                "linux: FAIL: listening on {}: {}",
                slot,
                e.name()
            );
            return 1;
        }
        listeners += 1;
    }
    {
        let s = shared.borrow();
        println!(
            s.out,
            "linux: up: {} listeners, {} CPUs, each program on its own Linux personality ({} bytes)",
            listeners,
            s.cpus,
            PERSONALITY.0.len()
        );
    }
    let mut served = 0u64;
    loop {
        // While a program runs with a ring of this server's, the turn ends in time to read
        // it.
        let deadline = if shared.borrow().rings.is_empty() {
            DEADLINE_INFINITE
        } else {
            moruna_svc::now().saturating_add(READ_EVERY)
        };
        if let Err(e) = dispatcher.turn(deadline) {
            println!(
                shared.borrow().out,
                "linux: FAIL: the dispatch loop: {}",
                e.name()
            );
            return 1;
        }
        {
            let s = shared.borrow();
            for ring in &s.rings {
                ring.borrow_mut().read(&s.out);
            }
        }
        let jobs: Vec<Job> = shared.borrow_mut().jobs.drain(..).collect();
        for job in jobs {
            served += 1;
            if let Err((pending, e)) = start_job(job, &shared, &mut dispatcher) {
                let _ = dispatcher.complete(pending, Outcome::Failure(e.code()));
            }
        }
        if dispatcher.channels() == 0 && shared.borrow().running == 0 {
            break;
        }
    }
    println!(
        shared.borrow().out,
        "linux: served: {} programs run", served
    );
    0
}
