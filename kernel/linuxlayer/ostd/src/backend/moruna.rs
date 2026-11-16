// SPDX-License-Identifier: MPL-2.0

//! Moruna's core under the personality, through `moruna-sys`.
//!
//! Every thread the personality runs on has a stack of `STACK_SIZE` bytes aligned to its
//! size; at its base is the thread's block: its task, the program's frame, and the
//! program's vector state. Finding them takes no call and no TLS (the program owns FS,
//! and the core keeps no GS base per thread).
//!
//! The program's memory is reached through a fault-tolerant copy that runs in place: the
//! process is given the copy routines with `personality_set_with_copy` (`personality_set`
//! here), and the core resumes a fault in them at their recovery point. A personality whose
//! process has no routines copies through `process_read` and `process_write` on itself
//! instead (`copy_through_core`), which answer `Fault` safely.

use alloc::{boxed::Box, vec::Vec};
use core::{
    arch::global_asm,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
};

use moruna_sys::{DEADLINE_INFINITE, Error, Handle, RawHandle, Thread, raw};

use super::{Dir, FOREVER, shared};
use crate::sync::raw::RawLock;

/// A personality thread's stack: its size, and its alignment.
pub(crate) const STACK_SIZE: usize = linuxlayer_launch::STACK_SIZE as usize;

pub(crate) fn futex_wait(word: &AtomicU32, expected: u32, deadline: u64) -> bool {
    let deadline = if deadline == FOREVER {
        DEADLINE_INFINITE
    } else {
        deadline
    };
    !matches!(
        moruna_sys::futex_wait(word, expected, deadline),
        Err(Error::TimedOut)
    )
}

pub(crate) fn futex_wake(word: &AtomicU32, count: u32) -> u32 {
    moruna_sys::futex_wake(word, count).unwrap_or(0)
}

pub(crate) fn now() -> u64 {
    moruna_sys::now()
}

pub(crate) fn yield_now() {
    core::hint::spin_loop();
}

/// The size of a program's XSAVE area here; `query_xsave_area_size` is at most this.
pub(crate) const FPU_AREA: usize = 4096;

/// What a personality thread keeps at the base of its stack.
#[repr(C, align(64))]
pub(crate) struct ThreadBlock {
    /// The program's vector and x87 state, in the XSAVE layout, while the thread runs the
    /// personality: the core switches general registers and FS base only.
    pub(crate) fpu: [u8; FPU_AREA],
    /// The program's registers between entries; inside the region, as the core requires.
    pub(crate) frame: moruna_sys::personality::Frame,
    /// The thread's task (`Task::current`), zero for none.
    pub(crate) task: usize,
    /// The program's FS base, handed to the program in the frame and taken back.
    pub(crate) fs_base: usize,
    /// The program's GS base: kept, but the core loads none per thread.
    pub(crate) gs_base: usize,
    /// The thread's own handle, for interrupting it; zero for a launched first thread,
    /// which runs no program. Its spawner writes it before the thread runs anything.
    pub(crate) thread: AtomicU32,
    /// The process the thread is in, zero until its start says: a thread in the
    /// personality's statics would be every process's, since the task's processes share
    /// the image's data.
    pub(crate) process: AtomicU32,
    /// The interrupt level the thread runs at (`irq::InterruptLevel`): zero but while the
    /// ticker runs a tick.
    pub(crate) irq_level: core::sync::atomic::AtomicU8,
}

/// The calling thread's block.
fn block() -> *mut ThreadBlock {
    let sp: usize;
    // SAFETY: reads the stack pointer and touches nothing.
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack)) };
    (sp & !(STACK_SIZE - 1)) as *mut ThreadBlock
}

pub(crate) fn current_slot() -> usize {
    // SAFETY: every personality thread runs on a stack made by `new_stack`, whose block is
    // written before the thread runs; only the thread itself reaches it.
    unsafe { (*block()).task }
}

pub(crate) fn set_current_slot(value: usize) {
    // SAFETY: as above.
    unsafe { (*block()).task = value };
}

/// The calling thread's interrupt level.
pub(crate) fn irq_level() -> u8 {
    // SAFETY: the calling thread's block is its own.
    unsafe { (*block()).irq_level.load(Ordering::Relaxed) }
}

pub(crate) fn set_irq_level(level: u8) {
    // SAFETY: as above.
    unsafe { (*block()).irq_level.store(level, Ordering::Relaxed) };
}

/// What names the calling thread for [`interrupt`]: its block's address.
pub(crate) fn thread_token() -> usize {
    block() as usize
}

/// Interrupts the thread whose block is at `token` (`moruna_sys::personality`'s
/// `thread_interrupt`). A thread that has ended answers an error, which is ignored: its
/// block and handle stay while the personality runs.
pub(crate) fn interrupt(token: usize) {
    let block = token as *const ThreadBlock;
    // SAFETY: a token is a block's address, and blocks are never freed.
    let raw = unsafe { (*block).thread.load(Ordering::Acquire) };
    if raw != 0 {
        let thread = core::mem::ManuallyDrop::new(Thread(Handle::from_raw(raw)));
        let _ = moruna_sys::personality::thread_interrupt(&thread);
    }
}

/// A stack for a personality thread: its aligned base, with the thread's block written, and
/// its top. It is never freed while the personality runs.
pub(crate) fn new_stack() -> Option<(usize, usize)> {
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(2 * STACK_SIZE).ok()?;
    let start = v.as_mut_ptr() as usize;
    core::mem::forget(v);
    let base = (start + STACK_SIZE - 1) & !(STACK_SIZE - 1);
    // SAFETY: the block lies inside the reservation just leaked, at its aligned base.
    unsafe { init_block(base as *mut ThreadBlock) };
    Some((base, base + STACK_SIZE))
}

/// Writes a fresh block: no task, a zero frame, and the program's initial vector state (the
/// XSAVE header's components all at their initial state, MXCSR 0x1F80).
///
/// # Safety
///
/// `block` is valid for writes and 64-byte aligned.
pub(crate) unsafe fn init_block(block: *mut ThreadBlock) {
    // SAFETY: the caller upholds it; every field is plain bytes.
    unsafe {
        core::ptr::write_bytes(block, 0, 1);
        (&mut (*block).fpu)[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
    }
}

/// The program's FS base.
pub(crate) fn program_fs_base() -> usize {
    // SAFETY: the calling thread's block is its own.
    unsafe { (*block()).fs_base }
}

pub(crate) fn set_program_fs_base(value: usize) {
    // SAFETY: as above.
    unsafe { (*block()).fs_base = value };
}

/// The program's GS base.
pub(crate) fn program_gs_base() -> usize {
    // SAFETY: as above.
    unsafe { (*block()).gs_base }
}

pub(crate) fn set_program_gs_base(value: usize) {
    // SAFETY: as above.
    unsafe { (*block()).gs_base = value };
}

/// Copies the program's saved vector state into `area`.
pub(crate) fn program_fpu_save(area: &mut [u8]) {
    // SAFETY: the calling thread's block is its own.
    let fpu = unsafe { &(*block()).fpu };
    let n = area.len().min(FPU_AREA);
    area[..n].copy_from_slice(&fpu[..n]);
}

/// Sets the program's vector state from `area`, loaded when the program next runs.
pub(crate) fn program_fpu_load(area: &[u8]) {
    // SAFETY: as above.
    let fpu = unsafe { &mut (*block()).fpu };
    let n = area.len().min(FPU_AREA);
    fpu[..n].copy_from_slice(&area[..n]);
}

/// The XSAVE components a program's state holds: what the CPU enables, of the x87, SSE,
/// AVX and AVX-512 states.
fn xsave_mask() -> u64 {
    static MASK: AtomicUsize = AtomicUsize::new(0);
    let m = MASK.load(Ordering::Relaxed);
    if m != 0 {
        return m as u64;
    }
    // SAFETY: XGETBV with ECX 0 reads XCR0, readable in user mode on a CPU with XSAVE,
    // which the data OS's machines have.
    let xcr0 = unsafe { core::arch::x86_64::_xgetbv(0) };
    let m = xcr0 & crate::arch::cpu::context::XFEATURE_MASK_USER_RESTORE;
    MASK.store(m as usize, Ordering::Relaxed);
    m
}

/// Runs the program with `ctx` until it enters the personality again
/// (`moruna_sys::personality::personality_return`).
pub(crate) fn run_program(
    ctx: &mut crate::arch::cpu::context::UserContext,
) -> crate::user::ReturnReason {
    let block = block();
    let mask = xsave_mask();
    // SAFETY: the block is the calling thread's own and lies in the personality's region.
    let (frame, fpu) = unsafe { (&mut (*block).frame, (*block).fpu.as_mut_ptr()) };
    ctx.to_frame(&mut frame.0);
    // The FS base the program runs with is the block's (`FsBase::load` set it).
    let fs = crate::arch::cpu::context::frame::FS_BASE;
    // SAFETY: as above.
    let fs_base = unsafe { (*block).fs_base } as u64;
    frame.0[fs..fs + 8].copy_from_slice(&fs_base.to_le_bytes());
    let mut control = [0u32; 2];
    let rax: u64;
    // SAFETY: XRSTOR loads the program's state just before the call, and XSAVE stores it
    // just after, with no compiler code between that could use vector registers; the
    // personality's own MXCSR and x87 control word, which the ABI keeps across calls, are
    // put back after. The call reads and writes only the frame.
    unsafe {
        core::arch::asm!(
            "stmxcsr [r12]",
            "fnstcw [r12 + 4]",
            "mov eax, r13d",
            "mov edx, r14d",
            "xrstor64 [rsi]",
            "mov rax, {nr}",
            "syscall",
            "mov r8, rax",
            "mov eax, r13d",
            "mov edx, r14d",
            "xsave64 [rsi]",
            "ldmxcsr [r12]",
            "fldcw [r12 + 4]",
            nr = const moruna_abi::nr::PERSONALITY_RETURN,
            in("rdi") frame as *mut moruna_sys::personality::Frame,
            in("rsi") fpu,
            in("r12") control.as_mut_ptr(),
            in("r13") mask as u32,
            in("r14") (mask >> 32) as u32,
            out("r8") rax,
            clobber_abi("sysv64"),
        );
    }
    let reason = match Error::from_raw(rax) {
        Ok(reason) => reason,
        Err(e) => panic!("the personality entry refused the program's frame: {e:?}"),
    };
    let at = crate::arch::cpu::context::frame::REASON;
    frame.0[at..at + 8].copy_from_slice(&reason.to_le_bytes());
    // SAFETY: as above.
    unsafe {
        (*block).fs_base = u64::from_le_bytes(frame.0[fs..fs + 8].try_into().unwrap()) as usize
    };
    ctx.from_frame(&frame.0)
}

extern "C" fn thread_start(arg: u64) -> ! {
    // Its spawner writes the thread's handle into its block before anything may interrupt it.
    // SAFETY: the block is the thread's own, written by `new_stack`.
    let handle = unsafe { &(*block()).thread };
    while handle.load(Ordering::Acquire) == 0 {
        futex_wait(handle, 0, FOREVER);
    }
    // SAFETY: `spawn` leaked this box for the new thread, which takes it once.
    let f = unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) };
    f();
    moruna_sys::thread_exit()
}

pub(crate) fn spawn(f: Box<dyn FnOnce() + Send>) -> Result<(), ()> {
    let (base, top) = new_stack().ok_or(())?;
    // The new thread is in the spawner's process.
    // SAFETY: `base` is the new thread's block, which nothing reads yet; the calling
    // thread's block is its own.
    unsafe {
        let process = (*block()).process.load(Ordering::Acquire);
        (*(base as *const ThreadBlock))
            .process
            .store(process, Ordering::Release);
    }
    let arg = Box::into_raw(Box::new(f)) as u64;
    match Thread::spawn(thread_start, top as u64, arg) {
        Ok(thread) => {
            // SAFETY: the block at `base` is the new thread's, which only reads this word
            // until it is set.
            let handle = unsafe { &(*(base as *const ThreadBlock)).thread };
            handle.store(thread.0.into_raw(), Ordering::Release);
            futex_wake(handle, 1);
            Ok(())
        }
        Err(_) => {
            // SAFETY: the thread did not start, so the box is still ours.
            drop(unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) });
            Err(())
        }
    }
}

pub(crate) fn debug_write(bytes: &[u8]) {
    if let Some(console) = moruna_sys::start::peek("console") {
        let _ = raw::debug_write(console, bytes);
    }
}

pub(crate) fn random(buf: &mut [u8]) {
    let _ = moruna_sys::random(buf);
}

pub(crate) fn num_cpus() -> usize {
    moruna_sys::start::cpus().max(1)
}

pub(crate) fn exit(code: u32) -> ! {
    end_task();
    moruna_sys::exit(code as i64)
}

/// Kills every other process of the task: a task ends with its personality (a power-off or
/// a panic), whatever its other processes are doing.
fn end_task() {
    let me = self_process();
    let mut others = [0 as RawHandle; TASK_PROCESSES];
    let mut n = 0;
    // A panic may come while this thread holds the table's lock: then no process is killed
    // rather than none ending.
    let task = (0..100_000).find_map(|_| {
        core::hint::spin_loop();
        TASK.try_lock()
    });
    for &process in task.as_ref().map_or(&[][..], |t| t.processes.as_slice()) {
        if Some(process) != me {
            others[n] = process;
            n += 1;
        }
    }
    drop(task);
    for &process in &others[..n] {
        let process = core::mem::ManuallyDrop::new(moruna_sys::Process(Handle::from_raw(process)));
        // One already ended answers an error, which changes nothing.
        let _ = process.kill();
    }
}

/// Whether copies run in place: the process was given the copy routines.
static IN_PLACE: AtomicBool = AtomicBool::new(true);
/// The personality's own process: the program's.
/// Records the calling thread's process, a handle with `READ` and `WRITE` (the launcher
/// grants the first process its own as the slot `self`): the process its program runs in.
/// The threads it spawns are in the same process.
pub fn set_self_process(process: RawHandle) {
    // SAFETY: the calling thread's block is its own.
    unsafe { (*block()).process.store(process, Ordering::Release) };
}

/// The calling thread's process, once recorded.
pub fn self_process() -> Option<RawHandle> {
    // SAFETY: the calling thread's block is its own.
    match unsafe { (*block()).process.load(Ordering::Acquire) } {
        0 => None,
        v => Some(v),
    }
}

/// Copies go through `process_read` and `process_write` on the personality's own process:
/// for a process given no copy routines.
pub fn copy_through_core() {
    IN_PLACE.store(false, Ordering::Release);
}

/// Writes the calling thread's block, for a first thread its launcher gave a stack of
/// `STACK_SIZE` bytes aligned to its size (`linuxlayer_launch`).
///
/// # Safety
///
/// The calling thread runs on such a stack, and nothing else uses its block.
pub unsafe fn start_thread_block() {
    // SAFETY: the caller upholds it.
    unsafe { init_block(block()) };
}

/// Gives the created process `process` its personality region, `len` bytes from `base`,
/// with this image's copy routines (`moruna_sys::personality::personality_set_with_copy`):
/// a fault in them resumes at their recovery point with the fault's address in `rdx` and
/// every other register as it was. The process must map this personality's image at the
/// addresses it has here.
pub fn personality_set(
    process: &moruna_sys::Process,
    base: u64,
    len: u64,
) -> moruna_sys::Result<()> {
    let (start, end, recovery) = user_copy_range();
    moruna_sys::personality::personality_set_with_copy(
        process,
        base,
        len,
        start as u64..end as u64,
        recovery as u64,
    )
}

/// The copy routines' range and their recovery point, for the core.
pub fn user_copy_range() -> (usize, usize, usize) {
    unsafe extern "C" {
        static linuxlayer_user_start: u8;
        static linuxlayer_user_end: u8;
        static linuxlayer_user_fixup: u8;
    }
    (
        &raw const linuxlayer_user_start as usize,
        &raw const linuxlayer_user_end as usize,
        &raw const linuxlayer_user_fixup as usize,
    )
}

// Each routine leaves in rcx how much is left to do and ends at the shared epilogue,
// which returns it; a fault resumes at the epilogue with rcx as the fault left it, which
// `rep movsb` and `rep stosb` keep exact. The core puts the fault's address in rdx there,
// which no routine needs after its fault, and leaves the rest; no routine pushes, so the
// epilogue's `ret` finds the caller's return address.
global_asm!(
    ".pushsection .text.linuxlayer_user, \"ax\"",
    ".global linuxlayer_user_start",
    ".global linuxlayer_user_end",
    ".global linuxlayer_user_fixup",
    "linuxlayer_user_start:",
    // copy(dst = rdi, src = rsi, len = rdx) -> left
    "linuxlayer_copy:",
    "mov rcx, rdx",
    "rep movsb",
    "jmp linuxlayer_user_fixup",
    // set(dst = rdi, value = sil, len = rdx) -> left
    "linuxlayer_set:",
    "mov rcx, rdx",
    "mov eax, esi",
    "rep stosb",
    "jmp linuxlayer_user_fixup",
    // load(ptr = rdi, out = rsi) -> 0, or 4 on a fault
    "linuxlayer_load32:",
    "mov ecx, 4",
    "mov eax, dword ptr [rdi]",
    "mov dword ptr [rsi], eax",
    "xor ecx, ecx",
    "jmp linuxlayer_user_fixup",
    // cmpxchg(ptr = rdi, old = esi, new = edx, out = rcx) -> 0, or 4 on a fault
    "linuxlayer_cmpxchg32:",
    "mov r8, rcx",
    "mov ecx, 4",
    "mov eax, esi",
    "lock cmpxchg dword ptr [rdi], edx",
    "mov dword ptr [r8], eax",
    "xor ecx, ecx",
    "linuxlayer_user_end:",
    "linuxlayer_user_fixup:",
    "mov rax, rcx",
    "ret",
    ".popsection",
);

unsafe extern "C" {
    fn linuxlayer_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    fn linuxlayer_set(dst: *mut u8, value: u32, len: usize) -> usize;
    fn linuxlayer_load32(ptr: *const u32, out: *mut u32) -> usize;
    fn linuxlayer_cmpxchg32(ptr: *mut u32, old: u32, new: u32, out: *mut u32) -> usize;
}

/// A copy through the core, the largest prefix that succeeds, a page at a time once one
/// fails; returns how many bytes were not copied.
fn copy_via_core(dst: usize, src: usize, len: usize, to_program: bool) -> usize {
    let Some(process) = self_process() else {
        return len;
    };
    let call = |d: usize, s: usize, n: usize| {
        // SAFETY: the personality's side of the copy is its own memory, valid for `n`.
        let ok = unsafe {
            if to_program {
                let data = core::slice::from_raw_parts(s as *const u8, n);
                raw::process_write(process, d as u64, data)
            } else {
                let buf = core::slice::from_raw_parts_mut(d as *mut u8, n);
                raw::process_read(process, s as u64, buf)
            }
        };
        Error::from_raw(ok).is_ok()
    };
    if call(dst, src, len) {
        return 0;
    }
    let mut done = 0;
    while done < len {
        let program = if to_program { dst } else { src } + done;
        let n = (4096 - program % 4096).min(len - done);
        if !call(dst + done, src + done, n) {
            break;
        }
        done += n;
    }
    len - done
}

/// Copies `len` bytes, one side or both the program's memory; returns how many were not
/// copied.
///
/// # Safety
///
/// The personality's side must be valid for `len` bytes.
pub(crate) unsafe fn copy_fallible(dst: *mut u8, src: *const u8, len: usize, dir: Dir) -> usize {
    if IN_PLACE.load(Ordering::Acquire) {
        // SAFETY: a fault in the routine resumes at its recovery point.
        return unsafe { linuxlayer_copy(dst, src, len) };
    }
    match dir {
        Dir::ToProgram => copy_via_core(dst as usize, src as usize, len, true),
        Dir::FromProgram => copy_via_core(dst as usize, src as usize, len, false),
        Dir::Both => {
            let mut page = [0u8; 4096];
            let mut done = 0;
            while done < len {
                let n = (len - done).min(4096);
                let p = page.as_mut_ptr() as usize;
                let got = n - copy_via_core(p, src as usize + done, n, false);
                let put = got - copy_via_core(dst as usize + done, p, got, true);
                done += put;
                if put != n {
                    break;
                }
            }
            len - done
        }
    }
}

/// Fills `len` bytes of the program's memory with `value`; returns how many were not
/// filled.
///
/// # Safety
///
/// None beyond the copy's.
pub(crate) unsafe fn set_fallible(dst: *mut u8, value: u8, len: usize) -> usize {
    if IN_PLACE.load(Ordering::Acquire) {
        // SAFETY: as for `copy_fallible`.
        return unsafe { linuxlayer_set(dst, value as u32, len) };
    }
    let page = [value; 4096];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(4096);
        let left = copy_via_core(dst as usize + done, page.as_ptr() as usize, n, true);
        done += n - left;
        if left != 0 {
            break;
        }
    }
    len - done
}

/// # Safety
///
/// `ptr` is aligned.
pub(crate) unsafe fn load_u32_fallible(ptr: *const u32) -> Option<u32> {
    let mut out = 0u32;
    if IN_PLACE.load(Ordering::Acquire) {
        // SAFETY: as for `copy_fallible`.
        return (unsafe { linuxlayer_load32(ptr, &mut out) } == 0).then_some(out);
    }
    let left = copy_via_core((&raw mut out) as usize, ptr as usize, 4, false);
    (left == 0).then_some(out)
}

/// # Safety
///
/// `ptr` is aligned.
pub(crate) unsafe fn cmpxchg_u32_fallible(ptr: *mut u32, old: u32, new: u32) -> Option<u32> {
    if !IN_PLACE.load(Ordering::Acquire) {
        // Not atomic through the core: refused until copies run in place.
        return None;
    }
    let mut out = 0u32;
    // SAFETY: as for `copy_fallible`.
    (unsafe { linuxlayer_cmpxchg32(ptr, old, new, &mut out) } == 0).then_some(out)
}

/// The core's calls for the task's shared memory (`shared.rs`).
struct CoreCalls;

impl shared::Core for CoreCalls {
    type Process = RawHandle;
    type Object = moruna_sys::Memory;

    fn create(&mut self, len: usize) -> crate::Result<moruna_sys::Memory> {
        moruna_sys::Memory::create(len as u64).map_err(|_| crate::Error::NoMemory)
    }

    fn map(
        &mut self,
        process: RawHandle,
        object: &moruna_sys::Memory,
        addr: usize,
        len: usize,
    ) -> crate::Result<()> {
        let flags = moruna_sys::map::READ | moruna_sys::map::WRITE;
        Error::from_raw(raw::process_map(
            process,
            object.0.raw(),
            0,
            len as u64,
            addr as u64,
            flags,
        ))
        .map(|_| ())
        .map_err(|_| crate::Error::NoMemory)
    }

    fn unmap(&mut self, process: RawHandle, addr: usize, len: usize) {
        let _ = raw::process_unmap(process, addr as u64, len as u64);
    }
}

/// The most processes a task has.
const TASK_PROCESSES: usize = 64;

/// The task's processes and its heap's addresses: nothing allocates under this lock, which
/// the heap's source takes with the heap's own held.
struct Task {
    processes: shared::Processes<RawHandle, TASK_PROCESSES>,
    heap: shared::Bump,
}

static TASK: RawLock<Task> = RawLock::new(Task {
    processes: shared::Processes::new(0),
    heap: shared::Bump::new(
        linuxlayer_launch::HEAP.start as usize..linuxlayer_launch::HEAP.end as usize,
    ),
});

/// The frames' chunks. Lock order: this, then the heap's, then [`TASK`].
static TASK_MEMORY: RawLock<Option<shared::TaskMemory<CoreCalls>>> = RawLock::new(None);

/// Why the task's memory cannot start.
#[derive(Debug)]
pub enum TaskMemoryError {
    /// The heap allocated before its source was set.
    HeapStarted(moruna_sys::heap::SourceError),
}

/// Starts the task's shared memory in its first process, `own`: the heap's memory from now
/// on is mapped in every process of the task. Before anything is allocated.
pub fn start_task_memory(own: RawHandle) -> Result<(), TaskMemoryError> {
    TASK.lock()
        .processes
        .add(own)
        .expect("an empty table has room");
    moruna_sys::heap::set_source(heap_map, heap_unmap).map_err(TaskMemoryError::HeapStarted)
}

/// The heap's source: `len` bytes at the next heap address, in every process of the task.
fn heap_map(len: usize) -> Option<usize> {
    let mut task = TASK.lock();
    let addr = task.heap.take(len)?;
    let object = moruna_sys::Memory::create(len as u64).ok()?;
    shared::map_everywhere(
        &mut CoreCalls,
        task.processes.as_slice(),
        &object,
        addr,
        len,
    )
    .ok()?;
    // The mappings keep the pages; the handle closes.
    drop(object);
    Some(addr)
}

/// Gives back what [`heap_map`] mapped, in every process of the task.
fn heap_unmap(addr: usize, len: usize) {
    let task = TASK.lock();
    for &process in task.processes.as_slice() {
        shared::Core::unmap(&mut CoreCalls, process, addr, len);
    }
}

/// The task's processes now, copied out so the caller may allocate.
fn processes() -> ([RawHandle; TASK_PROCESSES], usize) {
    let task = TASK.lock();
    let mut list = [0; TASK_PROCESSES];
    let now = task.processes.as_slice();
    list[..now.len()].copy_from_slice(now);
    (list, now.len())
}

/// Runs `f` on the frames' chunks.
fn with_task<R>(f: impl FnOnce(&mut shared::TaskMemory<CoreCalls>) -> R) -> R {
    let mut task = TASK_MEMORY.lock();
    let task = task.get_or_insert_with(|| {
        let range = linuxlayer_launch::SHARED;
        shared::TaskMemory::new(CoreCalls, range.start as usize..range.end as usize)
    });
    f(task)
}

/// A new object of at least `len` bytes (at most `limits::MEMORY_MAX_BYTES`), mapped
/// readable and writable at one address in every process of the task; that address.
pub fn shared_map(len: usize) -> crate::Result<usize> {
    with_task(|task| {
        let (list, n) = processes();
        task.map(len, &list[..n])
    })
}

/// Unmaps the object [`shared_map`] mapped at `addr` from every process of the task.
pub fn shared_unmap(addr: usize) -> crate::Result<()> {
    with_task(|task| {
        let (list, n) = processes();
        task.unmap(addr, &list[..n])
    })
}

/// The shared object holding `addr`: its start, its handle (the same in every process of
/// the task) and its length.
pub fn shared_chunk(addr: usize) -> Option<(usize, RawHandle, usize)> {
    with_task(|task| {
        task.chunk(addr)
            .map(|(start, object, len)| (start, object.0.raw(), len))
    })
}

/// Runs `clone`, a `process_clone` of a process of the task sharing its handle table, with
/// the task's memory held still, and adds the clone it makes: the clone keeps every object
/// mapping, so no chunk is missing from it. `clone` must not allocate.
pub fn clone_process<E>(clone: impl FnOnce() -> Result<RawHandle, E>) -> Result<RawHandle, E> {
    with_task(|_| {
        let mut task = TASK.lock();
        let child = clone()?;
        if task.processes.add(child).is_err() {
            panic!("a task has more than {TASK_PROCESSES} processes");
        }
        Ok(child)
    })
}

/// Takes `process` out of the task: it has ended.
pub fn task_leave(process: RawHandle) {
    with_task(|_| TASK.lock().processes.remove(process));
}

/// The process resource `fork` clones the task's processes from: one value in every process
/// of the task, whose processes share one handle table.
static PROCESS_RESOURCE: AtomicU32 = AtomicU32::new(0);

/// Records the process resource the launcher granted (`linuxlayer_launch::slot::PROCESSES`).
pub fn set_process_resource(resource: RawHandle) {
    PROCESS_RESOURCE.store(resource, Ordering::Release);
}

/// A `process_clone` of the task's process `parent` sharing its handle table, added to the
/// task with every shared object mapped (`clone_process`); not started.
pub fn fork_process(parent: RawHandle) -> crate::Result<RawHandle> {
    let resource = PROCESS_RESOURCE.load(Ordering::Acquire);
    if resource == 0 {
        return Err(crate::Error::AccessDenied);
    }
    let resource = core::mem::ManuallyDrop::new(Handle::from_raw(resource));
    let parent = core::mem::ManuallyDrop::new(moruna_sys::Process(Handle::from_raw(parent)));
    clone_process(|| {
        moruna_sys::personality::process_clone(
            &resource,
            Some(&parent),
            None,
            moruna_abi::process::SHARE_HANDLES,
        )
        .map(|child| child.0.into_raw())
    })
    .map_err(|_| crate::Error::NoMemory)
}

/// Takes `process` out of the task and closes its handle: it has ended and been reaped.
pub fn task_release(process: RawHandle) {
    task_leave(process);
    drop(moruna_sys::Process(Handle::from_raw(process)));
}

// A first thread in another process of the task: its block is on a stack in the task's
// heap, which every process maps, and its argument at the stack pointer.
global_asm!(
    ".globl linuxlayer_first_thread",
    "linuxlayer_first_thread:",
    "mov rdi, [rsp]",
    "and rsp, -16",
    "call {entry}",
    "ud2",
    entry = sym first_thread_entry,
);

extern "C" fn first_thread_entry(arg: u64) -> ! {
    // SAFETY: `spawn_in` leaked this box for the thread, which takes it once.
    let f = unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) };
    // A thread of its own runs `f`, so it has a handle to be interrupted by; this one,
    // launched with none, ends.
    if spawn(*f).is_err() {
        exit(1);
    }
    moruna_sys::thread_exit()
}

/// Runs `f` on a new thread in `process`, a process of the task: the caller's own (or `0`)
/// spawns it there; another's, created and not started, is started with it.
pub(crate) fn spawn_in(process: RawHandle, f: Box<dyn FnOnce() + Send>) -> Result<(), ()> {
    if process == 0 || self_process() == Some(process) {
        return spawn(f);
    }
    let (base, top) = new_stack().ok_or(())?;
    // SAFETY: `base` is the new thread's block, which nothing reads yet.
    unsafe {
        (*(base as *const ThreadBlock))
            .process
            .store(process, Ordering::Release)
    };
    let arg = Box::into_raw(Box::new(f));
    let sp = top - linuxlayer_launch::STACK_ARGS as usize;
    // SAFETY: `sp` is inside the new stack, which nothing uses yet.
    unsafe { (sp as *mut u64).write(arg as u64) };
    unsafe extern "C" {
        fn linuxlayer_first_thread();
    }
    let target = core::mem::ManuallyDrop::new(moruna_sys::Process(Handle::from_raw(process)));
    match target.launch(linuxlayer_first_thread as *const () as u64, sp as u64, 0) {
        Ok(_) => Ok(()),
        Err(_) => {
            // SAFETY: the thread did not start, so the box is still ours.
            drop(unsafe { Box::from_raw(arg) });
            Err(())
        }
    }
}
