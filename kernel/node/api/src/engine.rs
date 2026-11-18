//! The node's engine host (system design 3A, work item 3.2): `api` runs each job, and each
//! check of a document, in a fresh engine that `node-admission` starts, and speaks the host
//! protocol (`moruna-hosted-engine.md` 4.3) with it on a channel.
//!
//! - **The engine's start** is a configuration of the engine's supervisor (`kernel/engine`), as
//!   `kernel/node/tools/configure.py` writes the standalone node's `engine.conf`, less its vsock
//!   and launcher pieces: the program `/moruna/bin/moruna` with the arguments of
//!   [`node_protocol::serve_args`] or [`node_protocol::check_args`], `HOME=/tmp`, the Linux
//!   personality, the channel to `api` handed over as it is, and one run ([`job_config`],
//!   [`check_config`]); and for a job that names hosts, the network channel `node-admission`
//!   mints it for them, handed over as it is, on which the engine opens a session a socket
//!   (system design 3D, item 3.4).
//! - **Memory:** a job's budget is its whole account; the engine's supervisor carves the
//!   program's memory and scratch from it as the engine sizes itself ([`sizes`]), with no
//!   scratch when the check's needs mark a staging volume (item 3.3). A check runs in a
//!   [`CHECK_BYTES`] account with no scratch, and reads the node's volumes from the data
//!   `node-admission` grants it ([`node_protocol::VOLUMES_SLOT`]).
//! - **The channel** carries the host protocol's bytes in messages of at most 64 KiB each, both
//!   ways: `api` writes `check`, `spec` and `cancel` in such pieces, and reads the engine's
//!   bytes, the check's `checked` line among them.
//! - **An engine that has ended** is released at `node-admission` once its process has ended, so
//!   its account is free for the next: a check before the next check starts; a job as soon as its
//!   channel has ended with nothing left to read ([`Host::tend`]), which the server has then
//!   heard its `exit` on, and at the latest before the next job starts.
//! - **Functions** (system design 3E, item 3.6): at each start `api` sends `node-admission` node
//!   config's `functions.cache_bytes` before it asks the job bound, which then counts the cache
//!   ([`Host::set_up`]). Every job's engine is configured `grant functions functions`, and its
//!   launch carries the job's functions as registered, which `node-admission` fetches and checks
//!   before it starts the engine: `api` asks without waiting, and looks for the answer as it
//!   serves, for up to [`LAUNCH_NS`], since a fetch takes as long as it takes within
//!   `node-admission`'s own bound ([`node_protocol::FETCH_NS`] an image). A refusal's sentence,
//!   which names the function that is the cause, is what the job's submit is answered with.

use alloc::{collections::VecDeque, format, rc::Rc, string::String, vec, vec::Vec};
use core::cell::RefCell;

use moruna_api_server::{EngineHost, HostError, Verdict, check_line, verdict};
use moruna_svc::Wire;
use moruna_svc::{CallError, Channel, Handle, Memory, Rights, Signals, limits, map};
use node_protocol::{
    CHECK_BYTES, CHECK_CHANNEL, ENGINE_CHANNEL, Launched, RUNNING, Refusal, Registered,
    VOLUMES_SLOT, admission, check_args, serve_args,
};

/// The engine, a Linux program in the node's image.
pub const PROGRAM: &str = "/moruna/bin/moruna";

/// The engine's own share of a job's account: its supervisor and the Linux personality's process
/// (system design 3A; `moruna-runtime`'s `host::node::ENGINE_OVERHEAD`).
pub const ENGINE_OVERHEAD: u64 = 128 << 20;

/// The services `node-admission` lends each engine: the Linux personality. A job's network is
/// minted for it from the hosts its check named (item 3.4), not lent.
pub const SERVICES: [&str; 1] = ["linux"];

/// The largest piece of the host protocol's stream a channel message carries.
pub const PIECE: usize = limits::CHANNEL_MAX_BYTES;

/// How long a call to `node-admission` may take, and how long an engine whose channel has ended
/// is waited for before it is killed.
pub const CALL_NS: u64 = 30 * 1_000_000_000;

/// How long `api` waits for `node-admission` to answer a job's launch: ten minutes, a backstop
/// (PM, item 3.6), since `node-admission` fetches each of the job's functions within its own
/// bound and answers when they are fetched or refused.
pub const LAUNCH_NS: u64 = 600 * 1_000_000_000;

/// How often `api` looks for a launch's answer while it waits for one.
pub const LAUNCH_POLL_NS: u64 = 50_000_000;

/// What every job's engine supervisor is given beside its channel: the job's functions, which
/// `node-admission` gives it as `function.N` and the `functions` index (item 3.6). A job that
/// uses none is given no index, and the grant hands on nothing.
pub const FUNCTIONS_GRANT: &str = "grant functions functions\n";

/// What a job's engine supervisor is given when the job uses a function: the process resource
/// `node-admission` grants it as `process`, from which the engine starts a native function's
/// program itself (stream P; item 3.6). A job that uses none is given none.
pub const PROCESS_GRANT: &str = "grant process process process\n";

/// What the engine's supervisor carves from a job's account for the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizes {
    /// The program's memory: the arena's share, which `moruna` sizes itself to.
    pub memory: u64,
    /// Its in-memory scratch.
    pub scratch: u64,
}

/// The split of a job's budget of `budget` bytes, as the engine makes it in node mode (system
/// design 3A, `moruna-runtime`'s `host::node::sizes`): [`ENGINE_OVERHEAD`] for the engine's own,
/// scratch a quarter of the rest in whole pages (none when the job stages on a volume,
/// `staging`), and the program's memory what remains.
pub fn sizes(budget: u64, staging: bool) -> Sizes {
    let rest = budget.saturating_sub(ENGINE_OVERHEAD);
    let scratch = if staging { 0 } else { (rest / 4) & !4095 };
    Sizes {
        memory: rest - scratch,
        scratch,
    }
}

/// The engine supervisor's configuration for a job of `budget` bytes on a node whose job bound
/// is `bound`, `staging` when it stages on a volume, `network` when it names hosts (its network,
/// which `node-admission` mints and grants it as `network`), `volumes` volumes, which
/// `node-admission` grants it as `volume.<n>` with their list as `volumes` (item 3.3), each
/// handed to the program as it is given, and `functions` when it uses a registered function
/// (the process resource a native one is started from, item 3.6); refused when the budget
/// leaves the program no memory past the engine's own.
pub fn job_config(
    bound: u64,
    budget: u64,
    staging: bool,
    network: bool,
    volumes: usize,
    functions: bool,
) -> Result<String, HostError> {
    let Sizes { memory, scratch } = sizes(budget, staging);
    if memory == 0 || memory % 4096 != 0 {
        return Err(HostError::Refused(format!(
            "budget.memory_bytes: {budget} bytes leave the engine nothing past its own {} MiB: \
             a job on a node needs more, in whole pages",
            ENGINE_OVERHEAD >> 20
        )));
    }
    let mut more = String::new();
    if network {
        more.push_str("grant network channel network\n");
    }
    for n in 0..volumes {
        let slot = node_protocol::volume_slot(n);
        more.push_str(&format!("grant {slot} channel {slot}\n"));
    }
    if volumes > 0 {
        more.push_str(&format!("grant {VOLUMES_SLOT} data {VOLUMES_SLOT}\n"));
    }
    more.push_str(FUNCTIONS_GRANT);
    if functions {
        more.push_str(PROCESS_GRANT);
    }
    Ok(config(
        &serve_args(bound),
        memory,
        scratch,
        ENGINE_CHANNEL,
        &more,
    ))
}

/// The engine supervisor's configuration for a check on a node whose job bound is `bound`: the
/// [`CHECK_BYTES`] account less the engine's own, no scratch, and the node's volumes, which
/// `node-admission` grants every check, handed to the program as they are given.
pub fn check_config(bound: u64) -> String {
    config(
        &check_args(bound),
        CHECK_BYTES - ENGINE_OVERHEAD,
        0,
        CHECK_CHANNEL,
        &format!("grant {VOLUMES_SLOT} data {VOLUMES_SLOT}\n"),
    )
}

fn config(args: &[String], memory: u64, scratch: u64, channel: &str, more: &str) -> String {
    let mut text = format!("program {PROGRAM}\n");
    for arg in args {
        text.push_str(&format!("arg {arg}\n"));
    }
    text.push_str(&format!(
        "env HOME=/tmp\nmemory {memory}\nscratch {scratch}\ngrant linux personality\n\
         grant {channel} channel {channel}\n{more}runs 1\n"
    ));
    text
}

/// What a refusal of a job's launch means for the submit that asked: its sentence, as the job's
/// refusal for the node's refusals of it (its budget, its volumes, a function that is not what
/// was registered), as the node's failure otherwise.
fn refused(refusal: Refusal) -> HostError {
    match refusal.error() {
        Some(admission::Error::Quota | admission::Error::Invalid | admission::Error::Refused) => {
            HostError::Refused(refusal.message)
        }
        Some(admission::Error::Limit) => HostError::Unavailable,
        _ => HostError::Failed(format!("node-admission: {}", refusal.message)),
    }
}

/// A job's launch asked of `node-admission`, not yet answered: its request, until when it is
/// waited for, and the job's channel and `spec`, for the engine once it starts.
struct Launching {
    txid: u32,
    by: u64,
    channel: Channel,
    spec: Vec<u8>,
}

/// What the node's admission failing means for the call that asked.
fn admission_error(what: &str, error: CallError<admission::Error>) -> HostError {
    match error {
        CallError::Remote(admission::Error::Quota) => HostError::Refused(format!(
            "budget.memory_bytes: more than node-admission has for a job ({what})"
        )),
        CallError::Remote(admission::Error::Limit) => HostError::WouldBlock,
        CallError::Remote(error) => HostError::Failed(format!("node-admission: {what}: {error:?}")),
        error => HostError::Failed(format!("node-admission is unreachable: {what}: {error:?}")),
    }
}

/// One engine's channel: what `api` has still to send it, and what it has sent that `api` has
/// not yet read.
struct Run {
    /// Its id at `node-admission`.
    id: u64,
    channel: Rc<Channel>,
    out: VecDeque<u8>,
    inbox: Vec<u8>,
    read: usize,
    /// The engine's end has closed.
    ended: bool,
}

impl Run {
    fn new(id: u64, channel: Channel, first: Vec<u8>) -> Run {
        Run {
            id,
            channel: Rc::new(channel),
            out: first.into(),
            inbox: Vec::new(),
            read: 0,
            ended: false,
        }
    }

    /// Sends what it can of `out`, in pieces of at most [`PIECE`] bytes. An engine that has
    /// gone takes nothing more.
    fn flush(&mut self) {
        while !self.out.is_empty() && !self.ended {
            let count = self.out.len().min(PIECE);
            let (front, back) = self.out.as_slices();
            let piece: Vec<u8> = front.iter().chain(back).take(count).copied().collect();
            match self.channel.write_with(&piece, &mut []) {
                Ok(()) => {
                    self.out.drain(..count);
                }
                Err(moruna_svc::Error::ShouldWait) => return,
                Err(_) => {
                    self.out.clear();
                    self.ended = true;
                }
            }
        }
    }

    /// Takes the next message the engine has sent, if one has come; whether one did.
    fn fill(&mut self) -> bool {
        if self.ended {
            return false;
        }
        if self.read == self.inbox.len() {
            self.inbox.clear();
            self.read = 0;
        }
        match self.channel.needed() {
            Ok(Some((bytes, handles))) => {
                let at = self.inbox.len();
                self.inbox.resize(at + bytes, 0);
                // A handle has no place in the host protocol: it is closed as it drops.
                let mut carried: Vec<Option<Handle>> = (0..handles).map(|_| None).collect();
                match self.channel.read(&mut self.inbox[at..], &mut carried) {
                    Ok((count, _)) => {
                        self.inbox.truncate(at + count);
                        true
                    }
                    Err(_) => {
                        self.inbox.truncate(at);
                        self.ended = true;
                        false
                    }
                }
            }
            Ok(None) => false,
            Err(_) => {
                self.ended = true;
                false
            }
        }
    }

    /// The first whole line it has sent, its line break included, once one has come.
    fn line(&mut self) -> Option<Vec<u8>> {
        loop {
            let unread = &self.inbox[self.read..];
            if let Some(at) = unread.iter().position(|b| *b == b'\n') {
                let line = unread[..=at].to_vec();
                self.read += at + 1;
                return Some(line);
            }
            if !self.fill() {
                return None;
            }
        }
    }

    /// What `api` waits for on it: the engine's bytes or its end, and room to write while it
    /// has something to send.
    fn signals(&self) -> Signals {
        if self.out.is_empty() {
            Signals::READABLE | Signals::PEER_CLOSED
        } else {
            Signals::READABLE | Signals::PEER_CLOSED | Signals::WRITABLE
        }
    }
}

/// The node's engine host: a connection to `node-admission`, the node's job bound, and the
/// check and the job under way.
pub struct Host {
    admission: admission::Client,
    bound: u64,
    check: Option<Run>,
    job: Option<Run>,
    /// The job's launch, while `node-admission` has not answered it.
    launching: Option<Launching>,
    /// Checks that have answered, to be released before the next starts.
    answered: Vec<u64>,
}

impl Host {
    /// The host over `admission`, a connection to `node-admission`, whose first questions are
    /// [`Host::set_up`]'s.
    pub fn new(mut admission: admission::Client) -> Host {
        admission.core().set_timeout(CALL_NS);
        Host {
            admission,
            bound: 0,
            check: None,
            job: None,
            launching: None,
            answered: Vec::new(),
        }
    }

    /// Sends `node-admission` the function cache's size, node config's `functions.cache_bytes`
    /// (item 3.6), then asks the node's job bound, which counts the cache, and which the node
    /// document reports and every engine is started with.
    pub fn set_up(&mut self, cache_bytes: u64) -> Result<u64, CallError<admission::Error>> {
        self.admission.functions(cache_bytes)?;
        self.bound = self.admission.bound()?;
        Ok(self.bound)
    }

    /// Whether a job's launch waits for `node-admission`'s answer.
    pub fn launching(&self) -> bool {
        self.launching.is_some()
    }

    /// When the process's loop is to look again for a launch's answer: `Some` while one is
    /// awaited, since the answer comes on `node-admission`'s connection, which it does not wait
    /// on.
    pub fn look_at(&self) -> Option<u64> {
        self.launching
            .as_ref()
            .map(|_| moruna_svc::now().saturating_add(LAUNCH_POLL_NS))
    }

    /// Takes the launch's answer, if it has come: the job's engine, which is then sent its
    /// `spec`; `WouldBlock` while it is still to come; the refusal's sentence or the failure
    /// otherwise, and after [`LAUNCH_NS`].
    fn launched(&mut self) -> Result<(), HostError> {
        let Some(txid) = self.launching.as_ref().map(|l| l.txid) else {
            return Ok(());
        };
        let what = "starting the job's engine";
        let mut answer = self.admission.core().take_reply(txid);
        if answer.is_none() {
            match self.admission.core().file(moruna_svc::now()) {
                Ok(()) | Err(CallError::TimedOut) => {}
                Err(error) => {
                    self.launching = None;
                    return Err(admission_error(what, error.typed()));
                }
            }
            answer = self.admission.core().take_reply(txid);
        }
        let Some(answer) = answer else {
            if self
                .launching
                .as_ref()
                .is_some_and(|l| moruna_svc::now() > l.by)
            {
                self.launching = None;
                return Err(HostError::Failed(format!(
                    "node-admission did not answer the job's start within {} minutes",
                    LAUNCH_NS / 60_000_000_000
                )));
            }
            return Err(HostError::WouldBlock);
        };
        let Some(launching) = self.launching.take() else {
            return Ok(());
        };
        let mut frame = answer.map_err(|error| admission_error(what, error.typed()))?;
        let mut d = frame.decoder();
        let launched = Launched::decode(&mut d)
            .ok()
            .filter(|_| d.finish().is_ok())
            .ok_or_else(|| {
                HostError::Failed(String::from(
                    "node-admission answered the job's start with something else",
                ))
            })?;
        match launched {
            Launched::Started(id) => {
                let mut run = Run::new(id, launching.channel, launching.spec);
                run.flush();
                self.job = Some(run);
                Ok(())
            }
            Launched::Refused(refusal) => Err(refused(refusal)),
        }
    }

    /// The most memory a job may have, as `node-admission` answered it.
    pub fn bound(&self) -> u64 {
        self.bound
    }

    /// Lets go of the job's engine once its channel has ended and everything it sent has been
    /// read, so its account is free and its closed channel is no longer waited on. The process's
    /// loop calls it each turn.
    pub fn tend(&mut self) {
        let Some(run) = self.job.as_mut() else {
            return;
        };
        if run.read == run.inbox.len() && !run.fill() && run.ended {
            let id = run.id;
            self.job = None;
            self.finish(id);
        }
    }

    /// The channels of the engines under way, and what to wait for on each.
    pub fn waits(&self) -> Vec<(Rc<Channel>, Signals)> {
        self.check
            .iter()
            .chain(&self.job)
            .map(|run| (run.channel.clone(), run.signals()))
            .collect()
    }

    /// Releases each check that has answered, once its process has ended: `WouldBlock` while
    /// one still runs.
    fn release_checks(&mut self) -> Result<(), HostError> {
        while let Some(&id) = self.answered.first() {
            match self.admission.status(id) {
                Ok(RUNNING) => return Err(HostError::WouldBlock),
                Ok(_) | Err(CallError::Remote(admission::Error::NotFound)) => {}
                Err(error) => return Err(admission_error("a check's state", error)),
            }
            match self.admission.release(id) {
                Ok(()) | Err(CallError::Remote(admission::Error::NotFound)) => {}
                Err(error) => return Err(admission_error("releasing a check", error)),
            }
            self.answered.remove(0);
        }
        Ok(())
    }

    /// Waits for the engine `id`, whose channel has ended, to end, killing it if it has not
    /// within [`CALL_NS`], and releases it.
    fn finish(&mut self, id: u64) {
        if let Err(CallError::TimedOut) = self.admission.wait(id) {
            let _ = self.admission.cancel(id);
            let _ = self.admission.wait(id);
        }
        let _ = self.admission.release(id);
    }

    /// A fresh channel for an engine: `api`'s end, and the engine's, which `node-admission`
    /// hands it.
    fn channel() -> Result<(Channel, Handle), HostError> {
        let (ours, theirs) = Channel::create()
            .map_err(|e| HostError::Failed(format!("an engine's channel: {}", e.name())))?;
        Ok((ours, theirs.0))
    }
}

/// The engine supervisor's configuration as the data handle it reads: a page, the text then
/// zeros.
fn config_memory(text: &str) -> Result<Handle, HostError> {
    let failed = |e: moruna_svc::Error| {
        HostError::Failed(format!("the engine's configuration: {}", e.name()))
    };
    if text.len() >= 4096 {
        return Err(HostError::Failed(String::from(
            "the engine's configuration is larger than a page",
        )));
    }
    let memory = Memory::create(4096).map_err(failed)?;
    memory
        .map(0, 4096, map::READ | map::WRITE)
        .and_then(|mapping| mapping.write(0, text.as_bytes()))
        .map_err(failed)?;
    memory
        .0
        .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER | Rights::DUPLICATE)
        .map_err(failed)
}

impl EngineHost for Host {
    fn check(&mut self, document: &[u8], secrets: &[String]) -> Result<(), HostError> {
        if self.check.is_some() {
            return Err(HostError::WouldBlock);
        }
        self.release_checks()?;
        let config = config_memory(&check_config(self.bound))?;
        let (ours, theirs) = Host::channel()?;
        let id = self
            .admission
            .check(
                config,
                vec![String::from(CHECK_CHANNEL)],
                vec![theirs],
                SERVICES.iter().map(|s| String::from(*s)).collect(),
            )
            .map_err(|e| admission_error("starting a check", e))?;
        let mut run = Run::new(id, ours, check_line(document, secrets));
        run.flush();
        self.check = Some(run);
        Ok(())
    }

    fn checked(&mut self) -> Option<Result<Verdict, HostError>> {
        let run = self.check.as_mut()?;
        run.flush();
        let answer = match run.line() {
            Some(line) => verdict(&line),
            None if run.ended => Err(HostError::Failed(String::from(
                "the engine's check ended without an answer",
            ))),
            None => return None,
        };
        let id = run.id;
        self.check = None;
        self.answered.push(id);
        Some(answer)
    }

    fn start(&mut self, spec: &[u8], verdict: &Verdict) -> Result<(), HostError> {
        // The server starts a job only when none is in flight: an engine still here has said its
        // `exit`, and it is let go before the next is started.
        if let Some(run) = self.job.take() {
            self.finish(run.id);
        }
        if self.launching.is_some() {
            return Err(HostError::Unavailable);
        }
        let budget = verdict
            .memory_bytes
            .ok_or_else(|| HostError::Failed(String::from("the engine's check named no budget")))?;
        let staging = verdict.needs.volumes.iter().any(|v| v.staging);
        let network = !verdict.needs.hosts.is_empty();
        let volumes = verdict.needs.volumes.len();
        let functions = !verdict.needs.functions.is_empty();
        let config = config_memory(&job_config(
            self.bound, budget, staging, network, volumes, functions,
        )?)?;
        let (ours, theirs) = Host::channel()?;
        let needs = &verdict.needs;
        let needs: node_protocol::Needs = (
            needs.hosts.clone(),
            needs
                .volumes
                .iter()
                .map(|v| (v.serial.clone(), v.write))
                .collect(),
            needs.functions.clone(),
        );
        let functions: Vec<Registered> = verdict
            .registered
            .iter()
            .map(|f| (f.name.clone(), f.address.clone(), f.sha256.clone()))
            .collect();
        // The launch is asked, not waited for: `node-admission` answers once it has fetched and
        // checked the job's functions (item 3.6), which `receive` looks for.
        let mut body = moruna_svc::Encoder::new();
        (
            (
                budget,
                config,
                vec![String::from(ENGINE_CHANNEL)],
                vec![theirs],
                SERVICES
                    .iter()
                    .map(|s| String::from(*s))
                    .collect::<Vec<String>>(),
                needs,
            ),
            functions,
        )
            .encode(&mut body)
            .map_err(|e| HostError::Failed(format!("the job's launch: {e:?}")))?;
        let txid = self
            .admission
            .core()
            .send(1, body)
            .map_err(|e| admission_error("starting the job's engine", e.typed()))?;
        self.launching = Some(Launching {
            txid,
            by: moruna_svc::now().saturating_add(LAUNCH_NS),
            channel: ours,
            spec: spec.to_vec(),
        });
        Ok(())
    }

    fn send(&mut self, line: &[u8]) -> Result<(), HostError> {
        let run = self.job.as_mut().ok_or(HostError::Unavailable)?;
        run.out.extend(line);
        run.flush();
        Ok(())
    }

    fn receive(&mut self, buf: &mut [u8]) -> Result<usize, HostError> {
        // The job's launch, until `node-admission` answers it.
        self.launched()?;
        // No engine: its channel has ended, and it was let go ([`Host::tend`]).
        let Some(run) = self.job.as_mut() else {
            return Ok(0);
        };
        run.flush();
        if run.read == run.inbox.len() && !run.fill() && run.ended {
            let id = run.id;
            self.job = None;
            self.finish(id);
            return Ok(0);
        }
        let unread = &run.inbox[run.read..];
        if unread.is_empty() {
            return Err(HostError::WouldBlock);
        }
        let count = unread.len().min(buf.len());
        buf[..count].copy_from_slice(&unread[..count]);
        run.read += count;
        Ok(count)
    }
}

/// The host shared between the API server, which drives it, and the process's loop, which
/// waits on its channels: the server's [`EngineHost`].
#[derive(Clone)]
pub struct Shared(pub Rc<RefCell<Host>>);

impl EngineHost for Shared {
    fn check(&mut self, document: &[u8], secrets: &[String]) -> Result<(), HostError> {
        self.0.borrow_mut().check(document, secrets)
    }

    fn checked(&mut self) -> Option<Result<Verdict, HostError>> {
        self.0.borrow_mut().checked()
    }

    fn start(&mut self, spec: &[u8], verdict: &Verdict) -> Result<(), HostError> {
        self.0.borrow_mut().start(spec, verdict)
    }

    fn send(&mut self, line: &[u8]) -> Result<(), HostError> {
        self.0.borrow_mut().send(line)
    }

    fn receive(&mut self, buf: &mut [u8]) -> Result<usize, HostError> {
        self.0.borrow_mut().receive(buf)
    }
}
