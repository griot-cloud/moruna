// SPDX-License-Identifier: MPL-2.0

//! The core's calls on memory that a space and its frames make.
//!
//! - A frame is a page of an [`Object`], a memory object the core holds and the personality
//!   maps in its own region.
//! - A space shown in a [`ProgramProcess`] is mirrored into that process's lower range with
//!   `process_map` of each frame's object and offset, `process_unmap` and
//!   `process_protect`.
//!
//! On the target these are Moruna's calls (`moruna-sys`). In host tests they are an
//! emulated core (`emulated`), which keeps each process's pages as the core would, so a
//! test can check what the program sees.

use core::sync::atomic::{AtomicU64, Ordering};

use moruna_abi::map;

#[cfg(not(target_os = "none"))]
use super::emulated as sys;
#[cfg(target_os = "none")]
use super::moruna as sys;
use crate::Error;

/// The core's error, as `moruna_abi` names it.
pub(crate) type CoreError = moruna_abi::Error;

/// A process whose lower range can show a space: the core's handle to it, which the
/// personality keeps open while a space is shown in it. In host tests it names an
/// emulated process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProgramProcess(u32);

impl ProgramProcess {
    /// The process a handle value names.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The handle value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A memory object holding frames, mapped in the personality's region. Dropping it unmaps
/// it there and closes it; nothing may show its pages by then.
#[derive(Debug)]
pub(crate) struct Object {
    raw: u32,
    addr: usize,
    len: usize,
    #[cfg(target_os = "none")]
    _held: super::moruna::Held,
}

impl Object {
    /// A zeroed object of `len` bytes, whole pages, mapped writable in the region.
    pub(crate) fn create(len: usize) -> crate::Result<Object> {
        let made = sys::object_create(len).map_err(to_error)?;
        OBJECTS.fetch_add(1, Ordering::Relaxed);
        Ok(Object {
            raw: made.raw,
            addr: made.addr,
            len,
            #[cfg(target_os = "none")]
            _held: made.held,
        })
    }

    /// Its handle value.
    pub(crate) fn raw(&self) -> u32 {
        self.raw
    }

    /// Where it is mapped in the personality.
    pub(crate) fn addr(&self) -> usize {
        self.addr
    }

    /// Its length.
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        OBJECTS.fetch_sub(1, Ordering::Relaxed);
        #[cfg(not(target_os = "none"))]
        sys::object_release(self.raw, self.addr, self.len);
    }
}

/// What an object comes back as from the core.
pub(crate) struct Made {
    pub(crate) raw: u32,
    pub(crate) addr: usize,
    #[cfg(target_os = "none")]
    pub(crate) held: super::moruna::Held,
}

/// The `map` bits a page shows with: readable whenever it is accessible, writable, and
/// executable. The core refuses a page both writable and executable, so such a page shows
/// writable until the program executes it (`exec`), then executable until it writes.
pub(crate) fn prot_of(r: bool, w: bool, x: bool, exec: bool) -> u64 {
    let mut prot = 0;
    if r || w || x {
        prot |= map::READ;
    }
    match (w, x) {
        (true, true) if exec => prot |= map::EXECUTE,
        (true, _) => prot |= map::WRITE,
        (false, true) => prot |= map::EXECUTE,
        (false, false) => {}
    }
    prot
}

/// Maps `len` bytes of `object` from `offset` into `process` at `va`, replacing what is
/// there.
pub(crate) fn map(
    process: ProgramProcess,
    va: usize,
    object: u32,
    offset: usize,
    len: usize,
    prot: u64,
) -> Result<(), CoreError> {
    MAPS.fetch_add(1, Ordering::Relaxed);
    MAPPED_PAGES.fetch_add((len / super::PAGE_SIZE) as u64, Ordering::Relaxed);
    sys::map(process, va, object, offset, len, prot | map::FIXED)
}

/// Unmaps `va .. va + len` of `process`; pages nothing maps are skipped.
pub(crate) fn unmap(process: ProgramProcess, va: usize, len: usize) -> Result<(), CoreError> {
    UNMAPS.fetch_add(1, Ordering::Relaxed);
    sys::unmap(process, va, len)
}

/// Sets the protection of `va .. va + len` of `process`, every page of which is mapped.
pub(crate) fn protect(
    process: ProgramProcess,
    va: usize,
    len: usize,
    prot: u64,
) -> Result<(), CoreError> {
    PROTECTS.fetch_add(1, Ordering::Relaxed);
    sys::protect(process, va, len, prot)
}

/// The personality's error for one of the core's.
pub(crate) fn to_error(e: CoreError) -> Error {
    match e {
        CoreError::NoMemory | CoreError::QuotaExceeded | CoreError::Limit => Error::NoMemory,
        CoreError::InvalidArgs | CoreError::OutOfRange => Error::InvalidArgs,
        CoreError::AccessDenied | CoreError::BadHandle | CoreError::WrongType => {
            Error::AccessDenied
        }
        _ => Error::NotEnoughResources,
    }
}

static MAPS: AtomicU64 = AtomicU64::new(0);
static MAPPED_PAGES: AtomicU64 = AtomicU64::new(0);
static UNMAPS: AtomicU64 = AtomicU64::new(0);
static PROTECTS: AtomicU64 = AtomicU64::new(0);
static OBJECTS: AtomicU64 = AtomicU64::new(0);
pub(super) static LAGGING: AtomicU64 = AtomicU64::new(0);

/// How many of the core's calls the spaces and frames have made, since the start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallCounts {
    /// `process_map` calls.
    pub maps: u64,
    /// Pages those calls mapped.
    pub mapped_pages: u64,
    /// `process_unmap` calls.
    pub unmaps: u64,
    /// `process_protect` calls.
    pub protects: u64,
    /// Objects held for frames now.
    pub objects: u64,
    /// Pages a call the core refused left lagging the record, served on their next fault.
    pub lagging: u64,
}

impl CallCounts {
    /// The calls made between `earlier` and `self`.
    pub fn since(&self, earlier: &CallCounts) -> CallCounts {
        CallCounts {
            maps: self.maps - earlier.maps,
            mapped_pages: self.mapped_pages - earlier.mapped_pages,
            unmaps: self.unmaps - earlier.unmaps,
            protects: self.protects - earlier.protects,
            objects: self.objects.wrapping_sub(earlier.objects),
            lagging: self.lagging - earlier.lagging,
        }
    }

    /// Every call made.
    pub fn calls(&self) -> u64 {
        self.maps + self.unmaps + self.protects
    }
}

/// The calls made so far.
pub fn call_counts() -> CallCounts {
    CallCounts {
        maps: MAPS.load(Ordering::Relaxed),
        mapped_pages: MAPPED_PAGES.load(Ordering::Relaxed),
        unmaps: UNMAPS.load(Ordering::Relaxed),
        protects: PROTECTS.load(Ordering::Relaxed),
        objects: OBJECTS.load(Ordering::Relaxed),
        lagging: LAGGING.load(Ordering::Relaxed),
    }
}
