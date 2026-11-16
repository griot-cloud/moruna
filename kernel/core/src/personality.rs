//! The user-space personality's mechanisms (ABI 2.6; `kernel-is-the-machine.md` §4.1,
//! `architecture/data-os.md` §4): stream B1's module. Their arguments are
//! `moruna_sys::personality`'s. None of them knows Linux. Only stream B1 writes this file.
//!
//! - **The entry (§4.1 a).** A thread of a process with a personality region runs its
//!   program on the program's page table, where the region is absent. `personality_return`
//!   loads the program's registers from a frame in the region and runs it; a system call
//!   (any number), an exception the core does not serve, or an interrupt brings the thread
//!   back, the frame holding the program's registers and `personality_return` returning
//!   the reason. The thread switches its general registers, FS base and page table; the
//!   vector registers are the personality's to save.
//! - **Snapshots and clones (§4.1 b)** are the memory objects' (`memory.rs`) and the
//!   space's (`vm.rs`); here are their calls.
//! - **The pager (§4.1 c)** likewise.
//! - **Thread state and interruption (§4.1 d).** A thread asleep in the core publishes
//!   the registers it will resume with, which a holder may read and change. An interrupt
//!   stops its wait (`killed` is true while one is pending, so every wait of the core
//!   stops), and the wait returns `Interrupted`; a thread running its program enters its
//!   personality instead.

use alloc::{format, string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

use moruna_abi::{
    Error, PAGE_SIZE, RawHandle, ResourceKind, Rights, nr,
    personality::{self as frame, reason},
    read_u64, write_u64,
};
use ostd::{
    arch::cpu::context::{CpuException, FsBase, UserContext},
    sync::SpinLock,
};

use crate::{
    account::Account,
    foreign,
    memory::MemoryObject,
    object::{Handle, Object},
    process::{Process, Thread},
    syscall::{self, Flow},
    usercopy,
};

/// What a call returns: `rax` and `rdx`, or the error.
pub(crate) type Answer = Result<(u64, u64), Error>;

/// The registers of one side of a thread, its FS base, and the frame's address.
struct Saved {
    context: UserContext,
    fs: u64,
    frame: u64,
}

#[derive(Default)]
struct State {
    /// While the program runs: the personality's registers at its `personality_return`.
    personality: Option<Saved>,
    /// From `personality_return`'s checks to the switch: the program's registers.
    pending: Option<Saved>,
    /// While asleep in the core: the registers it will resume with.
    asleep: Option<Saved>,
    /// Whether a holder wrote `asleep`.
    written: bool,
}

/// A thread's side of the personality, held in `Thread::mode`.
pub(crate) struct Mode {
    /// Whether the thread runs its program, on the program's table.
    program: AtomicBool,
    /// An interrupt not yet delivered.
    interrupt: AtomicBool,
    state: SpinLock<State>,
}

impl Mode {
    pub(crate) fn new() -> Mode {
        Mode {
            program: AtomicBool::new(false),
            interrupt: AtomicBool::new(false),
            state: SpinLock::new(State::default()),
        }
    }

    pub(crate) fn in_program(&self) -> bool {
        self.program.load(Ordering::Acquire)
    }

    pub(crate) fn interrupt_pending(&self) -> bool {
        self.interrupt.load(Ordering::Acquire)
    }

    pub(crate) fn raise(&self) {
        self.interrupt.store(true, Ordering::Release);
    }

    fn take_interrupt(&self) -> bool {
        self.interrupt.swap(false, Ordering::AcqRel)
    }
}

/// The native calls that may sleep in the core.
fn sleeps(number: u64) -> bool {
    matches!(
        number,
        nr::WAIT | nr::FUTEX_WAIT | nr::PORT_CALL | nr::BATCH_GET | nr::PLANE_GET
    )
}

/// The user-mode loop's syscall branch: a program's call enters its personality; a native
/// call is dispatched, a foreign one forwarded.
pub(crate) fn syscall(thread: &Arc<Thread>, ctx: &mut UserContext) -> Flow {
    if thread.mode.in_program() {
        return enter(thread, ctx, reason::SYSCALL, None);
    }
    let number = ctx.rax() as u64;
    if nr::is_native(number) {
        let asleep = sleeps(number);
        if asleep {
            fall_asleep(thread, ctx);
        }
        let flow = syscall::dispatch(thread, ctx);
        if asleep {
            wake(
                thread,
                ctx,
                Error::BadState.to_raw(),
                Error::Interrupted.to_raw(),
            );
        }
        if number == nr::PERSONALITY_RETURN {
            to_program(thread, ctx);
        }
        flow
    } else if let Some(forward) = thread.process.forward() {
        let flow = foreign::call(thread, &forward, ctx);
        // A fast call's sleep an interrupt stopped returned `EINTR`: the interrupt is spent.
        if ctx.rax() as u64 == EINTR && !thread.dying() {
            thread.mode.take_interrupt();
        }
        flow
    } else {
        ctx.set_rax(Error::BadSyscall.to_raw() as usize);
        Flow::Continue
    }
}

/// Linux's `-EINTR`, which a forwarded process's interrupted sleep returns.
const EINTR: u64 = 4u64.wrapping_neg();

/// Publishes the registers a thread entering a wait will resume with.
pub(crate) fn fall_asleep(thread: &Thread, ctx: &UserContext) {
    let mut state = thread.mode.state.lock();
    state.asleep = Some(Saved {
        context: ctx.clone(),
        fs: thread.fs(),
        frame: 0,
    });
    state.written = false;
}

/// Takes them back: a holder's changes apply, all but `rax`; a wait an interrupt stopped
/// (`rax` is `stopped`) returns `interrupted`, and the interrupt is spent.
pub(crate) fn wake(thread: &Thread, ctx: &mut UserContext, stopped: u64, interrupted: u64) {
    let (saved, written) = {
        let mut state = thread.mode.state.lock();
        (state.asleep.take(), core::mem::take(&mut state.written))
    };
    if let (Some(saved), true) = (saved, written) {
        let rax = ctx.rax();
        *ctx = saved.context;
        ctx.set_rax(rax);
        if saved.fs != thread.fs() {
            thread.set_fs(saved.fs);
        }
    }
    if ctx.rax() as u64 == stopped && !thread.dying() && thread.mode.take_interrupt() {
        ctx.set_rax(interrupted as usize);
    }
}

/// After `personality_return` passed its checks: the thread saves the personality's
/// registers and runs the program, unless an interrupt is pending, which returns at once.
fn to_program(thread: &Arc<Thread>, ctx: &mut UserContext) {
    let Some(program) = thread.mode.state.lock().pending.take() else {
        return;
    };
    if thread.mode.take_interrupt() {
        ctx.set_rax(reason::INTERRUPTED as usize);
        return;
    }
    let personality = Saved {
        context: ctx.clone(),
        fs: thread.fs(),
        frame: program.frame,
    };
    thread.mode.state.lock().personality = Some(personality);
    *ctx = program.context;
    thread.set_fs(program.fs);
    thread.mode.program.store(true, Ordering::Release);
    thread.activate();
}

/// The program enters its personality for `why` (a reason, and an exception's vector,
/// error code and address): its registers go to the frame, and the thread resumes the
/// personality where its `personality_return` left it, the reason in `rax`.
pub(crate) fn enter(
    thread: &Arc<Thread>,
    ctx: &mut UserContext,
    why: u64,
    exception: Option<(u64, u64, u64)>,
) -> Flow {
    let saved = thread.mode.state.lock().personality.take();
    let Some(saved) = saved else {
        return broken(
            thread,
            String::from("its program ran with no personality to enter"),
        );
    };
    thread.mode.program.store(false, Ordering::Release);
    thread.activate();
    // The program's FS base as it is now: it may have set it itself.
    let mut fs = FsBase::default();
    fs.save();
    let (vector, code, address) = exception.unwrap_or((0, 0, 0));
    let mut bytes = encode(ctx, fs.addr() as u64);
    write_u64(&mut bytes, frame::REASON, why);
    write_u64(&mut bytes, frame::VECTOR, vector);
    write_u64(&mut bytes, frame::ERROR_CODE, code);
    write_u64(&mut bytes, frame::FAULT_ADDRESS, address);
    if usercopy::write(&thread.process.space, saved.frame, &bytes).is_err() {
        return broken(
            thread,
            format!(
                "its personality's frame at {:#x} could not be written",
                saved.frame
            ),
        );
    }
    *ctx = saved.context;
    ctx.set_rax(why as usize);
    thread.set_fs(saved.fs);
    Flow::Continue
}

fn broken(thread: &Thread, why: String) -> Flow {
    thread.process.end(crate::process::Exit::Fault(why));
    Flow::ThreadExit
}

/// An interrupt that arrived while the thread ran its program: it enters its personality.
pub(crate) fn kernel_event(thread: &Arc<Thread>, ctx: &mut UserContext) -> Flow {
    if thread.mode.in_program() && !thread.dying() && thread.mode.take_interrupt() {
        return enter(thread, ctx, reason::INTERRUPTED, None);
    }
    Flow::Continue
}

/// An exception's vector and error code, as the frame carries them.
pub(crate) fn vector(exception: &CpuException) -> (u64, u64) {
    use CpuException as E;
    let vector = match exception {
        E::DivisionError => 0,
        E::Debug => 1,
        E::NonMaskableInterrupt => 2,
        E::BreakPoint => 3,
        E::Overflow => 4,
        E::BoundRangeExceeded => 5,
        E::InvalidOpcode => 6,
        E::DeviceNotAvailable => 7,
        E::DoubleFault => 8,
        E::CoprocessorSegmentOverrun => 9,
        E::InvalidTss(_) => 10,
        E::SegmentNotPresent(_) => 11,
        E::StackSegmentFault(_) => 12,
        E::GeneralProtectionFault(_) => 13,
        E::PageFault(info) => return (14, info.error_code.bits() as u64),
        E::X87FloatingPointException => 16,
        E::AlignmentCheck => 17,
        E::MachineCheck => 18,
        E::SIMDFloatingPointException => 19,
        E::VirtualizationException => 20,
        E::ControlProtectionException => 21,
        E::HypervisorInjectionException => 28,
        E::VMMCommunicationException => 29,
        E::SecurityException => 30,
        E::Reserved => 31,
    };
    (vector, 0)
}

/// A frame from registers: the general registers, the instruction pointer, the flags
/// and the FS base. The GS base is not kept per thread, and reads zero.
fn encode(ctx: &UserContext, fs: u64) -> [u8; frame::LEN] {
    let mut bytes = [0u8; frame::LEN];
    let g = ctx.general_regs();
    let registers = [
        g.rax, g.rbx, g.rcx, g.rdx, g.rsi, g.rdi, g.rbp, g.rsp, g.r8, g.r9, g.r10, g.r11, g.r12,
        g.r13, g.r14, g.r15,
    ];
    for (i, r) in registers.iter().enumerate() {
        write_u64(&mut bytes, frame::REGISTERS + 8 * i, *r as u64);
    }
    write_u64(&mut bytes, frame::RIP, g.rip as u64);
    write_u64(&mut bytes, frame::RFLAGS, ctx.rflags() as u64);
    write_u64(&mut bytes, frame::FS_BASE, fs);
    bytes
}

/// Registers from a frame: `InvalidArgs` for an instruction pointer, stack pointer or FS
/// base outside the lower half, which no program may hold. Only the flags a program may
/// change are taken.
fn decode(bytes: &[u8]) -> Result<(UserContext, u64), Error> {
    let word = |at: usize| read_u64(bytes, at).ok_or(Error::InvalidArgs);
    let mut ctx = UserContext::default();
    let mut r = [0u64; 16];
    for (i, v) in r.iter_mut().enumerate() {
        *v = word(frame::REGISTERS + 8 * i)?;
    }
    let (rip, fs) = (word(frame::RIP)?, word(frame::FS_BASE)?);
    if !foreign::canonical(rip) || !foreign::canonical(r[7]) || !foreign::canonical(fs) {
        return Err(Error::InvalidArgs);
    }
    let g = ctx.general_regs_mut();
    [
        g.rax, g.rbx, g.rcx, g.rdx, g.rsi, g.rdi, g.rbp, g.rsp, g.r8, g.r9, g.r10, g.r11, g.r12,
        g.r13, g.r14, g.r15,
    ] = r.map(|v| v as usize);
    g.rip = rip as usize;
    ctx.set_rflags(word(frame::RFLAGS)? as usize);
    Ok((ctx, fs))
}

// The calls. Each looks its handles up in the caller's table, the type before the rights,
// as every native call does (KK-I2).

fn typed<T>(
    p: &Process,
    value: u64,
    rights: Rights,
    cast: impl Fn(&Object) -> Option<T>,
) -> Result<T, Error> {
    let value = RawHandle::try_from(value).map_err(|_| Error::BadHandle)?;
    let handle = p.handles.lock().get(value)?.clone();
    let object = cast(&handle.object).ok_or(Error::WrongType)?;
    if !handle.rights.contains(rights) {
        return Err(Error::AccessDenied);
    }
    Ok(object)
}

fn process(o: &Object) -> Option<Arc<Process>> {
    match o {
        Object::Process(p) => Some(p.clone()),
        _ => None,
    }
}

fn thread_of(o: &Object) -> Option<Arc<Thread>> {
    match o {
        Object::Thread(t) => Some(t.clone()),
        _ => None,
    }
}

fn memory(o: &Object) -> Option<Arc<MemoryObject>> {
    match o {
        Object::Memory(m) => Some(m.clone()),
        _ => None,
    }
}

/// The account a call charges: the caller's own for zero, an account or a process's.
fn account(p: &Process, value: u64) -> Result<Arc<Account>, Error> {
    if value == 0 {
        return Ok(p.account().clone());
    }
    typed(p, value, Rights::WRITE, |o| match o {
        Object::Account(a) => Some(a.clone()),
        Object::Process(q) => Some(q.account().clone()),
        _ => None,
    })
}

fn insert(p: &Process, object: Object) -> Answer {
    Ok((p.insert(Handle::new(object))? as u64, 0))
}

/// Whole pages: `(first page, page count)` of `offset .. offset + len`, `len` not zero.
fn pages(offset: u64, len: u64) -> Result<(usize, usize), Error> {
    if len == 0 || !offset.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
        return Err(Error::InvalidArgs);
    }
    Ok(((offset / PAGE_SIZE) as usize, (len / PAGE_SIZE) as usize))
}

/// `personality_set(process, base, len, copy_start, copy_end, recovery)`: a created
/// process's personality region, and its fault-tolerant copy routine (none when all three
/// are zero).
pub(crate) fn personality_set(p: &Process, a: [u64; 6]) -> Answer {
    let target = typed(p, a[0], Rights::WRITE, process)?;
    if !target.is_created() || target.forward().is_some() {
        return Err(Error::BadState);
    }
    let copy = (a[3..] != [0, 0, 0]).then_some((a[3]..a[4], a[5]));
    target.space.set_program(a[1], a[2], copy)?;
    Ok((0, 0))
}

/// `personality_return(frame) -> reason`: checks the frame and the program's registers,
/// which the loop then switches to (`to_program`).
pub(crate) fn personality_return(caller: &Arc<Thread>, a: [u64; 6]) -> Answer {
    let space = &caller.process.space;
    let program = space.program().ok_or(Error::BadState)?;
    let at = a[0];
    let end = at
        .checked_add(frame::LEN as u64)
        .ok_or(Error::InvalidArgs)?;
    if !at.is_multiple_of(8) || at < program.region.start || end > program.region.end {
        return Err(Error::InvalidArgs);
    }
    let bytes = usercopy::read(space, at, frame::LEN)?;
    let (context, fs) = decode(&bytes)?;
    caller.mode.state.lock().pending = Some(Saved {
        context,
        fs,
        frame: at,
    });
    Ok((0, 0))
}

/// `memory_snapshot(memory, offset, len, account) -> memory`.
pub(crate) fn memory_snapshot(p: &Process, a: [u64; 6]) -> Answer {
    let source = typed(p, a[0], Rights::READ, memory)?;
    let account = account(p, a[3])?;
    let (first, count) = pages(a[1], a[2])?;
    let snapshot = MemoryObject::snapshot(&source, first, count, &account)?;
    insert(p, Object::Memory(Arc::new(snapshot)))
}

/// `process_clone(resource, process, account) -> process`.
pub(crate) fn process_clone(p: &Process, a: [u64; 6]) -> Answer {
    typed(p, a[0], Rights::WRITE, |o| match o {
        Object::Resource(r) => Some(r.kind == ResourceKind::Process),
        _ => None,
    })?
    .then_some(())
    .ok_or(Error::AccessDenied)?;
    let source = typed(p, a[1], Rights::READ | Rights::WRITE, process)?;
    let account = account(p, a[2])?;
    let child = Process::new(source.name.as_bytes(), false, &account)?;
    source.space.clone_into(&child.space)?;
    insert(p, Object::Process(child))
}

/// `pager_create(port, size, key, account) -> memory`: the endpoint leaves the caller's
/// table for the object.
pub(crate) fn pager_create(p: &Process, a: [u64; 6]) -> Answer {
    let port = typed(
        p,
        a[0],
        Rights::READ | Rights::WRITE | Rights::TRANSFER,
        |o| match o {
            Object::Channel(e) => Some(e.clone()),
            _ => None,
        },
    )?;
    let account = account(p, a[3])?;
    let object = MemoryObject::pager(a[1], port, a[2], &account)?;
    let raw = RawHandle::try_from(a[0]).map_err(|_| Error::BadHandle)?;
    let moved = p.handles.lock().remove(raw)?;
    match p.insert(Handle::new(Object::Memory(Arc::new(object)))) {
        Ok(value) => {
            drop(moved);
            Ok((value as u64, 0))
        }
        Err(e) => {
            // Put back what the call took: nothing changed.
            let _ = p.insert(moved);
            Err(e)
        }
    }
}

/// `pager_supply(memory, offset, len, source, source_offset)`; a refusal when `source`
/// is zero.
pub(crate) fn pager_supply(p: &Process, a: [u64; 6]) -> Answer {
    let object = typed(p, a[0], Rights::WRITE, memory)?;
    let (first, count) = pages(a[1], a[2])?;
    if a[3] == 0 {
        object.refuse(first, count)?;
    } else {
        let source = typed(p, a[3], Rights::READ, memory)?;
        let (source_first, _) = pages(a[4], PAGE_SIZE)?;
        object.supply(first, count, &source, source_first)?;
    }
    Ok((0, 0))
}

/// `thread_state_read(thread, frame, len)`: the registers of a thread asleep in the core.
pub(crate) fn thread_state_read(p: &Process, a: [u64; 6]) -> Answer {
    let target = typed(p, a[0], Rights::READ, thread_of)?;
    if a[2] != frame::LEN as u64 {
        return Err(Error::InvalidArgs);
    }
    let bytes = {
        let state = target.mode.state.lock();
        let saved = state.asleep.as_ref().ok_or(Error::BadState)?;
        encode(&saved.context, saved.fs)
    };
    usercopy::write(&p.space, a[1], &bytes)?;
    Ok((0, 0))
}

/// `thread_state_write(thread, frame, len)`: the registers a thread asleep in the core
/// resumes with, all but `rax`.
pub(crate) fn thread_state_write(p: &Process, a: [u64; 6]) -> Answer {
    let target = typed(p, a[0], Rights::WRITE, thread_of)?;
    if a[2] != frame::LEN as u64 {
        return Err(Error::InvalidArgs);
    }
    let bytes = usercopy::read(&p.space, a[1], frame::LEN)?;
    let (context, fs) = decode(&bytes)?;
    let mut state = target.mode.state.lock();
    let saved = state.asleep.as_mut().ok_or(Error::BadState)?;
    saved.context = context;
    saved.fs = fs;
    state.written = true;
    Ok((0, 0))
}

/// `thread_interrupt(thread)`.
pub(crate) fn thread_interrupt(p: &Process, a: [u64; 6]) -> Answer {
    let target = typed(p, a[0], Rights::WRITE, thread_of)?;
    target.interrupt();
    Ok((0, 0))
}
