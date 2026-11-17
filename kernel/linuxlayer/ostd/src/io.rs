// SPDX-License-Identifier: MPL-2.0

//! Device memory and ports: a personality drives no device, so none exists.

use core::marker::PhantomData;

use crate::mm::{HasPaddr, HasSize, Paddr};

enum Never {}

/// Device memory. No value of it exists in user space.
pub struct IoMem {
    never: Never,
}

impl IoMem {
    /// Acquires device memory: there is none to acquire in user space.
    pub fn acquire(_range: core::ops::Range<Paddr>) -> crate::Result<Self> {
        Err(crate::Error::AccessDenied)
    }

    /// Its end.
    pub fn end_paddr(&self) -> Paddr {
        match self.never {}
    }

    /// Its cache policy.
    pub fn cache_policy(&self) -> crate::mm::CachePolicy {
        match self.never {}
    }
}

impl Clone for IoMem {
    fn clone(&self) -> Self {
        match self.never {}
    }
}

impl core::fmt::Debug for IoMem {
    fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.never {}
    }
}

impl HasPaddr for IoMem {
    fn paddr(&self) -> Paddr {
        match self.never {}
    }
}

impl HasSize for IoMem {
    fn size(&self) -> usize {
        match self.never {}
    }
}

/// An I/O port. No value of it exists in user space.
pub struct IoPort<T, A> {
    never: Never,
    _p: PhantomData<(T, A)>,
}

impl<T, A> IoPort<T, A> {
    /// Writes the port.
    pub fn write(&self, _value: T) {
        match self.never {}
    }
}
