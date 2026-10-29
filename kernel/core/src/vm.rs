//! A process's address space (SDD 20, f.5 and f.12): its areas, and the page table
//! they fill.
//!
//! An area is a run of whole pages with one protection and one backing: pages of a
//! memory object, zero pages allocated the first time a thread touches them, or device
//! memory. Unmapping and changing the protection of part of an area splits it, so any
//! run of whole pages can be unmapped or protected. A zero page, once allocated, is
//! kept with the space until its page is unmapped, so a page made inaccessible and
//! then accessible again keeps its bytes; the page table holds an entry only for pages
//! that are accessible, because OSTD tracks a frame only while its entry is present.
//!
//! Lock order: the space's lock is a leaf; it is never held while a handle is dropped.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use moruna_abi::{Error, PAGE_SIZE, map};
use ostd::{
    mm::{
        CachePolicy, FrameAllocOptions, MAX_USERSPACE_VADDR, PageFlags, PageProperty, UFrame, VmIo,
        VmSpace,
    },
    sync::SpinLock,
    task::disable_preempt,
};

use crate::memory::{DeviceMemory, MemoryObject};

pub use moruna_abi::IMAGE_RANGE;
/// Where `memory_map`, stacks and the start page are placed when the kernel chooses.
pub const MAP_BASE: u64 = IMAGE_RANGE.end;

/// The end of user space, page aligned.
pub fn map_end() -> u64 {
    MAX_USERSPACE_VADDR as u64 & !(PAGE_SIZE - 1)
}

/// Whether `addr` is a user address (used for entry points and stack tops).
pub fn is_user(addr: u64) -> bool {
    (IMAGE_RANGE.start..=map_end()).contains(&addr)
}

/// Rounds `len` up to whole pages.
pub fn pages(len: u64) -> Result<u64, Error> {
    len.checked_add(PAGE_SIZE - 1)
        .map(|v| v & !(PAGE_SIZE - 1))
        .ok_or(Error::OutOfRange)
}

/// What made an area, which decides what a native process may do to it itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A segment of the image the kernel loaded.
    Image,
    /// The stack the kernel gave the first thread.
    Stack,
    /// The start page.
    StartPage,
    /// `memory_map`, or anything another process mapped with `process_map`, or a
    /// forwarded process's own fast `mmap` and `brk`.
    Mapping,
}

/// What fills an area's pages.
#[derive(Clone)]
pub enum Backing {
    /// Pages of a memory object, from its page `first`; `writable` when the handle it
    /// was mapped through carried `WRITE`.
    Object {
        memory: Arc<MemoryObject>,
        first: usize,
        writable: bool,
    },
    /// Zero pages, each allocated when first touched.
    Zero,
    /// Device memory, uncached, from byte `offset` of the region.
    Device {
        device: Arc<DeviceMemory>,
        offset: u64,
        writable: bool,
    },
}

impl Backing {
    fn writable(&self) -> bool {
        match self {
            Backing::Object { writable, .. } | Backing::Device { writable, .. } => *writable,
            Backing::Zero => true,
        }
    }

    /// The same backing for the part of an area that starts `skip` pages in.
    fn advanced(&self, skip: u64) -> Backing {
        match self {
            Backing::Object {
                memory,
                first,
                writable,
            } => Backing::Object {
                memory: memory.clone(),
                first: first + skip as usize,
                writable: *writable,
            },
            Backing::Zero => Backing::Zero,
            Backing::Device {
                device,
                offset,
                writable,
            } => Backing::Device {
                device: device.clone(),
                offset: offset + skip * PAGE_SIZE,
                writable: *writable,
            },
        }
    }
}

#[derive(Clone)]
struct Area {
    end: u64,
    kind: Kind,
    /// `map::PROTECTION` bits; zero is no access.
    prot: u64,
    backing: Backing,
}

/// How a thread touched a page, from a page fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    Write,
    Execute,
}

fn allows(prot: u64, access: Access) -> bool {
    match access {
        Access::Read => prot & map::PROTECTION != 0,
        Access::Write => prot & map::WRITE != 0,
        Access::Execute => prot & map::EXECUTE != 0,
    }
}

fn page_flags(prot: u64) -> PageFlags {
    let mut f = PageFlags::R;
    if prot & map::WRITE != 0 {
        f |= PageFlags::W;
    }
    if prot & map::EXECUTE != 0 {
        f |= PageFlags::X;
    }
    f
}

/// The program break of a forwarded process: where its heap starts, and its end as
/// last asked for; the pages up to that end, rounded up, are mapped.
#[derive(Clone, Copy)]
struct Brk {
    start: u64,
    current: u64,
}

struct Inner {
    areas: BTreeMap<u64, Area>,
    /// Every zero page allocated, by its address.
    zero: BTreeMap<u64, UFrame>,
    brk: Option<Brk>,
}

pub struct Space {
    pub vm: Arc<VmSpace>,
    inner: SpinLock<Inner>,
}

fn zeroed_frame() -> Result<UFrame, Error> {
    let mut options = FrameAllocOptions::new();
    options.zeroed(true);
    options
        .alloc_frame()
        .map(Into::into)
        .map_err(|_| Error::NoMemory)
}

impl Default for Space {
    fn default() -> Space {
        Space::new()
    }
}

impl Space {
    pub fn new() -> Space {
        Space {
            vm: Arc::new(VmSpace::new()),
            inner: SpinLock::new(Inner {
                areas: BTreeMap::new(),
                zero: BTreeMap::new(),
                brk: None,
            }),
        }
    }

    fn unmap_ptes(&self, va: u64, len: u64) {
        if len == 0 {
            return;
        }
        let guard = disable_preempt();
        let range = va as usize..(va + len) as usize;
        if let Ok(mut cursor) = self.vm.cursor_mut(&guard, &range) {
            cursor.unmap(len as usize);
        }
    }

    /// Fills the page table for `va .. end` of `area` (which starts at `start`), whose
    /// entries must all be absent: every page of an object or a device, the zero pages
    /// already allocated. An inaccessible area has no entries.
    fn fill_ptes(
        &self,
        inner: &Inner,
        start: u64,
        area: &Area,
        va: u64,
        end: u64,
    ) -> Result<(), Error> {
        if area.prot & map::PROTECTION == 0 || va >= end {
            return Ok(());
        }
        let guard = disable_preempt();
        let range = va as usize..end as usize;
        let mut cursor = self
            .vm
            .cursor_mut(&guard, &range)
            .map_err(|_| Error::InvalidArgs)?;
        match &area.backing {
            Backing::Object { memory, first, .. } => {
                let property =
                    PageProperty::new_user(page_flags(area.prot), CachePolicy::Writeback);
                let skip = ((va - start) / PAGE_SIZE) as usize;
                let count = ((end - va) / PAGE_SIZE) as usize;
                let frames = memory
                    .frames()
                    .get(first + skip..first + skip + count)
                    .ok_or(Error::OutOfRange)?;
                for frame in frames {
                    cursor.map(frame.clone(), property);
                }
            }
            Backing::Zero => {
                let property =
                    PageProperty::new_user(page_flags(area.prot), CachePolicy::Writeback);
                for (&page, frame) in inner.zero.range(va..end) {
                    cursor.jump(page as usize).map_err(|_| Error::InvalidArgs)?;
                    cursor.map(frame.clone(), property);
                }
            }
            Backing::Device { device, offset, .. } => {
                let property =
                    PageProperty::new_user(page_flags(area.prot), CachePolicy::Uncacheable);
                cursor.map_iomem(
                    device.io.clone(),
                    property,
                    (end - va) as usize,
                    (*offset + (va - start)) as usize,
                );
            }
        }
        Ok(())
    }

    /// Splits the area holding `at`, if one does and `at` is inside it, so that an area
    /// begins at `at`.
    fn split(inner: &mut Inner, at: u64) {
        let found = inner
            .areas
            .range(..at)
            .next_back()
            .filter(|(_, a)| a.end > at)
            .map(|(s, a)| (*s, a.clone()));
        if let Some((start, area)) = found {
            let skip = (at - start) / PAGE_SIZE;
            let tail = Area {
                end: area.end,
                kind: area.kind,
                prot: area.prot,
                backing: area.backing.advanced(skip),
            };
            if let Some(head) = inner.areas.get_mut(&start) {
                head.end = at;
            }
            inner.areas.insert(at, tail);
        }
    }

    fn overlaps(inner: &Inner, va: u64, end: u64) -> bool {
        inner
            .areas
            .range(..end)
            .next_back()
            .is_some_and(|(_, a)| a.end > va)
    }

    /// A free range of `len` bytes in the kernel's mapping area, with an unmapped
    /// guard page on each side, first fit.
    fn find_free(inner: &Inner, len: u64) -> Result<u64, Error> {
        let mut candidate = MAP_BASE + PAGE_SIZE;
        for (&start, area) in inner.areas.range(MAP_BASE..) {
            let fits = candidate
                .checked_add(len)
                .and_then(|e| e.checked_add(PAGE_SIZE))
                .is_some_and(|e| e <= start);
            if fits {
                break;
            }
            candidate = candidate.max(area.end + PAGE_SIZE);
        }
        let end = candidate.checked_add(len).ok_or(Error::Limit)?;
        if end > map_end() {
            return Err(Error::Limit);
        }
        Ok(candidate)
    }

    fn check_range(va: u64, len: u64) -> Result<u64, Error> {
        if len == 0 || !va.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        let end = va.checked_add(len).ok_or(Error::OutOfRange)?;
        if va < IMAGE_RANGE.start || end > map_end() {
            return Err(Error::OutOfRange);
        }
        Ok(end)
    }

    fn check_prot(prot: u64, backing: &Backing) -> Result<(), Error> {
        if prot & !map::PROTECTION != 0 {
            return Err(Error::InvalidArgs);
        }
        if prot & map::WRITE != 0 && prot & map::EXECUTE != 0 {
            return Err(Error::InvalidArgs);
        }
        if prot & map::WRITE != 0 && !backing.writable() {
            return Err(Error::AccessDenied);
        }
        Ok(())
    }

    /// Maps `len` bytes (whole pages) of `backing` with `prot`: at `at` when it is
    /// given, replacing what is there when `fixed` and refusing (`BadState`) when not;
    /// anywhere free in the mapping area otherwise. Returns where.
    pub fn map(
        &self,
        at: Option<u64>,
        len: u64,
        kind: Kind,
        prot: u64,
        backing: Backing,
        fixed: bool,
    ) -> Result<u64, Error> {
        Space::check_prot(prot, &backing)?;
        let len = pages(len)?;
        if len == 0 {
            return Err(Error::InvalidArgs);
        }
        let mut inner = self.inner.lock();
        let va = match at {
            Some(va) => {
                let end = Space::check_range(va, len)?;
                if Space::overlaps(&inner, va, end) {
                    if !fixed {
                        return Err(Error::BadState);
                    }
                    drop(inner);
                    self.unmap(va, len, None)?;
                    inner = self.inner.lock();
                    if Space::overlaps(&inner, va, end) {
                        return Err(Error::BadState);
                    }
                }
                va
            }
            None => Space::find_free(&inner, len)?,
        };
        let area = Area {
            end: va + len,
            kind,
            prot,
            backing,
        };
        self.fill_ptes(&inner, va, &area, va, va + len)?;
        inner.areas.insert(va, area);
        Ok(va)
    }

    /// Unmaps whole pages `va .. va + len`; pages nothing maps are skipped. With
    /// `only`, every area the range touches must be of that kind, or nothing changes
    /// (`InvalidArgs`).
    pub fn unmap(&self, va: u64, len: u64, only: Option<Kind>) -> Result<(), Error> {
        let end = Space::check_range(va, len)?;
        let (removed, frames) = {
            let mut inner = self.inner.lock();
            if let Some(kind) = only {
                let touched = inner
                    .areas
                    .range(..end)
                    .rev()
                    .take_while(|(_, a)| a.end > va)
                    .any(|(_, a)| a.kind != kind);
                if touched {
                    return Err(Error::InvalidArgs);
                }
            }
            Space::split(&mut inner, va);
            Space::split(&mut inner, end);
            let starts: Vec<u64> = inner.areas.range(va..end).map(|(s, _)| *s).collect();
            let mut removed = Vec::new();
            for s in starts {
                if let Some(a) = inner.areas.remove(&s) {
                    removed.push(a);
                }
            }
            let pages: Vec<u64> = inner.zero.range(va..end).map(|(p, _)| *p).collect();
            let mut frames = Vec::new();
            for p in pages {
                if let Some(f) = inner.zero.remove(&p) {
                    frames.push(f);
                }
            }
            if let Some(brk) = &mut inner.brk
                && brk.current > va
                && brk.start < end
            {
                brk.current = brk.current.min(va).max(brk.start);
            }
            (removed, frames)
        };
        self.unmap_ptes(va, end - va);
        drop(removed);
        drop(frames);
        Ok(())
    }

    /// Sets the protection of whole pages `va .. va + len`, every one of which must be
    /// mapped (`OutOfRange` otherwise). With `only`, every area must be of that kind.
    pub fn protect(&self, va: u64, len: u64, prot: u64, only: Option<Kind>) -> Result<(), Error> {
        let end = Space::check_range(va, len)?;
        let mut inner = self.inner.lock();
        // Every page covered, of the allowed kind, and the protection allowed.
        let mut cursor = va;
        for (&start, area) in inner.areas.range(..end) {
            if area.end <= va {
                continue;
            }
            if start > cursor {
                return Err(Error::OutOfRange);
            }
            if only.is_some_and(|k| k != area.kind) {
                return Err(Error::InvalidArgs);
            }
            Space::check_prot(prot, &area.backing)?;
            cursor = area.end;
        }
        if cursor < end {
            return Err(Error::OutOfRange);
        }
        Space::split(&mut inner, va);
        Space::split(&mut inner, end);
        let starts: Vec<u64> = inner.areas.range(va..end).map(|(s, _)| *s).collect();
        for s in &starts {
            if let Some(a) = inner.areas.get_mut(s) {
                a.prot = prot;
            }
        }
        self.unmap_ptes(va, end - va);
        for s in starts {
            if let Some(area) = inner.areas.get(&s).cloned() {
                self.fill_ptes(&inner, s, &area, s, area.end)?;
            }
        }
        Ok(())
    }

    /// The area holding `addr`, with its start.
    fn area_at(inner: &Inner, addr: u64) -> Option<(u64, &Area)> {
        inner
            .areas
            .range(..=addr)
            .next_back()
            .filter(|(_, a)| a.end > addr)
            .map(|(s, a)| (*s, a))
    }

    /// The frame of page `page` of `area`, allocating a zero page the first time.
    fn frame(inner: &mut Inner, start: u64, area: &Area, page: u64) -> Result<UFrame, Error> {
        match &area.backing {
            Backing::Object { memory, first, .. } => {
                let index = first + ((page - start) / PAGE_SIZE) as usize;
                memory.frames().get(index).cloned().ok_or(Error::OutOfRange)
            }
            Backing::Zero => {
                if let Some(f) = inner.zero.get(&page) {
                    return Ok(f.clone());
                }
                let frame = zeroed_frame()?;
                inner.zero.insert(page, frame.clone());
                Ok(frame)
            }
            Backing::Device { .. } => Err(Error::Fault),
        }
    }

    /// Maps the frame of the page holding `addr` if `access` is allowed there and the
    /// page has no entry: a first touch of a zero page. Returns whether it did.
    pub fn fault(&self, addr: u64, access: Access) -> bool {
        let page = addr & !(PAGE_SIZE - 1);
        let mut inner = self.inner.lock();
        let Some((start, area)) = Space::area_at(&inner, page) else {
            return false;
        };
        if !allows(area.prot, access) {
            return false;
        }
        let area = area.clone();
        self.touch(&mut inner, start, &area, page).is_ok()
    }

    /// Gives page `page` of `area` its entry if it has none.
    fn touch(&self, inner: &mut Inner, start: u64, area: &Area, page: u64) -> Result<(), Error> {
        let guard = disable_preempt();
        let range = page as usize..(page + PAGE_SIZE) as usize;
        let mut cursor = self
            .vm
            .cursor_mut(&guard, &range)
            .map_err(|_| Error::InvalidArgs)?;
        if matches!(cursor.query(), Ok((_, Some(_)))) {
            return Ok(());
        }
        let frame = Space::frame(inner, start, area, page)?;
        cursor.jump(page as usize).map_err(|_| Error::InvalidArgs)?;
        cursor.map(
            frame,
            PageProperty::new_user(page_flags(area.prot), CachePolicy::Writeback),
        );
        Ok(())
    }

    /// Gives every page of `va .. va + len` its entry, as the first touch of each would,
    /// so the kernel can copy to and from it; `Fault` where `write` (or reading) is not
    /// allowed or nothing is mapped.
    pub fn populate(&self, va: u64, len: u64, write: bool) -> Result<(), Error> {
        if len == 0 {
            return Ok(());
        }
        let first = va & !(PAGE_SIZE - 1);
        let end = va.checked_add(len).ok_or(Error::Fault)?;
        let mut inner = self.inner.lock();
        let mut page = first;
        while page < end {
            let (start, area) = Space::area_at(&inner, page).ok_or(Error::Fault)?;
            let access = if write { Access::Write } else { Access::Read };
            if !allows(area.prot, access) {
                return Err(Error::Fault);
            }
            let area = area.clone();
            self.touch(&mut inner, start, &area, page)?;
            page += PAGE_SIZE;
        }
        Ok(())
    }

    /// Copies `buf.len()` bytes from `va` (a copy for another process: the pages are
    /// reached through their frames, not this space's page table). Reading needs pages
    /// that are accessible.
    pub fn copy_out(&self, va: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.each_page(va, buf.len(), false, |frame, within, at, n| {
            frame
                .read_bytes(within, &mut buf[at..at + n])
                .map_err(|_| Error::Fault)
        })
    }

    /// Copies `data` to `va` of this space, whose pages must be writable.
    pub fn copy_in(&self, va: u64, data: &[u8]) -> Result<(), Error> {
        self.each_page(va, data.len(), true, |frame, within, at, n| {
            frame
                .write_bytes(within, &data[at..at + n])
                .map_err(|_| Error::Fault)
        })
    }

    fn each_page(
        &self,
        va: u64,
        len: usize,
        write: bool,
        mut f: impl FnMut(&UFrame, usize, usize, usize) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut done = 0usize;
        while done < len {
            let at = va.checked_add(done as u64).ok_or(Error::Fault)?;
            let page = at & !(PAGE_SIZE - 1);
            let within = (at - page) as usize;
            let n = (PAGE_SIZE as usize - within).min(len - done);
            let frame = {
                let mut inner = self.inner.lock();
                let (start, area) = Space::area_at(&inner, page).ok_or(Error::Fault)?;
                let access = if write { Access::Write } else { Access::Read };
                if !allows(area.prot, access) {
                    return Err(Error::Fault);
                }
                let area = area.clone();
                let frame = Space::frame(&mut inner, start, &area, page)?;
                if matches!(area.backing, Backing::Zero) {
                    // A page first touched by a copy is touched for the process too.
                    self.touch(&mut inner, start, &area, page)?;
                }
                frame
            };
            f(&frame, within, done, n)?;
            done += n;
        }
        Ok(())
    }

    /// Sets where a forwarded process's heap begins, once.
    pub fn set_brk(&self, start: u64) -> Result<(), Error> {
        let start = pages(start)?;
        let mut inner = self.inner.lock();
        if inner.brk.is_some() {
            return Err(Error::BadState);
        }
        inner.brk = Some(Brk {
            start,
            current: start,
        });
        Ok(())
    }

    /// Moves where the heap begins to `start` (none when zero), for a program the
    /// process's server loaded in place of another; the old heap must be unmapped.
    pub fn reset_brk(&self, start: u64) -> Result<(), Error> {
        let mut inner = self.inner.lock();
        inner.brk = None;
        if start == 0 {
            return Ok(());
        }
        let start = pages(start)?;
        if Space::area_at(&inner, start).is_some() {
            return Err(Error::BadState);
        }
        inner.brk = Some(Brk {
            start,
            current: start,
        });
        Ok(())
    }

    /// `brk`: moves the heap's end to `want` and returns the end. A request it cannot
    /// meet (below the start, over another mapping, no memory) leaves the end where it
    /// was, and that is what it returns, as Linux does.
    pub fn brk(&self, want: u64) -> u64 {
        let (start, current) = {
            let inner = self.inner.lock();
            match inner.brk {
                Some(b) => (b.start, b.current),
                None => return 0,
            }
        };
        if want < start {
            return current;
        }
        let (Ok(have), Ok(need)) = (pages(current), pages(want)) else {
            return current;
        };
        if need > MAP_BASE {
            return current;
        }
        let moved = if need > have {
            self.map(
                Some(have),
                need - have,
                Kind::Mapping,
                map::READ | map::WRITE,
                Backing::Zero,
                false,
            )
            .map(|_| ())
        } else if need < have {
            self.unmap(need, have - need, None)
        } else {
            Ok(())
        };
        if moved.is_err() {
            return current;
        }
        if let Some(b) = &mut self.inner.lock().brk {
            b.current = want;
        }
        want
    }

    /// `madvise(DONTNEED)`: the zero pages of the range go, so they read zero again; the
    /// pages of a memory object are its own and stay.
    pub fn discard(&self, va: u64, len: u64) -> Result<(), Error> {
        let end = Space::check_range(va, len)?;
        let frames: Vec<UFrame> = {
            let mut inner = self.inner.lock();
            let mut cursor = va;
            for (&start, area) in inner.areas.range(..end) {
                if area.end <= va {
                    continue;
                }
                if start > cursor {
                    return Err(Error::OutOfRange);
                }
                cursor = area.end;
            }
            if cursor < end {
                return Err(Error::OutOfRange);
            }
            let pages: Vec<u64> = inner.zero.range(va..end).map(|(p, _)| *p).collect();
            pages.iter().filter_map(|p| inner.zero.remove(p)).collect()
        };
        self.unmap_ptes(va, end - va);
        drop(frames);
        Ok(())
    }

    /// Ends the space: every area and every page goes.
    pub fn clear(&self) {
        let (areas, zero) = {
            let mut inner = self.inner.lock();
            inner.brk = None;
            (
                core::mem::take(&mut inner.areas),
                core::mem::take(&mut inner.zero),
            )
        };
        for (start, area) in &areas {
            self.unmap_ptes(*start, area.end - start);
        }
        drop(areas);
        drop(zero);
    }
}
