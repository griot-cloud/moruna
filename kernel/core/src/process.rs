//! Processes and threads (SDD 20, f.2, f.8 and f.9).
//!
//! A process is an address space and a handle table. A thread is an OSTD task that
//! runs user code in its process's address space and returns to the kernel on every
//! syscall and exception. A process ends when its last thread ends, when one of its
//! threads calls `process_exit`, or when one of them faults; ending closes every handle
//! it holds, unmaps its address space and raises `TERMINATED`. Nothing else in the
//! system stops: the kernel reports the end on the console and the others run on.

use alloc::{
    collections::BTreeMap,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use moruna_abi::{ABI_VERSION, Error, PAGE_SIZE, Signals, limits, start_page, valid_name};
use moruna_handles::HandleTable;
use moruna_loader::elf;
use ostd::{
    arch::{
        cpu::context::{CpuException, UserContext},
        read_random, read_tsc,
    },
    early_println,
    mm::{CachePolicy, MAX_USERSPACE_VADDR, PageFlags, PageProperty, UFrame, VmSpace},
    power::{ExitCode, poweroff},
    sync::{SpinLock, Waker},
    task::{Task, TaskOptions, disable_preempt},
    user::{ReturnReason, UserContextApi, UserMode, UserModeHooks},
};

use crate::{
    memory::{DeviceMemory, MemoryObject},
    object::Handle,
    signal::Signaller,
    syscall::{self, Flow},
};

/// Where ELF segments may lie: above the null page, below the mapping area.
pub const IMAGE_RANGE: core::ops::Range<u64> = 0x1000..0x1000_0000_0000;
/// Where `memory_map`, stacks and the start page are placed.
pub const MAP_BASE: u64 = 0x1000_0000_0000;

fn map_end() -> u64 {
    MAX_USERSPACE_VADDR as u64 & !(PAGE_SIZE - 1)
}

/// Whether `addr` is a user address (used for entry points and stack tops).
pub fn is_user(addr: u64) -> bool {
    addr >= IMAGE_RANGE.start && addr <= map_end()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionKind {
    Image,
    Stack,
    StartPage,
    Mapping,
}

struct Region {
    end: u64,
    kind: RegionKind,
}

/// How a process ended.
pub enum Exit {
    Code(i64),
    Fault(String),
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
    pub vm: Arc<VmSpace>,
    pub handles: SpinLock<HandleTable<Handle>>,
    regions: SpinLock<BTreeMap<u64, Region>>,
    state: SpinLock<State>,
    pub signaller: Signaller,
    supervisor: bool,
}

fn salt(koid: u64) -> u32 {
    let r = read_random().unwrap_or(0) ^ read_tsc() ^ koid.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (r ^ (r >> 32)) as u32
}

fn pages(len: u64) -> Result<u64, Error> {
    len.checked_add(PAGE_SIZE - 1)
        .map(|v| v & !(PAGE_SIZE - 1))
        .ok_or(Error::OutOfRange)
}

fn flags(write: bool, execute: bool) -> PageFlags {
    let mut f = PageFlags::R;
    if write {
        f |= PageFlags::W;
    }
    if execute {
        f |= PageFlags::X;
    }
    f
}

impl Process {
    pub fn new(name: &[u8], supervisor: bool) -> Result<Arc<Process>, Error> {
        if !valid_name(name) {
            return Err(Error::InvalidArgs);
        }
        let name = core::str::from_utf8(name).map_err(|_| Error::InvalidArgs)?;
        let koid = NEXT_KOID.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(Process {
            name: String::from(name),
            vm: Arc::new(VmSpace::new()),
            handles: SpinLock::new(HandleTable::new(salt(koid))),
            regions: SpinLock::new(BTreeMap::new()),
            state: SpinLock::new(State {
                life: Life::Created { entry: None },
                exit: None,
                threads: Vec::new(),
            }),
            signaller: Signaller::new(Signals::NONE),
            supervisor,
        }))
    }

    /// Reserves `len` bytes (whole pages) in the mapping area, with an unmapped guard
    /// page on each side.
    fn reserve(&self, len: u64, kind: RegionKind) -> Result<u64, Error> {
        let len = pages(len)?;
        if len == 0 {
            return Err(Error::InvalidArgs);
        }
        let mut regions = self.regions.lock();
        let mut candidate = MAP_BASE + PAGE_SIZE;
        for (&start, region) in regions.range(MAP_BASE..) {
            let fits = candidate
                .checked_add(len)
                .and_then(|e| e.checked_add(PAGE_SIZE))
                .is_some_and(|e| e <= start);
            if fits {
                break;
            }
            candidate = region.end + PAGE_SIZE;
        }
        let end = candidate.checked_add(len).ok_or(Error::Limit)?;
        if end > map_end() {
            return Err(Error::Limit);
        }
        regions.insert(candidate, Region { end, kind });
        Ok(candidate)
    }

    fn release(&self, start: u64) {
        self.regions.lock().remove(&start);
    }

    fn map_frames(&self, va: u64, frames: &[UFrame], f: PageFlags) -> Result<(), Error> {
        let len = frames.len() as u64 * PAGE_SIZE;
        let guard = disable_preempt();
        let range = va as usize..(va + len) as usize;
        let mut cursor = self
            .vm
            .cursor_mut(&guard, &range)
            .map_err(|_| Error::InvalidArgs)?;
        for frame in frames {
            cursor.map(
                frame.clone(),
                PageProperty::new_user(f, CachePolicy::Writeback),
            );
        }
        Ok(())
    }

    fn unmap_range(&self, va: u64, len: u64) {
        let guard = disable_preempt();
        let range = va as usize..(va + len) as usize;
        if let Ok(mut cursor) = self.vm.cursor_mut(&guard, &range) {
            cursor.unmap(len as usize);
        }
    }

    /// Maps `len` bytes of `object` from `offset` into this process; returns where.
    pub fn map_memory(
        &self,
        object: &MemoryObject,
        offset: u64,
        len: u64,
        write: bool,
        execute: bool,
    ) -> Result<u64, Error> {
        let first = (offset / PAGE_SIZE) as usize;
        let count = (len / PAGE_SIZE) as usize;
        let frames = object
            .frames()
            .get(first..first + count)
            .ok_or(Error::OutOfRange)?;
        let va = self.reserve(len, RegionKind::Mapping)?;
        if let Err(e) = self.map_frames(va, frames, flags(write, execute)) {
            self.release(va);
            return Err(e);
        }
        Ok(va)
    }

    /// Maps `len` bytes of a device memory region from `offset`, uncached.
    pub fn map_device(
        &self,
        device: &DeviceMemory,
        offset: u64,
        len: u64,
        write: bool,
    ) -> Result<u64, Error> {
        let va = self.reserve(len, RegionKind::Mapping)?;
        let mapped = {
            let guard = disable_preempt();
            let range = va as usize..(va + len) as usize;
            match self.vm.cursor_mut(&guard, &range) {
                Ok(mut cursor) => {
                    cursor.map_iomem(
                        device.io.clone(),
                        PageProperty::new_user(flags(write, false), CachePolicy::Uncacheable),
                        len as usize,
                        offset as usize,
                    );
                    true
                }
                Err(_) => false,
            }
        };
        if !mapped {
            self.release(va);
            return Err(Error::InvalidArgs);
        }
        Ok(va)
    }

    /// Unmaps exactly one mapping made by `memory_map`.
    pub fn unmap(&self, va: u64, len: u64) -> Result<(), Error> {
        let len = pages(len)?;
        {
            let mut regions = self.regions.lock();
            match regions.get(&va) {
                Some(r) if r.kind == RegionKind::Mapping && r.end - va == len => {}
                _ => return Err(Error::InvalidArgs),
            }
            regions.remove(&va);
        }
        self.unmap_range(va, len);
        Ok(())
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
            let object = MemoryObject::new(len)?;
            let from = segment.offset as usize;
            let bytes = image
                .get(from..from + segment.filesz as usize)
                .ok_or(Error::InvalidArgs)?;
            object.write(segment.vaddr - start, bytes)?;
            {
                let mut regions = self.regions.lock();
                if regions
                    .range(..start + len)
                    .next_back()
                    .is_some_and(|(_, r)| r.end > start)
                {
                    return Err(Error::InvalidArgs);
                }
                regions.insert(
                    start,
                    Region {
                        end: start + len,
                        kind: RegionKind::Image,
                    },
                );
            }
            self.map_frames(
                start,
                object.frames(),
                flags(segment.write, segment.execute),
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
        if !is_user(entry) || handles.len() > limits::START_HANDLES_MAX {
            return Err(Error::InvalidArgs);
        }
        let stack = MemoryObject::new(limits::MAIN_STACK_BYTES)?;
        let stack_va = self.reserve(limits::MAIN_STACK_BYTES, RegionKind::Stack)?;
        self.map_frames(stack_va, stack.frames(), flags(true, false))?;

        let mut page = [0u8; PAGE_SIZE as usize];
        moruna_abi::write_u32(&mut page, start_page::MAGIC_OFFSET, start_page::MAGIC);
        moruna_abi::write_u32(&mut page, start_page::VERSION_OFFSET, ABI_VERSION as u32);
        moruna_abi::write_u32(&mut page, start_page::COUNT_OFFSET, handles.len() as u32);
        let name = self.name.as_bytes();
        page[start_page::NAME_OFFSET..start_page::NAME_OFFSET + name.len()].copy_from_slice(name);
        {
            let mut table = self.handles.lock();
            for (i, (handle, slot)) in handles.into_iter().enumerate() {
                let at = start_page::HEADER_SIZE + i * start_page::ENTRY_SIZE;
                let rights = handle.rights.bits();
                let value = table.insert(handle)?;
                moruna_abi::write_u32(&mut page, at + start_page::ENTRY_HANDLE, value);
                moruna_abi::write_u32(&mut page, at + start_page::ENTRY_RIGHTS, rights);
                let n = slot.len().min(limits::NAME_MAX);
                let name_at = at + start_page::ENTRY_NAME;
                page[name_at..name_at + n].copy_from_slice(&slot[..n]);
            }
        }
        let start = MemoryObject::new(PAGE_SIZE)?;
        start.write(0, &page)?;
        let start_va = self.reserve(PAGE_SIZE, RegionKind::StartPage)?;
        self.map_frames(start_va, start.frames(), flags(false, false))?;

        {
            let mut state = self.state.lock();
            state.life = Life::Running { live: 1 };
        }
        LIVE.fetch_add(1, Ordering::AcqRel);
        let stack_top = stack_va + limits::MAIN_STACK_BYTES;
        if let Err(e) = Thread::spawn(self, entry, stack_top, start_va) {
            self.end(Exit::Fault(String::from(
                "its first thread could not be created",
            )));
            self.thread_ended();
            return Err(e);
        }
        Ok(())
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
        {
            let mut state = self.state.lock();
            if state.exit.is_some() {
                return Err(Error::BadState);
            }
            match &mut state.life {
                Life::Running { live } => *live += 1,
                _ => return Err(Error::BadState),
            }
        }
        match Thread::spawn(self, entry, stack, arg) {
            Ok(thread) => Ok(thread),
            Err(e) => {
                self.thread_ended();
                Err(e)
            }
        }
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
                    state.exit.take().unwrap_or(Exit::Code(0))
                }
                Life::Created { .. } | Life::Ended => return,
            }
        };
        self.terminate(exit);
    }

    fn terminate(&self, exit: Exit) {
        let failed = match &exit {
            Exit::Code(code) => {
                early_println!("kk: process {} exited with code {}", self.name, code);
                *code != 0
            }
            Exit::Fault(what) => {
                early_println!(
                    "kk: process {} faulted: {}; it has ended and the others run on",
                    self.name,
                    what
                );
                true
            }
        };
        let handles = self.handles.lock().drain();
        drop(handles);
        let regions = core::mem::take(&mut *self.regions.lock());
        for (start, region) in regions {
            self.unmap_range(start, region.end - start);
        }
        self.signaller.update(Signals::NONE, Signals::TERMINATED);
        if self.supervisor && failed {
            SUPERVISOR_FAILED.store(true, Ordering::Release);
        }
        if LIVE.fetch_sub(1, Ordering::AcqRel) == 1 {
            let failed = SUPERVISOR_FAILED.load(Ordering::Acquire);
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
}

pub struct Thread {
    pub process: Arc<Process>,
    pub signaller: Signaller,
    killed: AtomicBool,
    waker: SpinLock<Option<Arc<Waker>>>,
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
        entry: u64,
        stack: u64,
        arg: u64,
    ) -> Result<Arc<Thread>, Error> {
        let thread = Arc::new(Thread {
            process: process.clone(),
            signaller: Signaller::new(Signals::NONE),
            killed: AtomicBool::new(false),
            waker: SpinLock::new(None),
        });
        process.state.lock().threads.push(Arc::downgrade(&thread));
        let mut context = UserContext::default();
        context.set_instruction_pointer(entry as usize);
        // The thread starts as if called: its stack pointer is 8 below a 16-byte boundary.
        context.set_stack_pointer((stack & !0xf) as usize - 8);
        context.set_rdi(arg as usize);
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
}

fn describe(context: &mut UserContext) -> String {
    let ip = context.instruction_pointer();
    match context.take_exception() {
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
    thread.process.vm.activate();
    let mut user = UserMode::new(context);
    let hooks = Hooks(&thread);
    while !thread.killed() {
        match user.execute(&hooks) {
            ReturnReason::UserSyscall => match syscall::dispatch(&thread, user.context_mut()) {
                Flow::Continue => {}
                Flow::ThreadExit => break,
                Flow::ProcessExit(code) => {
                    thread.process.end(Exit::Code(code));
                    break;
                }
            },
            ReturnReason::UserException => {
                let what = describe(user.context_mut());
                thread.process.end(Exit::Fault(what));
                break;
            }
            ReturnReason::KernelEvent => {}
        }
    }
    thread.set_waker(None);
    thread.signaller.update(Signals::NONE, Signals::TERMINATED);
    thread.process.thread_ended();
}

/// OSTD's post-schedule hook: the address space of the thread now on this CPU is the
/// active one, so user copies and user mode always see their own process (KK-I4).
pub fn activate_current() {
    if let Some(task) = Task::current()
        && let Some(thread) = task.data().downcast_ref::<Arc<Thread>>()
    {
        thread.process.vm.activate();
    }
}
