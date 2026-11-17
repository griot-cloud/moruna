//! A mapping of a file (SDD 28, f.3): which of its pages come from the file and which
//! are zero, and whether the process gets the image filesystem's own read-only pages,
//! shared by every task that maps them, or a copy of its own. Only the range asked for
//! is mapped (SDD 27 f.4), never the whole file.

use crate::types::errno;

pub const PAGE: u64 = 4096;
pub const PROT_WRITE: u64 = 2;
pub const MAP_SHARED: u64 = 1;
pub const MAP_PRIVATE: u64 = 2;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilePlan {
    /// The whole mapping, in bytes (whole pages).
    pub len: u64,
    /// The pages from the file, from `offset`; the rest are zero.
    pub file_len: u64,
    /// A copy: a private mapping the process may write, or a file the image
    /// filesystem cannot map (a scratch file).
    pub copy: bool,
}

/// Plans `mmap(addr, len, prot, flags, fd, offset)` of a file of `size` bytes;
/// `mappable` when the image filesystem can hand over the file's pages. An errno for a
/// mapping Linux refuses or this personality cannot make.
pub fn plan(
    len: u64,
    prot: u64,
    flags: u64,
    offset: u64,
    size: u64,
    mappable: bool,
) -> Result<FilePlan, u64> {
    if len == 0 || !offset.is_multiple_of(PAGE) || prot & !7 != 0 {
        return Err(errno::EINVAL);
    }
    let kind = flags & 3;
    if kind == 0 {
        return Err(errno::EINVAL);
    }
    let len = len.checked_add(PAGE - 1).ok_or(errno::ENOMEM)? & !(PAGE - 1);
    let file_pages = size.div_ceil(PAGE) * PAGE;
    let file_len = file_pages.saturating_sub(offset).min(len);
    let write = prot & PROT_WRITE != 0;
    // A shared mapping writes through to the file: the image is read only, and a
    // scratch file's pages are the server's own.
    if kind & MAP_SHARED != 0 && kind & MAP_PRIVATE == 0 && write {
        return Err(errno::EACCES);
    }
    Ok(FilePlan {
        len,
        file_len,
        copy: write || !mappable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_t8_file_mapping_plans() {
        // A library's text: read only, its own pages.
        assert_eq!(
            plan(0x5000, 5, MAP_PRIVATE, 0, 0x4800, true),
            Ok(FilePlan {
                len: 0x5000,
                file_len: 0x5000,
                copy: false
            })
        );
        // Its data: private and writable, a copy.
        assert_eq!(
            plan(0x1800, 3, MAP_PRIVATE | MAP_FIXED, 0x4000, 0x4800, true),
            Ok(FilePlan {
                len: 0x2000,
                file_len: 0x1000,
                copy: true
            })
        );
        // Past the file's end: zero pages.
        assert_eq!(
            plan(0x2000, 1, MAP_PRIVATE, 0x8000, 0x4800, true),
            Ok(FilePlan {
                len: 0x2000,
                file_len: 0,
                copy: false
            })
        );
        // A scratch file is copied.
        assert!(plan(0x1000, 1, MAP_SHARED, 0, 10, false).unwrap().copy);
        assert_eq!(plan(0x1000, 3, MAP_SHARED, 0, 10, true), Err(errno::EACCES));
        assert_eq!(plan(0, 1, MAP_PRIVATE, 0, 10, true), Err(errno::EINVAL));
        assert_eq!(plan(1, 1, MAP_PRIVATE, 1, 10, true), Err(errno::EINVAL));
        assert_eq!(plan(1, 8, MAP_PRIVATE, 0, 10, true), Err(errno::EINVAL));
        assert_eq!(plan(1, 1, 0, 0, 10, true), Err(errno::EINVAL));
        assert_eq!(
            plan(u64::MAX, 1, MAP_PRIVATE, 0, 10, true),
            Err(errno::ENOMEM)
        );
    }
}
