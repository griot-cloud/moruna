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
//! Start slots: `console`; `root.process` (or the supervisor sends it on
//! `supervisor`); `fs`, a listener of the image filesystem; `listen*`, listeners of
//! `moruna.linux`.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

mod files;
mod load;
mod memory;
mod task;

use alloc::{boxed::Box, collections::VecDeque, rc::Rc, vec::Vec};
use core::cell::RefCell;

use linux_protocol::{Bytes, Ended, REQUEST_MAX, linux};
use moruna_svc::{
    Channel, Completions, Console, Context, DEADLINE_INFINITE, Decoder, Dispatcher, Handle,
    Handler, Outcome, Pending, ProtocolError, ProtocolId, Signals, Status, Watch, Wire, println,
    start,
};

use crate::task::Task;

moruna_svc::main!(run);

/// The most programs running at once: two waited handles each, in one `wait` of 64.
const TASKS_MAX: usize = 24;

/// A request to run a program, waiting for the main loop to start it.
struct Job {
    pending: Pending,
    path: Vec<u8>,
    argv: Vec<Vec<u8>>,
    env: Vec<Vec<u8>>,
}

/// What every task shares.
pub struct Shared {
    pub out: Console,
    pub fs: Channel,
    pub resource: Handle,
    pub cpus: usize,
    jobs: VecDeque<Job>,
    pub running: usize,
}

struct Connection(Rc<RefCell<Shared>>);

fn bytes_ok(b: &[u8]) -> bool {
    !b.contains(&0)
}

impl Handler for Connection {
    fn protocol(&self) -> ProtocolId {
        linux::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        if ordinal != 1 {
            return Outcome::Status(Status::UnknownMethod);
        }
        let (Ok(path), Ok(argv), Ok(env)) = (
            Bytes::decode(d),
            Vec::<Bytes>::decode(d),
            Vec::<Bytes>::decode(d),
        ) else {
            return Outcome::Status(Status::BadMessage);
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
        let mut shared = self.0.borrow_mut();
        if shared.running + shared.jobs.len() >= TASKS_MAX {
            return Outcome::Failure(linux::Error::Busy.code());
        }
        shared.jobs.push_back(Job {
            pending: cx.defer(),
            path: path.0,
            argv: argv.into_iter().map(|b| b.0).collect(),
            env: env.into_iter().map(|b| b.0).collect(),
        });
        Outcome::Deferred
    }
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
        !seen.contains(Signals::PEER_CLOSED) || task.readable(&self.channel)
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
        let ended: Ended = self.task.borrow_mut().ended();
        self.shared.borrow_mut().running -= 1;
        if let Some(pending) = self.pending.take() {
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
    } = job;
    let started = Task::start(shared, &path, &argv, &env);
    let (task, calls, process) = match started {
        Ok(t) => t,
        Err(e) => return Err((pending, e)),
    };
    let task = Rc::new(RefCell::new(task));
    shared.borrow_mut().running += 1;
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
    // Room was kept for both when the job was taken (TASKS_MAX).
    if dispatcher.watch(Box::new(calls)).is_err() || dispatcher.watch(Box::new(end)).is_err() {
        let out = &shared.borrow().out;
        println!(out, "linux: FAIL: no room to watch a task");
    }
    Ok(())
}

fn take_root(out: &Console) -> Option<Handle> {
    if let Some(h) = start::take("root.process") {
        return Some(h);
    }
    let supervisor = Channel(start::take("supervisor")?);
    let mut buf = [0u8; 16];
    let mut handles = [None];
    loop {
        match supervisor.read(&mut buf, &mut handles) {
            Ok(_) => return handles[0].take(),
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
    let Some(resource) = take_root(&out) else {
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
        let factory: moruna_svc::Factory =
            Box::new(move || Box::new(Connection(s.clone())) as Box<dyn Handler>);
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
        if let Err(e) = dispatcher.turn(DEADLINE_INFINITE) {
            println!(
                shared.borrow().out,
                "linux: FAIL: the dispatch loop: {}",
                e.name()
            );
            return 1;
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
