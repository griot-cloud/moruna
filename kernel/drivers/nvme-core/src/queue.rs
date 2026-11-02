//! A queue pair (NVMe Base 2.0, 3.3): a submission queue the driver writes commands
//! into and a completion queue the controller writes their completions into, each one
//! page of pinned memory.
//!
//! The queue keeps, in the driver's own memory, which command identifiers are in
//! flight and how far the controller has read the submission queue. A completion is
//! believed only when its phase tag says it is new, its submission queue is this
//! one's, its command identifier names a command in flight, and the submission queue
//! head it reports lies between the last one and the tail. Anything else is
//! [`NvmeError::Malformed`], with the value.

use alloc::{format, vec, vec::Vec};
use core::sync::atomic::{Ordering, fence};

use moruna_mmio::Region;

use crate::{CQE, Command, Completion, NvmeError, PAGE, SQE};

pub struct QueuePair<M: Region> {
    pub id: u16,
    size: u16,
    sq: M,
    cq: M,
    sq_tail: u16,
    /// The submission queue head the controller last reported.
    sq_head: u16,
    cq_head: u16,
    /// The phase tag a new completion carries.
    phase: bool,
    /// Free command identifiers, as a stack.
    free: Vec<u16>,
    in_flight: Vec<bool>,
}

impl<M: Region> QueuePair<M> {
    /// Queue pair `id` of `size` entries over `sq` and `cq` (a page each, which it
    /// clears).
    pub fn new(id: u16, size: u16, sq: M, cq: M) -> Result<QueuePair<M>, NvmeError> {
        if size < 2 || size as usize * SQE > sq.len().min(PAGE) || size as usize * CQE > cq.len() {
            return Err(NvmeError::Unsupported(format!(
                "a queue of {size} entries in {} and {} bytes",
                sq.len(),
                cq.len()
            )));
        }
        let mut q = QueuePair {
            id,
            size,
            sq,
            cq,
            sq_tail: 0,
            sq_head: 0,
            cq_head: 0,
            phase: true,
            free: Vec::new(),
            in_flight: vec![false; size as usize],
        };
        q.reset()?;
        Ok(q)
    }

    /// Clears both rings and forgets every command: for a controller that has been
    /// reset, which holds none of them.
    pub fn reset(&mut self) -> Result<(), NvmeError> {
        for at in (0..self.size as usize * SQE).step_by(8) {
            self.sq.write64(at, 0)?;
        }
        for at in (0..self.size as usize * CQE).step_by(8) {
            self.cq.write64(at, 0)?;
        }
        self.sq_tail = 0;
        self.sq_head = 0;
        self.cq_head = 0;
        self.phase = true;
        // A full queue holds `size - 1` commands; identifiers are below `size`.
        self.free = (0..self.size - 1).rev().collect();
        self.in_flight.iter_mut().for_each(|f| *f = false);
        Ok(())
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    /// Commands in flight.
    pub fn in_flight(&self) -> usize {
        self.size as usize - 1 - self.free.len()
    }

    /// Whether another command can be submitted.
    pub fn has_room(&self) -> bool {
        !self.free.is_empty()
    }

    /// The identifier the next [`QueuePair::submit`] gives its command.
    pub fn next_cid(&self) -> Option<u16> {
        self.free.last().copied()
    }

    /// Writes `command` at the tail and returns its identifier and the new tail, which
    /// the caller writes to the submission doorbell.
    pub fn submit(&mut self, command: &Command) -> Result<(u16, u16), NvmeError> {
        let cid = self.free.pop().ok_or(NvmeError::Full)?;
        if let Err(e) = command.write(&self.sq, self.sq_tail as usize * SQE, cid) {
            self.free.push(cid);
            return Err(e.into());
        }
        self.in_flight[cid as usize] = true;
        self.sq_tail = (self.sq_tail + 1) % self.size;
        // The entry is visible before the doorbell that publishes it.
        fence(Ordering::SeqCst);
        Ok((cid, self.sq_tail))
    }

    /// Distance from `from` to `to` going forward around the ring.
    fn ahead(&self, from: u16, to: u16) -> u16 {
        (to + self.size - from) % self.size
    }

    /// The next completion, if the controller has written one; the caller writes
    /// [`QueuePair::cq_head`] to the completion doorbell once it has taken them.
    pub fn pop(&mut self) -> Result<Option<Completion>, NvmeError> {
        let at = self.cq_head as usize * CQE;
        if Completion::phase(&self.cq, at)? != self.phase {
            return Ok(None);
        }
        // The rest of the entry is read after its phase tag.
        fence(Ordering::SeqCst);
        let c = Completion::read(&self.cq, at)?;
        if c.sq_id != self.id {
            return Err(NvmeError::Malformed(format!(
                "a completion on queue {} naming submission queue {}",
                self.id, c.sq_id
            )));
        }
        let known = self.in_flight.get(c.cid as usize).copied().unwrap_or(false);
        if !known {
            return Err(NvmeError::Malformed(format!(
                "a completion on queue {} for command {} which is not in flight",
                self.id, c.cid
            )));
        }
        if c.sq_head >= self.size
            || self.ahead(self.sq_head, c.sq_head) > self.ahead(self.sq_head, self.sq_tail)
        {
            return Err(NvmeError::Malformed(format!(
                "a submission head of {} on queue {} (head {}, tail {}, {} entries)",
                c.sq_head, self.id, self.sq_head, self.sq_tail, self.size
            )));
        }
        self.sq_head = c.sq_head;
        self.in_flight[c.cid as usize] = false;
        self.free.push(c.cid);
        self.cq_head += 1;
        if self.cq_head == self.size {
            self.cq_head = 0;
            self.phase = !self.phase;
        }
        Ok(Some(c))
    }

    pub fn cq_head(&self) -> u16 {
        self.cq_head
    }
}
