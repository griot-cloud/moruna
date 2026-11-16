// SPDX-License-Identifier: MPL-2.0

//! A program's address space.
//!
//! OSTD's `VmSpace` is a user page table. Here it has two parts:
//!
//! - **The record:** which frame or device page each virtual page maps, with what property,
//!   kept with OSTD's cursor API, so Asterinas's `vm/` carries unchanged, copy-on-write
//!   `fork` included.
//! - **What the program sees:** a space shown in a process ([`VmSpace::show_in`]) is
//!   mirrored into that process's lower range by the core: `process_map` of each frame's
//!   chunk and offset, `process_unmap` and `process_protect` (`calls`).
//!
//! A cursor gathers its changes and makes the calls when it is dropped, a run of
//! neighbouring pages of one chunk in one call: fault-around's pages, a fork's protections,
//! an unmap of any length. The frames it unmaps are kept until the calls are made, so the
//! program never reaches a frame the personality has freed.
//!
//! A call the core refuses leaves the page in the record but not shown: the program's next
//! fault there reaches the personality, which shows it then ([`VmSpace::serve_lagging`]).
//! A refused unmap would leave the program a frame the personality frees, so it ends the
//! personality.
//!
//! A cursor locks its space for as long as it lives; OSTD locks only the cursor's range,
//! so here two cursors on one space wait for each other.

pub(crate) mod calls;
#[cfg(not(target_os = "none"))]
pub mod emulated;
#[cfg(target_os = "none")]
mod moruna;
#[cfg(all(test, ktest))]
mod test;

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::ops::Range;

pub use self::calls::{CallCounts, ProgramProcess, call_counts};
use super::{
    AnyUFrameMeta, Fallible, FrameRef, HasPaddr, HasSize, PAGE_SIZE, Paddr, UFrame, Vaddr,
    VmReader, VmWriter, is_in_user_space,
    page_prop::{CachePolicy, PageFlags, PageProperty},
    tlb::TlbFlusher,
};
use crate::{
    Error, Result,
    io::IoMem,
    sync::raw::{RawGuard, RawLock},
    task::{DisabledPreemptGuard, atomic_mode::AsAtomicModeGuard, disable_preempt},
};

#[derive(Debug)]
enum Item {
    Frame(UFrame),
    Io(Paddr),
}

/// What the core shows of a page: nothing, or the page with some `map` bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown(u8);

impl Shown {
    const NOT: Shown = Shown(0);
    const BIT: u8 = 0x80;

    fn with(prot: u64) -> Shown {
        Shown(Self::BIT | prot as u8)
    }

    fn prot(self) -> Option<u64> {
        (self.0 & Self::BIT != 0).then_some((self.0 & !Self::BIT) as u64)
    }
}

#[derive(Debug)]
struct Entry {
    item: Item,
    prop: PageProperty,
    shown: Shown,
}

/// The `map` bits a page shows with; a page both writable and executable keeps the side it
/// shows now.
fn prot_for(prop: &PageProperty, shown: Shown) -> u64 {
    let f = prop.flags;
    let exec = shown
        .prot()
        .is_some_and(|p| p & moruna_abi::map::EXECUTE != 0);
    calls::prot_of(
        f.contains(PageFlags::R),
        f.contains(PageFlags::W),
        f.contains(PageFlags::X),
        exec,
    )
}

type Pages = BTreeMap<Vaddr, Entry>;

struct State {
    pages: Pages,
    /// The process the space is shown in.
    process: Option<ProgramProcess>,
}

/// A program's address space.
pub struct VmSpace {
    state: RawLock<State>,
}

/// The space shown in each process. Taken before any space's own lock.
static SHOWN: RawLock<BTreeMap<ProgramProcess, Weak<VmSpace>>> = RawLock::new(BTreeMap::new());

/// How a thread finds the process its program runs in.
static OWN: spin::Once<fn() -> Option<ProgramProcess>> = spin::Once::new();

/// Injects how a thread finds the process its program runs in, which
/// [`VmSpace::activate`] shows the space in; once. Without one it is the personality's own
/// process (`backend::self_process`), and in host tests each thread is an emulated process
/// of its own.
pub fn inject_own_process(own: fn() -> Option<ProgramProcess>) {
    OWN.call_once(|| own);
}

fn own_process() -> Option<ProgramProcess> {
    if let Some(own) = OWN.get() {
        return own();
    }
    #[cfg(not(target_os = "none"))]
    return Some(emulated::own_process());
    #[cfg(target_os = "none")]
    crate::backend::self_process().map(ProgramProcess::from_raw)
}

/// The program's whole lower range, for an unmap of all of it.
fn lower_range() -> (usize, usize) {
    let start = moruna_abi::IMAGE_RANGE.start as usize;
    (start, super::MAX_USERSPACE_VADDR - start)
}

impl core::fmt::Debug for VmSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmSpace").finish_non_exhaustive()
    }
}

impl VmSpace {
    /// An empty space, shown nowhere.
    pub fn new() -> Self {
        Self {
            state: RawLock::new(State {
                pages: BTreeMap::new(),
                process: None,
            }),
        }
    }

    /// A cursor over `va`, which must be page-aligned user addresses.
    pub fn cursor<'a, G: AsAtomicModeGuard + ?Sized>(
        &'a self,
        _guard: &'a G,
        va: &Range<Vaddr>,
    ) -> Result<Cursor<'a>> {
        Ok(Cursor(Inner::new(self, va)?))
    }

    /// A cursor that may change the mappings in `va`.
    pub fn cursor_mut<'a, G: AsAtomicModeGuard + ?Sized>(
        &'a self,
        _guard: &'a G,
        va: &Range<Vaddr>,
    ) -> Result<CursorMut<'a>> {
        let inner = Inner::new(self, va)?;
        let batch = Batch::new(inner.state.process);
        Ok(CursorMut {
            inner,
            flusher: TlbFlusher::new(disable_preempt()),
            batch,
        })
    }

    /// Makes the space the current one: shows it in the calling thread's process
    /// ([`inject_own_process`]), which then shows no other. In host tests the thread's
    /// program memory is read through it too.
    pub fn activate(self: &Arc<Self>) {
        #[cfg(not(target_os = "none"))]
        crate::backend::set_active_space(self.clone());
        if let Some(process) = own_process()
            && self.shown_in() != Some(process)
        {
            self.show_in(process);
        }
    }

    /// Shows the space in `process`: its lower range then maps this space's pages and no
    /// other's. The space shown there before is hidden, and this space leaves the process
    /// it was shown in. A page the core refuses shows on its first fault.
    pub fn show_in(self: &Arc<Self>, process: ProgramProcess) {
        let displaced;
        {
            let mut shown = SHOWN.lock();
            displaced = clear_process(&mut shown, process, self);
            let mut state = self.state.lock();
            if state.process == Some(process) {
                return;
            }
            if let Some(old) = state.process.take() {
                hide_pages(old, &mut state.pages);
                if shown
                    .get(&old)
                    .is_some_and(|w| w.as_ptr() == Arc::as_ptr(self))
                {
                    shown.remove(&old);
                }
            }
            state.process = Some(process);
            shown.insert(process, Arc::downgrade(self));
            let mut ops = Vec::new();
            for (va, e) in state.pages.iter_mut() {
                if let Item::Frame(frame) = &e.item {
                    let prot = prot_for(&e.prop, Shown::NOT);
                    e.shown = Shown::with(prot);
                    push_op(&mut ops, map_op(*va, frame, prot));
                }
            }
            for op in ops {
                issue(process, op, &mut state.pages);
            }
        }
        // A space displaced here may end now: outside the lock its drop takes.
        drop(displaced);
    }

    /// Shows `self`, a space `vm/`'s fork has just made from `parent`, in a clone of the
    /// process `parent` is shown in, and returns the clone. `clone` makes it from that
    /// process (`process_clone`) while `parent` is held still. The clone maps every page
    /// `parent` shows, which is what a fork's child maps, so only the pages where `self`
    /// differs from `parent` take calls.
    pub fn show_cloned(
        self: &Arc<Self>,
        parent: &VmSpace,
        clone: impl FnOnce(ProgramProcess) -> Result<ProgramProcess>,
    ) -> Result<ProgramProcess> {
        assert!(
            !core::ptr::eq(Arc::as_ptr(self), parent),
            "a space cloned from itself"
        );
        let displaced;
        let child = {
            let mut shown = SHOWN.lock();
            let from = parent.state.lock();
            let source = from.process.ok_or(Error::InvalidArgs)?;
            let mut state = self.state.lock();
            if state.process.is_some() {
                return Err(Error::InvalidArgs);
            }
            let child = clone(source)?;
            displaced = clear_process(&mut shown, child, self);
            let mut ops = Vec::new();
            for (va, e) in state.pages.iter_mut() {
                let Item::Frame(frame) = &e.item else {
                    continue;
                };
                let theirs = match from.pages.get(va) {
                    Some(Entry {
                        item: Item::Frame(f),
                        shown,
                        ..
                    }) if f == frame => shown.prot().map(|p| (p, *shown)),
                    _ => None,
                };
                match theirs {
                    Some((have, shown_there)) => {
                        let want = prot_for(&e.prop, shown_there);
                        e.shown = Shown::with(want);
                        if want != have {
                            push_op(
                                &mut ops,
                                Op::Protect {
                                    va: *va,
                                    len: PAGE_SIZE,
                                    prot: want,
                                },
                            );
                        }
                    }
                    None => {
                        let want = prot_for(&e.prop, Shown::NOT);
                        e.shown = Shown::with(want);
                        push_op(&mut ops, map_op(*va, frame, want));
                    }
                }
            }
            // What the parent shows and the child does not hold.
            for (va, e) in from.pages.iter() {
                let held = matches!(
                    state.pages.get(va),
                    Some(Entry {
                        item: Item::Frame(_),
                        ..
                    })
                );
                if e.shown.prot().is_some() && !held {
                    push_op(
                        &mut ops,
                        Op::Unmap {
                            va: *va,
                            len: PAGE_SIZE,
                        },
                    );
                }
            }
            state.process = Some(child);
            shown.insert(child, Arc::downgrade(self));
            for op in ops {
                issue(child, op, &mut state.pages);
            }
            child
        };
        drop(displaced);
        Ok(child)
    }

    /// Hides the space: the process it is shown in maps none of its pages.
    pub fn hide(&self) {
        let mut shown = SHOWN.lock();
        let mut state = self.state.lock();
        if let Some(process) = state.process.take() {
            hide_pages(process, &mut state.pages);
            if shown
                .get(&process)
                .is_some_and(|w| core::ptr::eq(w.as_ptr(), self))
            {
                shown.remove(&process);
            }
        }
    }

    /// The process the space is shown in.
    pub fn shown_in(&self) -> Option<ProgramProcess> {
        self.state.lock().process
    }

    /// Serves a fault of the program at `va` that the record already allows but the core
    /// does not show: a page a refused call left behind, or the other side of a page both
    /// writable and executable, which the core never shows at once. A write when `write`,
    /// an instruction fetch when `exec`. True when served: the program may go on.
    pub fn serve_lagging(&self, va: Vaddr, write: bool, exec: bool) -> Result<bool> {
        let mut state = self.state.lock();
        let Some(process) = state.process else {
            return Ok(false);
        };
        let page = va & !(PAGE_SIZE - 1);
        let Some(e) = state.pages.get_mut(&page) else {
            return Ok(false);
        };
        let Item::Frame(frame) = &e.item else {
            return Ok(false);
        };
        let f = e.prop.flags;
        let allowed = if write {
            f.contains(PageFlags::W)
        } else if exec {
            f.contains(PageFlags::X)
        } else {
            f.intersects(PageFlags::RWX)
        };
        if !allowed {
            return Ok(false);
        }
        let want = calls::prot_of(
            f.contains(PageFlags::R),
            f.contains(PageFlags::W),
            f.contains(PageFlags::X),
            exec,
        );
        if e.shown.prot() == Some(want) {
            // Shown meanwhile, by another thread.
            return Ok(true);
        }
        let done = match e.shown.prot() {
            None => {
                let (object, offset) = frame.object_offset();
                calls::map(process, page, object, offset, PAGE_SIZE, want)
            }
            Some(_) => calls::protect(process, page, PAGE_SIZE, want),
        };
        done.map_err(calls::to_error)?;
        e.shown = Shown::with(want);
        Ok(true)
    }

    /// The personality's address of the byte at `va`, if the process the space is shown in
    /// maps a page there with the access asked for, as its program would reach it; `None`
    /// too when the space is shown nowhere or locked by a cursor. Host tests only.
    #[cfg(not(target_os = "none"))]
    pub(crate) fn translate(&self, va: Vaddr, write: bool) -> Option<usize> {
        let process = self.state.try_lock()?.process?;
        emulated::translate(process, va, write)
    }

    /// A reader over the program's memory at `vaddr`.
    pub fn reader(&self, vaddr: Vaddr, len: usize) -> Result<VmReader<'_, Fallible>> {
        if !is_in_user_space(vaddr, len) {
            return Err(Error::AccessDenied);
        }
        // SAFETY: the range is in the program's memory, reached by the fault-tolerant copy.
        Ok(unsafe { VmReader::<Fallible>::from_user_space(vaddr as *const u8, len) })
    }

    /// A writer over the program's memory at `vaddr`.
    pub fn writer(&self, vaddr: Vaddr, len: usize) -> Result<VmWriter<'_, Fallible>> {
        if !is_in_user_space(vaddr, len) {
            return Err(Error::AccessDenied);
        }
        // SAFETY: as for `reader`.
        Ok(unsafe { VmWriter::<Fallible>::from_user_space(vaddr as *mut u8, len) })
    }

    /// A reader and a writer over the same range.
    pub fn reader_writer(
        &self,
        vaddr: Vaddr,
        len: usize,
    ) -> Result<(VmReader<'_, Fallible>, VmWriter<'_, Fallible>)> {
        Ok((self.reader(vaddr, len)?, self.writer(vaddr, len)?))
    }

    /// How many pages the space maps.
    pub fn mapped_pages(&self) -> usize {
        self.state.lock().pages.len()
    }
}

impl Default for VmSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VmSpace {
    fn drop(&mut self) {
        let Some(process) = self.state.lock().process else {
            return;
        };
        let mut shown = SHOWN.lock();
        // A space that took the process over has unmapped its whole lower range already.
        if shown
            .get(&process)
            .is_some_and(|w| core::ptr::eq(w.as_ptr(), self))
        {
            shown.remove(&process);
            let mut state = self.state.lock();
            hide_pages(process, &mut state.pages);
            state.process = None;
        }
    }
}

/// Hides the space shown in `process`, unless it is `keep`; returns it, to be dropped
/// once `SHOWN` is released.
fn clear_process(
    shown: &mut BTreeMap<ProgramProcess, Weak<VmSpace>>,
    process: ProgramProcess,
    keep: &Arc<VmSpace>,
) -> Option<Arc<VmSpace>> {
    let weak = shown.get(&process)?;
    if weak.as_ptr() == Arc::as_ptr(keep) {
        return None;
    }
    let old = weak.upgrade();
    shown.remove(&process);
    match &old {
        Some(old) => {
            let mut state = old.state.lock();
            hide_pages(process, &mut state.pages);
            state.process = None;
        }
        None => {
            // The space is ending, its pages not yet unmapped: unmap them all here, so its
            // drop, which finds the process taken, may free them.
            let (start, len) = lower_range();
            if let Err(e) = calls::unmap(process, start, len) {
                panic!("the core refused to clear a process for a new space: {e:?}");
            }
        }
    }
    old
}

/// Unmaps every page of `pages` that `process` shows.
fn hide_pages(process: ProgramProcess, pages: &mut Pages) {
    let mut span: Option<(Vaddr, Vaddr)> = None;
    for (va, e) in pages.iter_mut() {
        if e.shown.prot().is_some() {
            span = Some((span.map_or(*va, |s| s.0), *va + PAGE_SIZE));
            e.shown = Shown::NOT;
        }
    }
    if let Some((start, end)) = span
        && let Err(e) = calls::unmap(process, start, end - start)
    {
        panic!("the core refused to unmap a hidden space: {e:?}");
    }
}

/// A call on the core not made yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Map {
        va: Vaddr,
        object: u32,
        offset: usize,
        len: usize,
        prot: u64,
    },
    Unmap {
        va: Vaddr,
        len: usize,
    },
    Protect {
        va: Vaddr,
        len: usize,
        prot: u64,
    },
}

fn map_op(va: Vaddr, frame: &UFrame, prot: u64) -> Op {
    let (object, offset) = frame.object_offset();
    Op::Map {
        va,
        object,
        offset,
        len: PAGE_SIZE,
        prot,
    }
}

/// Grows `pending` by `next` when one call makes both.
fn absorb(pending: &mut Op, next: &Op) -> bool {
    let grown = match (*pending, *next) {
        (
            Op::Map {
                va,
                object,
                offset,
                len,
                prot,
            },
            Op::Map {
                va: v,
                object: o,
                offset: off,
                len: l,
                prot: p,
            },
        ) if va + len == v && object == o && offset + len == off && prot == p => Op::Map {
            va,
            object,
            offset,
            len: len + l,
            prot,
        },
        // A map is `FIXED`: over just what was unmapped, it replaces it in one call.
        (Op::Unmap { va, len }, Op::Map { va: v, len: l, .. }) if va == v && len == l => *next,
        (Op::Unmap { va, len }, Op::Unmap { va: v, len: l }) if va + len == v => {
            Op::Unmap { va, len: len + l }
        }
        (
            Op::Protect { va, len, prot },
            Op::Protect {
                va: v,
                len: l,
                prot: p,
            },
        ) if va + len == v && prot == p => Op::Protect {
            va,
            len: len + l,
            prot,
        },
        _ => return false,
    };
    *pending = grown;
    true
}

fn push_op(ops: &mut Vec<Op>, op: Op) {
    if let Some(last) = ops.last_mut()
        && absorb(last, &op)
    {
        return;
    }
    ops.push(op);
}

/// Makes `op`'s call on `process`. A refused map or protection leaves its pages lagging
/// (shown on their next fault); a refused unmap ends the personality, since the program
/// would keep frames the personality frees.
fn issue(process: ProgramProcess, op: Op, pages: &mut Pages) {
    let lag = |pages: &mut Pages, va: Vaddr, len: usize| {
        for (_, e) in pages.range_mut(va..va + len) {
            e.shown = Shown::NOT;
        }
        calls::LAGGING.fetch_add(
            (len / PAGE_SIZE) as u64,
            core::sync::atomic::Ordering::Relaxed,
        );
    };
    match op {
        Op::Map {
            va,
            object,
            offset,
            len,
            prot,
        } => {
            if calls::map(process, va, object, offset, len, prot).is_err() {
                lag(pages, va, len);
            }
        }
        Op::Protect { va, len, prot } => {
            if calls::protect(process, va, len, prot).is_err() {
                // Access the record took away must not stay: the pages go, and show again
                // on their next fault.
                if let Err(e) = calls::unmap(process, va, len) {
                    panic!("the core refused to take a program's pages away: {e:?}");
                }
                lag(pages, va, len);
            }
        }
        Op::Unmap { va, len } => {
            if let Err(e) = calls::unmap(process, va, len) {
                panic!("the core refused to unmap a program's pages: {e:?}");
            }
        }
    }
}

/// Frames kept for calls not made yet, at most, before the calls are made.
const HOLD_MAX: usize = 1024;

/// A cursor's calls not made yet, and the frames they unmap.
struct Batch {
    process: Option<ProgramProcess>,
    pending: Option<Op>,
    held: Vec<UFrame>,
}

impl Batch {
    fn new(process: Option<ProgramProcess>) -> Self {
        Self {
            process,
            pending: None,
            held: Vec::new(),
        }
    }

    fn push(&mut self, pages: &mut Pages, op: Op) {
        if let Some(pending) = &mut self.pending
            && absorb(pending, &op)
        {
            return;
        }
        self.issue_pending(pages);
        self.pending = Some(op);
    }

    fn issue_pending(&mut self, pages: &mut Pages) {
        if let (Some(process), Some(op)) = (self.process, self.pending.take()) {
            issue(process, op, pages);
        }
    }

    /// Makes every call, then lets the frames they unmapped go.
    fn flush(&mut self, pages: &mut Pages) {
        self.issue_pending(pages);
        self.held.clear();
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        debug_assert!(self.pending.is_none(), "a cursor's calls were never made");
    }
}

/// What a query finds at the cursor.
pub enum VmQueriedItem<'a> {
    /// A frame.
    MappedRam {
        /// The frame.
        frame: FrameRef<'a, dyn AnyUFrameMeta>,
        /// Its property.
        prop: PageProperty,
    },
    /// A device page.
    MappedIoMem {
        /// Its address.
        paddr: Paddr,
        /// Its property.
        prop: PageProperty,
    },
}

impl VmQueriedItem<'_> {
    /// The property of the page.
    pub fn prop(&self) -> &PageProperty {
        match self {
            Self::MappedRam { prop, .. } | Self::MappedIoMem { prop, .. } => prop,
        }
    }
}

struct Inner<'a> {
    state: RawGuard<'a, State>,
    range: Range<Vaddr>,
    va: Vaddr,
}

impl<'a> Inner<'a> {
    fn new(space: &'a VmSpace, va: &Range<Vaddr>) -> Result<Self> {
        if !va.start.is_multiple_of(PAGE_SIZE)
            || !va.end.is_multiple_of(PAGE_SIZE)
            || va.start >= va.end
            || !is_in_user_space(va.start, va.end - va.start)
        {
            return Err(Error::InvalidArgs);
        }
        Ok(Self {
            state: space.state.lock(),
            range: va.clone(),
            va: va.start,
        })
    }

    fn query(&self) -> Result<(Range<Vaddr>, Option<VmQueriedItem<'_>>)> {
        if self.va >= self.range.end {
            return Err(Error::InvalidArgs);
        }
        if let Some(e) = self.state.pages.get(&self.va) {
            let item = match &e.item {
                Item::Frame(f) => VmQueriedItem::MappedRam {
                    frame: FrameRef::new(f),
                    prop: e.prop,
                },
                Item::Io(paddr) => VmQueriedItem::MappedIoMem {
                    paddr: *paddr,
                    prop: e.prop,
                },
            };
            return Ok((self.va..self.va + PAGE_SIZE, Some(item)));
        }
        // A hole: up to the next mapped page or the cursor's end.
        let end = self
            .state
            .pages
            .range(self.va..self.range.end)
            .next()
            .map_or(self.range.end, |(va, _)| *va);
        Ok((self.va..end, None))
    }

    /// The first mapped page in `[va, va + len)`.
    fn next_mapped(&self, len: usize) -> Option<Vaddr> {
        self.check_len(len);
        self.state
            .pages
            .range(self.va..self.va + len)
            .next()
            .map(|(va, _)| *va)
    }

    fn check_len(&self, len: usize) {
        assert!(
            len.is_multiple_of(PAGE_SIZE),
            "the length is not page-aligned"
        );
        assert!(
            self.va + len <= self.range.end,
            "the length passes the cursor's range"
        );
    }

    fn find_next(&mut self, len: usize) -> Option<Vaddr> {
        match self.next_mapped(len) {
            Some(va) => {
                self.va = va;
                Some(va)
            }
            None => {
                self.va += len;
                None
            }
        }
    }

    fn jump(&mut self, va: Vaddr) -> Result<()> {
        assert!(
            va.is_multiple_of(PAGE_SIZE),
            "the address is not page-aligned"
        );
        if !(self.range.start..=self.range.end).contains(&va) {
            return Err(Error::InvalidArgs);
        }
        self.va = va;
        Ok(())
    }
}

/// A cursor that reads mappings.
pub struct Cursor<'a>(Inner<'a>);

impl Cursor<'_> {
    /// The mapping at the cursor: its range and what it maps, if anything. The cursor
    /// stays.
    pub fn query(&mut self) -> Result<(Range<Vaddr>, Option<VmQueriedItem<'_>>)> {
        self.0.query()
    }

    /// Moves to the next mapped page within `len` bytes and returns it; or past them.
    pub fn find_next(&mut self, len: usize) -> Option<Vaddr> {
        self.0.find_next(len)
    }

    /// Moves to `va`, inside the cursor's range.
    pub fn jump(&mut self, va: Vaddr) -> Result<()> {
        self.0.jump(va)
    }

    /// Where the cursor is.
    pub fn virt_addr(&self) -> Vaddr {
        self.0.va
    }
}

/// A cursor that may change mappings. Its calls on the core are made when it is dropped.
pub struct CursorMut<'a> {
    inner: Inner<'a>,
    flusher: TlbFlusher<'a, DisabledPreemptGuard>,
    batch: Batch,
}

impl Drop for CursorMut<'_> {
    fn drop(&mut self) {
        self.batch.flush(&mut self.inner.state.pages);
    }
}

impl<'a> CursorMut<'a> {
    /// As [`Cursor::query`].
    pub fn query(&mut self) -> Result<(Range<Vaddr>, Option<VmQueriedItem<'_>>)> {
        self.inner.query()
    }

    /// As [`Cursor::find_next`].
    pub fn find_next(&mut self, len: usize) -> Option<Vaddr> {
        self.inner.find_next(len)
    }

    /// As [`Cursor::jump`].
    pub fn jump(&mut self, va: Vaddr) -> Result<()> {
        self.inner.jump(va)
    }

    /// Where the cursor is.
    pub fn virt_addr(&self) -> Vaddr {
        self.inner.va
    }

    /// The flusher for the TLB entries the cursor's changes leave.
    pub fn flusher(&mut self) -> &mut TlbFlusher<'a, DisabledPreemptGuard> {
        &mut self.flusher
    }

    /// Maps `frame` at the cursor, which must not be mapped, and moves past it.
    pub fn map(&mut self, frame: UFrame, prop: PageProperty) {
        let va = self.inner.va;
        let call = self.batch.process.map(|_| {
            let prot = prot_for(&prop, Shown::NOT);
            (map_op(va, &frame, prot), Shown::with(prot))
        });
        let shown = call.map_or(Shown::NOT, |(_, s)| s);
        self.put(Item::Frame(frame), prop, shown);
        if let Some((op, _)) = call {
            self.batch.push(&mut self.inner.state.pages, op);
        }
    }

    fn put(&mut self, item: Item, prop: PageProperty, shown: Shown) {
        let va = self.inner.va;
        assert!(
            va < self.inner.range.end,
            "mapping outside the cursor's range"
        );
        let old = self
            .inner
            .state
            .pages
            .insert(va, Entry { item, prop, shown });
        assert!(old.is_none(), "mapping over an already mapped page");
        self.inner.va += PAGE_SIZE;
    }

    /// Maps `len` bytes of `io_mem` from `offset` at the cursor. The personality keeps the
    /// record; no device memory is shown to a program.
    pub fn map_iomem(&mut self, io_mem: IoMem, prop: PageProperty, len: usize, offset: usize) {
        assert!(len.is_multiple_of(PAGE_SIZE) && offset.is_multiple_of(PAGE_SIZE));
        if offset >= io_mem.size() {
            return;
        }
        let end = io_mem.size().min(offset + len);
        for paddr in (io_mem.paddr() + offset..io_mem.paddr() + end).step_by(PAGE_SIZE) {
            self.put(Item::Io(paddr), prop, Shown::NOT);
        }
    }

    /// The device memory mapped at `paddr`, and the offset in it: none in user space.
    pub fn find_iomem_by_paddr(&self, _paddr: Paddr) -> Option<(IoMem, usize)> {
        None
    }

    /// Unmaps the mapped pages in `len` bytes from the cursor; returns how many frames it
    /// unmapped. The cursor ends at the end of the range.
    pub fn unmap(&mut self, len: usize) -> usize {
        self.inner.check_len(len);
        let end = self.inner.va + len;
        let pages = &mut self.inner.state.pages;
        let vas: Vec<Vaddr> = pages.range(self.inner.va..end).map(|(va, _)| *va).collect();
        let mut frames = 0;
        let mut span: Option<(Vaddr, Vaddr)> = None;
        let mut kept = Vec::new();
        for va in vas {
            let Some(e) = pages.remove(&va) else {
                continue;
            };
            let shown = e.shown.prot().is_some();
            if shown {
                span = Some((span.map_or(va, |s| s.0), va + PAGE_SIZE));
            }
            if let Item::Frame(frame) = e.item {
                frames += 1;
                if shown {
                    kept.push(frame);
                }
            }
        }
        if let Some((start, stop)) = span {
            self.batch.push(
                pages,
                Op::Unmap {
                    va: start,
                    len: stop - start,
                },
            );
            self.batch.held.extend(kept);
            if self.batch.held.len() >= HOLD_MAX {
                self.batch.flush(pages);
            }
        }
        self.inner.va = end;
        frames
    }

    /// Applies `op` to the next mapped page in `len` bytes from the cursor, moves past it
    /// and returns its range; or moves past the range and returns `None`.
    pub fn protect_next(
        &mut self,
        len: usize,
        mut op: impl FnMut(&mut PageFlags, &mut CachePolicy),
    ) -> Option<Range<Vaddr>> {
        let Some(va) = self.inner.next_mapped(len) else {
            self.inner.va += len;
            return None;
        };
        let pages = &mut self.inner.state.pages;
        let e = pages.get_mut(&va).unwrap();
        op(&mut e.prop.flags, &mut e.prop.cache);
        let mut call = None;
        if let (Some(have), Item::Frame(_)) = (e.shown.prot(), &e.item) {
            let want = prot_for(&e.prop, e.shown);
            if want != have {
                e.shown = Shown::with(want);
                call = Some(Op::Protect {
                    va,
                    len: PAGE_SIZE,
                    prot: want,
                });
            }
        }
        if let Some(call) = call {
            self.batch.push(pages, call);
        }
        self.inner.va = va + PAGE_SIZE;
        Some(va..va + PAGE_SIZE)
    }
}
