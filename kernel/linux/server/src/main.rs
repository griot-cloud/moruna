//! The Linux personality server (SDD 28): runs unmodified Linux programs as processes
//! on the Moruna microkernel.
//!
//! A client asks `moruna.linux` to run a program. The server makes a process with a
//! forward (SDD 20 d.2, `process_forward`) whose table sends the fast calls to the
//! kernel and every other call to this server; opens a connection to the image
//! filesystem for it (its files and its scratch directory); maps the program and its
//! interpreter as Linux's `execve` would; builds its first stack; and launches it.
//! Each of the process's calls then arrives as a request on the forward's channel,
//! is answered here with Linux's meaning (or refused with `ENOSYS` and counted), and
//! the answer goes back. When the process ends, the client's request is answered with
//! how it ended, its output and its refused calls, and the counts go to the console.
//!
//! `layers` (1.3) says which layers its programs start on, from its image filesystem, so
//! a starter can check an image's identity before it asks for one (SDD 30 f.12).
//!
//! A program started with `run_granted` (1.2, SDD 30) holds the handles its starter gave,
//! named on a start page its auxiliary vector carries: the engine is such a program. A
//! program lives no longer than the connection that asked for it. A call that blocks
//! (`epoll_wait`, a read of an empty eventfd) is answered when it can be, or at its
//! deadline, while every other task is served.
//!
//! Start slots: `console`; `root.process` (or the supervisor sends it on
//! `supervisor`); `fs`, a listener of the image filesystem; `listen*`, listeners of
//! `moruna.linux`.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

mod events;
mod files;
mod load;
mod memory;
mod namespace;
mod plane;
mod task;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    rc::{Rc, Weak},
    vec::Vec,
};
use core::cell::RefCell;

use linux_protocol::{Bytes, Ended, Grant, Plane, REQUEST_MAX, linux};
use moruna_svc::{
    Channel, Completions, ConnectionId, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher,
    Handle, Handler, Outcome, Pending, ProtocolError, ProtocolId, Signals, Status, Watch, Wire,
    println, start,
};

use crate::task::Task;

moruna_svc::main!(run);

/// The most programs running at once. Each is two waited handles, and a plane task's slots
/// two more (SDD 30 f.10); a program is started only while its handles fit the server's one
/// `wait` of 64 beside its listeners and connections, and is refused `Busy` otherwise.
const TASKS_MAX: usize = 24;

/// How long `layers` waits for the image filesystem, which answers from what it mounted.
const LAYERS_WAIT: u64 = 60_000_000_000;

/// A request to run a program, waiting for the main loop to start it.
struct Job {
    pending: Pending,
    path: Vec<u8>,
    argv: Vec<Vec<u8>>,
    env: Vec<Vec<u8>>,
    /// A `run_plane`'s plane.
    plane: Option<Plane>,
    /// A `run_granted`'s grant.
    grant: Option<Grant>,
}

/// What every task shares.
pub struct Shared {
    pub out: Console,
    pub fs: Channel,
    pub resource: Handle,
    pub cpus: usize,
    jobs: VecDeque<Job>,
    pub running: usize,
    /// Every task started, for the deadlines of their blocked calls; a task that has
    /// ended has gone from here once nothing holds it.
    tasks: Vec<Weak<RefCell<Task>>>,
    /// The processes each connection's requests started and that still run: closing the
    /// connection ends them.
    owned: BTreeMap<ConnectionId, Vec<(u64, Handle)>>,
    pub guardian: moruna_node::Guardian,
}

impl Shared {
    /// The connection that asked for `pid` no longer needs it watched.
    fn disown(&mut self, connection: ConnectionId, pid: u64) {
        if let Some(list) = self.owned.get_mut(&connection) {
            list.retain(|(p, _)| *p != pid);
            if list.is_empty() {
                self.owned.remove(&connection);
            }
        }
    }
}

struct Connection {
    shared: Rc<RefCell<Shared>>,
    /// Its number, once it has made a request.
    id: Option<ConnectionId>,
}

fn bytes_ok(b: &[u8]) -> bool {
    !b.contains(&0)
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
        let plane = if ordinal == 2 {
            match Plane::decode(d) {
                Ok(plane) => Some(plane),
                Err(_) => return Outcome::Status(Status::BadMessage),
            }
        } else {
            None
        };
        let grant = if ordinal == 3 {
            match Grant::decode(d) {
                Ok(grant) => Some(grant),
                Err(_) => return Outcome::Status(Status::BadMessage),
            }
        } else {
            None
        };
        if d.finish().is_err() {
            return Outcome::Status(Status::BadMessage);
        }
        let size = path.0.len()
            + argv.iter().map(|a| a.0.len() + 1).sum::<usize>()
            + env.iter().map(|e| e.0.len() + 1).sum::<usize>();
        if size > REQUEST_MAX
            || path.0.first() != Some(&b'/')
            || !bytes_ok(&path.0)
            || argv.iter().chain(env.iter()).any(|b| !bytes_ok(&b.0))
        {
            return Outcome::Failure(linux::Error::BadRequest.code());
        }
        let mut shared = self.shared.borrow_mut();
        if shared.running + shared.jobs.len() >= TASKS_MAX {
            return Outcome::Failure(linux::Error::Busy.code());
        }
        shared.jobs.push_back(Job {
            pending: cx.defer(),
            path: path.0,
            argv: argv.into_iter().map(|b| b.0).collect(),
            env: env.into_iter().map(|b| b.0).collect(),
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
        for (pid, process) in owned.unwrap_or_default() {
            if let Err(e) = moruna_svc::Process(process).kill() {
                println!(
                    self.shared.borrow().out,
                    "linux: FAIL: ending pid {} for its closed connection: {}",
                    pid,
                    e.name()
                );
            }
        }
    }
}

/// (1.3) The seals of the layers its programs are started on: its image filesystem's
/// `layers`, on a connection of its own, which a program's would be (SDD 30 f.12).
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

/// A task's forward: its calls, served as they arrive.
struct Calls {
    channel: Channel,
    task: Rc<RefCell<Task>>,
}

impl Watch for Calls {
    fn handle(&self) -> &Handle {
        &self.channel.0
    }

    fn signals(&self) -> Signals {
        Signals::READABLE | Signals::PEER_CLOSED
    }

    fn signaled(&mut self, seen: Signals, _done: &mut Completions) -> bool {
        let mut task = self.task.borrow_mut();
        task.serve(&self.channel);
        task.wake();
        !seen.contains(Signals::PEER_CLOSED) || task.readable(&self.channel)
    }
}

/// One of a plane task's slots (SDD 30 f.10): while a call of the task waits on it for a
/// batch or a window, the server waits for its `SIGNALED` and answers the call then.
struct PlaneReady {
    slot: Handle,
    output: bool,
    task: Rc<RefCell<Task>>,
}

impl Watch for PlaneReady {
    fn handle(&self) -> &Handle {
        &self.slot
    }

    fn signals(&self) -> Signals {
        if self.task.borrow().waits_on_plane(self.output) {
            Signals::SIGNALED | Signals::PEER_CLOSED
        } else {
            Signals::PEER_CLOSED
        }
    }

    fn signaled(&mut self, seen: Signals, _done: &mut Completions) -> bool {
        let mut task = self.task.borrow_mut();
        task.wake();
        if seen.contains(Signals::PEER_CLOSED) {
            // The task has ended: what waited on its slots is answered, and the watch
            // goes.
            task.plane_ended();
            return false;
        }
        true
    }
}

/// A task's process: its end answers the request that started it.
struct End {
    process: Handle,
    task: Rc<RefCell<Task>>,
    shared: Rc<RefCell<Shared>>,
    pending: Option<Pending>,
}

impl Watch for End {
    fn handle(&self) -> &Handle {
        &self.process
    }

    fn signals(&self) -> Signals {
        Signals::TERMINATED
    }

    fn signaled(&mut self, _seen: Signals, done: &mut Completions) -> bool {
        let (ended, pid): (Ended, u64) = {
            let mut task = self.task.borrow_mut();
            (task.ended(), task.pid)
        };
        let mut shared = self.shared.borrow_mut();
        shared.running -= 1;
        if let Some(pending) = self.pending.take() {
            shared.disown(pending.connection(), pid);
            done.complete(pending, Outcome::reply(ended));
        }
        false
    }
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
    let needed = 2 + plane.as_ref().map_or(0, |p| {
        usize::from(p.input.is_some()) + usize::from(p.output.is_some())
    });
    if dispatcher.len() + needed > moruna_svc::limits::WAIT_MAX_ITEMS {
        return Err((pending, linux::Error::Busy));
    }
    let started = Task::start(shared, &path, &argv, &env, plane, grant);
    let (mut task, calls, process) = match started {
        Ok(t) => t,
        Err(e) => return Err((pending, e)),
    };
    let killer = task.process.0.duplicate(moruna_svc::Rights::WRITE);
    let pid = task.pid;
    let plane_slots = core::mem::take(&mut task.plane_slots);
    let task = Rc::new(RefCell::new(task));
    {
        let mut s = shared.borrow_mut();
        s.running += 1;
        s.tasks.retain(|t| t.strong_count() > 0);
        s.tasks.push(Rc::downgrade(&task));
        match killer {
            Ok(h) => s
                .owned
                .entry(pending.connection())
                .or_default()
                .push((pid, h)),
            Err(e) => println!(
                s.out,
                "linux: FAIL: pid {} cannot be ended with its connection: {}",
                pid,
                e.name()
            ),
        }
    }
    let task_ref = task.clone();
    let calls = Calls {
        channel: calls,
        task: task.clone(),
    };
    let end = End {
        process,
        task,
        shared: shared.clone(),
        pending: Some(pending),
    };
    // Room was kept for these when the job was taken (`room`).
    let mut watched =
        dispatcher.watch(Box::new(calls)).is_ok() && dispatcher.watch(Box::new(end)).is_ok();
    for (slot, output) in plane_slots {
        watched &= dispatcher
            .watch(Box::new(PlaneReady {
                slot,
                output,
                task: task_ref.clone(),
            }))
            .is_ok();
    }
    if !watched {
        let out = &shared.borrow().out;
        println!(out, "linux: FAIL: no room to watch a task");
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
        tasks: Vec::new(),
        owned: BTreeMap::new(),
        guardian: moruna_node::Guardian::new(supervisor),
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
            "linux: up: {} listeners, {} CPUs, {} calls in the table",
            listeners,
            s.cpus,
            moruna_linux::table::TABLE.len()
        );
    }
    let mut served = 0u64;
    loop {
        // The turn ends by the first blocked call's deadline, which is answered then.
        let deadline = shared
            .borrow()
            .tasks
            .iter()
            .filter_map(Weak::upgrade)
            .filter_map(|t| t.borrow().next_deadline())
            .min()
            .unwrap_or(DEADLINE_INFINITE);
        if let Err(e) = dispatcher.turn(deadline) {
            println!(
                shared.borrow().out,
                "linux: FAIL: the dispatch loop: {}",
                e.name()
            );
            return 1;
        }
        let live: Vec<Rc<RefCell<Task>>> = shared
            .borrow()
            .tasks
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let now = moruna_svc::now();
        for task in live {
            task.borrow_mut().expire(now);
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
