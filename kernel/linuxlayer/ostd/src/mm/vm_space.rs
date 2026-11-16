// SPDX-License-Identifier: MPL-2.0

//! A program's address space, as the personality keeps it.
//!
//! OSTD's `VmSpace` is a user page table. Here it is the personality's record of the
//! program's pages: which frame or device page each virtual page maps, with what property,
//! kept with OSTD's cursor API so Asterinas's `vm/` carries unchanged. Making the program
//! see them (the core's `process_map`, `process_unmap` and `process_protect` of the frames'
//! memory objects, then B1's clone and pager) is the space's backend, stream B2-vm's.
//!
//! A cursor locks its space for as long as it lives; OSTD locks only the cursor's range,
//! so here two cursors on one space wait for each other.

use alloc::{collections::BTreeMap, sync::Arc};
use core::ops::Range;

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

#[derive(Debug)]
struct Entry {
    item: Item,
    prop: PageProperty,
}

type Table = BTreeMap<Vaddr, Entry>;

/// A program's address space.
pub struct VmSpace {
    table: RawLock<Table>,
}

impl core::fmt::Debug for VmSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmSpace").finish_non_exhaustive()
    }
}

impl VmSpace {
    /// An empty space.
    pub fn new() -> Self {
        Self {
            table: RawLock::new(BTreeMap::new()),
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
        Ok(CursorMut {
            inner: Inner::new(self, va)?,
            flusher: TlbFlusher::new(disable_preempt()),
        })
    }

    /// Makes the space the current one. On the target the program's own address space is
    /// always current for its threads; in host tests the thread's program memory is read
    /// through this space's pages.
    pub fn activate(self: &Arc<Self>) {
        #[cfg(not(target_os = "none"))]
        crate::backend::set_active_space(self.clone());
    }

    /// The personality's address of the byte at `va`, if a frame is mapped there with the
    /// access asked for; `None` too when the space is locked by a cursor. Host tests only.
    #[cfg(not(target_os = "none"))]
    pub(crate) fn translate(&self, va: Vaddr, write: bool) -> Option<usize> {
        use super::HasPaddr as _;
        let table = self.table.try_lock()?;
        let e = table.get(&(va & !(PAGE_SIZE - 1)))?;
        let allowed = if write {
            e.prop.flags.contains(PageFlags::W)
        } else {
            e.prop.flags.contains(PageFlags::R)
        };
        match &e.item {
            Item::Frame(f) if allowed => Some(f.paddr() + va % PAGE_SIZE),
            _ => None,
        }
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
        self.table.lock().len()
    }
}

impl Default for VmSpace {
    fn default() -> Self {
        Self::new()
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
    table: RawGuard<'a, Table>,
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
            table: space.table.lock(),
            range: va.clone(),
            va: va.start,
        })
    }

    fn query(&self) -> Result<(Range<Vaddr>, Option<VmQueriedItem<'_>>)> {
        if self.va >= self.range.end {
            return Err(Error::InvalidArgs);
        }
        if let Some(e) = self.table.get(&self.va) {
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
            .table
            .range(self.va..self.range.end)
            .next()
            .map_or(self.range.end, |(va, _)| *va);
        Ok((self.va..end, None))
    }

    /// The first mapped page in `[va, va + len)`.
    fn next_mapped(&self, len: usize) -> Option<Vaddr> {
        self.check_len(len);
        self.table
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

/// A cursor that may change mappings.
pub struct CursorMut<'a> {
    inner: Inner<'a>,
    flusher: TlbFlusher<'a, DisabledPreemptGuard>,
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
        self.put(Item::Frame(frame), prop);
    }

    fn put(&mut self, item: Item, prop: PageProperty) {
        let va = self.inner.va;
        assert!(
            va < self.inner.range.end,
            "mapping outside the cursor's range"
        );
        let old = self.inner.table.insert(va, Entry { item, prop });
        assert!(old.is_none(), "mapping over an already mapped page");
        self.inner.va += PAGE_SIZE;
    }

    /// Maps `len` bytes of `io_mem` from `offset` at the cursor.
    pub fn map_iomem(&mut self, io_mem: IoMem, prop: PageProperty, len: usize, offset: usize) {
        assert!(len.is_multiple_of(PAGE_SIZE) && offset.is_multiple_of(PAGE_SIZE));
        if offset >= io_mem.size() {
            return;
        }
        let end = io_mem.size().min(offset + len);
        for paddr in (io_mem.paddr() + offset..io_mem.paddr() + end).step_by(PAGE_SIZE) {
            self.put(Item::Io(paddr), prop);
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
        let vas: alloc::vec::Vec<Vaddr> = self
            .inner
            .table
            .range(self.inner.va..end)
            .map(|(va, _)| *va)
            .collect();
        let mut frames = 0;
        for va in vas {
            if let Some(Entry {
                item: Item::Frame(_),
                ..
            }) = self.inner.table.remove(&va)
            {
                frames += 1;
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
        let e = self.inner.table.get_mut(&va).unwrap();
        op(&mut e.prop.flags, &mut e.prop.cache);
        self.inner.va = va + PAGE_SIZE;
        Some(va..va + PAGE_SIZE)
    }
}
