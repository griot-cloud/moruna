// SPDX-License-Identifier: MPL-2.0

//! The personality's memory shared by every process of its task (`architecture/data-os.md`
//! §4, the integrator's fork decision of 2026-11-16).
//!
//! One Linux process is one Moruna process, and the task's personality is one instance
//! whose state every process reaches at the same addresses. So memory the personality
//! hands out (its heap's chunks, B2-vm's frames' chunks) comes from objects mapped at an
//! address the personality picks in its region, in every process of the task, before any
//! of it is used:
//!
//! - the task's processes are a fixed table ([`Processes`]) under a lock of its own, which
//!   nothing allocates under: the heap's source reads it with the heap's lock held;
//! - a new chunk is mapped in every process before it is handed out ([`map_everywhere`]):
//!   the frames' chunks by [`TaskMemory`], the heap's by a bump allocator ([`Bump`]);
//! - a process joins with every chunk already mapped (a `process_clone` keeps object
//!   mappings), and the creator holds both locks across the clone, so a growth and a fork
//!   cannot interleave;
//! - with the task's processes sharing one handle table (`process::SHARE_HANDLES`), every
//!   value here means the same in each, so these are local calls.
//!
//! The core's calls are a [`Core`], so the bookkeeping runs in host tests.

#![cfg_attr(
    all(not(target_os = "none"), not(all(test, ktest))),
    expect(
        dead_code,
        reason = "the target's binding uses it, and the host's tests"
    )
)]

use alloc::collections::BTreeMap;

use crate::{Error, Result, mm::PAGE_SIZE};

/// The core's calls the task's memory needs.
pub(crate) trait Core {
    /// A process of the task, by its handle.
    type Process: Copy + Eq;
    /// A memory object.
    type Object;
    /// A new object of `len` bytes.
    fn create(&mut self, len: usize) -> Result<Self::Object>;
    /// Maps all of `object` at `addr` in `process`, readable and writable.
    fn map(
        &mut self,
        process: Self::Process,
        object: &Self::Object,
        addr: usize,
        len: usize,
    ) -> Result<()>;
    /// Unmaps `len` bytes at `addr` in `process`.
    fn unmap(&mut self, process: Self::Process, addr: usize, len: usize);
}

/// The task's processes: at most `N`, in a table that never allocates.
pub(crate) struct Processes<P: Copy + Eq, const N: usize> {
    list: [P; N],
    len: usize,
}

impl<P: Copy + Eq, const N: usize> Processes<P, N> {
    /// No process yet; `fill` stands for none.
    pub(crate) const fn new(fill: P) -> Self {
        Self {
            list: [fill; N],
            len: 0,
        }
    }

    /// Adds `process`; `NotEnoughResources` when the table is full.
    pub(crate) fn add(&mut self, process: P) -> Result<()> {
        if self.as_slice().contains(&process) {
            return Ok(());
        }
        if self.len == N {
            return Err(Error::NotEnoughResources);
        }
        self.list[self.len] = process;
        self.len += 1;
        Ok(())
    }

    /// Takes `process` out: it has ended.
    pub(crate) fn remove(&mut self, process: P) {
        if let Some(i) = self.as_slice().iter().position(|&p| p == process) {
            self.list.copy_within(i + 1..self.len, i);
            self.len -= 1;
        }
    }

    /// The processes.
    pub(crate) fn as_slice(&self) -> &[P] {
        &self.list[..self.len]
    }
}

/// Maps `object` at `addr` in every one of `processes`, or in none.
pub(crate) fn map_everywhere<C: Core>(
    core: &mut C,
    processes: &[C::Process],
    object: &C::Object,
    addr: usize,
    len: usize,
) -> Result<()> {
    for (i, &process) in processes.iter().enumerate() {
        if let Err(e) = core.map(process, object, addr, len) {
            for &done in &processes[..i] {
                core.unmap(done, addr, len);
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Addresses handed out upwards and never reused, for blocks whose addresses the heap
/// keeps track of itself; it never allocates.
pub(crate) struct Bump {
    next: usize,
    end: usize,
}

impl Bump {
    pub(crate) const fn new(range: core::ops::Range<usize>) -> Self {
        Self {
            next: range.start,
            end: range.end,
        }
    }

    /// `len` bytes, whole pages.
    pub(crate) fn take(&mut self, len: usize) -> Option<usize> {
        let len = len.max(1).next_multiple_of(PAGE_SIZE);
        let at = self.next;
        if self.end - at < len {
            return None;
        }
        self.next = at + len;
        Some(at)
    }
}

/// The task's chunks for frames: the free part of their range, and the live chunks.
pub(crate) struct TaskMemory<C: Core> {
    core: C,
    range: core::ops::Range<usize>,
    /// Free ranges: start to length, never adjacent.
    free: BTreeMap<usize, usize>,
    /// Live chunks: start to their object and length.
    chunks: BTreeMap<usize, (C::Object, usize)>,
}

impl<C: Core> TaskMemory<C> {
    /// The chunks of a task, handed out from `range` (page aligned).
    pub(crate) fn new(core: C, range: core::ops::Range<usize>) -> Self {
        debug_assert!(range.start.is_multiple_of(PAGE_SIZE) && range.end.is_multiple_of(PAGE_SIZE));
        let mut free = BTreeMap::new();
        free.insert(range.start, range.end - range.start);
        Self {
            core,
            range,
            free,
            chunks: BTreeMap::new(),
        }
    }

    /// A new chunk of at least `len` bytes, mapped in every one of `processes`; its address.
    pub(crate) fn map(&mut self, len: usize, processes: &[C::Process]) -> Result<usize> {
        let len = len.max(1).next_multiple_of(PAGE_SIZE);
        let addr = self.reserve(len).ok_or(Error::NoMemory)?;
        let mapped = self.core.create(len).and_then(|object| {
            map_everywhere(&mut self.core, processes, &object, addr, len).map(|()| object)
        });
        match mapped {
            Ok(object) => {
                self.chunks.insert(addr, (object, len));
                Ok(addr)
            }
            Err(e) => {
                self.release(addr, len);
                Err(e)
            }
        }
    }

    /// Unmaps the chunk at `addr` from every one of `processes`; its object goes, and its
    /// addresses are free again.
    pub(crate) fn unmap(&mut self, addr: usize, processes: &[C::Process]) -> Result<()> {
        let (object, len) = self.chunks.remove(&addr).ok_or(Error::InvalidArgs)?;
        for &process in processes {
            self.core.unmap(process, addr, len);
        }
        drop(object);
        self.release(addr, len);
        Ok(())
    }

    /// The chunk holding `addr`: its start, its object and its length.
    pub(crate) fn chunk(&self, addr: usize) -> Option<(usize, &C::Object, usize)> {
        let (&start, (object, len)) = self.chunks.range(..=addr).next_back()?;
        (addr < start + len).then_some((start, object, *len))
    }

    /// The core's calls, for a test.
    #[cfg(all(test, ktest))]
    pub(crate) fn core(&mut self) -> &mut C {
        &mut self.core
    }

    /// The first free range that holds `len` bytes, taken from its start.
    fn reserve(&mut self, len: usize) -> Option<usize> {
        let (&start, &free) = self.free.iter().find(|&(_, &free)| free >= len)?;
        self.free.remove(&start);
        if free > len {
            self.free.insert(start + len, free - len);
        }
        Some(start)
    }

    /// Gives `len` bytes at `addr` back, merged with free neighbours.
    fn release(&mut self, addr: usize, len: usize) {
        debug_assert!(self.range.start <= addr && addr + len <= self.range.end);
        let (mut start, mut len) = (addr, len);
        if let Some((&before, &before_len)) = self.free.range(..start).next_back()
            && before + before_len == start
        {
            self.free.remove(&before);
            start = before;
            len += before_len;
        }
        if let Some(&after_len) = self.free.get(&(start + len)) {
            self.free.remove(&(start + len));
            len += after_len;
        }
        self.free.insert(start, len);
    }
}
