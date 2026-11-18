//! `mmap`, `munmap` and `mprotect`: anonymous memory, each mapping a memory object of its own,
//! charged to the process's account. Files are not mapped (a function has none), nor is a
//! mapping placed at an address it asks for.

use crate::errno::{EINVAL, ENODEV};
use crate::os::{self, Access};
use crate::{Errno, Result, from_kernel};

/// The page size: what every mapping is a whole number of.
pub const PAGE: usize = 4096;

pub const PROT_READ: i32 = 1;
pub const PROT_WRITE: i32 = 2;
pub const PROT_EXEC: i32 = 4;
pub const MAP_SHARED: i32 = 0x01;
pub const MAP_PRIVATE: i32 = 0x02;
pub const MAP_FIXED: i32 = 0x10;
pub const MAP_ANONYMOUS: i32 = 0x20;

fn access(prot: i32) -> Result<Access> {
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return Err(Errno(EINVAL));
    }
    Ok(Access {
        read: prot != 0,
        write: prot & PROT_WRITE != 0,
        execute: prot & PROT_EXEC != 0,
    })
}

/// `len` rounded up to whole pages; `EINVAL` for none, or past what an address holds.
fn pages(len: usize) -> Result<usize> {
    if len == 0 {
        return Err(Errno(EINVAL));
    }
    len.checked_next_multiple_of(PAGE).ok_or(Errno(EINVAL))
}

/// `mmap`: new anonymous memory of `len` bytes (whole pages), zeroed, its address. Shared and
/// private are the same within one process, which is all there is. `MAP_FIXED` and a file
/// (no `MAP_ANONYMOUS`) are refused; `addr` is a hint, not taken, and `fd` is not read, as
/// Linux reads neither for anonymous memory.
pub fn map(len: usize, prot: i32, flags: i32, offset: i64) -> Result<usize> {
    let sharing = flags & (MAP_SHARED | MAP_PRIVATE);
    if sharing == 0 || sharing == (MAP_SHARED | MAP_PRIVATE) || flags & MAP_FIXED != 0 {
        return Err(Errno(EINVAL));
    }
    if flags & MAP_ANONYMOUS == 0 {
        return Err(Errno(ENODEV));
    }
    if offset % PAGE as i64 != 0 {
        return Err(Errno(EINVAL));
    }
    let access = access(prot)?;
    os::map(pages(len)?, access).map_err(from_kernel)
}

/// `munmap`: whole pages of mappings `map` made.
///
/// # Safety
///
/// Nothing in the process uses the range again.
pub unsafe fn unmap(addr: usize, len: usize) -> Result<()> {
    if !addr.is_multiple_of(PAGE) {
        return Err(Errno(EINVAL));
    }
    // SAFETY: the caller's contract.
    unsafe { os::unmap(addr, pages(len)?) }.map_err(from_kernel)
}

/// `mprotect`: the access of whole pages of mappings `map` made.
///
/// # Safety
///
/// Nothing in the process relies on an access the range loses.
pub unsafe fn protect(addr: usize, len: usize, prot: i32) -> Result<()> {
    if !addr.is_multiple_of(PAGE) {
        return Err(Errno(EINVAL));
    }
    let access = access(prot)?;
    // SAFETY: the caller's contract.
    unsafe { os::protect(addr, pages(len)?, access) }.map_err(from_kernel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errno::ENOMEM;

    const ANON: i32 = MAP_PRIVATE | MAP_ANONYMOUS;

    #[test]
    fn anonymous_memory_is_whole_pages_zeroed_and_given_back() {
        let a = map(10, PROT_READ | PROT_WRITE, ANON, 0).unwrap();
        assert!(a.is_multiple_of(PAGE));
        // SAFETY: the page was mapped just now, writable.
        let bytes = unsafe { core::slice::from_raw_parts_mut(a as *mut u8, PAGE) };
        assert!(bytes.iter().all(|b| *b == 0));
        bytes[PAGE - 1] = 7;
        assert_eq!(
            os::access_at(a),
            Some(Access {
                read: true,
                write: true,
                execute: false
            })
        );
        // SAFETY: nothing uses the page after.
        unsafe {
            assert_eq!(protect(a, 10, PROT_READ), Ok(()));
            assert_eq!(os::access_at(a).map(|x| x.write), Some(false));
            assert_eq!(protect(a, PAGE, 0), Ok(()));
            assert_eq!(os::access_at(a).map(|x| x.read), Some(false));
            assert_eq!(unmap(a, 1), Ok(()), "rounded up to the page");
        }
        assert_eq!(os::access_at(a), None);
        let shared = map(PAGE, PROT_READ, MAP_SHARED | MAP_ANONYMOUS, 4096).unwrap();
        // SAFETY: as above.
        unsafe { unmap(shared, PAGE).unwrap() };
    }

    #[test]
    fn what_cannot_be_mapped_is_refused() {
        let e = |r: Result<usize>| r.unwrap_err().0;
        assert_eq!(e(map(0, PROT_READ, ANON, 0)), EINVAL);
        assert_eq!(e(map(usize::MAX, PROT_READ, ANON, 0)), EINVAL);
        assert_eq!(e(map(PAGE, PROT_READ, MAP_ANONYMOUS, 0)), EINVAL);
        assert_eq!(
            e(map(PAGE, PROT_READ, ANON | MAP_SHARED, 0)),
            EINVAL,
            "both"
        );
        assert_eq!(e(map(PAGE, PROT_READ, ANON | MAP_FIXED, 0)), EINVAL);
        assert_eq!(e(map(PAGE, PROT_READ, MAP_PRIVATE, 0)), ENODEV);
        assert_eq!(e(map(PAGE, PROT_READ, ANON, 1)), EINVAL);
        assert_eq!(e(map(PAGE, 8, ANON, 0)), EINVAL);
        assert_eq!(e(map(os::MAP_MAX + PAGE, PROT_READ, ANON, 0)), ENOMEM);
        // SAFETY: the ranges are refused before anything is touched.
        unsafe {
            assert_eq!(unmap(3, PAGE), Err(Errno(EINVAL)));
            assert_eq!(unmap(PAGE, 0), Err(Errno(EINVAL)));
            assert_eq!(protect(3, PAGE, 0), Err(Errno(EINVAL)));
            assert_eq!(protect(PAGE, PAGE, 9), Err(Errno(EINVAL)));
            assert_eq!(unmap(PAGE, PAGE), Err(Errno(EINVAL)), "not a mapping");
            assert_eq!(protect(PAGE, PAGE, 0), Err(Errno(EINVAL)), "not a mapping");
        }
    }
}
