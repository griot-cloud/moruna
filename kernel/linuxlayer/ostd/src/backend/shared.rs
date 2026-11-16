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
//! - one lock covers the address ranges, the chunks and the task's processes;
//! - a new chunk is mapped in every process before [`TaskMemory::map`] returns;
//! - a process joins with every chunk already mapped (a `process_clone` keeps object
//!   mappings), and the creator holds the lock across the clone ([`TaskMemory::locked`]) so
//!   a growth and a fork cannot interleave;
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

use alloc::{collections::BTreeMap, vec::Vec};

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

/// The task's shared memory: the free part of its range, its chunks and its processes.
pub(crate) struct TaskMemory<C: Core> {
    core: C,
    range: core::ops::Range<usize>,
    /// Free ranges: start to length, never adjacent.
    free: BTreeMap<usize, usize>,
    /// Live chunks: start to their object and length.
    chunks: BTreeMap<usize, (C::Object, usize)>,
    processes: Vec<C::Process>,
}

impl<C: Core> TaskMemory<C> {
    /// The memory of a task whose first process is `first`, handed out from `range`
    /// (page aligned).
    pub(crate) fn new(core: C, range: core::ops::Range<usize>, first: C::Process) -> Self {
        debug_assert!(range.start.is_multiple_of(PAGE_SIZE) && range.end.is_multiple_of(PAGE_SIZE));
        let mut free = BTreeMap::new();
        free.insert(range.start, range.end - range.start);
        Self {
            core,
            range,
            free,
            chunks: BTreeMap::new(),
            processes: alloc::vec![first],
        }
    }

    /// A new chunk of at least `len` bytes, mapped in every process of the task; its
    /// address.
    pub(crate) fn map(&mut self, len: usize) -> Result<usize> {
        let len = len.max(1).next_multiple_of(PAGE_SIZE);
        let addr = self.reserve(len).ok_or(Error::NoMemory)?;
        let object = match self.core.create(len) {
            Ok(object) => object,
            Err(e) => {
                self.release(addr, len);
                return Err(e);
            }
        };
        for (i, &process) in self.processes.iter().enumerate() {
            if let Err(e) = self.core.map(process, &object, addr, len) {
                for &done in &self.processes[..i] {
                    self.core.unmap(done, addr, len);
                }
                self.release(addr, len);
                return Err(e);
            }
        }
        self.chunks.insert(addr, (object, len));
        Ok(addr)
    }

    /// Unmaps the chunk at `addr` from every process; its object goes, and its addresses
    /// are free again.
    pub(crate) fn unmap(&mut self, addr: usize) -> Result<()> {
        let (object, len) = self.chunks.remove(&addr).ok_or(Error::InvalidArgs)?;
        for &process in &self.processes {
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

    /// Adds `process`, which already maps every chunk at its address (a clone of a
    /// process of the task, made under [`TaskMemory::locked`]).
    pub(crate) fn join_cloned(&mut self, process: C::Process) {
        if !self.processes.contains(&process) {
            self.processes.push(process);
        }
    }

    /// Adds `process`, made empty, mapping every chunk into it first.
    pub(crate) fn join_empty(&mut self, process: C::Process) -> Result<()> {
        let mut done = Vec::new();
        for (&addr, (object, len)) in &self.chunks {
            if let Err(e) = self.core.map(process, object, addr, *len) {
                for (addr, len) in done {
                    self.core.unmap(process, addr, len);
                }
                return Err(e);
            }
            done.push((addr, *len));
        }
        self.join_cloned(process);
        Ok(())
    }

    /// Takes `process` out: it has ended, or is about to.
    pub(crate) fn leave(&mut self, process: C::Process) {
        self.processes.retain(|&p| p != process);
    }

    /// Runs `f` with the memory held still, as a `process_clone` of a task's process must
    /// run: no chunk comes or goes until it returns.
    pub(crate) fn locked<R>(&mut self, f: impl FnOnce(&mut C) -> R) -> R {
        f(&mut self.core)
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
