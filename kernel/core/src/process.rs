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
    ABI_VERSION, Error, PAGE_SIZE, RawHandle, Signals, charge, limits, map, start_page, valid_name,
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
    syscall::{self, Flow},
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
    pub handles: SpinLock<HandleTable<Handle>>,
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
    /// What its handle table's slots cost, charged as the table grows (SDD 20 f.18).
    table_charge: SpinLock<Charge>,
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
        if !valid_name(name) {
            return Err(Error::InvalidArgs);
        }
        let name = core::str::from_utf8(name).map_err(|_| Error::InvalidArgs)?;
        let record = Charge::new(account, charge::PROCESS)?;
        let space = Space::new(account)?;
        let koid = NEXT_KOID.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(Process {
            name: String::from(name),
            koid,
            space,
            handles: SpinLock::new(HandleTable::new(salt(koid))),
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
            table_charge: SpinLock::new(Charge::new(account, 0)?),
            _record: record,
        }))
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
            let mut charged = self.table_charge.lock();
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

    /// Maps `len` bytes of `object` from `offset` into this process where the kernel
    /// chooses (`memory_map`); returns where.
    pub fn map_memory(
        &self,
        object: &Arc<MemoryObject>,
        offset: u64,
        len: u64,
        write: bool,
        execute: bool,
        writable: bool,
    ) -> Result<u64, Error> {
        let first = (offset / PAGE_SIZE) as usize;
        let backing = Backing::Object {
            memory: object.clone(),
            first,
            writable,
        };
        self.space.map(
            None,
            len,
            Kind::Mapping,
            prot(write, execute),
            backing,
            false,
        )
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
        Ok(())
    }

    /// Records why the process is ending (the first reason wins) and stops its threads.
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
        let handles = self.handles.lock().drain();
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

    pub fn killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
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

/// What a fault was.
enum Touch {
    /// A zero page's first touch, now mapped (SDD 20, f.12).
    Served,
    /// The process's own fault.
    Fault,
    /// A first touch whose page could not be had: why, for the process's end.
    Refused(String),
}

/// A page fault on a page with no entry that the area allows is a zero page's first
/// touch, served when its page can be charged and allocated. Anything else is the
/// process's fault.
fn first_touch(space: &Space, exception: &Option<CpuException>) -> Touch {
    let Some(CpuException::PageFault(info)) = exception else {
        return Touch::Fault;
    };
    let code = info.error_code.bits();
    // Bit 0: the page was present (a protection fault, never a first touch).
    if code & 0b1 != 0 {
        return Touch::Fault;
    }
    let (access, word) = if code & 0b1_0000 != 0 {
        (Access::Execute, "execute")
    } else if code & 0b10 != 0 {
        (Access::Write, "write")
    } else {
        (Access::Read, "read")
    };
    match space.fault(info.addr as u64, access) {
        Ok(true) => Touch::Served,
        Ok(false) => Touch::Fault,
        Err(Error::QuotaExceeded) => Touch::Refused(format!(
            "a page fault at {:#x} on {word} could not be charged: {}",
            info.addr,
            space.account().refusal(charge::ZERO_PAGE)
        )),
        Err(e) => Touch::Refused(format!(
            "a page fault at {:#x} on {word} could not be served: {}",
            info.addr,
            e.name()
        )),
    }
}

fn run(thread: Arc<Thread>, context: UserContext) {
    thread.process.vm().activate();
    FsBase::new(thread.fs() as usize).load();
    let mut user = UserMode::new(context);
    let hooks = Hooks(&thread);
    while !thread.killed() {
        match user.execute(&hooks) {
            ReturnReason::UserSyscall => {
                let number = user.context_mut().rax() as u64;
                let flow = if moruna_abi::nr::is_native(number) {
                    syscall::dispatch(&thread, user.context_mut())
                } else if let Some(forward) = thread.process.forward() {
                    foreign::call(&thread, &forward, user.context_mut())
                } else {
                    user.context_mut()
                        .set_rax(Error::BadSyscall.to_raw() as usize);
                    Flow::Continue
                };
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
                let context = user.context_mut();
                let ip = context.instruction_pointer();
                let exception = context.take_exception();
                let why = match first_touch(&thread.process.space, &exception) {
                    Touch::Served => continue,
                    Touch::Fault => describe(exception, ip),
                    Touch::Refused(why) => why,
                };
                thread.process.end(Exit::Fault(why));
                break;
            }
            ReturnReason::KernelEvent => {}
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

impl Process {
    pub fn vm(&self) -> &Arc<ostd::mm::VmSpace> {
        &self.space.vm
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
        thread.process.vm().activate();
        thread.fpu.lock().load();
        FsBase::new(thread.fs() as usize).load();
        thread.on_cpu_since.store(clock::now(), Ordering::Relaxed);
        let bit = crate::sched::ran_process_thread();
        thread.process.cpus.fetch_or(bit, Ordering::Relaxed);
    }
}
