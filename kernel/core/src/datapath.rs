//! The data path's mechanisms (ABI 2.6; `architecture/data-os.md` §2): stream A's module.
//!
//! - `thread_set_cpu` places a thread on one CPU, never on a function (C5): the
//!   scheduler moves it once, and it stays (`sched::set_cpu`).
//! - `clock_page` maps the page the clock publishes its scale on, read only, once a
//!   process, so time is read with no call (C8; `clock.rs`).
//! - `windows_protect` sets one protection on a list of windows with one TLB flush: the
//!   grouped revoke before the record check (C1; `Space::protect_windows`).
//! - `map_memory` is `memory_map`'s mapping with `map::HUGE` and `map::POPULATE` (C4),
//!   which `Space::map` takes for `process_map` too.
//!
//! A handle of zero names the caller's own thread, or its own process, whose own
//! mappings it may change as `memory_protect` may. Only stream A writes this file.

use alloc::{sync::Arc, vec::Vec};

use moruna_abi::{Error, PAGE_SIZE, RawHandle, Rights, cpu, map, windows::RANGE_SIZE};
use moruna_datapath::windows;

use crate::{
    clock,
    memory::MemoryObject,
    object::Object,
    process::{Process, Thread},
    sched, usercopy,
    vm::{Backing, Kind},
};

/// What a call returns: `rax` and `rdx`, or the error.
pub(crate) type Answer = Result<(u64, u64), Error>;

/// The object `value` names in `process`'s table, with `rights`.
fn object(process: &Process, value: u64, rights: Rights) -> Result<Object, Error> {
    let raw = RawHandle::try_from(value).map_err(|_| Error::BadHandle)?;
    let handle = process.handles.lock().get(raw)?.clone();
    if !handle.rights.contains(rights) {
        return Err(Error::AccessDenied);
    }
    Ok(handle.object)
}

/// `thread_set_cpu(thread, cpu)`: runs `thread` (zero: the caller) on CPU `cpu` only, or
/// with `cpu::ANY` unpins it: free to run on any CPU, as a new thread is. The caller
/// placing itself on a CPU returns on it; any other change takes effect the next time the
/// thread is queued. Needs `WRITE` on the thread.
pub(crate) fn thread_set_cpu(caller: &Arc<Thread>, args: [u64; 6]) -> Answer {
    let [thread, target, ..] = args;
    let thread = if thread == 0 {
        caller.clone()
    } else {
        match object(&caller.process, thread, Rights::WRITE)? {
            Object::Thread(t) => t,
            _ => return Err(Error::WrongType),
        }
    };
    let target = if target == cpu::ANY {
        None
    } else {
        let cpu = u32::try_from(target).map_err(|_| Error::InvalidArgs)?;
        if cpu as usize >= ostd::cpu::num_cpus() {
            return Err(Error::InvalidArgs);
        }
        Some(cpu)
    };
    sched::set_cpu(caller, &thread, target)?;
    Ok((0, 0))
}

/// `clock_page() -> address`: maps the clock page read only into the caller, once (a
/// second call returns the same address while the mapping stays).
pub(crate) fn clock_page(process: &Process, _args: [u64; 6]) -> Answer {
    let page = clock::page().ok_or(Error::BadState)?;
    Ok((process.space.map_page_once(&page)?, 0))
}

/// `windows_protect(process, ranges, count, flags)`: sets `flags`, `map::READ` or
/// `map::READ | map::WRITE`, on each of `count` (at most `windows::MAX`) windows of
/// `process` (zero: the caller's own mappings), all or none, with one flush that has
/// finished on every CPU when the call returns. Needs `WRITE` on the process.
pub(crate) fn windows_protect(process: &Process, args: [u64; 6]) -> Answer {
    let [target, ranges, count, flags, ..] = args;
    windows::flags(flags)?;
    let bytes = usercopy::read(&process.space, ranges, windows::bytes(count)?)?;
    let mut list = Vec::new();
    list.try_reserve_exact(bytes.len() / RANGE_SIZE)
        .map_err(|_| Error::NoMemory)?;
    windows::parse(&bytes, &mut list)?;
    if target == 0 {
        process
            .space
            .protect_windows(&list, flags, Some(Kind::Mapping))?;
    } else {
        let Object::Process(other) = object(process, target, Rights::WRITE)? else {
            return Err(Error::WrongType);
        };
        other.space.protect_windows(&list, flags, None)?;
    }
    Ok((0, 0))
}

/// `memory_map`'s mapping of `len` bytes of `object` from `offset`, where the kernel
/// chooses, with `flags`' protection, `map::HUGE` and `map::POPULATE`; the dispatch has
/// checked the flags, the handle's rights and the range. Returns where.
#[allow(
    dead_code,
    reason = "memory_map calls it once the integrator's patch lands"
)]
pub(crate) fn map_memory(
    process: &Process,
    object: &Arc<MemoryObject>,
    offset: u64,
    len: u64,
    flags: u64,
    writable: bool,
) -> Result<u64, Error> {
    let backing = Backing::Object {
        memory: object.clone(),
        first: (offset / PAGE_SIZE) as usize,
        writable,
    };
    let prot = map::READ | flags & (map::WRITE | map::EXECUTE | map::HUGE | map::POPULATE);
    process
        .space
        .map(None, len, Kind::Mapping, prot, backing, false)
}
