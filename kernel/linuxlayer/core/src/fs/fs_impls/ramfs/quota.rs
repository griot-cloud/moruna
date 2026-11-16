// SPDX-License-Identifier: MPL-2.0

//! A task's scratch quota: how many bytes a tmpfs may hold.
//!
//! The scratch is the task's own memory, so its account pays for it anyway; the
//! quota bounds the part of it files hold, as the image filesystem's scratch
//! directory did (SDD 27 f.7). An entry costs [`ENTRY_COST`] and, for a link, its
//! target; a file costs its bytes rounded up to pages. A change that would pass the
//! quota changes nothing and is `ENOSPC`. A file removed while open keeps its charge
//! until its last close.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::prelude::*;

/// What one entry costs, whatever it is.
pub(crate) const ENTRY_COST: usize = 256;

/// The bytes a tmpfs may hold and the bytes it holds.
#[derive(Debug)]
pub(crate) struct Quota {
    limit: usize,
    used: AtomicUsize,
}

impl Quota {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    /// Takes `bytes`, or refuses them whole.
    pub(crate) fn charge(&self, bytes: usize) -> Result<()> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| Error::with_message(Errno::ENOSPC, "the scratch quota is full"))
    }

    /// Gives `bytes` back.
    pub(crate) fn release(&self, bytes: usize) {
        let _ = self
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(bytes))
            });
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}
