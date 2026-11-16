// SPDX-License-Identifier: MPL-2.0

//! Copies between the personality's memory and the program's.
//!
//! The swap of OSTD's `memcpy` and `memset`: an infallible side is the personality's own
//! memory, copied directly; a fallible side is the program's, reached through the
//! fault-tolerant copy (`backend`), so a bad pointer from a program is a short copy, never
//! a crash of the personality. There is no device memory (`Io`) in user space.

use super::{Fallible, Infallible};
use crate::{
    backend::{self, Dir},
    mm::fault,
};

/// Copies `len` bytes through the fault-tolerant copy; returns how many were copied. A copy
/// that stops at a page the personality has not mapped yet asks the layer's fault handler to
/// map it and goes on from there, as OSTD's fault path does; it stops for good where the
/// handler cannot, or where it makes no progress.
///
/// # Safety
///
/// As for [`memcpy`].
unsafe fn copy_resolving(dst: *mut u8, src: *const u8, len: usize, dir: Dir) -> usize {
    let mut done = 0;
    let mut last_fault = None;
    while done < len {
        // SAFETY: the caller upholds it for the whole range, so for its tail.
        let left = unsafe { backend::copy_fallible(dst.add(done), src.add(done), len - done, dir) };
        done = len - left;
        if left == 0 {
            break;
        }
        // The fault is at the first byte not copied, on the program's side.
        let at = done;
        if last_fault == Some(at) {
            break;
        }
        last_fault = Some(at);
        let resolved = match dir {
            Dir::ToProgram => fault::resolve(dst as usize + at, true),
            Dir::FromProgram => fault::resolve(src as usize + at, false),
            Dir::Both => {
                fault::resolve(src as usize + at, false) | fault::resolve(dst as usize + at, true)
            }
        };
        if !resolved {
            break;
        }
    }
    done
}

/// Copies `len` bytes from `src` to `dst`.
///
/// # Safety
///
/// Each side is valid for `len` bytes, or in the program's memory when fallible.
pub(crate) unsafe fn memcpy<Dst: Memcpy<Src>, Src>(
    dst: *mut u8,
    src: *const u8,
    len: usize,
) -> Dst::Result {
    // SAFETY: the caller upholds it.
    unsafe { Dst::memcpy(dst, src, len) }
}

/// Fills `len` bytes at `dst` with `value`.
///
/// # Safety
///
/// As for `memcpy`.
pub(crate) unsafe fn memset<Dst: Memset>(dst: *mut u8, value: u8, len: usize) -> Dst::Result {
    // SAFETY: the caller upholds it.
    unsafe { Dst::memset(dst, value, len) }
}

/// A copy into `Self` from `Src`.
///
/// # Safety
///
/// `memcpy` copies at most `len` bytes and reports a short copy only for a fault.
pub(crate) unsafe trait Memcpy<Src> {
    /// `()` when infallible, the bytes copied otherwise.
    type Result;

    /// # Safety
    ///
    /// As for [`memcpy`].
    unsafe fn memcpy(dst: *mut u8, src: *const u8, len: usize) -> Self::Result;
}

/// A fill of `Self`.
///
/// # Safety
///
/// As for [`Memcpy`].
pub(crate) unsafe trait Memset {
    /// `()` when infallible, the bytes set otherwise.
    type Result;

    /// # Safety
    ///
    /// As for [`memset`].
    unsafe fn memset(dst: *mut u8, value: u8, len: usize) -> Self::Result;
}

// SAFETY: both sides are the personality's own memory.
unsafe impl Memcpy<Infallible> for Infallible {
    type Result = ();

    unsafe fn memcpy(dst: *mut u8, src: *const u8, len: usize) {
        // SAFETY: the caller keeps both ranges valid; they may overlap with a racing
        // program, so the copy is volatile, as OSTD's is.
        unsafe { core::intrinsics::volatile_copy_memory(dst, src, len) };
    }
}

// SAFETY: the fallible source goes through the fault-tolerant copy.
unsafe impl Memcpy<Fallible> for Infallible {
    type Result = usize;

    unsafe fn memcpy(dst: *mut u8, src: *const u8, len: usize) -> usize {
        // SAFETY: as above.
        unsafe { copy_resolving(dst, src, len, Dir::FromProgram) }
    }
}

// SAFETY: the fallible destination goes through the fault-tolerant copy.
unsafe impl Memcpy<Infallible> for Fallible {
    type Result = usize;

    unsafe fn memcpy(dst: *mut u8, src: *const u8, len: usize) -> usize {
        // SAFETY: as above.
        unsafe { copy_resolving(dst, src, len, Dir::ToProgram) }
    }
}

// SAFETY: both sides go through the fault-tolerant copy.
unsafe impl Memcpy<Fallible> for Fallible {
    type Result = usize;

    unsafe fn memcpy(dst: *mut u8, src: *const u8, len: usize) -> usize {
        // SAFETY: as above.
        unsafe { copy_resolving(dst, src, len, Dir::Both) }
    }
}

// SAFETY: the personality's own memory.
unsafe impl Memset for Infallible {
    type Result = ();

    unsafe fn memset(dst: *mut u8, value: u8, len: usize) {
        // SAFETY: the caller keeps the range valid.
        unsafe { core::intrinsics::volatile_set_memory(dst, value, len) };
    }
}

// SAFETY: the program's memory, through the fault-tolerant fill.
unsafe impl Memset for Fallible {
    type Result = usize;

    unsafe fn memset(dst: *mut u8, value: u8, len: usize) -> usize {
        let mut done = 0;
        let mut last_fault = None;
        while done < len {
            // SAFETY: as above, for the tail.
            done = len - unsafe { backend::set_fallible(dst.add(done), value, len - done) };
            if done == len || last_fault == Some(done) {
                break;
            }
            last_fault = Some(done);
            if !fault::resolve(dst as usize + done, true) {
                break;
            }
        }
        done
    }
}

/// Loads a `u32` of the program's memory; `!0` on a fault, as OSTD's does.
///
/// # Safety
///
/// `ptr` is aligned.
pub(crate) unsafe fn __atomic_load_fallible(ptr: *const u32) -> u64 {
    // SAFETY: the caller upholds it.
    let first = unsafe { backend::load_u32_fallible(ptr) };
    let value = match first {
        None if fault::resolve(ptr as usize, false) => {
            // SAFETY: as above.
            unsafe { backend::load_u32_fallible(ptr) }
        }
        other => other,
    };
    value.map_or(!0, u64::from)
}

/// Compares and exchanges a `u32` of the program's memory; returns the old value, `!0`
/// on a fault.
///
/// # Safety
///
/// `ptr` is aligned.
pub(crate) unsafe fn __atomic_cmpxchg_fallible(ptr: *mut u32, old: u32, new: u32) -> u64 {
    // SAFETY: the caller upholds it.
    let first = unsafe { backend::cmpxchg_u32_fallible(ptr, old, new) };
    let value = match first {
        None if fault::resolve(ptr as usize, true) => {
            // SAFETY: as above.
            unsafe { backend::cmpxchg_u32_fallible(ptr, old, new) }
        }
        other => other,
    };
    value.map_or(!0, u64::from)
}
