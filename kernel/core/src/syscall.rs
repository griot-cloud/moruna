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
    fixed_name, limits, map, memory_flags, msi, nr, pin_flags, read_u32, resource_info,
    start_entry, valid_name, wait_item, write_u32, write_u64,
};
use ostd::{arch::cpu::context::UserContext, early_print, sync::Waiter};

use crate::{
    account::{Account, Charge},
    channel::{self, Endpoint, Message},
    clock::{self, Timer},
    foreign::{self, Forward},
    futex,
    interrupt::Interrupt,
    memory::{self, DeviceMemory, MemoryObject, Pin},
    object::{Console, Handle, Object, Resource},
    plane::{self as data_plane, Plane, Slot, SlotEnd},
    process::{self, Process, Thread},
    usercopy,
    vm::{self, Backing, Kind},
};

/// What the thread does after a syscall.
pub enum Flow {
    Continue,
    ThreadExit,
    /// The thread ends, and if it is the last, the process ends with this code.
    ThreadExitCode(i64),
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
        nr::MEMORY_CREATE => memory_create(p, a[0], a[1], a[2]),
        nr::MEMORY_MAP => memory_map(p, a[0], a[1], a[2], a[3]),
        nr::MEMORY_UNMAP => memory_unmap(p, a[0], a[1]),
        nr::PROCESS_CREATE => process_create(p, a[0], a[1], a[2], a[3]),
        nr::PROCESS_MAP_IMAGE => process_map_image(p, a[0], a[1]),
        nr::PROCESS_START => process_start(p, a[0], a[1], a[2], a[3]),
        nr::PROCESS_EXIT => return Flow::ProcessExit(a[0] as i64),
        nr::THREAD_CREATE => thread_create(p, a[0], a[1], a[2]),
        nr::THREAD_EXIT => return Flow::ThreadExit,
        nr::WAIT => wait(thread, a[0], a[1], a[2]),
        nr::CLOCK_GET => one(clock::now()),
        nr::TIMER_CREATE => timer_create(p),
        nr::TIMER_SET => timer_set(p, a[0], a[1]),
        nr::DEBUG_WRITE => debug_write(p, a[0], a[1], a[2]),
        nr::INTERRUPT_CREATE => interrupt_create(p, a[0], a[1]),
        nr::INTERRUPT_ACK => interrupt_ack(p, a[0]),
        nr::DEVICE_MEMORY_CREATE => device_memory_create(p, a[0], a[1], a[2]),
        nr::RESOURCE_NARROW => resource_narrow(p, a[0], a[1], a[2]),
        nr::INTERRUPT_CREATE_MSI => interrupt_create_msi(p, a[0], a[1]),
        nr::MEMORY_PIN => memory_pin(p, a[0], a[1], a[2], a[3], a[4]),
        nr::DMA_RELEASE => dma_release(p, a[0]),
        nr::RESOURCE_INFO => resource_info(p, a[0], a[1]),
        nr::PROCESS_KILL => process_kill(p, a[0]),
        nr::MEMORY_PROTECT => memory_protect(p, a[0], a[1], a[2]),
        nr::PROCESS_FORWARD => process_forward(p, a[0], a[1], a[2], a[3]),
        nr::PROCESS_LAUNCH => process_launch(p, a[0], a[1], a[2], a[3]),
        nr::PROCESS_MAP => process_map(p, a),
        nr::PROCESS_UNMAP => process_unmap(p, a[0], a[1], a[2]),
        nr::PROCESS_PROTECT => process_protect(p, a[0], a[1], a[2], a[3]),
        nr::PROCESS_READ => process_read(p, a[0], a[1], a[2], a[3]),
        nr::PROCESS_WRITE => process_write(p, a[0], a[1], a[2], a[3]),
        nr::FUTEX_WAIT => futex_wait(thread, a[0], a[1], a[2]),
        nr::FUTEX_WAKE => futex_wake(p, a[0], a[1]),
        nr::THREAD_SET_FS => thread_set_fs(thread, a[0]),
        nr::FORWARD_CLONE => forward_clone(p, a),
        nr::RANDOM_READ => random_read(p, a[0], a[1]),
        nr::SLOT_CREATE => slot_create(p, a[0]),
        nr::SLOT_PUSH => slot_push(p, a[0], a[1], a[2], a[3], a[4]),
        nr::SLOT_TAKE => slot_take(p, a[0], a[1]),
        nr::SLOT_CLOSE => slot_close(p, a[0]),
        nr::SLOT_REFUSE => slot_refuse(p, a[0], a[1], a[2]),
        nr::PROCESS_PLANE => process_plane(p, a[0], a[1], a[2], a[3]),
        nr::PROCESS_GRANT => process_grant(p, a[0], a[1], a[2]),
        nr::ACCOUNT_CREATE => account_create(p, a[0], a[1], a[2], a[3]),
        nr::ACCOUNT_INFO => account_info(p, a[0], a[1]),
        nr::ACCOUNT_MATCHES => account_matches(p, a[0], a[1]),
        nr::BATCH_GET => batch_get(thread, a[0]),
        nr::BATCH_PUT => batch_put(p, a[0], a[1], a[2]),
        nr::PORT_CALL => port_call(thread, a[0], a[1], a[2]),
        nr::PLANE_GET => plane_get(thread, a[0], a[1]),
        nr::PLANE_PUT => plane_put(p, a[0], a[1], a[2], a[3]),
        #[cfg(feature = "probe")]
        nr::PLANE_PROBE => plane_probe(p, a[0], a[1]),
        // The data plane's reserved numbers and any other: no such call.
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
    let value = p.insert(Handle::new(object))?;
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
    let value = p.insert(Handle {
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
    let (a, b) = channel::create(p.account())?;
    let mut table = p.handles.lock();
    if table.room() < 2 {
        drop(table);
        return Err(Error::Limit.into());
    }
    let first = match p.insert_into(&mut table, Handle::new(Object::Channel(a))) {
        Ok(first) => first,
        Err((e, refused)) => {
            drop(table);
            drop(refused);
            return Err(e.into());
        }
    };
    match p.insert_into(&mut table, Handle::new(Object::Channel(b))) {
        Ok(second) => Ok((first as u64, second as u64)),
        Err((e, refused)) => {
            let undone = table.remove(first);
            drop(table);
            drop(refused);
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
    // The message is the sender's while it is queued, charged before it is copied in.
    let charge = Charge::new(
        p.account(),
        Message::cost(nbytes as usize, nhandles as usize),
    )?;
    let payload = usercopy::read(&p.space, bytes, nbytes as usize)?;
    let values = values_of(&usercopy::read(&p.space, handles, nhandles as usize * 4)?);

    let mut table = p.handles.lock();
    let mut carried: Vec<Arc<Endpoint>> = Vec::new();
    for (i, v) in values.iter().enumerate() {
        let handle = table.get(*v)?;
        if !handle.rights.contains(Rights::TRANSFER) {
            return Err(Error::AccessDenied.into());
        }
        if let Object::Channel(e) = &handle.object {
            if Arc::ptr_eq(&e.channel, &endpoint.channel) {
                // A channel's own endpoints never travel through it.
                return Err(Error::InvalidArgs.into());
            }
            carried.try_reserve(1).map_err(|_| Error::NoMemory)?;
            carried.push(e.clone());
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
    // The cycle rule (SDD 20, f.4): an endpoint travels only while no message queued
    // towards it carries an endpoint, checked and counted under one global lock so
    // that two writes cannot each pass the check and close a cycle together.
    let _carrying = if carried.is_empty() {
        None
    } else {
        let guard = channel::TRANSFER.lock();
        if carried.iter().any(|e| e.carried() != 0) {
            return Err(Error::BadState.into());
        }
        endpoint.channel.carry(peer, carried.len());
        Some(guard)
    };
    for v in &values {
        moved.push(table.remove(*v)?);
    }
    inner.queues[peer].push_back(Message {
        bytes: payload,
        handles: moved,
        endpoints: carried.len(),
        charge: Some(charge),
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
    usercopy::write(&p.space, bytes, &front.bytes)?;
    let mut inserted: Vec<RawHandle> = Vec::new();
    let mut out = Vec::new();
    for h in &front.handles {
        // A clone: one refused is dropped here, under the lock, and the message keeps
        // the handle.
        match p.insert_into(&mut table, h.clone()) {
            Ok(v) => {
                inserted.push(v);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Err((e, _clone)) => {
                for v in &inserted {
                    let _ = table.remove(*v);
                }
                return Err(e.into());
            }
        }
    }
    if let Err(e) = usercopy::write(&p.space, handles, &out) {
        // The clones go; the message keeps its handles and stays queued.
        for v in &inserted {
            let _ = table.remove(*v);
        }
        return Err(e.into());
    }
    let popped = inner.queues[side].pop_front();
    if let Some(message) = &popped {
        endpoint.channel.uncarry(side, message.endpoints);
    }
    endpoint.channel.refresh(&inner);
    drop(inner);
    drop(table);
    drop(popped);
    Ok((nb, nh))
}

/// The account `value` names: the caller's own for zero, an account, or a process's,
/// the handle carrying `rights` (ABI 2.3).
fn account_of(p: &Process, value: u64, rights: Rights) -> Result<Arc<Account>, Error> {
    if value == 0 {
        return Ok(p.account().clone());
    }
    typed(p, value, rights, |h| match &h.object {
        Object::Account(a) => Ok(a.clone()),
        Object::Process(q) => Ok(q.account().clone()),
        _ => wrong(),
    })
}

/// `memory_create` (ABI 1.2 adds `flags`, ABI 2.3 the account it is charged to; zero
/// for either is the call before).
fn memory_create(p: &Process, size: u64, flags: u64, account: u64) -> Ret {
    let contiguous = flags & memory_flags::CONTIGUOUS != 0;
    let dma = flags & memory_flags::DMA != 0;
    if flags & !memory_flags::ALL != 0 || (contiguous && !dma) {
        return Err(Error::InvalidArgs.into());
    }
    if contiguous && size > memory_flags::CONTIGUOUS_MAX_BYTES {
        return Err(Error::Limit.into());
    }
    let account = account_of(p, account, Rights::WRITE)?;
    let object = if contiguous {
        MemoryObject::contiguous(size, &account)?
    } else {
        MemoryObject::with_dma(size, dma, &account)?
    };
    insert(p, Object::Memory(Arc::new(object)))
}

fn timer_create(p: &Process) -> Ret {
    let timer = Timer::new(p.account())?;
    insert(p, Object::Timer(Arc::new(timer)))
}

/// `account_create` (ABI 2.3; SDD 20 f.18): an account named by the `len` bytes at
/// `name`, whose limit of `limit` bytes (whole pages) is held in `parent` until it is
/// gone: the caller's own account for zero, an account or a process's, with `WRITE`.
fn account_create(p: &Process, parent: u64, limit: u64, name: u64, len: u64) -> Ret {
    let parent = account_of(p, parent, Rights::WRITE)?;
    if len == 0 || len > limits::NAME_MAX as u64 {
        return Err(Error::InvalidArgs.into());
    }
    let name = usercopy::read(&p.space, name, len as usize)?;
    let child = parent.carve(&name, limit, p.account())?;
    insert(p, Object::Account(child))
}

/// `account_info` (ABI 2.3): the 64 bytes of `moruna_abi::account_info` of the caller's
/// own account (zero), an account, or a process's, with `READ`, written to `out`.
fn account_info(p: &Process, account: u64, out: u64) -> Ret {
    let account = account_of(p, account, Rights::READ)?;
    usercopy::write(&p.space, out, &account.info())?;
    one(0)
}

/// Exact Account capability or immutable Memory charged-owner identity. Neither
/// zero/caller defaults nor Process/account aliases are accepted here.
fn account_matches(p: &Process, account: u64, object: u64) -> Ret {
    let expected = typed(p, account, Rights::READ, |h| match &h.object {
        Object::Account(account) => Ok(account.clone()),
        _ => wrong(),
    })?;
    let matches = typed(p, object, Rights::READ, |h| match &h.object {
        Object::Account(account) => Ok(Arc::ptr_eq(&expected, account)),
        Object::Memory(memory) => Ok(memory.charged_to(&expected)),
        _ => wrong(),
    })?;
    one(u64::from(matches))
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
    let writable = handle.rights.contains(Rights::WRITE);
    let va = match &handle.object {
        Object::Memory(m) => p.map_memory(m, offset, len, write, execute, writable)?,
        Object::DeviceMemory(d) => {
            if execute {
                return Err(Error::InvalidArgs.into());
            }
            p.map_device(d, offset, len, write, writable)?
        }
        _ => return Err(Error::WrongType.into()),
    };
    one(va)
}

/// `process_create` (ABI 2.3 adds the account the process and everything it causes to
/// exist are charged to: the caller's own for zero, an account or a process's, with
/// `WRITE`).
fn process_create(p: &Process, resource: u64, name: u64, len: u64, account: u64) -> Ret {
    typed(p, resource, Rights::WRITE, |h| {
        as_resource(h, ResourceKind::Process)
    })?;
    let account = account_of(p, account, Rights::WRITE)?;
    if len == 0 || len > limits::NAME_MAX as u64 {
        return Err(Error::InvalidArgs.into());
    }
    let name = usercopy::read(&p.space, name, len as usize)?;
    let child = Process::new(&name, false, &account)?;
    insert(p, Object::Process(child))
}

fn process_map_image(p: &Process, process: u64, image: u64) -> Ret {
    let child = typed(p, process, Rights::WRITE, as_process)?;
    let image = typed(p, image, Rights::READ, as_memory)?;
    if image.size() > IMAGE_MAX {
        return Err(Error::Limit.into());
    }
    // The kernel's copy of the image, the caller's for as long as the call holds it.
    let _copy = Charge::new(p.account(), image.size())?;
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
    let moved = take_named(p, entries, count)?;
    child.start(entry, moved)?;
    one(0)
}

/// `process_grant` (ABI 2.2, SDD 30 d.1): the handles named by `entries` move into a
/// created process its creator will launch, and a start page naming them is mapped into
/// it; returns the page's address in it.
fn process_grant(p: &Process, process: u64, entries: u64, count: u64) -> Ret {
    let child = typed(p, process, Rights::WRITE, as_process)?;
    if count > limits::START_HANDLES_MAX as u64 {
        return Err(Error::Limit.into());
    }
    if !child.is_created() {
        return Err(Error::BadState.into());
    }
    let moved = take_named(p, entries, count)?;
    one(child.grant(moved)?)
}

/// The handles `process_start`'s array names (`start_entry`), taken out of the caller's
/// table only once every one has been found with `TRANSFER` and every name is valid
/// and distinct, so a refused call leaves the table as it was.
fn take_named(p: &Process, entries: u64, count: u64) -> Result<Vec<(Handle, Vec<u8>)>, Fail> {
    let raw_entries = usercopy::read(&p.space, entries, count as usize * start_entry::SIZE)?;
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
    Ok(moved)
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
    let mut raw_items = usercopy::read(&p.space, items, count as usize * wait_item::SIZE)?;
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
    usercopy::write(&p.space, items, &raw_items)?;
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
    let bytes = usercopy::read(&p.space, ptr, len as usize)?;
    let text = core::str::from_utf8(&bytes).map_err(|_| Error::InvalidArgs)?;
    early_print!("{}", text);
    one(len)
}

fn interrupt_create(p: &Process, resource: u64, gsi: u64) -> Ret {
    let resource = typed(p, resource, Rights::WRITE, |h| {
        as_resource(h, ResourceKind::Interrupt)
    })?;
    let gsi = u32::try_from(gsi).map_err(|_| Error::InvalidArgs)?;
    if gsi > crate::interrupt::GSI_MAX {
        return Err(Error::InvalidArgs.into());
    }
    if !resource.covers(gsi as u64, 1) {
        return Err(Error::AccessDenied.into());
    }
    let line = Interrupt::new(gsi, p.account())?;
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
    let resource = typed(p, resource, Rights::WRITE, |h| {
        as_resource(h, ResourceKind::DeviceMemory)
    })?;
    if len == 0
        || !paddr.is_multiple_of(PAGE_SIZE)
        || !len.is_multiple_of(PAGE_SIZE)
        || paddr.checked_add(len).is_none()
    {
        return Err(Error::InvalidArgs.into());
    }
    if !resource.covers(paddr, len) {
        return Err(Error::AccessDenied.into());
    }
    let region = DeviceMemory::new(paddr, len, p.account())?;
    insert(p, Object::DeviceMemory(Arc::new(region)))
}

/// `resource_narrow` (SDD 20, f.10): a new handle to a resource of the same kind over
/// `base .. base + len`, which must lie inside the source's range. The new handle
/// carries the source handle's rights. A process resource has no range to narrow.
fn resource_narrow(p: &Process, value: u64, base: u64, len: u64) -> Ret {
    let handle = lookup(p, value, Rights::NONE)?;
    let source = match &handle.object {
        Object::Resource(r) => r.clone(),
        _ => return Err(Error::WrongType.into()),
    };
    if !handle.rights.contains(Rights::DUPLICATE) {
        return Err(Error::AccessDenied.into());
    }
    let aligned = match source.kind {
        ResourceKind::Process => return Err(Error::InvalidArgs.into()),
        ResourceKind::DeviceMemory => {
            base.is_multiple_of(PAGE_SIZE) && len.is_multiple_of(PAGE_SIZE)
        }
        ResourceKind::Interrupt | ResourceKind::Msi | ResourceKind::Dma => true,
    };
    if len == 0 || !aligned || base.checked_add(len).is_none() {
        return Err(Error::InvalidArgs.into());
    }
    if !source.covers(base, len) {
        return Err(Error::AccessDenied.into());
    }
    let narrowed = Resource::new(source.kind, base..base + len, p.account())?;
    let value = p.insert(Handle {
        object: Object::Resource(Arc::new(narrowed)),
        rights: handle.rights,
    })?;
    one(value as u64)
}

/// `interrupt_create_msi` (ABI 1.2; SDD 25, f.2): an interrupt a device raises by
/// writing a message, in MSI slot `slot` of the resource's range. Returns the
/// interrupt, and in `rdx` the message (`moruna_abi::msi`).
fn interrupt_create_msi(p: &Process, resource: u64, slot: u64) -> Ret {
    let resource = typed(p, resource, Rights::WRITE, |h| {
        as_resource(h, ResourceKind::Msi)
    })?;
    if slot >= moruna_abi::resource_range::MSI_END {
        return Err(Error::InvalidArgs.into());
    }
    if !resource.covers(slot, 1) {
        return Err(Error::AccessDenied.into());
    }
    let (line, address, data) = Interrupt::msi(slot, p.account())?;
    let (value, _) = insert(p, Object::Interrupt(Arc::new(line)))?;
    Ok((value, msi::pack(address, data)))
}

/// `memory_pin` (ABI 1.2; SDD 25, f.4): pins a DMA memory object for the devices the
/// DMA resource covers and writes the device address of each page to `addrs`, room
/// for `cap` of them. Returns the pin and, in `rdx`, the page count; on
/// `BufferTooSmall`, `rdx` is the count needed.
fn memory_pin(p: &Process, dma: u64, memory: u64, flags: u64, addrs: u64, cap: u64) -> Ret {
    let domain_handle = lookup(p, dma, Rights::NONE)?;
    let memory_handle = lookup(p, memory, Rights::NONE)?;
    let domain = as_resource(&domain_handle, ResourceKind::Dma)?;
    let object = as_memory(&memory_handle)?;
    if !domain_handle.rights.contains(Rights::WRITE) {
        return Err(Error::AccessDenied.into());
    }
    let mut needed = Rights::READ;
    if flags & pin_flags::DEVICE_WRITES != 0 {
        needed = needed | Rights::WRITE;
    }
    if !memory_handle.rights.contains(needed) {
        return Err(Error::AccessDenied.into());
    }
    if flags & !pin_flags::ALL != 0 || !object.is_dma() {
        return Err(Error::InvalidArgs.into());
    }
    let pages = object.frames().len() as u64;
    if cap < pages {
        return Err(Fail {
            error: Error::BufferTooSmall,
            rdx: pages,
        });
    }
    let (pin, addresses) = Pin::new(domain, object, p.account())?;
    let mut out = Vec::new();
    if out.try_reserve_exact(addresses.len() * 8).is_err() {
        pin.abandon();
        return Err(Error::NoMemory.into());
    }
    out.resize(addresses.len() * 8, 0);
    for (i, a) in addresses.iter().enumerate() {
        write_u64(&mut out, i * 8, *a);
    }
    // No device has the addresses until the caller reads them, so a pin that cannot
    // be reported ends without quarantine.
    if let Err(e) = usercopy::write(&p.space, addrs, &out) {
        pin.abandon();
        return Err(e.into());
    }
    let pin = Arc::new(pin);
    match p.insert(Handle::new(Object::Pin(pin))) {
        Ok(v) => Ok((v as u64, pages)),
        Err(e) => Err(e.into()),
    }
}

/// `dma_release` (ABI 1.2): drops the pins a DMA resource holds in quarantine. The
/// caller says its device no longer uses them; returns how many there were.
fn dma_release(p: &Process, dma: u64) -> Ret {
    let domain = typed(p, dma, Rights::WRITE, |h| as_resource(h, ResourceKind::Dma))?;
    one(memory::release_quarantine(&domain) as u64)
}

/// `resource_info` (ABI 1.2): writes a resource's kind and range (24 bytes,
/// `moruna_abi::resource_info`) to `out`.
fn resource_info(p: &Process, value: u64, out: u64) -> Ret {
    let resource = typed(p, value, Rights::NONE, |h| match &h.object {
        Object::Resource(r) => Ok(r.clone()),
        _ => wrong(),
    })?;
    let mut bytes = [0u8; resource_info::SIZE];
    write_u32(&mut bytes, resource_info::KIND, resource.kind as u32);
    write_u64(&mut bytes, resource_info::BASE, resource.range.start);
    write_u64(
        &mut bytes,
        resource_info::LEN,
        resource.range.end - resource.range.start,
    );
    usercopy::write(&p.space, out, &bytes)?;
    one(0)
}

/// `process_kill` (ABI 1.2): ends a running process through a handle to it, as a
/// fault ends it (KK-I10).
fn process_kill(p: &Process, value: u64) -> Ret {
    let target = typed(p, value, Rights::WRITE, as_process)?;
    target.kill()?;
    one(0)
}

/// `memory_unmap` (ABI 2.0: any whole pages of the caller's own mappings).
fn memory_unmap(p: &Process, addr: u64, len: u64) -> Ret {
    let len = vm::pages(len)?;
    p.space.unmap(addr, len, Some(Kind::Mapping))?;
    one(0)
}

/// `memory_protect` (ABI 2.0): the protection of whole pages of the caller's own
/// mappings; `map::PROTECTION` bits, none of them meaning no access.
fn memory_protect(p: &Process, addr: u64, len: u64, flags: u64) -> Ret {
    if flags & !map::PROTECTION != 0 {
        return Err(Error::InvalidArgs.into());
    }
    let len = vm::pages(len)?;
    p.space.protect(addr, len, flags, Some(Kind::Mapping))?;
    one(0)
}

/// `process_forward` (ABI 2.0; SDD 28 d.1): gives a created process a forward over the
/// endpoint, which leaves the caller's table, with the table at `table`.
fn process_forward(p: &Process, process: u64, endpoint: u64, table: u64, len: u64) -> Ret {
    let target = typed(p, process, Rights::WRITE, as_process)?;
    let end = typed(
        p,
        endpoint,
        Rights::READ | Rights::WRITE | Rights::TRANSFER,
        as_channel,
    )?;
    if len != moruna_abi::forward::table::SIZE as u64 {
        return Err(Error::InvalidArgs.into());
    }
    if !target.is_created() {
        return Err(Error::BadState.into());
    }
    let bytes = usercopy::read(&p.space, table, len as usize)?;
    let forward = Forward::new(end, &bytes)?;
    let moved = p.handles.lock().remove(raw(endpoint)?)?;
    if let Err(e) = target.set_forward(forward) {
        // Put back what the call took, into the slot it left: nothing changed.
        let _ = p.insert(moved);
        return Err(e.into());
    }
    drop(moved);
    one(0)
}

/// `process_launch` (ABI 2.0): starts a created process whose address space the caller
/// built; returns its first thread's id.
fn process_launch(p: &Process, process: u64, entry: u64, stack: u64, brk: u64) -> Ret {
    let target = typed(p, process, Rights::WRITE, as_process)?;
    one(target.launch(entry, stack, brk)?)
}

/// `process_map` (ABI 2.0): maps `len` bytes of a memory object from `offset`, or zero
/// pages filled on first touch when `memory` is zero, into a process: at `addr` when it
/// is not zero (replacing what is there with `map::FIXED`, refused with `BadState`
/// without), where the kernel chooses otherwise. Returns where.
fn process_map(p: &Process, a: [u64; 6]) -> Ret {
    let [process, memory, offset, len, addr, flags] = a;
    let target = typed(p, process, Rights::WRITE, as_process)?;
    let backing = if memory == 0 {
        if offset != 0 {
            return Err(Error::InvalidArgs.into());
        }
        Backing::Zero
    } else {
        let handle = lookup(p, memory, Rights::NONE)?;
        let object = as_memory(&handle)?;
        if !handle.rights.contains(Rights::MAP | Rights::READ) {
            return Err(Error::AccessDenied.into());
        }
        if !offset.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs.into());
        }
        let span = vm::pages(len)?;
        if offset
            .checked_add(span)
            .is_none_or(|end| end > object.size())
        {
            return Err(Error::OutOfRange.into());
        }
        Backing::Object {
            memory: object,
            first: (offset / PAGE_SIZE) as usize,
            writable: handle.rights.contains(Rights::WRITE),
        }
    };
    if flags & !map::PROCESS_ALL != 0 {
        return Err(Error::InvalidArgs.into());
    }
    let fixed = flags & map::FIXED != 0;
    let at = (addr != 0).then_some(addr);
    if fixed && at.is_none() {
        return Err(Error::InvalidArgs.into());
    }
    let va = target.space.map(
        at,
        len,
        Kind::Mapping,
        flags & map::PROTECTION,
        backing,
        fixed,
    )?;
    one(va)
}

fn process_unmap(p: &Process, process: u64, addr: u64, len: u64) -> Ret {
    let target = typed(p, process, Rights::WRITE, as_process)?;
    let len = vm::pages(len)?;
    target.space.unmap(addr, len, None)?;
    one(0)
}

fn process_protect(p: &Process, process: u64, addr: u64, len: u64, flags: u64) -> Ret {
    let target = typed(p, process, Rights::WRITE, as_process)?;
    if flags & !map::PROTECTION != 0 {
        return Err(Error::InvalidArgs.into());
    }
    let len = vm::pages(len)?;
    target.space.protect(addr, len, flags, None)?;
    one(0)
}

/// `process_read` (ABI 2.0): copies `len` bytes at `addr` of a process to `buf`.
fn process_read(p: &Process, process: u64, addr: u64, buf: u64, len: u64) -> Ret {
    let target = typed(p, process, Rights::READ, as_process)?;
    if len > limits::PROCESS_COPY_MAX as u64 {
        return Err(Error::Limit.into());
    }
    // The kernel's copy, the caller's for as long as the call holds it.
    let _copy = Charge::new(p.account(), len)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len as usize)
        .map_err(|_| Error::NoMemory)?;
    bytes.resize(len as usize, 0);
    target.space.copy_out(addr, &mut bytes)?;
    usercopy::write(&p.space, buf, &bytes)?;
    one(0)
}

/// `process_write` (ABI 2.0): copies `len` bytes from `buf` to `addr` of a process,
/// whose pages there must be writable.
fn process_write(p: &Process, process: u64, addr: u64, buf: u64, len: u64) -> Ret {
    let target = typed(p, process, Rights::WRITE, as_process)?;
    if len > limits::PROCESS_COPY_MAX as u64 {
        return Err(Error::Limit.into());
    }
    let bytes = usercopy::read(&p.space, buf, len as usize)?;
    target.space.copy_in(addr, &bytes)?;
    one(0)
}

/// `futex_wait` (ABI 2.0): sleeps while the `u32` at `addr` holds `expected`, until a
/// `futex_wake` on it or the deadline; `ShouldWait` when it holds another value.
fn futex_wait(thread: &Arc<Thread>, addr: u64, expected: u64, deadline: u64) -> Ret {
    let expected = u32::try_from(expected).map_err(|_| Error::InvalidArgs)?;
    futex::wait(thread, addr, expected, deadline, futex::ALL)?;
    one(0)
}

/// `futex_wake` (ABI 2.0): wakes at most `count` threads waiting on `addr`; how many.
fn futex_wake(p: &Process, addr: u64, count: u64) -> Ret {
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    one(futex::wake(p.koid, addr, count, futex::ALL)? as u64)
}

/// `thread_set_fs` (ABI 2.0): the calling thread's FS base, a lower-half address.
fn thread_set_fs(thread: &Arc<Thread>, base: u64) -> Ret {
    if !foreign::canonical(base) {
        return Err(Error::InvalidArgs.into());
    }
    thread.set_fs(base);
    one(0)
}

/// `forward_clone` (ABI 2.0; SDD 28 f.4): a thread of a forwarded process from the
/// registers of its thread `tid`, which is waiting on the forward.
fn forward_clone(p: &Process, a: [u64; 6]) -> Ret {
    let [process, tid, stack, tls, clear_tid, set_tid] = a;
    let target = typed(p, process, Rights::WRITE, as_process)?;
    one(target.clone_thread(tid, stack, tls, clear_tid, set_tid)?)
}

/// `random_read` (ABI 2.0): `len` bytes from the CPU's random source to `buf`;
/// `BadState` when the CPU has none.
fn random_read(p: &Process, buf: u64, len: u64) -> Ret {
    if len > limits::RANDOM_MAX as u64 {
        return Err(Error::Limit.into());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len as usize)
        .map_err(|_| Error::NoMemory)?;
    while (bytes.len() as u64) < len {
        let word = ostd::arch::read_random().ok_or(Error::BadState)?;
        let take = (len as usize - bytes.len()).min(8);
        bytes.extend_from_slice(&word.to_le_bytes()[..take]);
    }
    usercopy::write(&p.space, buf, &bytes)?;
    one(0)
}

fn as_slot(h: &Handle) -> Result<Arc<Slot>, Error> {
    match &h.object {
        Object::Slot(s) => Ok(s.0.clone()),
        _ => wrong(),
    }
}

/// `slot_create` (ABI 2.1; SDD 29 d.2): an input or an output slot, for the engine.
fn slot_create(p: &Process, flags: u64) -> Ret {
    let slot = Slot::new(flags, p.account())?;
    insert(p, Object::Slot(Arc::new(SlotEnd(Arc::new(slot)))))
}

/// `slot_push` (ABI 2.1): `memory` leaves the caller's table into the slot, as a batch
/// of `len` bytes or a window of `len` bytes (whole pages) from `offset`; `cookie` comes
/// back with the event that gives it back.
fn slot_push(p: &Process, slot: u64, memory: u64, offset: u64, len: u64, cookie: u64) -> Ret {
    let slot = typed(p, slot, Rights::WRITE, as_slot)?;
    let handle = lookup(p, memory, Rights::NONE)?;
    let object = as_memory(&handle)?;
    if !handle.rights.contains(Rights::TRANSFER) {
        return Err(Error::AccessDenied.into());
    }
    let mut table = p.handles.lock();
    // The handle is checked again under the lock: it is what leaves the table.
    let moved = table.get(raw(memory)?)?.clone();
    if !matches!(&moved.object, Object::Memory(m) if Arc::ptr_eq(m, &object)) {
        return Err(Error::BadHandle.into());
    }
    slot.push(object, moved.rights, offset, len, cookie)?;
    let gone = table.remove(raw(memory)?)?;
    drop(table);
    drop(gone);
    one(0)
}

/// `slot_take` (ABI 2.1): the slot's next event, 32 bytes at `out`, and the handle to
/// the memory object it gives back (zero for a release).
fn slot_take(p: &Process, slot: u64, out: u64) -> Ret {
    let slot = typed(p, slot, Rights::READ, as_slot)?;
    let mut table = p.handles.lock();
    let mut value = 0u64;
    slot.take(|taken| {
        if taken.memory.is_some() && table.room() == 0 {
            return Err(Error::Limit);
        }
        usercopy::write(&p.space, out, &taken.bytes)?;
        if let Some((memory, rights)) = &taken.memory {
            // A clone of the slot's: one refused is dropped here, and the event stays.
            let handle = Handle {
                object: Object::Memory(memory.clone()),
                rights: *rights,
            };
            value = p.insert_into(&mut table, handle).map_err(|(e, _clone)| e)? as u64;
        }
        Ok(())
    })?;
    one(value)
}

/// `slot_close` (ABI 2.1): nothing more will be pushed.
fn slot_close(p: &Process, slot: u64) -> Ret {
    typed(p, slot, Rights::WRITE, as_slot)?.close();
    one(0)
}

/// `slot_refuse` (ABI 2.1): the task attached to the slot ends with the reason.
fn slot_refuse(p: &Process, slot: u64, reason: u64, len: u64) -> Ret {
    let slot = typed(p, slot, Rights::WRITE, as_slot)?;
    if len > limits::REFUSAL_MAX as u64 {
        return Err(Error::Limit.into());
    }
    let bytes = usercopy::read(&p.space, reason, len as usize)?;
    slot.refuse(&data_plane::reason(&bytes)?)?;
    one(0)
}

/// `process_plane` (ABI 2.1; SDD 29 d.2): a created process's slots, in order, from the
/// handle values at `slots` (the caller keeps its handles), and its port over the
/// endpoint `port`, which leaves the caller's table (none when zero).
fn process_plane(p: &Process, process: u64, slots: u64, count: u64, port: u64) -> Ret {
    let target = typed(p, process, Rights::WRITE, as_process)?;
    if count > limits::PLANE_SLOTS as u64 {
        return Err(Error::Limit.into());
    }
    let values = values_of(&usercopy::read(&p.space, slots, count as usize * 4)?);
    let mut bound = Vec::new();
    bound
        .try_reserve_exact(values.len())
        .map_err(|_| Error::NoMemory)?;
    for v in &values {
        bound.push(typed(p, *v as u64, Rights::WRITE, as_slot)?);
    }
    let endpoint = if port == 0 {
        None
    } else {
        Some(typed(
            p,
            port,
            Rights::READ | Rights::WRITE | Rights::TRANSFER,
            as_channel,
        )?)
    };
    if !target.is_created() {
        return Err(Error::BadState.into());
    }
    let moved = match endpoint {
        Some(_) => Some(p.handles.lock().remove(raw(port)?)?),
        None => None,
    };
    let result = Plane::bind(&target, bound, endpoint).and_then(|plane| target.set_plane(plane));
    if let Err(e) = result {
        if let Some(handle) = moved {
            // Put back what the call took, into the slot it left: nothing changed.
            let _ = p.insert(handle);
        }
        return Err(e.into());
    }
    drop(moved);
    one(0)
}

/// `batch_get` (ABI 2.1; SDD 29 d.1): slot `slot` of the caller's own plane.
fn batch_get(thread: &Arc<Thread>, slot: u64) -> Ret {
    let p = &thread.process;
    let plane = p.plane().ok_or(Error::BadHandle)?;
    let slot = plane.slot(slot)?.clone();
    Ok(slot.get(thread, p)?)
}

/// `batch_put` (ABI 2.1): the record at `addr` of the caller's window in `slot`.
fn batch_put(p: &Arc<Process>, slot: u64, addr: u64, len: u64) -> Ret {
    let plane = p.plane().ok_or(Error::BadHandle)?;
    plane.slot(slot)?.put(p, addr, len)?;
    one(0)
}

/// `port_call` (ABI 2.1; SDD 29 f.4).
fn port_call(thread: &Arc<Thread>, port: u64, addr: u64, len: u64) -> Ret {
    let p = &thread.process;
    let plane = p.plane().ok_or(Error::BadHandle)?;
    if len > moruna_abi::plane::request::MAX as u64 {
        return Err(Error::Limit.into());
    }
    // The copied user request coexists with the queued request's framing until
    // the plane has built it. Charge both actual buffers, never the queue alone.
    let payload_charge = Charge::new(p.account(), len)?;
    let bytes = usercopy::read(&p.space, addr, len as usize)?;
    Ok(data_plane::port_call(
        thread,
        &plane,
        port,
        bytes,
        payload_charge,
    )?)
}

/// `plane_get` (ABI 2.1): `batch_get` on slot `slot` of `process`, mapped into the caller:
/// how a personality server reads a task's batches for it.
fn plane_get(thread: &Arc<Thread>, process: u64, slot: u64) -> Ret {
    let p = &thread.process;
    let target = typed(p, process, Rights::WRITE, as_process)?;
    let plane = target.plane().ok_or(Error::BadState)?;
    let slot = plane.slot(slot)?.clone();
    Ok(slot.get(thread, p)?)
}

/// `plane_put` (ABI 2.1): `batch_put` of the window `plane_get` mapped into the caller.
fn plane_put(p: &Arc<Process>, process: u64, slot: u64, addr: u64, len: u64) -> Ret {
    let target = typed(p, process, Rights::WRITE, as_process)?;
    let plane = target.plane().ok_or(Error::BadState)?;
    plane.slot(slot)?.put(p, addr, len)?;
    one(0)
}

/// `plane_probe` (ABI 2.1, the `probe` feature): what the data plane's tests measure.
#[cfg(feature = "probe")]
fn plane_probe(p: &Process, op: u64, arg: u64) -> Ret {
    use moruna_abi::plane::probe;
    match op {
        probe::FRAME => one(p.space.frame_at(arg)?),
        probe::COPIED => one(p.space.copied()),
        probe::OBJECT_FRAME => {
            let memory = typed(p, arg & 0xffff_ffff, Rights::READ, as_memory)?;
            let page = (arg >> 32) as usize;
            let frame = memory.frames().get(page).ok_or(Error::OutOfRange)?;
            use ostd::mm::HasPaddr;
            one(frame.paddr() as u64)
        }
        _ => Err(Error::InvalidArgs.into()),
    }
}
