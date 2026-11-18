//! Processes and threads (SDD 20, f.2, f.8, f.9 and f.12 to f.14).
//!
//! A process is an address space and a handle table. A thread is an OSTD task that
//! runs user code in its process's address space and returns to the kernel on every
//! syscall and exception. A process ends when its last thread ends, when one of its
//! threads calls `process_exit`, or when one of them faults; ending closes every handle
//! it holds, unmaps its address space and raises `TERMINATED`. Nothing else in the
//! system stops: the kernel reports the end on the console and the others run on.
//!
//! A process with a forward (SDD 28) has its foreign calls answered by a server and a
//! few by the kernel itself (`foreign`). Each thread keeps its FPU registers and its FS
//! base across switches, so threads of any program, Rust or C, can share a CPU.

use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

use moruna_abi::{
    ABI_VERSION, Error, PAGE_SIZE, RawHandle, Signals, charge, limits, map, process_end::End,
    start_page, valid_name,
};
use moruna_handles::HandleTable;
use moruna_loader::elf;
use ostd::{
    arch::{
        cpu::context::{CpuException, FpuContext, FsBase, UserContext},
        read_random, read_tsc,
    },
    early_println,
    irq::DisabledLocalIrqGuard,
    power::{ExitCode, poweroff},
    sync::{LocalIrqDisabled, SpinLock, Waker},
    task::{Task, TaskOptions},
    user::{ReturnReason, UserContextApi, UserMode, UserModeHooks},
};

use crate::{
    account::{Account, Charge},
    clock,
    foreign::{self, Forward},
    futex,
    memory::{DeviceMemory, MemoryObject},
    object::Handle,
    plane::Plane,
    signal::Signaller,
    syscall::Flow,
    usercopy,
    vm::{Access, Backing, Kind, Space},
};

pub use crate::vm::{IMAGE_RANGE, is_user};

/// How a process ended.
pub enum Exit {
    Code(i64),
    Fault(String),
    /// A holder of a handle to it called `process_kill`.
    Killed,
}

enum Life {
    /// Made by `process_create`; `entry` is set once an image is mapped.
    Created {
        entry: Option<u64>,
    },
    Running {
        live: usize,
    },
    Ended,
}

struct State {
    life: Life,
    exit: Option<Exit>,
    threads: Vec<Weak<Thread>>,
}

static NEXT_KOID: AtomicU64 = AtomicU64::new(1);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static SUPERVISOR_FAILED: AtomicBool = AtomicBool::new(false);

pub struct Process {
    pub name: String,
    /// The kernel's own number for the process, never reused; futex queues are keyed
    /// by it.
    pub koid: u64,
    pub space: Space,
    /// Its handle table, shared with the processes `process_clone` gave it to (stream B1
    /// hook): `handles.lock()` is the table.
    pub handles: Arc<Table>,
    state: SpinLock<State>,
    pub signaller: Signaller,
    supervisor: bool,
    /// Bit `i` is set once one of this process's threads has run on CPU `i`.
    cpus: AtomicU64,
    /// Its forward, given before it started (SDD 28).
    forward: SpinLock<Option<Arc<Forward>>>,
    /// Its data plane, given before it started (SDD 29).
    plane: SpinLock<Option<Arc<Plane>>>,
    /// Whether `process_grant` gave it its handles and start page (ABI 2.2, SDD 30).
    granted: AtomicBool,
    /// CPU time its threads have used, in nanoseconds, kernel time included.
    pub cpu_ns: AtomicU64,
    /// The code the last thread to end by a fast `exit` gave: a process whose last
    /// thread ends that way ends with it.
    last_code: AtomicI64,
    /// How it ended (`moruna_abi::process_end`), set as it ends: zero before (ABI 2.7).
    ended: AtomicU64,
    /// Its record, charged to its own account.
    _record: Charge,
}

// The fixed charges cover the records they stand for (SDD 20 f.18).
const _: () = assert!(core::mem::size_of::<Process>() as u64 <= charge::PROCESS);
const _: () = assert!(
    core::mem::size_of::<Thread>() as u64 + core::mem::size_of::<Task>() as u64
        <= charge::THREAD - 128 * charge::PAGE
);
const _: () = assert!(2 * moruna_handles::slot_bytes::<Handle>() as u64 <= charge::HANDLE_SLOT);

/// Counts the boot as one live process while it starts the manifest's processes, so a
/// process that ends on another CPU before the last one has started cannot power the
/// machine off. The boot calls [`boot_done`] when it has started them all.
pub fn boot_begins() {
    LIVE.fetch_add(1, Ordering::AcqRel);
}

/// The boot has started every process: it stops counting as one.
pub fn boot_done() {
    release_live();
}

fn release_live() {
    if LIVE.fetch_sub(1, Ordering::AcqRel) == 1 {
        let failed = SUPERVISOR_FAILED.load(Ordering::Acquire);
        early_println!(
            "kk: process threads ran on {} of {} CPUs",
            crate::sched::cpus_that_ran(),
            ostd::cpu::num_cpus()
        );
        early_println!(
            "kk: every process has ended; the supervisor {}; powering off",
            if failed { "failed" } else { "succeeded" }
        );
        poweroff(if failed {
            ExitCode::Failure
        } else {
            ExitCode::Success
        });
    }
}

/// The CPUs a bit set names, for the console: `0,2,3`.
fn cpu_list(bits: u64) -> String {
    let mut list = String::new();
    for cpu in 0..64 {
        if bits & (1 << cpu) != 0 {
            if !list.is_empty() {
                list.push(',');
            }
            list.push_str(&format!("{cpu}"));
        }
    }
    list
}

fn salt(koid: u64) -> u32 {
    let r = read_random().unwrap_or(0) ^ read_tsc() ^ koid.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (r ^ (r >> 32)) as u32
}

fn prot(write: bool, execute: bool) -> u64 {
    let mut p = map::READ;
    if write {
        p |= map::WRITE;
    }
    if execute {
        p |= map::EXECUTE;
    }
    p
}

impl Process {
    /// A created process named `name`, charged to `account`, as is everything it
    /// causes to exist.
    pub fn new(
        name: &[u8],
        supervisor: bool,
        account: &Arc<Account>,
    ) -> Result<Arc<Process>, Error> {
        Process::new_in(name, supervisor, account, None)
    }

    /// A created process as [`Process::new`] makes one, using `table` when given (stream B1
    /// hook: `process_clone`'s `SHARE_HANDLES`), else a table of its own.
    pub fn new_in(
        name: &[u8],
        supervisor: bool,
        account: &Arc<Account>,
        table: Option<Arc<Table>>,
    ) -> Result<Arc<Process>, Error> {
        if !valid_name(name) {
            return Err(Error::InvalidArgs);
        }
        let name = core::str::from_utf8(name).map_err(|_| Error::InvalidArgs)?;
        let record = Charge::new(account, charge::PROCESS)?;
        let space = Space::new(account)?;
        let koid = NEXT_KOID.fetch_add(1, Ordering::Relaxed);
        let handles = match table {
            Some(table) => table,
            None => Arc::new(Table::new(salt(koid), account)?),
        };
        let process = Arc::new(Process {
            name: String::from(name),
            koid,
            space,
            handles,
            state: SpinLock::new(State {
                life: Life::Created { entry: None },
                exit: None,
                threads: Vec::new(),
            }),
            signaller: Signaller::new(Signals::NONE),
            supervisor,
            cpus: AtomicU64::new(0),
            forward: SpinLock::new(None),
            plane: SpinLock::new(None),
            granted: AtomicBool::new(false),
            cpu_ns: AtomicU64::new(0),
            last_code: AtomicI64::new(0),
            ended: AtomicU64::new(0),
            _record: record,
        });
        process.handles.join(&process);
        Ok(process)
    }

    /// The account everything it causes to exist is charged to.
    pub fn account(&self) -> &Arc<Account> {
        self.space.account()
    }

    /// Inserts `handle` into `table`, this process's own, locked: the table's growth is
    /// charged first, `charge::HANDLE_SLOTS` slots at a time (`QuotaExceeded` with the
    /// handle not inserted, and given back to be dropped outside the lock).
    pub fn insert_into(
        &self,
        table: &mut HandleTable<Handle>,
        handle: Handle,
    ) -> Result<RawHandle, (Error, Handle)> {
        if table.grows() {
            let step = charge::HANDLE_SLOTS * charge::HANDLE_SLOT;
            let need = moruna_quota::table_steps(table.slots() + 1) * step;
            // Stream B1 hook: the table's own charge, however many processes share it.
            let mut charged = self.handles.charge.lock();
            let have = charged.bytes();
            if need > have
                && let Err(e) = charged.grow(need - have)
            {
                return Err((e, handle));
            }
        }
        table.insert_or_return(handle)
    }

    /// Inserts `handle` into this process's table, as [`Process::insert_into`] does,
    /// dropping a handle refused outside the table's lock.
    pub fn insert(&self, handle: Handle) -> Result<RawHandle, Error> {
        let result = {
            let mut table = self.handles.lock();
            self.insert_into(&mut table, handle)
        };
        result.map_err(|(e, refused)| {
            drop(refused);
            e
        })
    }

    /// Its forward, if it has one.
    pub fn forward(&self) -> Option<Arc<Forward>> {
        self.forward.lock().clone()
    }

    /// Maps `len` bytes of a device memory region from `offset`, uncached.
    pub fn map_device(
        &self,
        device: &Arc<DeviceMemory>,
        offset: u64,
        len: u64,
        write: bool,
        writable: bool,
    ) -> Result<u64, Error> {
        let backing = Backing::Device {
            device: device.clone(),
            offset,
            writable,
        };
        self.space
            .map(None, len, Kind::Mapping, prot(write, false), backing, false)
    }

    /// Loads a static ELF image into this not yet started process; returns its entry.
    pub fn load_image(&self, image: &[u8]) -> Result<u64, Error> {
        {
            let state = self.state.lock();
            if !matches!(state.life, Life::Created { entry: None }) {
                return Err(Error::BadState);
            }
        }
        let parsed = elf::parse(image, IMAGE_RANGE).map_err(|e| {
            early_println!("kk: process {} image refused: {}", self.name, e.describe());
            Error::InvalidArgs
        })?;
        for segment in &parsed.segments {
            let start = segment.page_start();
            let len = segment.page_end() - start;
            let object = Arc::new(MemoryObject::new(len, self.account())?);
            let from = segment.offset as usize;
            let bytes = image
                .get(from..from + segment.filesz as usize)
                .ok_or(Error::InvalidArgs)?;
            object.write(segment.vaddr - start, bytes)?;
            let backing = Backing::Object {
                memory: object,
                first: 0,
                writable: true,
            };
            self.space.map(
                Some(start),
                len,
                Kind::Image,
                prot(segment.write, segment.execute),
                backing,
                false,
            )?;
        }
        let mut state = self.state.lock();
        match state.life {
            Life::Created { entry: None } => {
                state.life = Life::Created {
                    entry: Some(parsed.entry),
                };
                Ok(parsed.entry)
            }
            _ => Err(Error::BadState),
        }
    }

    /// Whether the process was created, has its image, and has not started.
    pub fn ready_to_start(&self) -> bool {
        matches!(self.state.lock().life, Life::Created { entry: Some(_) })
    }

    /// Whether the process was created and has not started (it may have no image).
    pub fn is_created(&self) -> bool {
        matches!(self.state.lock().life, Life::Created { .. })
    }

    /// Gives a created process its forward (`process_forward`), once.
    pub fn set_forward(&self, forward: Forward) -> Result<(), Error> {
        if !self.is_created() {
            return Err(Error::BadState);
        }
        let mut slot = self.forward.lock();
        if slot.is_some() {
            return Err(Error::BadState);
        }
        *slot = Some(Arc::new(forward));
        Ok(())
    }

    /// Its plane, if it has one.
    pub fn plane(&self) -> Option<Arc<Plane>> {
        self.plane.lock().clone()
    }

    /// Gives a created process its plane (`process_plane`), once.
    pub fn set_plane(&self, plane: Plane) -> Result<(), Error> {
        if !self.is_created() {
            return Err(Error::BadState);
        }
        let mut slot = self.plane.lock();
        if slot.is_some() {
            return Err(Error::BadState);
        }
        *slot = Some(Arc::new(plane));
        Ok(())
    }

    fn begin_running(&self) -> Result<(), Error> {
        let mut state = self.state.lock();
        match state.life {
            Life::Created { .. } => {
                state.life = Life::Running { live: 1 };
                // stream B1 hook
                self.handles.running.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
            _ => Err(Error::BadState),
        }
    }

    /// Starts a created process whose image is mapped: gives it `handles` under the
    /// slot names given, maps its stack and start page, and runs its first thread.
    pub fn start(
        self: &Arc<Self>,
        entry: u64,
        handles: Vec<(Handle, Vec<u8>)>,
    ) -> Result<(), Error> {
        {
            let state = self.state.lock();
            match state.life {
                Life::Created { entry: Some(_) } => {}
                _ => return Err(Error::BadState),
            }
        }
        if self.granted.load(Ordering::Acquire) {
            return Err(Error::BadState);
        }
        if !is_user(entry) || handles.len() > limits::START_HANDLES_MAX {
            return Err(Error::InvalidArgs);
        }
        let stack = Arc::new(MemoryObject::new(limits::MAIN_STACK_BYTES, self.account())?);
        let stack_va = self.space.map(
            None,
            limits::MAIN_STACK_BYTES,
            Kind::Stack,
            prot(true, false),
            Backing::Object {
                memory: stack,
                first: 0,
                writable: true,
            },
            false,
        )?;
        let start_va = self.map_start_page(handles)?;

        self.begin_running()?;
        LIVE.fetch_add(1, Ordering::AcqRel);
        let stack_top = stack_va + limits::MAIN_STACK_BYTES;
        let mut context = UserContext::default();
        context.set_instruction_pointer(entry as usize);
        // The thread starts as if called: its stack pointer is 8 below a 16-byte boundary.
        context.set_stack_pointer((stack_top & !0xf) as usize - 8);
        context.set_rdi(start_va as usize);
        if let Err(e) = Thread::spawn(self, context, 0) {
            self.end(Exit::Fault(String::from(
                "its first thread could not be created",
            )));
            self.thread_ended();
            return Err(e);
        }
        Ok(())
    }

    /// Gives a created process its creator will launch (`process_grant`, ABI 2.2) the
    /// handles of `handles` under their slot names, and maps a start page naming them
    /// into it read only, where the kernel chooses; returns the page's address. Once a
    /// process; `process_start` refuses a process that has been granted.
    pub fn grant(&self, handles: Vec<(Handle, Vec<u8>)>) -> Result<u64, Error> {
        if !self.is_created() {
            return Err(Error::BadState);
        }
        if handles.len() > limits::START_HANDLES_MAX {
            return Err(Error::InvalidArgs);
        }
        if self.granted.swap(true, Ordering::AcqRel) {
            return Err(Error::BadState);
        }
        self.map_start_page(handles)
    }

    /// Moves `handles` into the table and maps the start page that names them (SDD 20
    /// d.4) read only; returns where.
    fn map_start_page(&self, handles: Vec<(Handle, Vec<u8>)>) -> Result<u64, Error> {
        let mut page = [0u8; PAGE_SIZE as usize];
        moruna_abi::write_u32(&mut page, start_page::MAGIC_OFFSET, start_page::MAGIC);
        moruna_abi::write_u32(&mut page, start_page::VERSION_OFFSET, ABI_VERSION as u32);
        moruna_abi::write_u32(&mut page, start_page::COUNT_OFFSET, handles.len() as u32);
        moruna_abi::write_u32(
            &mut page,
            start_page::CPUS_OFFSET,
            ostd::cpu::num_cpus() as u32,
        );
        let name = self.name.as_bytes();
        page[start_page::NAME_OFFSET..start_page::NAME_OFFSET + name.len()].copy_from_slice(name);
        let mut refused = None;
        // What a refusal leaves unmoved is dropped after the table's lock.
        let mut handles = handles.into_iter().enumerate();
        {
            let mut table = self.handles.lock();
            for (i, (handle, slot)) in handles.by_ref() {
                let at = start_page::HEADER_SIZE + i * start_page::ENTRY_SIZE;
                let rights = handle.rights.bits();
                let value = match self.insert_into(&mut table, handle) {
                    Ok(value) => value,
                    Err((e, handle)) => {
                        refused = Some((e, handle));
                        break;
                    }
                };
                moruna_abi::write_u32(&mut page, at + start_page::ENTRY_HANDLE, value);
                moruna_abi::write_u32(&mut page, at + start_page::ENTRY_RIGHTS, rights);
                let n = slot.len().min(limits::NAME_MAX);
                let name_at = at + start_page::ENTRY_NAME;
                page[name_at..name_at + n].copy_from_slice(&slot[..n]);
            }
        }
        if let Some((e, handle)) = refused {
            // Dropped outside the table's lock; what was moved in before stays in it.
            drop(handle);
            drop(handles);
            return Err(e);
        }
        let start = Arc::new(MemoryObject::new(PAGE_SIZE, self.account())?);
        start.write(0, &page)?;
        self.space.map(
            None,
            PAGE_SIZE,
            Kind::StartPage,
            map::READ,
            Backing::Object {
                memory: start,
                first: 0,
                writable: false,
            },
            false,
        )
    }

    /// Starts a created process whose address space its creator built
    /// (`process_launch`): its first thread at `entry` with its stack pointer at
    /// `stack` exactly and every other register zero; no start page and no handles.
    /// `brk`, when not zero, is where its heap begins. Returns the thread's id.
    pub fn launch(self: &Arc<Self>, entry: u64, stack: u64, brk: u64) -> Result<u64, Error> {
        if !self.is_created() {
            return Err(Error::BadState);
        }
        if !is_user(entry) || !is_user(stack) {
            return Err(Error::InvalidArgs);
        }
        if brk != 0 {
            self.space.set_brk(brk)?;
        }
        self.begin_running()?;
        LIVE.fetch_add(1, Ordering::AcqRel);
        let mut context = UserContext::default();
        context.set_instruction_pointer(entry as usize);
        context.set_stack_pointer(stack as usize);
        match Thread::spawn(self, context, 0) {
            Ok(thread) => Ok(thread.tid),
            Err(e) => {
                self.end(Exit::Fault(String::from(
                    "its first thread could not be created",
                )));
                self.thread_ended();
                Err(e)
            }
        }
    }

    fn count_thread(&self) -> Result<(), Error> {
        let mut state = self.state.lock();
        if state.exit.is_some() {
            return Err(Error::BadState);
        }
        match &mut state.life {
            Life::Running { live } => {
                *live += 1;
                Ok(())
            }
            _ => Err(Error::BadState),
        }
    }

    /// Adds a thread to this running process (`thread_create`).
    pub fn add_thread(
        self: &Arc<Self>,
        entry: u64,
        stack: u64,
        arg: u64,
    ) -> Result<Arc<Thread>, Error> {
        if !is_user(entry) || !is_user(stack) {
            return Err(Error::InvalidArgs);
        }
        self.count_thread()?;
        let mut context = UserContext::default();
        context.set_instruction_pointer(entry as usize);
        context.set_stack_pointer((stack & !0xf) as usize - 8);
        context.set_rdi(arg as usize);
        match Thread::spawn(self, context, 0) {
            Ok(thread) => Ok(thread),
            Err(e) => {
                self.thread_ended();
                Err(e)
            }
        }
    }

    /// The live thread with id `tid`.
    fn thread(&self, tid: u64) -> Option<Arc<Thread>> {
        self.state
            .lock()
            .threads
            .iter()
            .filter_map(Weak::upgrade)
            .find(|t| t.tid == tid)
    }

    /// `forward_clone`: a new thread of this forwarded process with the registers of
    /// thread `tid`, which is waiting on the forward: its result register zero, its
    /// stack pointer `stack`, its FS base `tls` (the parent's when zero), `clear_tid`
    /// the word cleared and woken when it ends, and its id written to `set_tid` before
    /// it runs (neither when zero). Returns its id.
    pub fn clone_thread(
        self: &Arc<Self>,
        tid: u64,
        stack: u64,
        tls: u64,
        clear_tid: u64,
        set_tid: u64,
    ) -> Result<u64, Error> {
        if !is_user(stack) || !foreign::canonical(tls) {
            return Err(Error::InvalidArgs);
        }
        let parent = self.thread(tid).ok_or(Error::InvalidArgs)?;
        let mut context = parent.parked.lock().clone().ok_or(Error::BadState)?;
        context.set_rax(0);
        context.set_stack_pointer(stack as usize);
        let fs = if tls != 0 {
            tls
        } else {
            parent.fs_base.load(Ordering::Relaxed)
        };
        self.count_thread()?;
        match Thread::spawn_with(self, context, fs, clear_tid, set_tid) {
            Ok(thread) => Ok(thread.tid),
            Err(e) => {
                self.thread_ended();
                Err(e)
            }
        }
    }

    /// The process's threads but `keep` end: an `execve` replaces the program under one
    /// thread.
    pub fn end_others(&self, keep: &Thread) {
        let others: Vec<Arc<Thread>> = self
            .state
            .lock()
            .threads
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|t| t.tid != keep.tid)
            .collect();
        for t in others {
            t.kill();
        }
    }

    /// Its live threads.
    pub fn live_threads(&self) -> u32 {
        match self.state.lock().life {
            Life::Running { live } => live as u32,
            _ => 0,
        }
    }

    /// `process_kill`: ends a running process. `BadState` for one that has not started
    /// or has already ended.
    pub fn kill(&self) -> Result<(), Error> {
        {
            let state = self.state.lock();
            if !matches!(state.life, Life::Running { .. }) || state.exit.is_some() {
                return Err(Error::BadState);
            }
        }
        self.end(Exit::Killed);
        // Stream B1 hook: a task sharing one table ends whole.
        for other in self.handles.others(self) {
            if other.is_running() {
                other.end(Exit::Killed);
            }
        }
        Ok(())
    }

    /// Records why the process is ending (the first reason wins) and stops its threads.
    /// How the process ended (`moruna_abi::process_end`), once it is `TERMINATED`: what
    /// `handle_info` gives in `rdx` (ABI 2.7). Zero before it ends.
    pub fn ended(&self) -> u64 {
        self.ended.load(Ordering::Acquire)
    }

    pub fn end(&self, exit: Exit) {
        let threads: Vec<Arc<Thread>> = {
            let mut state = self.state.lock();
            if state.exit.is_none() {
                state.exit = Some(exit);
            }
            state.threads.iter().filter_map(Weak::upgrade).collect()
        };
        for thread in threads {
            thread.kill();
        }
    }

    /// Whether the process is ending (a reason has been recorded).
    fn ending(&self) -> bool {
        self.state.lock().exit.is_some()
    }

    /// Called by each thread as it ends; the last one ends the process.
    fn thread_ended(&self) {
        let exit = {
            let mut state = self.state.lock();
            match &mut state.life {
                Life::Running { live } if *live > 1 => {
                    *live -= 1;
                    state.threads.retain(|t| t.strong_count() > 0);
                    return;
                }
                Life::Running { .. } => {
                    state.life = Life::Ended;
                    state.threads.clear();
                    state
                        .exit
                        .take()
                        .unwrap_or(Exit::Code(self.last_code.load(Ordering::Relaxed)))
                }
                Life::Created { .. } | Life::Ended => return,
            }
        };
        self.terminate(exit);
    }

    fn terminate(&self, exit: Exit) {
        let cpus = cpu_list(self.cpus.load(Ordering::Relaxed));
        let failed = match &exit {
            Exit::Code(code) => {
                early_println!(
                    "kk: process {} exited with code {} (it ran on cpus {})",
                    self.name,
                    code,
                    cpus
                );
                *code != 0
            }
            Exit::Fault(what) => {
                early_println!(
                    "kk: process {} faulted: {}; it has ended and the others run on (it ran on cpus {})",
                    self.name,
                    what,
                    cpus
                );
                true
            }
            Exit::Killed => {
                early_println!(
                    "kk: process {} was killed through a handle to it; it has ended and the others run on (it ran on cpus {})",
                    self.name,
                    cpus
                );
                true
            }
        };
        // Stream B1 hook: a shared table empties with the last of its running processes,
        // which frees the processes its handles held.
        let handles = if self.handles.running.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.handles.lock().drain()
        } else {
            Vec::new()
        };
        drop(handles);
        // The forward goes with the process: its server sees the channel close.
        let forward = self.forward.lock().take();
        drop(forward);
        // Its plane gives back what it held: the engine sees each batch released and
        // each window returned, and then the slot's end (SDD 29 f.6).
        let plane = self.plane.lock().take();
        if let Some(plane) = &plane {
            plane.detach();
        }
        drop(plane);
        self.space.clear();
        let end = match &exit {
            Exit::Code(code) => End::Code(*code),
            Exit::Fault(_) => End::Faulted,
            Exit::Killed => End::Killed,
        };
        // Stored before the signal, so a waiter that sees `TERMINATED` reads how it ended.
        self.ended.store(end.encode(), Ordering::Release);
        self.signaller.update(Signals::NONE, Signals::TERMINATED);
        if self.supervisor && failed {
            SUPERVISOR_FAILED.store(true, Ordering::Release);
        }
        release_live();
    }
}

pub struct Thread {
    pub process: Arc<Process>,
    /// Its kernel stack, registers and FPU state, charged to its process's account.
    _charge: Charge,
    pub signaller: Signaller,
    /// The thread's id: a forwarded process's `gettid`, and its forward token.
    pub tid: u64,
    killed: AtomicBool,
    waker: SpinLock<Option<Arc<Waker>>>,
    /// The FS base user code runs with.
    fs_base: AtomicU64,
    /// The FPU registers, saved while the thread is off its CPU.
    fpu: SpinLock<FpuContext, LocalIrqDisabled>,
    /// The word cleared and woken when the thread ends (`set_tid_address`).
    pub clear_tid: AtomicU64,
    /// The registers of a thread waiting on its forward, for `forward_clone`.
    pub parked: SpinLock<Option<UserContext>>,
    /// Its last port call's reply, mapped until its next port call or its end: the
    /// address, the bytes mapped and the memory object (SDD 29 f.4).
    pub port_reply: SpinLock<Option<(u64, u64, Arc<MemoryObject>)>>,
    /// CPU time the thread has used, in nanoseconds, and when it last took a CPU.
    pub cpu_ns: AtomicU64,
    on_cpu_since: AtomicU64,
    /// Stream B1: its side of the personality, and its interrupt.
    pub(crate) mode: crate::personality::Mode,
}

struct Hooks<'a>(&'a Thread);

impl UserModeHooks for Hooks<'_> {
    fn has_kernel_event(&self) -> bool {
        self.0.killed()
    }
}

impl Thread {
    fn spawn(
        process: &Arc<Process>,
        context: UserContext,
        fs_base: u64,
    ) -> Result<Arc<Thread>, Error> {
        Thread::spawn_with(process, context, fs_base, 0, 0)
    }

    /// Spawns a thread whose `set_tid_address` word is `clear_tid`, and which writes its
    /// id to `set_tid` in its process's memory before it runs (neither when zero).
    fn spawn_with(
        process: &Arc<Process>,
        context: UserContext,
        fs_base: u64,
        clear_tid: u64,
        set_tid: u64,
    ) -> Result<Arc<Thread>, Error> {
        let charge = Charge::new(process.account(), charge::THREAD)?;
        let thread = Arc::new(Thread {
            process: process.clone(),
            _charge: charge,
            signaller: Signaller::new(Signals::NONE),
            tid: NEXT_KOID.fetch_add(1, Ordering::Relaxed),
            killed: AtomicBool::new(false),
            waker: SpinLock::new(None),
            fs_base: AtomicU64::new(fs_base),
            fpu: SpinLock::new(FpuContext::new()),
            clear_tid: AtomicU64::new(clear_tid),
            parked: SpinLock::new(None),
            port_reply: SpinLock::new(None),
            cpu_ns: AtomicU64::new(0),
            on_cpu_since: AtomicU64::new(0),
            // stream B1 hook
            mode: crate::personality::Mode::new(),
        });
        if set_tid != 0 {
            process
                .space
                .copy_in(set_tid, &(thread.tid as u32).to_le_bytes())?;
        }
        process.state.lock().threads.push(Arc::downgrade(&thread));
        let runner = thread.clone();
        TaskOptions::new(move || run(runner, context))
            .data(thread.clone())
            .spawn()
            .map_err(|_| Error::NoMemory)?;
        Ok(thread)
    }

    /// Whether the thread is ending, or (stream B1) an interrupt is pending: either stops
    /// a wait in the core. [`Thread::dying`] is the end alone.
    pub fn killed(&self) -> bool {
        self.killed.load(Ordering::Acquire) || self.mode.interrupt_pending()
    }

    fn kill(&self) {
        self.killed.store(true, Ordering::Release);
        if let Some(waker) = self.waker.lock().as_ref() {
            waker.wake_up();
        }
    }

    /// Registers the waker a blocked syscall sleeps on, so `kill` can wake it.
    pub fn set_waker(&self, waker: Option<Arc<Waker>>) {
        *self.waker.lock() = waker;
    }

    /// Sets the FS base the thread runs with, now: it is on its CPU, in a call.
    pub fn set_fs(&self, base: u64) {
        self.fs_base.store(base, Ordering::Relaxed);
        FsBase::new(base as usize).load();
    }

    pub fn fs(&self) -> u64 {
        self.fs_base.load(Ordering::Relaxed)
    }

    /// The time since the thread last took its CPU: the slice it is running now,
    /// which its CPU time does not count yet.
    pub fn slice(&self) -> u64 {
        clock::now().saturating_sub(self.on_cpu_since.load(Ordering::Relaxed))
    }

    /// The CPU time this thread has used, the current slice included.
    pub fn cpu_time(&self) -> u64 {
        self.cpu_ns
            .load(Ordering::Relaxed)
            .saturating_add(self.slice())
    }
}

fn describe(exception: Option<CpuException>, ip: usize) -> String {
    match exception {
        Some(CpuException::PageFault(info)) => format!(
            "page fault at {:#x} on {} (ip {:#x})",
            info.addr,
            if info.error_code.bits() & 0b10 != 0 {
                "write"
            } else {
                "read"
            },
            ip
        ),
        Some(other) => format!("{other:?} (ip {ip:#x})"),
        None => format!("an exception with no record (ip {ip:#x})"),
    }
}

fn run(thread: Arc<Thread>, context: UserContext) {
    thread.process.vm().activate();
    FsBase::new(thread.fs() as usize).load();
    let mut user = UserMode::new(context);
    let hooks = Hooks(&thread);
    // stream B1 hook: the loop ends with the thread, not with an interrupt.
    while !thread.dying() {
        match user.execute(&hooks) {
            ReturnReason::UserSyscall => {
                // Stream B1: the branch is the personality's (`personality::syscall`).
                let flow = crate::personality::syscall(&thread, user.context_mut());
                match flow {
                    Flow::Continue => {}
                    Flow::ThreadExit => break,
                    Flow::ThreadExitCode(code) => {
                        thread.process.last_code.store(code, Ordering::Relaxed);
                        break;
                    }
                    Flow::ProcessExit(code) => {
                        thread.process.end(Exit::Code(code));
                        break;
                    }
                }
            }
            ReturnReason::UserException => {
                // Stream B1: served, entered into the personality, or the process's fault.
                match exception(&thread, user.context_mut()) {
                    Flow::Continue => {}
                    _ => break,
                }
            }
            ReturnReason::KernelEvent => {
                // Stream B1: an interrupt of a thread running its program.
                if let Flow::ThreadExit =
                    crate::personality::kernel_event(&thread, user.context_mut())
                {
                    break;
                }
            }
        }
    }
    clear_child_tid(&thread);
    if let Some((addr, span, memory)) = thread.port_reply.lock().take() {
        // The plane's own mapping: only it can have removed it, and it has not.
        if let Err(e) = thread.process.space.unmap_plane(addr, span, &memory) {
            early_println!(
                "kk: process {} port reply at {:#x} not unmapped at thread end: {}",
                thread.process.name,
                addr,
                e.name()
            );
        }
    }
    thread.set_waker(None);
    thread.signaller.update(Signals::NONE, Signals::TERMINATED);
    thread.process.thread_ended();
}

/// A thread that ends on its own, its process going on, clears its `set_tid_address`
/// word and wakes one waiter on it, which is how a thread library learns it has gone.
fn clear_child_tid(thread: &Thread) {
    let word = thread.clear_tid.swap(0, Ordering::Relaxed);
    if word == 0 || thread.process.ending() {
        return;
    }
    if usercopy::write_u32(&thread.process.space, word, 0).is_ok() {
        let _ = futex::wake(thread.process.koid, word, 1, futex::ALL);
    }
}

// ---- stream A (data path, `architecture/data-os-streams.md`): stream A's additions to
// ---- `Thread` (its CPU) go in this block.
impl Thread {}

// ---- stream B1 (personality, `architecture/data-os-streams.md`): stream B1's additions to
// ---- `Thread` and `Process` go in this block; it also owns the user-mode loop's syscall
// ---- branch (the forward) above.
impl Thread {
    /// Whether the thread is ending: killed, or its process ending.
    pub fn dying(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    /// `thread_interrupt`: stops its wait in the core, or brings it from its program into
    /// its personality.
    pub fn interrupt(&self) {
        self.mode.raise();
        if let Some(waker) = self.waker.lock().as_ref() {
            waker.wake_up();
        }
    }

    /// Activates the table the thread runs on: its program's while it runs its program.
    pub fn activate(&self) {
        if self.mode.in_program()
            && let Some(program) = self.process.space.program()
        {
            program.vm.activate();
            return;
        }
        self.process.vm().activate();
    }
}

/// The thread on this CPU, if a process's.
pub fn current() -> Option<Arc<Thread>> {
    current_thread()
}

/// An exception of user code: a page fault the space serves (a first touch, a page a
/// pager supplies, a page shared copy-on-write written, a page the program's table lacks),
/// or else the program's, which enters its personality, or the process's own, which ends
/// it (SDD 20, f.12).
fn exception(thread: &Arc<Thread>, context: &mut UserContext) -> Flow {
    let ip = context.instruction_pointer();
    let exception = context.take_exception();
    let program = thread.mode.in_program();
    let mut refused = None;
    if let Some(CpuException::PageFault(info)) = &exception {
        let code = info.error_code.bits();
        let (access, word) = if code & 0b1_0000 != 0 {
            (Access::Execute, "execute")
        } else if code & 0b10 != 0 {
            (Access::Write, "write")
        } else {
            (Access::Read, "read")
        };
        let space = &thread.process.space;
        loop {
            match space.fault(info.addr as u64, access, program) {
                Ok(true) => return Flow::Continue,
                Ok(false) => break,
                Err(Error::ShouldWait) => match space.want(info.addr as u64, thread) {
                    Ok(()) => continue,
                    // The thread is ending.
                    Err(Error::BadState) => return Flow::Continue,
                    Err(_) => {
                        refused = Some(format!(
                            "its pager refused the page at {:#x} on {word}",
                            info.addr
                        ));
                        break;
                    }
                },
                Err(Error::QuotaExceeded) => {
                    refused = Some(format!(
                        "a page fault at {:#x} on {word} could not be charged: {}",
                        info.addr,
                        space.account().refusal(charge::ZERO_PAGE)
                    ));
                    break;
                }
                Err(e) => {
                    refused = Some(format!(
                        "a page fault at {:#x} on {word} could not be served: {}",
                        info.addr,
                        e.name()
                    ));
                    break;
                }
            }
        }
    }
    if program {
        let why = match &exception {
            Some(CpuException::PageFault(_)) => moruna_abi::personality::reason::PAGE_FAULT,
            _ => moruna_abi::personality::reason::EXCEPTION,
        };
        let detail = exception.as_ref().map(|e| {
            let (vector, code) = crate::personality::vector(e);
            let address = match e {
                CpuException::PageFault(info) => info.addr as u64,
                _ => 0,
            };
            (vector, code, address)
        });
        return crate::personality::enter(thread, context, why, detail);
    }
    // The personality's copy routine faulted on memory it may not reach: it resumes at its
    // recovery address with the faulting address in `rdx`.
    if let Some(program) = thread.process.space.program()
        && let Some((routine, recovery)) = &program.copy
        && routine.contains(&(ip as u64))
    {
        let address = match &exception {
            Some(CpuException::PageFault(info)) => Some(info.addr as u64),
            Some(CpuException::GeneralProtectionFault(_)) => Some(0),
            _ => None,
        };
        if let Some(address) = address {
            context.set_rip(*recovery as usize);
            context.set_rdx(address as usize);
            return Flow::Continue;
        }
    }
    let why = refused.unwrap_or_else(|| describe(exception, ip));
    thread.process.end(Exit::Fault(why));
    Flow::ThreadExit
}

impl Process {
    pub fn vm(&self) -> &Arc<ostd::mm::VmSpace> {
        &self.space.vm
    }

    fn is_running(&self) -> bool {
        let state = self.state.lock();
        matches!(state.life, Life::Running { .. }) && state.exit.is_none()
    }
}

/// A handle table (SDD 20, e.2), which `process_clone`'s `SHARE_HANDLES` shares between the
/// processes of a task, as Linux's `CLONE_FILES` does. Its slots are charged once, to the
/// account of the process it was made for, and its limit is its own. It holds handles to
/// the processes that share it, which hold it: when the last of them that ran ends, it is
/// emptied, which frees every process its handles held, so no cycle outlives the task.
pub struct Table {
    table: SpinLock<HandleTable<Handle>>,
    /// What its slots cost, charged as it grows (SDD 20 f.18).
    charge: SpinLock<Charge>,
    /// The processes that use it.
    members: SpinLock<Vec<Weak<Process>>>,
    /// How many of them run.
    running: AtomicUsize,
}

// A process's record covers its own table's.
const _: () = assert!(
    (core::mem::size_of::<Process>() + core::mem::size_of::<Table>()) as u64 <= charge::PROCESS
);

impl Table {
    fn new(salt: u32, account: &Arc<Account>) -> Result<Table, Error> {
        Ok(Table {
            table: SpinLock::new(HandleTable::new(salt)),
            charge: SpinLock::new(Charge::new(account, 0)?),
            members: SpinLock::new(Vec::new()),
            running: AtomicUsize::new(0),
        })
    }

    fn join(&self, process: &Arc<Process>) {
        let mut members = self.members.lock();
        members.retain(|m| m.strong_count() > 0);
        // With no room to count it, a kill of another member leaves it to end on its own.
        if members.try_reserve(1).is_ok() {
            members.push(Arc::downgrade(process));
        }
    }

    /// The live processes that share it with `process`.
    fn others(&self, process: &Process) -> Vec<Arc<Process>> {
        self.members
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|m| !core::ptr::eq(&**m, process))
            .collect()
    }
}

impl core::ops::Deref for Table {
    type Target = SpinLock<HandleTable<Handle>>;

    fn deref(&self) -> &Self::Target {
        &self.table
    }
}

fn current_thread() -> Option<Arc<Thread>> {
    let task = Task::current()?;
    task.data().downcast_ref::<Arc<Thread>>().cloned()
}

/// OSTD's pre-schedule hook: the thread leaving its CPU keeps its FPU registers and FS
/// base, and is charged the CPU time of its slice.
pub fn deactivate_current(_guard: &DisabledLocalIrqGuard) {
    if let Some(thread) = current_thread() {
        thread.fpu.lock().save();
        let mut fs = FsBase::default();
        fs.save();
        thread.fs_base.store(fs.addr() as u64, Ordering::Relaxed);
        let since = thread.on_cpu_since.load(Ordering::Relaxed);
        let used = clock::now().saturating_sub(since);
        thread.cpu_ns.fetch_add(used, Ordering::Relaxed);
        thread.process.cpu_ns.fetch_add(used, Ordering::Relaxed);
    }
}

/// OSTD's post-schedule hook: the address space of the thread now on this CPU is the
/// active one, so user copies and user mode always see their own process (KK-I4), and
/// its FPU registers and FS base are back.
pub fn activate_current() {
    if let Some(thread) = current_thread() {
        // Stream B1: the thread's own table, the program's while it runs its program.
        thread.activate();
        thread.fpu.lock().load();
        FsBase::new(thread.fs() as usize).load();
        thread.on_cpu_since.store(clock::now(), Ordering::Relaxed);
        let bit = crate::sched::ran_process_thread();
        thread.process.cpus.fetch_or(bit, Ordering::Relaxed);
    }
}
