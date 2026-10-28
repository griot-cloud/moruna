//! The native syscalls (SDD 20, d.2).
//!
//! The number is in `rax`, the arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`.
//! A call returns its value in `rax` (and a second one in `rdx` where the table says
//! so), or `-(error code)` in `rax`. Every call that takes a handle looks it up in the
//! caller's own table before it checks anything else, so a value that names nothing
//! there is `BadHandle` whatever the other arguments are (KK-I2).

use alloc::{sync::Arc, vec::Vec};

use moruna_abi::{
    ABI_VERSION, DEADLINE_INFINITE, Error, PAGE_SIZE, RawHandle, ResourceKind, Rights, Signals,
    fixed_name, limits, map, nr, read_u32, start_entry, valid_name, wait_item, write_u32,
};
use ostd::{arch::cpu::context::UserContext, early_print, sync::Waiter};

use crate::{
    channel::{self, Endpoint, Message},
    clock::{self, Timer},
    interrupt::Interrupt,
    memory::{DeviceMemory, MemoryObject},
    object::{Console, Handle, Object, Resource},
    process::{self, Process, Thread},
    usercopy,
};

/// What the thread does after a syscall.
pub enum Flow {
    Continue,
    ThreadExit,
    ProcessExit(i64),
}

/// A failed call: the error, and what `rdx` carries with it.
struct Fail {
    error: Error,
    rdx: u64,
}

impl From<Error> for Fail {
    fn from(error: Error) -> Fail {
        Fail { error, rdx: 0 }
    }
}

type Ret = Result<(u64, u64), Fail>;

/// The largest image `process_map_image` copies in.
const IMAGE_MAX: u64 = 64 * 1024 * 1024;

fn one(v: u64) -> Ret {
    Ok((v, 0))
}

pub fn dispatch(thread: &Arc<Thread>, ctx: &mut UserContext) -> Flow {
    let number = ctx.rax() as u64;
    let a = [
        ctx.rdi() as u64,
        ctx.rsi() as u64,
        ctx.rdx() as u64,
        ctx.r10() as u64,
        ctx.r8() as u64,
        ctx.r9() as u64,
    ];
    let p = &thread.process;
    let result = match number {
        nr::ABI_VERSION => one(ABI_VERSION),
        nr::HANDLE_CLOSE => handle_close(p, a[0]),
        nr::HANDLE_DUPLICATE => handle_duplicate(p, a[0], a[1]),
        nr::HANDLE_INFO => handle_info(p, a[0]),
        nr::CHANNEL_CREATE => channel_create(p),
        nr::CHANNEL_WRITE => channel_write(p, a[0], a[1], a[2], a[3], a[4]),
        nr::CHANNEL_READ => channel_read(p, a[0], a[1], a[2], a[3], a[4]),
        nr::MEMORY_CREATE => memory_create(p, a[0]),
        nr::MEMORY_MAP => memory_map(p, a[0], a[1], a[2], a[3]),
        nr::MEMORY_UNMAP => p.unmap(a[0], a[1]).map(|_| (0, 0)).map_err(Fail::from),
        nr::PROCESS_CREATE => process_create(p, a[0], a[1], a[2]),
        nr::PROCESS_MAP_IMAGE => process_map_image(p, a[0], a[1]),
        nr::PROCESS_START => process_start(p, a[0], a[1], a[2], a[3]),
        nr::PROCESS_EXIT => return Flow::ProcessExit(a[0] as i64),
        nr::THREAD_CREATE => thread_create(p, a[0], a[1], a[2]),
        nr::THREAD_EXIT => return Flow::ThreadExit,
        nr::WAIT => wait(thread, a[0], a[1], a[2]),
        nr::CLOCK_GET => one(clock::now()),
        nr::TIMER_CREATE => insert(p, Object::Timer(Arc::new(Timer::new()))),
        nr::TIMER_SET => timer_set(p, a[0], a[1]),
        nr::DEBUG_WRITE => debug_write(p, a[0], a[1], a[2]),
        nr::INTERRUPT_CREATE => interrupt_create(p, a[0], a[1]),
        nr::INTERRUPT_ACK => interrupt_ack(p, a[0]),
        nr::DEVICE_MEMORY_CREATE => device_memory_create(p, a[0], a[1], a[2]),
        _ => Err(Error::BadSyscall.into()),
    };
    match result {
        Ok((rax, rdx)) => {
            ctx.set_rax(rax as usize);
            ctx.set_rdx(rdx as usize);
        }
        Err(fail) => {
            ctx.set_rax(fail.error.to_raw() as usize);
            ctx.set_rdx(fail.rdx as usize);
        }
    }
    Flow::Continue
}

fn raw(value: u64) -> Result<RawHandle, Error> {
    RawHandle::try_from(value).map_err(|_| Error::BadHandle)
}

/// The handle `value` names in `p`'s table, with `rights` required.
fn lookup(p: &Process, value: u64, rights: Rights) -> Result<Handle, Error> {
    let handle = p.handles.lock().get(raw(value)?)?.clone();
    if !handle.rights.contains(rights) {
        return Err(Error::AccessDenied);
    }
    Ok(handle)
}

fn insert(p: &Process, object: Object) -> Ret {
    let value = p.handles.lock().insert(Handle::new(object))?;
    one(value as u64)
}

fn wrong<T>() -> Result<T, Error> {
    Err(Error::WrongType)
}

fn as_channel(h: &Handle) -> Result<Arc<Endpoint>, Error> {
    match &h.object {
        Object::Channel(e) => Ok(e.clone()),
        _ => wrong(),
    }
}

fn as_process(h: &Handle) -> Result<Arc<Process>, Error> {
    match &h.object {
        Object::Process(p) => Ok(p.clone()),
        _ => wrong(),
    }
}

fn as_memory(h: &Handle) -> Result<Arc<MemoryObject>, Error> {
    match &h.object {
        Object::Memory(m) => Ok(m.clone()),
        _ => wrong(),
    }
}

/// A resource of `kind`: the type is checked first, then that it grants this.
fn as_resource(h: &Handle, kind: ResourceKind) -> Result<Arc<Resource>, Error> {
    match &h.object {
        Object::Resource(r) if r.kind == kind => Ok(r.clone()),
        Object::Resource(_) => Err(Error::AccessDenied),
        _ => wrong(),
    }
}

/// Looks up a handle and checks its type before its rights, as every call does.
fn typed<T>(
    p: &Process,
    value: u64,
    rights: Rights,
    cast: impl Fn(&Handle) -> Result<T, Error>,
) -> Result<T, Error> {
    let handle = lookup(p, value, Rights::NONE)?;
    let object = cast(&handle)?;
    if !handle.rights.contains(rights) {
        return Err(Error::AccessDenied);
    }
    Ok(object)
}

fn handle_close(p: &Process, value: u64) -> Ret {
    let handle = p.handles.lock().remove(raw(value)?)?;
    // Dropped outside the table's lock: closing may close a channel endpoint.
    drop(handle);
    one(0)
}

fn handle_duplicate(p: &Process, value: u64, rights: u64) -> Ret {
    let handle = lookup(p, value, Rights::DUPLICATE)?;
    let rights = u32::try_from(rights)
        .ok()
        .and_then(Rights::from_bits)
        .ok_or(Error::InvalidArgs)?;
    if !handle.rights.contains(rights) {
        return Err(Error::AccessDenied.into());
    }
    let value = p.handles.lock().insert(Handle {
        object: handle.object,
        rights,
    })?;
    one(value as u64)
}

fn handle_info(p: &Process, value: u64) -> Ret {
    let handle = lookup(p, value, Rights::NONE)?;
    one(((handle.object.kind() as u64) << 32) | handle.rights.bits() as u64)
}

fn channel_create(p: &Process) -> Ret {
    let (a, b) = channel::create();
    let mut table = p.handles.lock();
    if table.room() < 2 {
        return Err(Error::Limit.into());
    }
    let first = table.insert(Handle::new(Object::Channel(a)))?;
    match table.insert(Handle::new(Object::Channel(b))) {
        Ok(second) => Ok((first as u64, second as u64)),
        Err(e) => {
            let undone = table.remove(first);
            drop(table);
            drop(undone);
            Err(e.into())
        }
    }
}

fn values_of(bytes: &[u8]) -> Vec<RawHandle> {
    (0..bytes.len() / 4)
        .filter_map(|i| read_u32(bytes, i * 4))
        .collect()
}

fn channel_write(
    p: &Process,
    value: u64,
    bytes: u64,
    nbytes: u64,
    handles: u64,
    nhandles: u64,
) -> Ret {
    let endpoint = typed(p, value, Rights::WRITE, as_channel)?;
    if nbytes > limits::CHANNEL_MAX_BYTES as u64 || nhandles > limits::CHANNEL_MAX_HANDLES as u64 {
        return Err(Error::Limit.into());
    }
    let payload = usercopy::read(&p.vm, bytes, nbytes as usize)?;
    let values = values_of(&usercopy::read(&p.vm, handles, nhandles as usize * 4)?);

    let mut table = p.handles.lock();
    for (i, v) in values.iter().enumerate() {
        let handle = table.get(*v)?;
        if !handle.rights.contains(Rights::TRANSFER) {
            return Err(Error::AccessDenied.into());
        }
        if let Object::Channel(e) = &handle.object
            && Arc::ptr_eq(&e.channel, &endpoint.channel)
        {
            // A channel's own endpoints never travel through it.
            return Err(Error::InvalidArgs.into());
        }
        if values[..i].contains(v) {
            return Err(Error::InvalidArgs.into());
        }
    }
    let mut inner = endpoint.channel.inner.lock();
    let peer = endpoint.peer();
    if !inner.open[peer] {
        return Err(Error::PeerClosed.into());
    }
    if inner.queues[peer].len() >= limits::CHANNEL_DEPTH {
        return Err(Error::ShouldWait.into());
    }
    let mut moved = Vec::new();
    moved
        .try_reserve_exact(values.len())
        .map_err(|_| Error::NoMemory)?;
    for v in &values {
        moved.push(table.remove(*v)?);
    }
    inner.queues[peer].push_back(Message {
        bytes: payload,
        handles: moved,
    });
    endpoint.channel.refresh(&inner);
    one(0)
}

fn channel_read(p: &Process, value: u64, bytes: u64, cap: u64, handles: u64, hcap: u64) -> Ret {
    let endpoint = typed(p, value, Rights::READ, as_channel)?;
    let mut table = p.handles.lock();
    let mut inner = endpoint.channel.inner.lock();
    let side = endpoint.side;
    let Some(front) = inner.queues[side].front() else {
        return Err(if inner.open[endpoint.peer()] {
            Error::ShouldWait
        } else {
            Error::PeerClosed
        }
        .into());
    };
    let (nb, nh) = (front.bytes.len() as u64, front.handles.len() as u64);
    if nb > cap || nh > hcap {
        return Err(Fail {
            error: Error::BufferTooSmall,
            rdx: (nh << 32) | nb,
        });
    }
    if table.room() < front.handles.len() {
        return Err(Error::Limit.into());
    }
    usercopy::write(&p.vm, bytes, &front.bytes)?;
    let mut inserted: Vec<RawHandle> = Vec::new();
    let mut out = Vec::new();
    for h in &front.handles {
        match table.insert(h.clone()) {
            Ok(v) => {
                inserted.push(v);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Err(e) => {
                for v in &inserted {
                    let _ = table.remove(*v);
                }
                return Err(e.into());
            }
        }
    }
    if let Err(e) = usercopy::write(&p.vm, handles, &out) {
        // The clones go; the message keeps its handles and stays queued.
        for v in &inserted {
            let _ = table.remove(*v);
        }
        return Err(e.into());
    }
    let popped = inner.queues[side].pop_front();
    endpoint.channel.refresh(&inner);
    drop(inner);
    drop(table);
    drop(popped);
    Ok((nb, nh))
}

fn memory_create(p: &Process, size: u64) -> Ret {
    let object = MemoryObject::new(size)?;
    insert(p, Object::Memory(Arc::new(object)))
}

fn memory_map(p: &Process, value: u64, offset: u64, len: u64, flags: u64) -> Ret {
    let handle = lookup(p, value, Rights::NONE)?;
    let size = match &handle.object {
        Object::Memory(m) => m.size(),
        Object::DeviceMemory(d) => d.size,
        _ => return Err(Error::WrongType.into()),
    };
    if !handle.rights.contains(Rights::MAP | Rights::READ) {
        return Err(Error::AccessDenied.into());
    }
    if flags & !map::ALL != 0 {
        return Err(Error::InvalidArgs.into());
    }
    let write = flags & map::WRITE != 0;
    let execute = flags & map::EXECUTE != 0;
    if write && !handle.rights.contains(Rights::WRITE) {
        return Err(Error::AccessDenied.into());
    }
    if write && execute {
        return Err(Error::InvalidArgs.into());
    }
    if len == 0 || !offset.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
        return Err(Error::InvalidArgs.into());
    }
    if offset.checked_add(len).is_none_or(|end| end > size) {
        return Err(Error::OutOfRange.into());
    }
    let va = match &handle.object {
        Object::Memory(m) => p.map_memory(m, offset, len, write, execute)?,
        Object::DeviceMemory(d) => {
            if execute {
                return Err(Error::InvalidArgs.into());
            }
            p.map_device(d, offset, len, write)?
        }
        _ => return Err(Error::WrongType.into()),
    };
    one(va)
}

fn process_create(p: &Process, resource: u64, name: u64, len: u64) -> Ret {
    typed(p, resource, Rights::WRITE, |h| {
        as_resource(h, ResourceKind::Process)
    })?;
    if len == 0 || len > limits::NAME_MAX as u64 {
        return Err(Error::InvalidArgs.into());
    }
    let name = usercopy::read(&p.vm, name, len as usize)?;
    let child = Process::new(&name, false)?;
    insert(p, Object::Process(child))
}

fn process_map_image(p: &Process, process: u64, image: u64) -> Ret {
    let child = typed(p, process, Rights::WRITE, as_process)?;
    let image = typed(p, image, Rights::READ, as_memory)?;
    if image.size() > IMAGE_MAX {
        return Err(Error::Limit.into());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(image.size() as usize)
        .map_err(|_| Error::NoMemory)?;
    bytes.resize(image.size() as usize, 0);
    image.read(0, &mut bytes)?;
    one(child.load_image(&bytes)?)
}

fn process_start(p: &Process, process: u64, entry: u64, entries: u64, count: u64) -> Ret {
    let child = typed(p, process, Rights::WRITE, as_process)?;
    if count > limits::START_HANDLES_MAX as u64 {
        return Err(Error::Limit.into());
    }
    if !child.ready_to_start() {
        return Err(Error::BadState.into());
    }
    if !process::is_user(entry) {
        return Err(Error::InvalidArgs.into());
    }
    let raw_entries = usercopy::read(&p.vm, entries, count as usize * start_entry::SIZE)?;
    let mut specs: Vec<(RawHandle, Vec<u8>)> = Vec::new();
    for i in 0..count as usize {
        let at = i * start_entry::SIZE;
        let value = read_u32(&raw_entries, at + start_entry::HANDLE).ok_or(Error::InvalidArgs)?;
        let field = raw_entries
            .get(at + start_entry::NAME..at + start_entry::NAME + limits::NAME_MAX)
            .ok_or(Error::InvalidArgs)?;
        let name = fixed_name(field);
        specs.push((value, name.to_vec()));
    }
    let mut moved = Vec::new();
    {
        let mut table = p.handles.lock();
        for (i, (value, name)) in specs.iter().enumerate() {
            let handle = table.get(*value)?;
            if !handle.rights.contains(Rights::TRANSFER) {
                return Err(Error::AccessDenied.into());
            }
            if !valid_name(name) || specs[..i].iter().any(|(v, n)| v == value || n == name) {
                return Err(Error::InvalidArgs.into());
            }
        }
        for (value, name) in specs {
            moved.push((table.remove(value)?, name));
        }
    }
    child.start(entry, moved)?;
    one(0)
}

fn thread_create(p: &Arc<Process>, entry: u64, stack: u64, arg: u64) -> Ret {
    let thread = p.add_thread(entry, stack, arg)?;
    insert(p, Object::Thread(thread))
}

fn wait(thread: &Arc<Thread>, items: u64, count: u64, deadline: u64) -> Ret {
    let p = &thread.process;
    if count == 0 || count > limits::WAIT_MAX_ITEMS as u64 {
        return Err(Error::InvalidArgs.into());
    }
    let mut raw_items = usercopy::read(&p.vm, items, count as usize * wait_item::SIZE)?;
    let mut watched: Vec<(Object, Signals)> = Vec::new();
    for i in 0..count as usize {
        let at = i * wait_item::SIZE;
        let value = read_u32(&raw_items, at + wait_item::HANDLE).ok_or(Error::InvalidArgs)?;
        let handle = lookup(p, value as u64, Rights::NONE)?;
        if handle.object.signaller().is_none() {
            return Err(Error::WrongType.into());
        }
        if !handle.rights.contains(Rights::WAIT) {
            return Err(Error::AccessDenied.into());
        }
        let awaited = read_u32(&raw_items, at + wait_item::AWAITED)
            .and_then(Signals::from_bits)
            .ok_or(Error::InvalidArgs)?;
        watched.push((handle.object, awaited));
    }

    let (waiter, waker) = Waiter::new_pair();
    thread.set_waker(Some(waker.clone()));
    for (object, _) in &watched {
        if let Some(s) = object.signaller() {
            s.observe(&waker);
        }
    }
    let key = (deadline != 0 && deadline != DEADLINE_INFINITE)
        .then(|| clock::arm_wake(deadline, waker.clone()));
    let now_bits = |watched: &[(Object, Signals)]| -> Vec<Signals> {
        watched
            .iter()
            .map(|(o, _)| o.signaller().map(|s| s.get()).unwrap_or(Signals::NONE))
            .collect()
    };
    let result = loop {
        let bits = now_bits(&watched);
        let hits = watched
            .iter()
            .zip(&bits)
            .filter(|((_, awaited), seen)| seen.intersects(*awaited))
            .count();
        if hits > 0 {
            break Ok(hits as u64);
        }
        if deadline != DEADLINE_INFINITE && clock::now() >= deadline {
            break Err(Error::TimedOut);
        }
        if thread.killed() {
            break Err(Error::BadState);
        }
        waiter.wait();
    };
    for (object, _) in &watched {
        if let Some(s) = object.signaller() {
            s.unobserve(&waker);
        }
    }
    if let Some(key) = key {
        clock::disarm(key);
    }
    thread.set_waker(None);

    for (i, seen) in now_bits(&watched).iter().enumerate() {
        write_u32(
            &mut raw_items,
            i * wait_item::SIZE + wait_item::OBSERVED,
            seen.bits(),
        );
    }
    usercopy::write(&p.vm, items, &raw_items)?;
    one(result?)
}

fn timer_set(p: &Process, value: u64, deadline: u64) -> Ret {
    let timer = typed(p, value, Rights::WRITE, |h| match &h.object {
        Object::Timer(t) => Ok(t.clone()),
        _ => wrong(),
    })?;
    timer.set(deadline);
    one(0)
}

fn debug_write(p: &Process, value: u64, ptr: u64, len: u64) -> Ret {
    let _console: Arc<Console> = typed(p, value, Rights::WRITE, |h| match &h.object {
        Object::Console(c) => Ok(c.clone()),
        _ => wrong(),
    })?;
    if len > limits::DEBUG_WRITE_MAX as u64 {
        return Err(Error::Limit.into());
    }
    let bytes = usercopy::read(&p.vm, ptr, len as usize)?;
    let text = core::str::from_utf8(&bytes).map_err(|_| Error::InvalidArgs)?;
    early_print!("{}", text);
    one(len)
}

fn interrupt_create(p: &Process, resource: u64, gsi: u64) -> Ret {
    typed(p, resource, Rights::WRITE, |h| {
        as_resource(h, ResourceKind::Interrupt)
    })?;
    let gsi = u32::try_from(gsi).map_err(|_| Error::InvalidArgs)?;
    let line = Interrupt::new(gsi)?;
    insert(p, Object::Interrupt(Arc::new(line)))
}

fn interrupt_ack(p: &Process, value: u64) -> Ret {
    let line = typed(p, value, Rights::WRITE, |h| match &h.object {
        Object::Interrupt(i) => Ok(i.clone()),
        _ => wrong(),
    })?;
    line.ack();
    one(0)
}

fn device_memory_create(p: &Process, resource: u64, paddr: u64, len: u64) -> Ret {
    typed(p, resource, Rights::WRITE, |h| {
        as_resource(h, ResourceKind::DeviceMemory)
    })?;
    let region = DeviceMemory::new(paddr, len)?;
    insert(p, Object::DeviceMemory(Arc::new(region)))
}
