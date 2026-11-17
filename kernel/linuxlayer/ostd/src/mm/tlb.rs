// SPDX-License-Identifier: MPL-2.0

//! TLB flushes, in user space.
//!
//! The core flushes every CPU's TLB before its `process_unmap` or `process_protect`
//! returns, so a flusher here only keeps OSTD's API: what it is asked to drop after a
//! flush is dropped when it is dispatched.

use alloc::vec::Vec;
use core::{marker::PhantomData, ops::Range};

use super::{Frame, PAGE_SIZE, Vaddr, frame::AnyFrameMeta};
use crate::{cpu::PinCurrentCpu, sync::RcuDrop};

/// A TLB flush of some virtual addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlbFlushOp(Range<Vaddr>);

impl TlbFlushOp {
    /// Every address.
    pub fn for_all() -> Self {
        Self(0..usize::MAX)
    }

    /// One page.
    pub fn for_single(va: Vaddr) -> Self {
        Self(va..va + PAGE_SIZE)
    }

    /// A range.
    pub fn for_range(range: Range<Vaddr>) -> Self {
        Self(range)
    }

    /// The range.
    pub fn range(&self) -> &Range<Vaddr> {
        &self.0
    }

    /// Flushes on the current CPU: the core's to do.
    pub fn perform_on_current(&self) {}
}

/// Gathers TLB flushes and what may be dropped after them.
pub struct TlbFlusher<'a, G: PinCurrentCpu> {
    pending: Vec<TlbFlushOp>,
    deferred: Vec<RcuDrop<Frame<dyn AnyFrameMeta>>>,
    _guard: G,
    _lifetime: PhantomData<&'a ()>,
}

impl<G: PinCurrentCpu> TlbFlusher<'_, G> {
    /// A flusher holding `pin_current_guard`.
    pub fn new(pin_current_guard: G) -> Self {
        Self {
            pending: Vec::new(),
            deferred: Vec::new(),
            _guard: pin_current_guard,
            _lifetime: PhantomData,
        }
    }

    /// Records a flush.
    pub fn issue_tlb_flush(&mut self, op: TlbFlushOp) {
        self.pending.push(op);
    }

    /// Records a flush, and a frame to drop after it.
    pub fn issue_tlb_flush_with(
        &mut self,
        op: TlbFlushOp,
        drop_after_flush: RcuDrop<Frame<dyn AnyFrameMeta>>,
    ) {
        self.pending.push(op);
        self.deferred.push(drop_after_flush);
    }

    /// Sends the flushes: done already by the core, so the frames go.
    pub fn dispatch_tlb_flush(&mut self) {
        self.pending.clear();
        self.deferred.clear();
    }

    /// Waits for the flushes sent: there is nothing to wait for.
    pub fn sync_tlb_flush(&mut self) {
        self.dispatch_tlb_flush();
    }
}
