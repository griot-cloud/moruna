// SPDX-License-Identifier: MPL-2.0

//! An emulated core, for host tests: memory objects are host memory, and each process keeps
//! the pages its lower range maps, with the checks the core's `process_map`,
//! `process_unmap` and `process_protect` make. A test reads a process's pages to see what
//! its program would.

extern crate std;

use alloc::{collections::BTreeMap, vec::Vec};
use core::{alloc::Layout, cell::Cell};
use std::{sync::Mutex, thread_local};

use moruna_abi::{IMAGE_RANGE, map};

use super::{
    PAGE_SIZE,
    calls::{CoreError, Made, ProgramProcess},
};
use crate::mm::MAX_USERSPACE_VADDR;

/// A call the emulated core can be made to refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    /// `memory_create` and its mapping.
    Object,
    /// `process_map`.
    Map,
    /// `process_unmap`.
    Unmap,
    /// `process_protect`.
    Protect,
}

/// A page a process maps: its object, the page's offset in it, and its `map` bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    /// The object's handle value.
    pub object: u32,
    /// The page's offset in it.
    pub offset: usize,
    /// Its protection.
    pub prot: u64,
}

/// A process: the pages it maps, and whether it has ended.
#[derive(Clone, Default)]
struct Process {
    pages: BTreeMap<usize, Page>,
    ended: bool,
}

struct World {
    /// Each object's address and length.
    objects: BTreeMap<u32, (usize, usize)>,
    processes: BTreeMap<u32, Process>,
    next: u32,
    refusals: Vec<(Call, CoreError)>,
}

static WORLD: Mutex<World> = Mutex::new(World {
    objects: BTreeMap::new(),
    processes: BTreeMap::new(),
    next: 1,
    refusals: Vec::new(),
});

fn world() -> std::sync::MutexGuard<'static, World> {
    WORLD.lock().unwrap_or_else(|e| e.into_inner())
}

impl World {
    fn refused(&mut self, call: Call) -> Result<(), CoreError> {
        match self.refusals.iter().position(|(c, _)| *c == call) {
            Some(i) => Err(self.refusals.remove(i).1),
            None => Ok(()),
        }
    }

    fn id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    fn process(&mut self, process: ProgramProcess) -> Result<&mut Process, CoreError> {
        self.processes
            .get_mut(&process.raw())
            .ok_or(CoreError::BadHandle)
    }
}

/// Makes the next call of kind `call` fail with `error`.
pub fn refuse_next(call: Call, error: moruna_abi::Error) {
    world().refusals.push((call, error));
}

/// Drops refusals no call took.
pub fn clear_refusals() {
    world().refusals.clear();
}

/// A new process with an empty lower range.
pub fn process_create() -> ProgramProcess {
    let mut w = world();
    let id = w.id();
    w.processes.insert(id, Process::default());
    ProgramProcess::from_raw(id)
}

/// A clone of `process`, as `process_clone` makes one: every object mapping is shared, so
/// the clone maps the same pages with the same protection.
pub fn process_clone(process: ProgramProcess) -> Result<ProgramProcess, moruna_abi::Error> {
    let mut w = world();
    let pages = w.process(process)?.pages.clone();
    let id = w.id();
    w.processes.insert(
        id,
        Process {
            pages,
            ended: false,
        },
    );
    Ok(ProgramProcess::from_raw(id))
}

/// Ends `process`: its pages go, and nothing more is mapped there. Its handle stays.
pub fn process_end(process: ProgramProcess) {
    if let Some(p) = world().processes.get_mut(&process.raw()) {
        p.pages.clear();
        p.ended = true;
    }
}

/// The pages `process` maps, by address.
pub fn pages(process: ProgramProcess) -> BTreeMap<usize, Page> {
    world()
        .processes
        .get(&process.raw())
        .map(|p| p.pages.clone())
        .unwrap_or_default()
}

/// Where the byte at `va` of `process` is in the host, for an access that writes when
/// `write`: what its program would reach.
pub fn translate(process: ProgramProcess, va: usize, write: bool) -> Option<usize> {
    let w = world();
    let page = *w
        .processes
        .get(&process.raw())?
        .pages
        .get(&(va & !(PAGE_SIZE - 1)))?;
    let allowed = if write {
        page.prot & map::WRITE != 0
    } else {
        page.prot & map::PROTECTION != 0
    };
    if !allowed {
        return None;
    }
    let (addr, _) = w.objects.get(&page.object)?;
    Some(addr + page.offset + va % PAGE_SIZE)
}

/// The `u64` at `va` as the program of `process` reads it.
pub fn peek(process: ProgramProcess, va: usize) -> Option<u64> {
    let at = translate(process, va & !7, false)?;
    // SAFETY: the address is an aligned word of an object's page the process maps; an
    // object is freed only once no process maps it.
    Some(unsafe { (at as *const u64).read_volatile() })
}

/// Writes `value` at `va` as the program of `process` would; false when it may not.
pub fn poke(process: ProgramProcess, va: usize, value: u64) -> bool {
    let Some(at) = translate(process, va & !7, true) else {
        return false;
    };
    // SAFETY: as for `peek`, and the page is mapped writable.
    unsafe { (at as *mut u64).write_volatile(value) };
    true
}

/// The object holding the host address `addr`, and the address's offset in it.
pub fn object_at(addr: usize) -> Option<(u32, usize)> {
    world()
        .objects
        .iter()
        .find(|(_, (a, len))| (*a..a + len).contains(&addr))
        .map(|(id, (a, _))| (*id, addr - a))
}

thread_local! {
    static OWN: Cell<Option<ProgramProcess>> = const { Cell::new(None) };
}

/// The calling thread's process: in host tests each thread is a process of its own.
pub fn own_process() -> ProgramProcess {
    OWN.with(|own| {
        own.get().unwrap_or_else(|| {
            let p = process_create();
            own.set(Some(p));
            p
        })
    })
}

pub(super) fn object_create(len: usize) -> Result<Made, CoreError> {
    let mut w = world();
    w.refused(Call::Object)?;
    if len == 0 || !len.is_multiple_of(PAGE_SIZE) {
        return Err(CoreError::InvalidArgs);
    }
    let layout = Layout::from_size_align(len, PAGE_SIZE).map_err(|_| CoreError::InvalidArgs)?;
    // SAFETY: the layout has a non-zero size.
    let addr = unsafe { alloc::alloc::alloc_zeroed(layout) } as usize;
    if addr == 0 {
        return Err(CoreError::NoMemory);
    }
    let id = w.id();
    w.objects.insert(id, (addr, len));
    Ok(Made { raw: id, addr })
}

pub(super) fn object_release(raw: u32, addr: usize, len: usize) {
    let mut w = world();
    for (process, p) in &w.processes {
        if let Some((va, _)) = p.pages.iter().find(|(_, p)| p.object == raw) {
            panic!("object {raw} released while process {process} maps it at {va:#x}");
        }
    }
    w.objects.remove(&raw);
    drop(w);
    // SAFETY: `object_create` allocated it with this layout, and nothing maps it.
    unsafe {
        alloc::alloc::dealloc(
            addr as *mut u8,
            Layout::from_size_align(len, PAGE_SIZE).unwrap(),
        )
    };
}

fn check_range(va: usize, len: usize) -> Result<usize, CoreError> {
    if len == 0 || !va.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
        return Err(CoreError::InvalidArgs);
    }
    let end = va.checked_add(len).ok_or(CoreError::OutOfRange)?;
    if (va as u64) < IMAGE_RANGE.start || end > MAX_USERSPACE_VADDR {
        return Err(CoreError::OutOfRange);
    }
    Ok(end)
}

fn check_prot(prot: u64) -> Result<(), CoreError> {
    if prot & !map::PROTECTION != 0 || (prot & map::WRITE != 0 && prot & map::EXECUTE != 0) {
        return Err(CoreError::InvalidArgs);
    }
    Ok(())
}

pub(super) fn map(
    process: ProgramProcess,
    va: usize,
    object: u32,
    offset: usize,
    len: usize,
    flags: u64,
) -> Result<(), CoreError> {
    let mut w = world();
    w.refused(Call::Map)?;
    if flags & !map::PROCESS_ALL != 0 {
        return Err(CoreError::InvalidArgs);
    }
    let prot = flags & map::PROTECTION;
    check_prot(prot)?;
    let end = check_range(va, len)?;
    let &(_, size) = w.objects.get(&object).ok_or(CoreError::BadHandle)?;
    if !offset.is_multiple_of(PAGE_SIZE) || offset + len > size {
        return Err(CoreError::OutOfRange);
    }
    let target = w.process(process)?;
    if target.ended {
        return Err(CoreError::BadState);
    }
    let pages = &mut target.pages;
    if flags & map::FIXED == 0 && pages.range(va..end).next().is_some() {
        return Err(CoreError::BadState);
    }
    for (i, page) in (va..end).step_by(PAGE_SIZE).enumerate() {
        pages.insert(
            page,
            Page {
                object,
                offset: offset + i * PAGE_SIZE,
                prot,
            },
        );
    }
    Ok(())
}

pub(super) fn unmap(process: ProgramProcess, va: usize, len: usize) -> Result<(), CoreError> {
    let mut w = world();
    w.refused(Call::Unmap)?;
    let end = check_range(va, len)?;
    let pages = &mut w.process(process)?.pages;
    let gone: Vec<usize> = pages.range(va..end).map(|(p, _)| *p).collect();
    for page in gone {
        pages.remove(&page);
    }
    Ok(())
}

pub(super) fn protect(
    process: ProgramProcess,
    va: usize,
    len: usize,
    prot: u64,
) -> Result<(), CoreError> {
    let mut w = world();
    w.refused(Call::Protect)?;
    check_prot(prot)?;
    let end = check_range(va, len)?;
    let pages = &mut w.process(process)?.pages;
    if pages.range(va..end).count() != len / PAGE_SIZE {
        return Err(CoreError::OutOfRange);
    }
    for (_, page) in pages.range_mut(va..end) {
        page.prot = prot;
    }
    Ok(())
}
