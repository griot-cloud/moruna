//! One Linux program: its process, its connection to the image filesystem, its file
//! descriptors, its signal dispositions, and the calls it makes (SDD 28, f.1 and the
//! table of d.2). Every call the kernel forwards is answered here, with Linux's
//! meaning; one the table does not have is refused with `ENOSYS` and counted.

use alloc::{collections::BTreeMap, format, rc::Rc, string::String, vec, vec::Vec};
use core::cell::RefCell;

use imagefs_protocol::{Bytes as FsBytes, READ_MAX, imagefs, kind, open as fsopen};
use linux_protocol::{Bytes, Ended, OUTPUT_MAX, how, linux};
use moruna_abi::{
    forward::{reply, request},
    read_u32, read_u64, write_u64,
};
use moruna_linux::{
    fd::{DupError, Entry, Fds},
    path,
    refused::Refused,
    table::{self, nr},
    types::{self, SIG_DFL, SIG_IGN, SIGACTION_SIZE, Stat, errno, errno::fail, open},
};
use moruna_svc::{Account, Channel, Handle, Process, Rights, Signals, deadline_in, println};

use crate::{
    Shared,
    events::Blocked,
    files::{self, Device, FsFile, Open},
    load, memory,
    plane::PlaneIo,
};

const SECONDS: u64 = 1_000_000_000;
/// The most descriptors a task holds: `RLIMIT_NOFILE`.
const FILES_MAX: usize = 1024;
/// The most bytes one `read` or `write` moves.
const IO_MAX: u64 = 1 << 20;
/// Signals 1 to 64.
const SIGNALS: usize = 65;

/// What the thread that called does next.
pub enum Answer {
    Value(u64),
    ExitProcess(i64),
    Restart {
        entry: u64,
        stack: u64,
        brk: u64,
    },
    /// Nothing yet: the call blocks, and is answered later (SDD 30 f.4).
    Later,
}

impl From<Result<u64, u64>> for Answer {
    fn from(r: Result<u64, u64>) -> Answer {
        Answer::Value(match r {
            Ok(v) => v,
            Err(e) => fail(e),
        })
    }
}

pub struct Task {
    pub shared: Rc<RefCell<Shared>>,
    pub name: String,
    pub process: Process,
    pub fs: crate::namespace::Namespace,
    pub fds: Fds<Open>,
    pub cwd: Vec<u8>,
    pub exe: Vec<u8>,
    pub pid: u64,
    pub refused: Refused,
    served: u64,
    output: Vec<u8>,
    dropped: u64,
    line: Vec<u8>,
    actions: Vec<[u8; SIGACTION_SIZE]>,
    masks: BTreeMap<u64, u64>,
    altstacks: BTreeMap<u64, [u8; 24]>,
    umask: u64,
    comm: [u8; 16],
    ended: Option<(u8, i64)>,
    /// A data-plane task's streams (SDD 29 f.8).
    pub plane: Option<PlaneIo>,
    /// The account its process is charged to when its starter gave one (`run_granted`,
    /// `run_plane`): what it is told its machine has (SDD 30 f.3, SDD 20 f.18). `None`
    /// for a program `run` starts, which is charged to the server's own account and told
    /// nothing of memory.
    pub account: Option<Account>,
    /// Its granted start page, which its auxiliary vector names (SDD 30 d.2).
    pub start_page: Option<u64>,
    /// Its forward's server end, for answering a blocked call later.
    forward: Channel,
    /// Its threads whose calls are answered later.
    pub blocked: Vec<Blocked>,
    /// The handles to its plane's slots the server watches, until the main loop takes
    /// them for its watches: `(handle, output)`.
    pub plane_slots: Vec<(Handle, bool)>,
}

/// Writes the reply to call `token`, preserving the actual transport refusal.
/// A call answered later sends nothing now.
fn send_reply(channel: &Channel, token: u64, answer: Answer) -> Result<(), moruna_svc::Error> {
    let mut out = [0u8; reply::SIZE];
    write_u64(&mut out, reply::TOKEN, token);
    match answer {
        Answer::Later => return Ok(()),
        Answer::Value(v) => {
            write_u64(&mut out, reply::VALUE, v);
            write_u64(&mut out, reply::ACTION, reply::RETURN);
        }
        Answer::ExitProcess(code) => {
            write_u64(&mut out, reply::VALUE, code as u64);
            write_u64(&mut out, reply::ACTION, reply::EXIT_PROCESS);
        }
        Answer::Restart { entry, stack, brk } => {
            write_u64(&mut out, reply::VALUE, entry);
            write_u64(&mut out, reply::ACTION, reply::RESTART);
            write_u64(&mut out, reply::STACK, stack);
            write_u64(&mut out, reply::BRK, brk);
        }
    }
    loop {
        match channel.write_with(&out, &mut []) {
            Ok(()) => return Ok(()),
            Err(moruna_svc::Error::ShouldWait) => {
                moruna_svc::wait_one(
                    &channel.0,
                    Signals::WRITABLE | Signals::PEER_CLOSED,
                    moruna_svc::DEADLINE_INFINITE,
                )?;
            }
            Err(cause) => return Err(cause),
        }
    }
}

fn process_name(path: &[u8]) -> String {
    let base = path::base_name(path);
    let mut name: String = base
        .iter()
        .take(29)
        .map(|b| match b {
            b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => *b as char,
            b'A'..=b'Z' => b.to_ascii_lowercase() as char,
            _ => '-',
        })
        .collect();
    if name.is_empty() {
        name.push_str("program");
    }
    format!("lx-{name}")
}

impl Task {
    /// Makes the process, loads the program and launches it. Returns the task, the
    /// server's end of its forward, and a handle to wait for its end on.
    pub fn start(
        shared: &Rc<RefCell<Shared>>,
        exe: &[u8],
        argv: &[Vec<u8>],
        env: &[Vec<u8>],
        mut plane: Option<linux_protocol::Plane>,
        grant: Option<linux_protocol::Grant>,
    ) -> Result<(Task, Channel, Handle), linux::Error> {
        let name = process_name(exe);
        let assets = plane.as_mut().and_then(|p| p.assets.take());
        // The account its starter carved for it: the kernel charges its process and
        // everything the process causes to exist to it (SDD 20 f.18).
        let (grant, plane, account) = match (grant, plane) {
            (Some(g), plane) => {
                let linux_protocol::Grant {
                    handles,
                    account,
                    scratch,
                } = g;
                (
                    Some((handles, scratch)),
                    plane.map(Streams::of),
                    Some(Account(account)),
                )
            }
            (None, Some(p)) => {
                let account = Account(p.account);
                let streams = Streams {
                    input: p.input,
                    output: p.output,
                    port: p.port,
                    log: p.log,
                    log_bytes: p.log_bytes,
                };
                (None, Some(streams), Some(account))
            }
            (None, None) => (None, None, None),
        };
        let (mut fs, process) = {
            let s = shared.borrow();
            let channel = moruna_svc::connect(&s.fs).map_err(|e| {
                println!(
                    s.out,
                    "linux: FAIL: connecting to the image filesystem: {e:?}"
                );
                linux::Error::NoProcess
            })?;
            let mut fs =
                imagefs::Client::connect(channel, deadline_in(300 * SECONDS)).map_err(|e| {
                    println!(s.out, "linux: FAIL: hello to the image filesystem: {e:?}");
                    linux::Error::NoProcess
                })?;
            fs.core().set_timeout(300 * SECONDS);
            let made = match &account {
                Some(account) => Process::create_in(&s.resource, &name, &account.0),
                None => Process::create(&s.resource, &name),
            };
            let process = made.map_err(|e| {
                println!(s.out, "linux: {}: process_create: {}", name, e.name());
                linux::Error::NoProcess
            })?;
            let assets = assets
                .map(|channel| {
                    let mut client = imagefs::Client::connect(channel, deadline_in(300 * SECONDS))
                        .map_err(|_| linux::Error::BadGrant)?;
                    client.core().set_timeout(300 * SECONDS);
                    Ok::<_, linux::Error>(client)
                })
                .transpose()?;
            (crate::namespace::Namespace::new(fs, assets), process)
        };
        let (kernel, server) = Channel::create().map_err(|_| linux::Error::NoProcess)?;
        if let Err((e, _)) = process.forward(kernel, &table::forward_table()) {
            println!(
                shared.borrow().out,
                "linux: {}: process_forward: {}",
                name,
                e.name()
            );
            return Err(linux::Error::NoProcess);
        }
        let waiter = process
            .0
            .duplicate(Rights::WAIT)
            .map_err(|_| linux::Error::NoProcess)?;
        let forward = server
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::WAIT)
            .map(Channel)
            .map_err(|_| linux::Error::NoProcess)?;
        let (start_page, scratch) = match grant {
            Some((mut granted, scratch)) => {
                if granted.iter().any(|(name, _)| name.0 == b"filesystem") {
                    return Err(linux::Error::BadGrant);
                }
                let source = fs
                    .export_directory(FsBytes(b"/".to_vec()))
                    .map_err(|_| linux::Error::BadGrant)?;
                granted.push((Bytes(b"filesystem".to_vec()), source.0));
                let names: Vec<String> = granted
                    .iter()
                    .map(|(n, _)| String::from_utf8_lossy(&n.0).into_owned())
                    .collect();
                let mut handles: Vec<(Option<Handle>, &str)> = granted
                    .into_iter()
                    .zip(names.iter())
                    .map(|((_, h), n)| (Some(h), n.as_str()))
                    .collect();
                let page = process.grant(&mut handles).map_err(|e| {
                    println!(
                        shared.borrow().out,
                        "linux: {}: process_grant of {} handles ({:?}): {}",
                        name,
                        names.len(),
                        names,
                        e.name()
                    );
                    linux::Error::BadGrant
                })?;
                (Some(page), scratch)
            }
            None if plane.is_some() => {
                // The exact Account already supplied to Process::create_in.
                // READ is metadata authority; TRANSFER is required for this one
                // startup move. No WRITE, DUPLICATE, caller limit or new issuer.
                let observer = account
                    .as_ref()
                    .ok_or(linux::Error::BadGrant)?
                    .0
                    .duplicate(Rights::READ | Rights::TRANSFER)
                    .map_err(|_| linux::Error::BadGrant)?;
                let source = fs
                    .export_directory(FsBytes(b"/".to_vec()))
                    .map_err(|_| linux::Error::BadGrant)?;
                let mut handles = [(Some(observer), "account"), (Some(source.0), "filesystem")];
                let page = process
                    .grant(&mut handles)
                    .map_err(|_| linux::Error::BadGrant)?;
                (Some(page), None)
            }
            None => (None, None),
        };
        let (plane, plane_slots) = match plane {
            Some(plane) => {
                let (io, slots) = attach(shared, &name, &process, plane)?;
                (Some(io), slots)
            }
            None => (None, Vec::new()),
        };
        let mut fds = Fds::new(FILES_MAX);
        match &plane {
            None => {
                for stream in 0..3 {
                    fds.open(Open::Console(stream), false, 0);
                }
            }
            Some(p) => {
                fds.open(
                    if p.has_input() {
                        Open::Plane(0)
                    } else {
                        Open::Console(0)
                    },
                    false,
                    0,
                );
                fds.open(
                    if p.has_output() {
                        Open::Plane(1)
                    } else {
                        Open::Log(1)
                    },
                    false,
                    0,
                );
                fds.open(Open::Log(2), false, 0);
            }
        }
        let mut task = Task {
            shared: shared.clone(),
            name,
            process,
            fs,
            fds,
            cwd: b"/".to_vec(),
            exe: exe.to_vec(),
            pid: 0,
            refused: Refused::new(),
            served: 0,
            output: Vec::new(),
            dropped: 0,
            line: Vec::new(),
            actions: vec![[0u8; SIGACTION_SIZE]; SIGNALS],
            masks: BTreeMap::new(),
            altstacks: BTreeMap::new(),
            umask: 0o022,
            comm: [0u8; 16],
            ended: None,
            plane,
            account,
            start_page,
            forward,
            blocked: Vec::new(),
            plane_slots,
        };
        if let Some(scratch) = scratch {
            // The scratch directory's pages are charged to the account its starter
            // carved for them, which the kernel holds the directory to (SDD 27 f.7).
            task.fs.scratch_account(scratch).map_err(|e| {
                println!(
                    task.shared.borrow().out,
                    "linux: {}: its scratch directory's account: {e:?}", task.name
                );
                linux::Error::BadGrant
            })?;
        }
        let base = path::base_name(exe);
        let n = base.len().min(15);
        task.comm[..n].copy_from_slice(&base[..n]);
        let image = task.load(exe, argv, env).map_err(|e| {
            println!(
                task.shared.borrow().out,
                "linux: {}: not started: {}",
                task.name,
                load_error(e)
            );
            match e {
                errno::ENOENT | errno::ENOTDIR => linux::Error::NotFound,
                _ => linux::Error::NotExecutable,
            }
        })?;
        let escrow = task
            .shared
            .borrow_mut()
            .guardian
            .publish(&task.process)
            .map_err(|_| linux::Error::NoProcess)?;
        let pid = task
            .process
            .launch(image.entry, image.stack, image.brk)
            .map_err(|e| {
                let _ = task.shared.borrow().guardian.retract(escrow);
                println!(
                    task.shared.borrow().out,
                    "linux: {}: process_launch: {}",
                    task.name,
                    e.name()
                );
                linux::Error::NoProcess
            })?;
        task.pid = pid;
        Ok((task, server, waiter))
    }

    fn out(&self) -> core::cell::Ref<'_, Shared> {
        self.shared.borrow()
    }

    /// Serves a bounded turn so a continuously readable process cannot starve
    /// other forwards. Unread requests retain the channel's level READABLE signal
    /// and are picked up by the dispatcher's next turn.
    pub fn serve(&mut self, channel: &Channel) {
        const REQUESTS_PER_TURN: usize = 16;
        let mut buf = [0u8; request::SIZE];
        for _ in 0..REQUESTS_PER_TURN {
            match channel.read(&mut buf, &mut []) {
                Ok((n, _)) if n == request::SIZE => {}
                Ok((n, _)) => {
                    println!(
                        self.out().out,
                        "linux: {}: FAIL: a request of {} bytes", self.name, n
                    );
                    continue;
                }
                Err(_) => return,
            }
            if read_u32(&buf, request::KIND) != Some(request::CALL) {
                continue;
            }
            let token = read_u64(&buf, request::TOKEN).unwrap_or(0);
            let number = read_u64(&buf, request::NUMBER).unwrap_or(u64::MAX);
            let threads = read_u32(&buf, request::THREADS).unwrap_or(1);
            let mut args = [0u64; 6];
            for (i, a) in args.iter_mut().enumerate() {
                *a = read_u64(&buf, request::ARGS + 8 * i).unwrap_or(0);
            }
            self.served += 1;
            let answer = self.call(token, threads, number, args);
            if send_reply(channel, token, answer).is_err() {
                return;
            }
        }
    }

    /// Answers a call that blocked, on the task's forward.
    pub fn answer_later(&mut self, token: u64, answer: Answer) {
        match send_reply(&self.forward, token, answer) {
            Ok(()) => {}
            Err(moruna_svc::Error::PeerClosed) => {
                // This is the dedicated kernel-owned forward, never a caller's
                // arbitrary channel. Process::thread_ended marks Life::Ended
                // BEFORE terminate drops its sole forward endpoint. Thus this
                // exact refusal proves the recipient ended, even while its
                // memory/plane drain precedes the final TERMINATED signal.
                // Do not claim resource retirement or complete another call.
                println!(
                    self.out().out,
                    "linux: {}: blocked reply discarded: recipient forward closed", self.name
                );
            }
            Err(cause) => {
                println!(
                    self.out().out,
                    "linux: {}: FAIL: a blocked call's answer could not be sent: {:?}",
                    self.name,
                    cause
                );
            }
        }
    }

    /// The answer to the client, when the process has ended; the counts go to the
    /// console.
    pub fn ended(&mut self) -> Ended {
        self.flush_line();
        if let Some(plane) = &mut self.plane {
            plane.finish(&self.name);
            self.dropped += plane.dropped;
        }
        let (how_, code) = self.ended.unwrap_or((how::FAULTED, -1));
        let counts = self.refused.counts();
        let rendered = Refused::render(&counts);
        let what = match how_ {
            how::EXITED => format!("exited with code {code}"),
            how::SIGNALED => format!("ended by signal {code}"),
            _ => String::from("ended without exiting (the kernel's line says why)"),
        };
        println!(
            self.out().out,
            "linux: {} (pid {}) {}; {} calls served; refused: {}",
            self.name,
            self.pid,
            what,
            self.served,
            rendered
        );
        Ended {
            how: how_,
            code,
            output: Bytes(core::mem::take(&mut self.output)),
            dropped: self.dropped,
            refused: counts,
            served: self.served,
        }
    }

    fn refuse(&mut self, number: u64) -> Answer {
        self.refused.count(number);
        Answer::Value(fail(errno::ENOSYS))
    }

    /// Answers call `number` of the thread `token`.
    fn call(&mut self, token: u64, threads: u32, number: u64, a: [u64; 6]) -> Answer {
        let p = &self.process;
        if let Some(stream) = self.plane_fd(a[0]) {
            match number {
                nr::READ if stream == 0 => return self.read_plane(token, a[1], a[2]),
                nr::WRITE if stream == 1 => return self.write_plane(token, a[1], a[2]),
                nr::READV if stream == 0 => return self.readv_plane(token, a[1], a[2]),
                nr::WRITEV if stream == 1 => return self.writev_plane(token, a[1], a[2]),
                nr::READ | nr::WRITE | nr::READV | nr::WRITEV => {
                    return Answer::Value(fail(errno::EBADF));
                }
                _ => {}
            }
        }
        match number {
            nr::READ => match self.eventfd_of(a[0]) {
                Some(blocks) => self.read_eventfd(token, a[0], a[1], a[2], blocks),
                None => self.read(a[0], a[1], a[2], None).into(),
            },
            nr::PREAD64 => self.read(a[0], a[1], a[2], Some(a[3])).into(),
            nr::WRITE => match self.eventfd_of(a[0]) {
                Some(blocks) => self.write_eventfd(token, a[0], a[1], a[2], blocks),
                None => self.write(a[0], a[1], a[2], None).into(),
            },
            nr::PWRITE64 => self.write(a[0], a[1], a[2], Some(a[3])).into(),
            nr::READV => self.vector(a[0], a[1], a[2], false).into(),
            nr::WRITEV => self.vector(a[0], a[1], a[2], true).into(),
            nr::OPEN => self.open(at_fdcwd(), a[0], a[1], a[2]).into(),
            nr::OPENAT => self.open(a[0], a[1], a[2], a[3]).into(),
            nr::CLOSE => self.close(a[0]).into(),
            nr::CLOSE_RANGE => {
                let closed = self.fds.close_range(a[0], a[1]);
                for e in closed {
                    self.release(e);
                }
                Answer::Value(0)
            }
            nr::STAT => self.stat_at(at_fdcwd(), a[0], a[1], 0).into(),
            nr::LSTAT => self
                .stat_at(at_fdcwd(), a[0], a[1], types::at::AT_SYMLINK_NOFOLLOW)
                .into(),
            nr::NEWFSTATAT => self.stat_at(a[0], a[1], a[2], a[3]).into(),
            nr::FSTAT => self.fstat(a[0], a[1]).into(),
            nr::POLL => self.poll(a[0], a[1]).into(),
            nr::LSEEK => self.lseek(a[0], a[1], a[2]).into(),
            nr::GETDENTS64 => self.getdents(a[0], a[1], a[2]).into(),
            nr::READLINK => self.readlink(at_fdcwd(), a[0], a[1], a[2]).into(),
            nr::READLINKAT => self.readlink(a[0], a[1], a[2], a[3]).into(),
            nr::ACCESS => self.access(at_fdcwd(), a[0], a[1]).into(),
            nr::FACCESSAT | nr::FACCESSAT2 => self.access(a[0], a[1], a[2]).into(),
            nr::GETCWD => self.getcwd(a[0], a[1]).into(),
            nr::CHDIR => self.chdir(a[0]).into(),
            nr::MKDIR => self.mkdir(at_fdcwd(), a[0], a[1]).into(),
            nr::MKDIRAT => self.mkdir(a[0], a[1], a[2]).into(),
            nr::UNLINK => self.unlink(at_fdcwd(), a[0], 0).into(),
            nr::UNLINKAT => self.unlink(a[0], a[1], a[2]).into(),
            nr::RMDIR => self
                .unlink(at_fdcwd(), a[0], types::at::AT_REMOVEDIR)
                .into(),
            nr::FTRUNCATE => self.ftruncate(a[0], a[1]).into(),
            nr::FSYNC | nr::FDATASYNC => self.with_fd(a[0], |_| Ok(0)).into(),
            nr::FADVISE64 => self.with_fd(a[0], |_| Ok(0)).into(),
            nr::DUP => self.dup(a[0], 0, false).into(),
            nr::DUP2 => {
                if a[0] == a[1] {
                    return self.with_fd(a[0], |_| Ok(a[1])).into();
                }
                self.dup_to(a[0], a[1], false).into()
            }
            nr::DUP3 => {
                if a[0] == a[1] || a[2] & !open::O_CLOEXEC != 0 {
                    return Answer::Value(fail(errno::EINVAL));
                }
                self.dup_to(a[0], a[1], a[2] & open::O_CLOEXEC != 0).into()
            }
            nr::FCNTL => self.fcntl(a[0], a[1], a[2]),
            nr::RENAME => self.rename(at_fdcwd(), a[0], at_fdcwd(), a[1], 0).into(),
            nr::RENAMEAT => self.rename(a[0], a[1], a[2], a[3], 0).into(),
            nr::RENAMEAT2 => self.rename(a[0], a[1], a[2], a[3], a[4]).into(),
            nr::SYSINFO => self.sysinfo(a[0]).into(),
            nr::EVENTFD => self.eventfd(a[0], 0).into(),
            nr::EVENTFD2 => self.eventfd(a[0], a[1]).into(),
            nr::EPOLL_CREATE => {
                if a[0] as i32 <= 0 {
                    return Answer::Value(fail(errno::EINVAL));
                }
                self.epoll_create(0).into()
            }
            nr::EPOLL_CREATE1 => self.epoll_create(a[0]).into(),
            nr::EPOLL_CTL => self.epoll_ctl(a[0], a[1], a[2], a[3]).into(),
            nr::EPOLL_WAIT | nr::EPOLL_PWAIT => {
                let deadline = moruna_linux::events::deadline_ms(a[3] as i32, moruna_svc::now());
                self.epoll_wait(token, a[0], a[1], a[2], deadline)
            }
            nr::EPOLL_PWAIT2 => match self.timespec_deadline(a[3]) {
                Ok(deadline) => self.epoll_wait(token, a[0], a[1], a[2], deadline),
                Err(e) => Answer::Value(fail(e)),
            },
            nr::IOCTL => self.ioctl(a[0], a[1], a[2]).into(),
            nr::MMAP => self.mmap(a).into(),
            nr::UNAME => {
                let u = types::utsname([
                    b"Linux",
                    b"moruna",
                    b"6.12.0-moruna",
                    b"#1 SMP Moruna microkernel",
                    b"x86_64",
                    b"(none)",
                ]);
                memory::write(p, a[0], &u).map(|_| 0).into()
            }
            nr::GETPID | nr::GETPGRP => Answer::Value(self.pid),
            nr::GETPPID => Answer::Value(0),
            nr::GETUID | nr::GETGID | nr::GETEUID | nr::GETEGID => Answer::Value(0),
            nr::GETGROUPS => Answer::Value(0),
            nr::GETRESUID | nr::GETRESGID => {
                let r = [a[0], a[1], a[2]]
                    .iter()
                    .try_for_each(|addr| memory::write_u32(p, *addr, 0));
                r.map(|_| 0).into()
            }
            nr::UMASK => {
                let old = self.umask;
                self.umask = a[0] & 0o777;
                Answer::Value(old)
            }
            nr::GETRLIMIT => self.rlimit(a[0], 0, a[1]).into(),
            nr::PRLIMIT64 => {
                if a[0] != 0 && a[0] != self.pid {
                    return Answer::Value(fail(errno::ESRCH));
                }
                self.rlimit(a[1], a[2], a[3]).into()
            }
            nr::SETRLIMIT => match types::limit_of(a[0]) {
                Some(_) => Answer::Value(0),
                None => Answer::Value(fail(errno::EINVAL)),
            },
            nr::SCHED_GETAFFINITY => self.affinity(a[0], a[1], a[2]).into(),
            nr::SCHED_SETAFFINITY => Answer::Value(0),
            nr::RT_SIGACTION => self.sigaction(a[0], a[1], a[2], a[3]).into(),
            nr::RT_SIGPROCMASK => self.sigprocmask(token, a[0], a[1], a[2], a[3]).into(),
            nr::SIGALTSTACK => self.sigaltstack(token, a[0], a[1]).into(),
            nr::KILL => self.kill(a[0], a[1]),
            nr::TGKILL => {
                if a[0] != self.pid {
                    return Answer::Value(fail(errno::ESRCH));
                }
                self.signal(a[2])
            }
            nr::TKILL => self.signal(a[1]),
            nr::PRCTL => self.prctl(a[0], a[1]),
            nr::ARCH_PRCTL => {
                // The kernel took the FS base; any other code is refused as Linux
                // refuses an unknown one, and counted.
                self.refused.count(number);
                Answer::Value(fail(errno::EINVAL))
            }
            nr::FUTEX => self.refuse(number),
            nr::WAIT4 | nr::WAITID => Answer::Value(fail(errno::ECHILD)),
            nr::CLONE => self.clone(token, a),
            nr::EXECVE => self.execve(threads, a[0], a[1], a[2]),
            nr::EXIT_GROUP => {
                let code = a[0] as i32 & 0xff;
                self.ended = Some((how::EXITED, code as i64));
                Answer::ExitProcess(code as i64)
            }
            _ => self.refuse(number),
        }
    }

    fn with_fd(
        &mut self,
        fd: u64,
        f: impl FnOnce(&mut Open) -> Result<u64, u64>,
    ) -> Result<u64, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let mut file = entry.file.borrow_mut();
        f(&mut file)
    }

    /// Drops a descriptor's entry: the last one to an image filesystem file closes it.
    fn release(&mut self, entry: Entry<Open>) {
        if Rc::strong_count(&entry.file) == 1
            && let Open::Fs(f) = &*entry.file.borrow()
        {
            let _ = self.fs.close(f.number);
        }
    }

    fn flush_line(&mut self) {
        if !self.line.is_empty() {
            let text = String::from_utf8_lossy(&self.line).into_owned();
            println!(self.out().out, "linux {}: {}", self.name, text);
            self.line.clear();
        }
    }

    fn console_write(&mut self, data: &[u8]) {
        let room = OUTPUT_MAX.saturating_sub(self.output.len());
        let kept = room.min(data.len());
        self.output.extend_from_slice(&data[..kept]);
        self.dropped += (data.len() - kept) as u64;
        for b in data {
            if *b == b'\n' {
                self.flush_line();
            } else if self.line.len() < 400 {
                self.line.push(*b);
            }
        }
    }

    fn read(&mut self, fd: u64, buf: u64, count: u64, at: Option<u64>) -> Result<u64, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let mut file = entry.file.borrow_mut();
        let count = count.min(IO_MAX);
        match &mut *file {
            Open::Console(0) => Ok(0),
            Open::Console(_) | Open::Log(_) => Err(errno::EBADF),
            // A plane descriptor's `read`, `readv`, `write` and `writev` are `call`'s, which
            // may answer them later (SDD 30 f.10); what reaches here is `pread`, and a
            // stream has no offsets.
            Open::Plane(_) => Err(errno::ESPIPE),
            Open::Device(Device::Null) => Ok(0),
            Open::Device(Device::Zero) => {
                memory::write(&self.process, buf, &vec![0u8; count as usize])?;
                Ok(count)
            }
            Open::Device(Device::Random) => {
                let mut bytes = vec![0u8; count as usize];
                moruna_sys::random(&mut bytes).map_err(|_| errno::EIO)?;
                memory::write(&self.process, buf, &bytes)?;
                Ok(count)
            }
            Open::Text { bytes, offset } => {
                let start = at.unwrap_or(*offset).min(bytes.len() as u64) as usize;
                let end = (start + count as usize).min(bytes.len());
                memory::write(&self.process, buf, &bytes[start..end])?;
                if at.is_none() {
                    *offset = end as u64;
                }
                Ok((end - start) as u64)
            }
            // An eventfd reached here by `pread` (Linux's `ESPIPE`) or `readv`, which
            // takes it without blocking: a `readv` of an empty one is `EAGAIN` though
            // opened blocking, counted as taken in part.
            Open::EventFd { counter, .. } => {
                if at.is_some() {
                    return Err(errno::ESPIPE);
                }
                match counter.read(count)? {
                    moruna_linux::events::Step::Done(v) => {
                        drop(file);
                        memory::write(&self.process, buf, &v.to_le_bytes())?;
                        Ok(8)
                    }
                    moruna_linux::events::Step::WouldBlock => {
                        drop(file);
                        self.refused.count(nr::READV);
                        Err(errno::EAGAIN)
                    }
                }
            }
            Open::Epoll(_) => Err(errno::EINVAL),
            Open::Fs(f) => {
                if f.is_dir() {
                    return Err(errno::EISDIR);
                }
                if f.flags & open::O_ACCMODE == open::O_WRONLY {
                    return Err(errno::EBADF);
                }
                let start = at.unwrap_or(f.offset);
                let mut done = 0u64;
                while done < count {
                    let want = (count - done).min(READ_MAX as u64) as u32;
                    let FsBytes(chunk) = self
                        .fs
                        .read(f.number, start + done, want)
                        .map_err(files::errno_of)?;
                    if chunk.is_empty() {
                        break;
                    }
                    memory::write(&self.process, buf + done, &chunk)?;
                    done += chunk.len() as u64;
                    if (chunk.len() as u32) < want {
                        break;
                    }
                }
                if at.is_none() {
                    f.offset += done;
                }
                Ok(done)
            }
        }
    }

    fn write(&mut self, fd: u64, buf: u64, count: u64, at: Option<u64>) -> Result<u64, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let mut file = entry.file.borrow_mut();
        let count = count.min(IO_MAX);
        match &mut *file {
            Open::Console(0) => Err(errno::EBADF),
            Open::Console(_) => {
                let data = memory::read(&self.process, buf, count as usize)?;
                drop(file);
                self.console_write(&data);
                Ok(count)
            }
            // `pwrite` of a plane descriptor (the rest is `call`'s).
            Open::Plane(_) => Err(errno::ESPIPE),
            Open::Log(fd) => {
                let fd = *fd as usize;
                let data = memory::read(&self.process, buf, count as usize)?;
                drop(file);
                let plane = self.plane.as_mut().ok_or(errno::EBADF)?;
                plane.log(&self.name, fd, &data);
                Ok(count)
            }
            Open::Device(_) => Ok(count),
            Open::Text { .. } => Err(errno::EBADF),
            // `pwrite` (Linux's `ESPIPE`) or `writev` of an eventfd: without blocking.
            Open::EventFd { counter, .. } => {
                if at.is_some() {
                    return Err(errno::ESPIPE);
                }
                let data = memory::read(&self.process, buf, 8.min(count as usize))?;
                if data.len() < 8 {
                    return Err(errno::EINVAL);
                }
                let mut w = [0u8; 8];
                w.copy_from_slice(&data);
                match counter.write(count, u64::from_le_bytes(w))? {
                    moruna_linux::events::Step::Done(()) => Ok(8),
                    moruna_linux::events::Step::WouldBlock => {
                        drop(file);
                        self.refused.count(nr::WRITEV);
                        Err(errno::EAGAIN)
                    }
                }
            }
            Open::Epoll(_) => Err(errno::EINVAL),
            Open::Fs(f) => {
                if f.is_dir() {
                    return Err(errno::EISDIR);
                }
                if f.flags & open::O_ACCMODE == open::O_RDONLY {
                    return Err(errno::EBADF);
                }
                let mut start = at.unwrap_or(f.offset);
                if at.is_none() && f.flags & open::O_APPEND != 0 {
                    start = self.fs.fstat(f.number).map_err(files::errno_of)?.size;
                }
                let data = memory::read(&self.process, buf, count as usize)?;
                let mut done = 0usize;
                for chunk in data.chunks(READ_MAX as usize) {
                    let n = self
                        .fs
                        .write(f.number, start + done as u64, FsBytes(chunk.to_vec()))
                        .map_err(files::errno_of)?;
                    done += n as usize;
                    if (n as usize) < chunk.len() {
                        break;
                    }
                }
                if at.is_none() {
                    f.offset = start + done as u64;
                }
                Ok(done as u64)
            }
        }
    }

    /// `poll`: every descriptor this personality has is ready for what it is opened for
    /// (a read or a write of one blocks inside the call, never before it), so each entry
    /// asking for input or output is answered at once; a closed one is `POLLNVAL`. A
    /// call asking for nothing answers 0 at once, whatever its timeout: no descriptor
    /// here becomes ready later. Rust's runtime asks this of 0, 1 and 2 when it starts.
    fn poll(&mut self, fds: u64, count: u64) -> Result<u64, u64> {
        const POLLIN: u16 = 0x1;
        const POLLOUT: u16 = 0x4;
        const POLLNVAL: u16 = 0x20;
        const POLLRDNORM: u16 = 0x40;
        const POLLWRNORM: u16 = 0x100;
        if count > FILES_MAX as u64 {
            return Err(errno::EINVAL);
        }
        let mut table = memory::read(&self.process, fds, count as usize * 8)?;
        let mut ready = 0u64;
        for entry in table.chunks_mut(8) {
            let fd = i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
            let events = u16::from_le_bytes([entry[4], entry[5]]);
            let revents = if fd < 0 {
                0
            } else if self.fds.get(fd as u64).is_none() {
                POLLNVAL
            } else {
                events & (POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM)
            };
            entry[6..8].copy_from_slice(&revents.to_le_bytes());
            if revents != 0 {
                ready += 1;
            }
        }
        memory::write(&self.process, fds, &table)?;
        Ok(ready)
    }

    fn vector(&mut self, fd: u64, iov: u64, count: u64, write: bool) -> Result<u64, u64> {
        if count > 1024 {
            return Err(errno::EINVAL);
        }
        let table = memory::read(&self.process, iov, count as usize * 16)?;
        let mut total = 0u64;
        for i in 0..count as usize {
            let (base, len) = types::iovec(&table[i * 16..i * 16 + 16]).ok_or(errno::EFAULT)?;
            if len == 0 {
                continue;
            }
            let n = if write {
                self.write(fd, base, len, None)
            } else {
                self.read(fd, base, len, None)
            };
            match n {
                Ok(n) => {
                    total += n;
                    if n < len {
                        break;
                    }
                }
                Err(e) if total == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(total)
    }

    /// The absolute path `path` (in the task's memory) names from directory `dirfd`.
    pub(crate) fn resolve(&self, dirfd: u64, path: u64) -> Result<Vec<u8>, u64> {
        let p = memory::string(&self.process, path, path::PATH_MAX)?;
        self.resolve_bytes(dirfd, &p)
    }

    fn resolve_bytes(&self, dirfd: u64, p: &[u8]) -> Result<Vec<u8>, u64> {
        let base = if p.first() == Some(&b'/') || dirfd == at_fdcwd() {
            self.cwd.clone()
        } else {
            let entry = self.fds.get(dirfd).ok_or(errno::EBADF)?;
            match &*entry.file.borrow() {
                Open::Fs(f) if f.is_dir() => f.path.clone(),
                _ => return Err(errno::ENOTDIR),
            }
        };
        path::join(&base, p).map_err(|e| match e {
            path::PathError::Empty => errno::ENOENT,
            path::PathError::TooLong => errno::ENAMETOOLONG,
        })
    }

    fn open(&mut self, dirfd: u64, path: u64, flags: u64, mode: u64) -> Result<u64, u64> {
        let full = self.resolve(dirfd, path)?;
        self.open_path(&full, flags, mode)
    }

    pub fn open_path(&mut self, full: &[u8], flags: u64, mode: u64) -> Result<u64, u64> {
        if flags & !open::KNOWN != 0 {
            self.refused.count(nr::OPENAT);
            return Err(errno::EOPNOTSUPP);
        }
        let cloexec = flags & open::O_CLOEXEC != 0;
        if let Some(d) = files::device(full) {
            return self
                .fds
                .open(Open::Device(d), cloexec, 0)
                .map(|n| n as u64)
                .ok_or(errno::EMFILE);
        }
        if let Some(text) = self.text_file(full)? {
            if flags & open::O_ACCMODE != open::O_RDONLY {
                return Err(errno::EACCES);
            }
            return self
                .fds
                .open(text, cloexec, 0)
                .map(|n| n as u64)
                .ok_or(errno::EMFILE);
        }
        let mut fs_flags = 0u32;
        let access = flags & open::O_ACCMODE;
        if access == open::O_WRONLY || access == open::O_RDWR {
            fs_flags |= fsopen::WRITE;
        }
        if flags & open::O_CREAT != 0 {
            fs_flags |= fsopen::CREATE;
        }
        if flags & open::O_EXCL != 0 {
            fs_flags |= fsopen::EXCLUSIVE;
        }
        if flags & open::O_TRUNC != 0 && access != open::O_RDONLY {
            fs_flags |= fsopen::TRUNCATE;
        }
        if flags & open::O_DIRECTORY != 0 {
            fs_flags |= fsopen::DIRECTORY;
        }
        if flags & open::O_NOFOLLOW != 0 {
            fs_flags |= fsopen::NOFOLLOW;
        }
        let perm = (mode as u32) & 0o7777 & !(self.umask as u32);
        let (number, stat) = self
            .fs
            .open(FsBytes(full.to_vec()), fs_flags, perm)
            .map_err(files::errno_of)?;
        if stat.kind == kind::DIRECTORY && access != open::O_RDONLY {
            let _ = self.fs.close(number);
            return Err(errno::EISDIR);
        }
        let file = FsFile {
            number,
            path: full.to_vec(),
            stat,
            offset: 0,
            flags: flags & (open::O_ACCMODE | open::O_APPEND | open::O_NONBLOCK),
            listing: None,
        };
        match self.fds.open(Open::Fs(file), cloexec, 0) {
            Some(n) => Ok(n as u64),
            None => {
                let _ = self.fs.close(number);
                Err(errno::EMFILE)
            }
        }
    }

    fn close(&mut self, fd: u64) -> Result<u64, u64> {
        let entry = self.fds.close(fd).ok_or(errno::EBADF)?;
        self.release(entry);
        Ok(0)
    }

    fn put_stat(&self, buf: u64, s: Stat) -> Result<u64, u64> {
        memory::write(&self.process, buf, &s.encode())?;
        Ok(0)
    }

    fn stat_of_fd(&mut self, fd: u64) -> Result<Stat, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let file = entry.file.borrow();
        Ok(match &*file {
            Open::Console(s) | Open::Plane(s) | Open::Log(s) => files::console_stat(*s),
            Open::Device(d) => files::device_stat(*d),
            Open::Text { bytes, .. } => files::text_stat(bytes.len() as u64),
            Open::EventFd { .. } | Open::Epoll(_) => files::anon_stat(),
            Open::Fs(f) => {
                // A scratch file's size changes as it is written.
                let s = self.fs.fstat(f.number).map_err(files::errno_of)?;
                files::stat_of(&s)
            }
        })
    }

    fn fstat(&mut self, fd: u64, buf: u64) -> Result<u64, u64> {
        let s = self.stat_of_fd(fd)?;
        self.put_stat(buf, s)
    }

    fn stat_at(&mut self, dirfd: u64, path: u64, buf: u64, flags: u64) -> Result<u64, u64> {
        let p = memory::string(&self.process, path, path::PATH_MAX)?;
        if p.is_empty() && flags & types::at::AT_EMPTY_PATH != 0 {
            if dirfd == at_fdcwd() {
                let full = self.cwd.clone();
                return self.stat_path(&full, true, buf);
            }
            return self.fstat(dirfd, buf);
        }
        let full = self.resolve_bytes(dirfd, &p)?;
        self.stat_path(&full, flags & types::at::AT_SYMLINK_NOFOLLOW == 0, buf)
    }

    fn stat_path(&mut self, full: &[u8], follow: bool, buf: u64) -> Result<u64, u64> {
        if let Some(d) = files::device(full) {
            return self.put_stat(buf, files::device_stat(d));
        }
        if let Some(Open::Text { bytes, .. }) = self.text_file(full)? {
            return self.put_stat(buf, files::text_stat(bytes.len() as u64));
        }
        let s = self
            .fs
            .stat(FsBytes(full.to_vec()), follow)
            .map_err(files::errno_of)?;
        self.put_stat(buf, files::stat_of(&s))
    }

    fn lseek(&mut self, fd: u64, offset: u64, whence: u64) -> Result<u64, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let mut file = entry.file.borrow_mut();
        let Open::Fs(f) = &mut *file else {
            return Err(errno::ESPIPE);
        };
        let base = match whence {
            0 => 0i64,
            1 => f.offset as i64,
            2 => self.fs.fstat(f.number).map_err(files::errno_of)?.size as i64,
            _ => return Err(errno::EINVAL),
        };
        let to = base.checked_add(offset as i64).ok_or(errno::EINVAL)?;
        if to < 0 {
            return Err(errno::EINVAL);
        }
        if f.is_dir() {
            // A directory's offset is its position in the listing.
            if to != 0 && whence != 0 {
                return Err(errno::EINVAL);
            }
            f.listing = None;
        }
        f.offset = to as u64;
        Ok(to as u64)
    }

    fn getdents(&mut self, fd: u64, dirp: u64, count: u64) -> Result<u64, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let mut file = entry.file.borrow_mut();
        let Open::Fs(f) = &mut *file else {
            return Err(errno::ENOTDIR);
        };
        if !f.is_dir() {
            return Err(errno::ENOTDIR);
        }
        if f.listing.is_none() {
            let mut all = vec![
                (FsBytes(b".".to_vec()), kind::DIRECTORY, f.stat.ino),
                (FsBytes(b"..".to_vec()), kind::DIRECTORY, f.stat.ino),
            ];
            let mut cookie = 0u32;
            loop {
                let (batch, next) = self.fs.readdir(f.number, cookie).map_err(files::errno_of)?;
                if batch.is_empty() {
                    break;
                }
                all.extend(batch);
                cookie = next;
            }
            f.listing = Some(all);
        }
        let listing = f.listing.as_ref().ok_or(errno::EIO)?;
        let mut out = Vec::new();
        let mut at_ = f.offset as usize;
        while let Some((name, k, ino)) = listing.get(at_) {
            let record = types::dirent64(*ino, at_ as u64 + 1, files::dtype_of(*k), &name.0);
            if (out.len() + record.len()) as u64 > count {
                if out.is_empty() {
                    return Err(errno::EINVAL);
                }
                break;
            }
            out.extend_from_slice(&record);
            at_ += 1;
        }
        memory::write(&self.process, dirp, &out)?;
        f.offset = at_ as u64;
        Ok(out.len() as u64)
    }

    fn readlink(&mut self, dirfd: u64, path: u64, buf: u64, size: u64) -> Result<u64, u64> {
        if size == 0 || (size as i64) < 0 {
            return Err(errno::EINVAL);
        }
        let full = self.resolve(dirfd, path)?;
        let target = if full == b"/proc/self/exe" {
            self.exe.clone()
        } else {
            self.fs
                .readlink(FsBytes(full))
                .map_err(|e| match files::errno_of(e) {
                    // A path that is not a link: Linux says so with EINVAL.
                    errno::EOPNOTSUPP => errno::EINVAL,
                    other => other,
                })?
                .0
        };
        let n = target.len().min(size as usize);
        memory::write(&self.process, buf, &target[..n])?;
        Ok(n as u64)
    }

    fn access(&mut self, dirfd: u64, path: u64, mode: u64) -> Result<u64, u64> {
        let full = self.resolve(dirfd, path)?;
        if files::device(&full).is_some() {
            return Ok(0);
        }
        let s = self
            .fs
            .stat(FsBytes(full.clone()), true)
            .map_err(files::errno_of)?;
        // W_OK: only the scratch directory is writable. X_OK: an execute bit.
        if mode & 2 != 0 && !(full.starts_with(b"/tmp/") || full == b"/tmp") {
            return Err(errno::EROFS);
        }
        if mode & 1 != 0 && s.kind != kind::DIRECTORY && s.mode & 0o111 == 0 {
            return Err(errno::EACCES);
        }
        Ok(0)
    }

    fn getcwd(&mut self, buf: u64, size: u64) -> Result<u64, u64> {
        let mut cwd = self.cwd.clone();
        cwd.push(0);
        if (size as usize) < cwd.len() {
            return Err(errno::ERANGE);
        }
        memory::write(&self.process, buf, &cwd)?;
        Ok(cwd.len() as u64)
    }

    fn chdir(&mut self, path: u64) -> Result<u64, u64> {
        let full = self.resolve(at_fdcwd(), path)?;
        let s = self
            .fs
            .stat(FsBytes(full.clone()), true)
            .map_err(files::errno_of)?;
        if s.kind != kind::DIRECTORY {
            return Err(errno::ENOTDIR);
        }
        self.cwd = full;
        Ok(0)
    }

    fn mkdir(&mut self, dirfd: u64, path: u64, mode: u64) -> Result<u64, u64> {
        let full = self.resolve(dirfd, path)?;
        let perm = (mode as u32) & 0o7777 & !(self.umask as u32);
        self.fs
            .mkdir(FsBytes(full), perm)
            .map_err(files::errno_of)?;
        Ok(0)
    }

    fn unlink(&mut self, dirfd: u64, path: u64, flags: u64) -> Result<u64, u64> {
        let full = self.resolve(dirfd, path)?;
        let s = self
            .fs
            .stat(FsBytes(full.clone()), false)
            .map_err(files::errno_of)?;
        let dir = s.kind == kind::DIRECTORY;
        if flags & types::at::AT_REMOVEDIR != 0 && !dir {
            return Err(errno::ENOTDIR);
        }
        if flags & types::at::AT_REMOVEDIR == 0 && dir {
            return Err(errno::EISDIR);
        }
        self.fs.remove(FsBytes(full)).map_err(files::errno_of)?;
        Ok(0)
    }

    fn ftruncate(&mut self, fd: u64, len: u64) -> Result<u64, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let file = entry.file.borrow();
        let Open::Fs(f) = &*file else {
            return Err(errno::EINVAL);
        };
        self.fs.truncate(f.number, len).map_err(files::errno_of)?;
        Ok(0)
    }

    fn dup(&mut self, fd: u64, from: u64, cloexec: bool) -> Result<u64, u64> {
        let from = usize::try_from(from).map_err(|_| errno::EINVAL)?;
        if from >= FILES_MAX {
            return Err(errno::EINVAL);
        }
        self.fds
            .dup(fd, from, cloexec)
            .map(|n| n as u64)
            .map_err(|e| match e {
                DupError::BadFd => errno::EBADF,
                DupError::TooMany => errno::EMFILE,
            })
    }

    fn dup_to(&mut self, old: u64, new: u64, cloexec: bool) -> Result<u64, u64> {
        let replaced = self
            .fds
            .dup_to(old, new, cloexec)
            .map_err(|_| errno::EBADF)?;
        if let Some(e) = replaced {
            self.release(e);
        }
        Ok(new)
    }

    fn fcntl(&mut self, fd: u64, cmd: u64, arg: u64) -> Answer {
        const F_DUPFD: u64 = 0;
        const F_GETFD: u64 = 1;
        const F_SETFD: u64 = 2;
        const F_GETFL: u64 = 3;
        const F_SETFL: u64 = 4;
        const F_DUPFD_CLOEXEC: u64 = 1030;
        let Some(entry) = self.fds.get_mut(fd) else {
            return Answer::Value(fail(errno::EBADF));
        };
        let r = match cmd {
            F_DUPFD => return self.dup(fd, arg, false).into(),
            F_DUPFD_CLOEXEC => return self.dup(fd, arg, true).into(),
            F_GETFD => Ok(entry.cloexec as u64),
            F_SETFD => {
                entry.cloexec = arg & 1 != 0;
                Ok(0)
            }
            F_GETFL => Ok(match &*entry.file.borrow() {
                Open::Console(0) | Open::Plane(0) => open::O_RDONLY,
                Open::Console(_) | Open::Plane(_) | Open::Log(_) => open::O_WRONLY,
                Open::Device(_) | Open::Epoll(_) => open::O_RDWR,
                Open::Text { .. } => open::O_RDONLY,
                Open::EventFd { flags, .. } => *flags,
                Open::Fs(f) => f.flags,
            }),
            F_SETFL => {
                match &mut *entry.file.borrow_mut() {
                    Open::Fs(f) => {
                        let settable = open::O_APPEND | open::O_NONBLOCK;
                        f.flags = (f.flags & !settable) | (arg & settable);
                    }
                    Open::EventFd { flags, .. } => {
                        *flags = (*flags & !open::O_NONBLOCK) | (arg & open::O_NONBLOCK);
                    }
                    _ => {}
                }
                Ok(0)
            }
            // Record locks (F_GETLK, F_SETLK, F_SETLKW and the OFD forms): no other
            // process shares a file, so every lock is granted.
            5..=7 | 36..=38 => Ok(0),
            _ => {
                self.refused.count(nr::FCNTL);
                Err(errno::EINVAL)
            }
        };
        r.into()
    }

    fn ioctl(&mut self, fd: u64, request: u64, _arg: u64) -> Result<u64, u64> {
        const FIONCLEX: u64 = 0x5450;
        const FIOCLEX: u64 = 0x5451;
        let entry = self.fds.get_mut(fd).ok_or(errno::EBADF)?;
        match request {
            FIOCLEX => {
                entry.cloexec = true;
                Ok(0)
            }
            FIONCLEX => {
                entry.cloexec = false;
                Ok(0)
            }
            // TCGETS, TIOCGWINSZ and every other: no file here is a terminal.
            _ => Err(errno::ENOTTY),
        }
    }

    fn rlimit(&mut self, resource: u64, new: u64, old: u64) -> Result<u64, u64> {
        let (soft, hard) = types::limit_of(resource).ok_or(errno::EINVAL)?;
        if old != 0 {
            memory::write(&self.process, old, &types::rlimit(soft, hard))?;
        }
        let _ = new;
        Ok(0)
    }

    fn affinity(&mut self, pid: u64, len: u64, mask: u64) -> Result<u64, u64> {
        if pid != 0 && pid != self.pid {
            return Err(errno::ESRCH);
        }
        let cpus = self.shared.borrow().cpus;
        let bytes = cpus.div_ceil(64) * 8;
        if (len as usize) < bytes {
            return Err(errno::EINVAL);
        }
        let mut out = vec![0u8; bytes];
        for cpu in 0..cpus {
            out[cpu / 8] |= 1 << (cpu % 8);
        }
        memory::write(&self.process, mask, &out)?;
        Ok(bytes as u64)
    }

    fn sigaction(&mut self, sig: u64, act: u64, old: u64, size: u64) -> Result<u64, u64> {
        if size != 8 || sig == 0 || sig as usize >= SIGNALS || sig == 9 || sig == 19 {
            return Err(errno::EINVAL);
        }
        if old != 0 {
            memory::write(&self.process, old, &self.actions[sig as usize])?;
        }
        if act != 0 {
            let bytes = memory::read(&self.process, act, SIGACTION_SIZE)?;
            self.actions[sig as usize].copy_from_slice(&bytes);
        }
        Ok(0)
    }

    fn sigprocmask(
        &mut self,
        token: u64,
        how_: u64,
        set: u64,
        old: u64,
        size: u64,
    ) -> Result<u64, u64> {
        if size != 8 {
            return Err(errno::EINVAL);
        }
        let current = self.masks.get(&token).copied().unwrap_or(0);
        if old != 0 {
            memory::write(&self.process, old, &current.to_le_bytes())?;
        }
        if set != 0 {
            let s = memory::read_u64(&self.process, set)?;
            // SIGKILL and SIGSTOP cannot be blocked.
            let s = s & !((1 << 8) | (1 << 18));
            let next = match how_ {
                0 => current | s,
                1 => current & !s,
                2 => s,
                _ => return Err(errno::EINVAL),
            };
            self.masks.insert(token, next);
        }
        Ok(0)
    }

    fn sigaltstack(&mut self, token: u64, new: u64, old: u64) -> Result<u64, u64> {
        // SS_DISABLE until one is set.
        let mut current = [0u8; 24];
        current[8] = 2;
        let current = self.altstacks.get(&token).copied().unwrap_or(current);
        if old != 0 {
            memory::write(&self.process, old, &current)?;
        }
        if new != 0 {
            let b = memory::read(&self.process, new, 24)?;
            let mut s = [0u8; 24];
            s.copy_from_slice(&b);
            self.altstacks.insert(token, s);
        }
        Ok(0)
    }

    fn kill(&mut self, pid: u64, sig: u64) -> Answer {
        if pid != self.pid && pid != 0 && pid as i64 != -(self.pid as i64) {
            return Answer::Value(fail(errno::ESRCH));
        }
        self.signal(sig)
    }

    /// A signal sent to the process itself: nothing for signal 0 and an ignored one;
    /// the process ends for one whose action is the default. A handler is not run:
    /// delivering signals is not in the table, so the process ends, saying so.
    fn signal(&mut self, sig: u64) -> Answer {
        if sig == 0 {
            return Answer::Value(0);
        }
        if sig as usize >= SIGNALS {
            return Answer::Value(fail(errno::EINVAL));
        }
        let handler =
            u64::from_le_bytes(self.actions[sig as usize][..8].try_into().unwrap_or([0; 8]));
        if handler == SIG_IGN || (handler == SIG_DFL && types::ignored_by_default(sig)) {
            return Answer::Value(0);
        }
        if handler != SIG_DFL {
            println!(
                self.out().out,
                "linux: {}: signal {} has a handler; signals are not delivered, so the process ends",
                self.name,
                sig
            );
            self.refused.count(nr::RT_SIGRETURN);
        }
        self.ended = Some((how::SIGNALED, sig as i64));
        Answer::ExitProcess(128 + sig as i64)
    }

    fn prctl(&mut self, option: u64, arg: u64) -> Answer {
        const PR_SET_PDEATHSIG: u64 = 1;
        const PR_GET_DUMPABLE: u64 = 3;
        const PR_SET_DUMPABLE: u64 = 4;
        const PR_SET_NAME: u64 = 15;
        const PR_GET_NAME: u64 = 16;
        const PR_SET_NO_NEW_PRIVS: u64 = 38;
        const PR_GET_NO_NEW_PRIVS: u64 = 39;
        let r = match option {
            PR_SET_PDEATHSIG | PR_SET_DUMPABLE | PR_SET_NO_NEW_PRIVS => Ok(0),
            PR_GET_DUMPABLE | PR_GET_NO_NEW_PRIVS => Ok(1),
            PR_SET_NAME => memory::read(&self.process, arg, 16).map(|b| {
                self.comm.copy_from_slice(&b);
                self.comm[15] = 0;
                0
            }),
            PR_GET_NAME => memory::write(&self.process, arg, &self.comm).map(|_| 0),
            _ => {
                self.refused.count(nr::PRCTL);
                Err(errno::EINVAL)
            }
        };
        r.into()
    }

    /// `clone`: a thread of this process, as a thread library makes one. A new process
    /// (`fork`, `vfork`, a `clone` without `CLONE_VM`) is not in the table: refused.
    fn clone(&mut self, token: u64, a: [u64; 6]) -> Answer {
        const CLONE_VM: u64 = 0x100;
        const CLONE_SIGHAND: u64 = 0x800;
        const CLONE_VFORK: u64 = 0x4000;
        const CLONE_THREAD: u64 = 0x10000;
        const CLONE_SETTLS: u64 = 0x80000;
        const CLONE_PARENT_SETTID: u64 = 0x10_0000;
        const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
        const CLONE_CHILD_SETTID: u64 = 0x100_0000;
        let [flags, stack, ptid, ctid, tls, _] = a;
        let thread = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
        if flags & thread != thread || flags & CLONE_VFORK != 0 {
            return self.refuse(nr::CLONE);
        }
        if stack == 0 {
            return Answer::Value(fail(errno::EINVAL));
        }
        let tls = if flags & CLONE_SETTLS != 0 { tls } else { 0 };
        let clear = if flags & CLONE_CHILD_CLEARTID != 0 {
            ctid
        } else {
            0
        };
        let set = if flags & CLONE_PARENT_SETTID != 0 {
            ptid
        } else {
            0
        };
        match self.process.clone_thread(token, stack, tls, clear, set) {
            Ok(tid) => {
                if flags & CLONE_CHILD_SETTID != 0
                    && memory::write_u32(&self.process, ctid, tid as u32).is_err()
                {
                    return Answer::Value(fail(errno::EFAULT));
                }
                Answer::Value(tid)
            }
            Err(moruna_svc::Error::InvalidArgs) => Answer::Value(fail(errno::EINVAL)),
            Err(_) => Answer::Value(fail(errno::EAGAIN)),
        }
    }

    /// `execve`: the program replaced in place, under the one thread a process may
    /// have when it does it here.
    fn execve(&mut self, threads: u32, path: u64, argv: u64, envp: u64) -> Answer {
        let full = match self.resolve(at_fdcwd(), path) {
            Ok(f) => f,
            Err(e) => return Answer::Value(fail(e)),
        };
        let total = moruna_linux::stack::ARGS_MAX;
        let args = memory::strings(&self.process, argv, 4096, total);
        let env = memory::strings(&self.process, envp, 4096, total);
        let (args, env) = match (args, env) {
            (Ok(a), Ok(e)) => (a, e),
            (Err(e), _) | (_, Err(e)) => return Answer::Value(fail(e)),
        };
        if threads > 1 {
            println!(
                self.out().out,
                "linux: {}: execve with {} threads: only a process of one thread may replace its program here",
                self.name,
                threads
            );
            self.refused.count(nr::EXECVE);
            return Answer::Value(fail(errno::EAGAIN));
        }
        if let Err(e) = self.check_executable(&full) {
            return Answer::Value(fail(e));
        }
        // The point of no return: the old program's memory goes.
        if let Err(e) = self
            .process
            .unmap(load::USER_START, load::USER_END - load::USER_START)
        {
            println!(
                self.out().out,
                "linux: {}: execve: unmapping: {}",
                self.name,
                e.name()
            );
            return Answer::ExitProcess(127);
        }
        for e in self.fds.close_on_exec() {
            self.release(e);
        }
        for action in self.actions.iter_mut() {
            let handler = u64::from_le_bytes(action[..8].try_into().unwrap_or([0; 8]));
            if handler != SIG_IGN {
                *action = [0u8; SIGACTION_SIZE];
            }
        }
        match self.load(&full, &args, &env) {
            Ok(image) => {
                self.exe = full;
                Answer::Restart {
                    entry: image.entry,
                    stack: image.stack,
                    brk: image.brk,
                }
            }
            Err(e) => {
                println!(
                    self.out().out,
                    "linux: {}: execve: {}",
                    self.name,
                    load_error(e)
                );
                self.ended = Some((how::SIGNALED, 11));
                Answer::ExitProcess(128 + 11)
            }
        }
    }

    /// Whether the channel still holds requests.
    pub fn readable(&self, channel: &Channel) -> bool {
        matches!(channel.needed(), Ok(Some(_)))
    }
}

pub fn at_fdcwd() -> u64 {
    types::at::AT_FDCWD
}

fn load_error(e: u64) -> &'static str {
    match e {
        errno::ENOENT => "no such file",
        errno::ENOTDIR => "a part of the path is not a directory",
        errno::EACCES => "not executable",
        errno::ENOEXEC => "not an ELF program this personality loads",
        errno::ENOMEM => "no memory for its mappings",
        errno::EFAULT => "its stack could not be written",
        errno::E2BIG => "its arguments and environment are too large",
        errno::EIO => "the image filesystem failed reading it",
        _ => "it could not be loaded",
    }
}

/// What a `run_plane`'s plane gives the task besides its account: its slots, its port and
/// its log ring.
pub struct Streams {
    input: Option<Handle>,
    output: Option<Handle>,
    port: Option<Channel>,
    log: Handle,
    log_bytes: u64,
}

impl Streams {
    /// A plane's streams, its account closed: for a request that carries its account
    /// elsewhere.
    fn of(p: linux_protocol::Plane) -> Streams {
        Streams {
            input: p.input,
            output: p.output,
            port: p.port,
            log: p.log,
            log_bytes: p.log_bytes,
        }
    }
}

/// Gives the created `process` the plane of a `run_plane` (SDD 29 f.8): its input slot,
/// then its output slot, numbered in that order, and its port; and maps the log ring.
fn attach(
    shared: &Rc<RefCell<Shared>>,
    name: &str,
    process: &Process,
    plane: Streams,
) -> Result<(PlaneIo, Vec<(Handle, bool)>), linux::Error> {
    use moruna_sys::plane::Slot;
    let failed = |what: &str, e: moruna_svc::Error| {
        println!(
            shared.borrow().out,
            "linux: {}: {}: {}",
            name,
            what,
            e.name()
        );
        linux::Error::NoProcess
    };
    let input = plane.input.map(Slot);
    let output = plane.output.map(Slot);
    let mut slots: Vec<&Slot> = Vec::new();
    slots.extend(input.as_ref());
    slots.extend(output.as_ref());
    // The server keeps its handles to the slots, and waits on them while a call waits
    // for a batch or a window (SDD 30 f.10).
    let numbers = (
        input.as_ref().map(|s| (0u32, s.0.raw())),
        output
            .as_ref()
            .map(|s| (u32::from(input.is_some()), s.0.raw())),
    );
    if let Err((e, _)) = moruna_sys::plane::attach(process, &slots, plane.port) {
        return Err(failed("process_plane", e));
    }
    let log = moruna_sys::Memory(plane.log)
        .map_detached(
            0,
            plane.log_bytes,
            moruna_abi::map::READ | moruna_abi::map::WRITE,
        )
        .map_err(|e| failed("mapping the log ring", e))?;
    let mut watched = Vec::new();
    watched.extend(input.map(|s| (s.0, false)));
    watched.extend(output.map(|s| (s.0, true)));
    Ok((PlaneIo::new(numbers.0, numbers.1, log), watched))
}
