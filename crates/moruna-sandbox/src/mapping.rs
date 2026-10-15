//! The shared mapping (17 e.4): a memfd both Moruna and the child map, its first page the
//! counters, the rest where batches are placed and read in place (SB-I7).

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};

/// The counters page's size, and where the data area begins.
pub const PAGE: usize = 4096;
/// "MORUNASB", the counters page's first word.
pub const MAGIC: u64 = 0x4d4f_5255_4e41_5342;
/// The counters page's layout version.
pub const VERSION: u64 = 1;

/// The counters page's fields, by byte offset (17 e.4).
pub mod field {
    /// `MAGIC`.
    pub const MAGIC: usize = 0;
    /// The stage ceiling; Moruna writes it.
    pub const CEILING: usize = 8;
    /// The child's memory now; the child writes it.
    pub const IN_USE: usize = 16;
    /// The child's memory at its highest; the child writes it.
    pub const PEAK: usize = 24;
    /// Calls the child has run.
    pub const CALLS: usize = 32;
    /// `VERSION`.
    pub const VERSION: usize = 40;
}

/// A memfd mapped shared, read and write.
pub struct Mapping {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain shared memory; the pointer is valid for `len` bytes for the
// mapping's lifetime and every access goes through the counters' atomics or through the byte
// ranges the protocol gives one side at a time (17 e.3, e.4).
unsafe impl Send for Mapping {}
// SAFETY: as for `Send`; shared references only read the atomics or the ranges the protocol
// hands out.
unsafe impl Sync for Mapping {}

impl std::fmt::Debug for Mapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mapping").field("len", &self.len).finish()
    }
}

fn last_error(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

impl Mapping {
    /// A new memfd of `PAGE + data_bytes` (rounded up to a page), mapped, with its counters page
    /// initialised (17 e.4). Pages are touched only by what is written.
    pub fn create(data_bytes: u64) -> io::Result<Mapping> {
        let data = usize::try_from(data_bytes)
            .map_err(|_| io::Error::other("the mapping does not fit the address space"))?;
        let len = PAGE
            .checked_add(data.div_ceil(PAGE) * PAGE)
            .ok_or_else(|| io::Error::other("the mapping does not fit the address space"))?;
        // SAFETY: a NUL-terminated name and valid flags; the returned descriptor is checked.
        let raw = unsafe { libc::memfd_create(c"moruna-sandbox".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(last_error("memfd_create"));
        }
        // SAFETY: `raw` is a descriptor this process just opened and owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: a descriptor we own and a length that fits `off_t` (it fits `usize`, 64-bit).
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(last_error("ftruncate"));
        }
        let mapping = Mapping::map(fd, len)?;
        mapping
            .counter(field::MAGIC)
            .store(MAGIC, Ordering::Release);
        mapping
            .counter(field::VERSION)
            .store(VERSION, Ordering::Release);
        Ok(mapping)
    }

    /// Map an existing memfd of `len` bytes (the child's side); refused when its counters page
    /// does not carry the magic and the version.
    pub fn open(fd: OwnedFd, len: usize) -> io::Result<Mapping> {
        let mapping = Mapping::map(fd, len)?;
        let magic = mapping.counter(field::MAGIC).load(Ordering::Acquire);
        let version = mapping.counter(field::VERSION).load(Ordering::Acquire);
        if magic != MAGIC || version != VERSION {
            return Err(io::Error::other(format!(
                "the mapping's counters page is not version {VERSION} of 17 e.4 (magic {magic:#x}, \
                 version {version})"
            )));
        }
        Ok(mapping)
    }

    fn map(fd: OwnedFd, len: usize) -> io::Result<Mapping> {
        if len < PAGE {
            return Err(io::Error::other(
                "a mapping holds at least its counters page",
            ));
        }
        // SAFETY: a fresh shared mapping of a descriptor we own, of its whole length; the result
        // is checked against MAP_FAILED.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(last_error("mmap"));
        }
        Ok(Mapping {
            fd,
            ptr: ptr.cast(),
            len,
        })
    }

    /// The memfd, to pass to the child.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The whole mapping's length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the data area is empty (never, for a mapping `create` made).
    pub fn is_empty(&self) -> bool {
        self.len <= PAGE
    }

    /// One counter of the page (e.4 offsets).
    pub fn counter(&self, offset: usize) -> &AtomicU64 {
        assert!(
            offset.is_multiple_of(8) && offset + 8 <= PAGE,
            "a counter of the page"
        );
        // SAFETY: `ptr` is page aligned and valid for `len >= PAGE` bytes, `offset` is 8-byte
        // aligned and inside the page, and `AtomicU64` has `u64`'s layout; both processes access
        // the word only through atomics.
        unsafe { &*(self.ptr.add(offset).cast::<AtomicU64>()) }
    }

    /// Where the data area starts.
    pub fn data_offset(&self) -> usize {
        PAGE
    }

    fn check(&self, offset: usize, len: usize) -> io::Result<()> {
        match offset.checked_add(len) {
            Some(end) if offset >= PAGE && end <= self.len => Ok(()),
            _ => Err(io::Error::other(format!(
                "bytes {offset}..+{len} are outside the mapping's data area ({PAGE}..{})",
                self.len
            ))),
        }
    }

    /// The bytes at `offset..offset + len` of the data area.
    ///
    /// The protocol gives a range to one side at a time (17 e.3): Moruna writes an input and the
    /// child reads it during the call; the child writes an output and Moruna reads it after the
    /// reply. A caller holds a slice only while the range is its own.
    pub fn bytes(&self, offset: usize, len: usize) -> io::Result<&[u8]> {
        self.check(offset, len)?;
        // SAFETY: the range is inside the mapping (checked) and, by the protocol, not written by
        // the other process while it is this side's.
        Ok(unsafe { core::slice::from_raw_parts(self.ptr.add(offset), len) })
    }

    /// As [`Mapping::bytes`], to write; the same ownership rule.
    #[allow(clippy::mut_from_ref)]
    pub fn bytes_mut(&self, offset: usize, len: usize) -> io::Result<&mut [u8]> {
        self.check(offset, len)?;
        // SAFETY: the range is inside the mapping (checked); by the protocol the other process
        // neither reads nor writes it while it is this side's, and this side holds one slice of
        // it at a time.
        Ok(unsafe { core::slice::from_raw_parts_mut(self.ptr.add(offset), len) })
    }

    /// The address of `offset`, for a reader that reads in place (the child's pyarrow buffer).
    pub fn address(&self, offset: usize, len: usize) -> io::Result<*mut u8> {
        self.check(offset, len)?;
        // SAFETY: inside the mapping (checked).
        Ok(unsafe { self.ptr.add(offset) })
    }

    /// Give back the whole pages inside `offset..offset + len` (17 e.4); they read back as
    /// zeros. A page the range shares with a neighbour is left alone: the neighbour may be an
    /// input the child still reads (SB-I13).
    pub fn punch(&self, offset: usize, len: usize) -> io::Result<()> {
        self.check(offset, len)?;
        let start = offset.div_ceil(PAGE) * PAGE;
        let end = (offset + len) / PAGE * PAGE;
        if end <= start {
            return Ok(());
        }
        // SAFETY: a descriptor we own and a range inside the file.
        let r = unsafe {
            libc::fallocate(
                self.fd.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                start as libc::off_t,
                (end - start) as libc::off_t,
            )
        };
        if r != 0 {
            return Err(last_error("fallocate"));
        }
        Ok(())
    }
}

/// Something that holds a mapping: what keeps it alive under an Arrow buffer read in place.
pub trait HasMapping: Send + Sync {
    /// The mapping.
    fn mapping(&self) -> &Mapping;
}

/// A shared owner of a mapping.
pub type Owner = std::sync::Arc<dyn HasMapping>;

/// Keeps the owner alive for as long as the Arrow buffer over its bytes lives.
struct Keep(#[allow(dead_code)] Owner);

impl std::panic::RefUnwindSafe for Keep {}

/// The bytes at `offset..offset + len` as an Arrow buffer, without a copy: what Moruna decodes a
/// child's output from (SB-I7). The owner keeps the mapping mapped; by the protocol the child does
/// not write the range again until Moruna's next call, after the output has been landed.
pub fn arrow_buffer(
    owner: Owner,
    offset: usize,
    len: usize,
) -> io::Result<moruna_kernel::arrow::buffer::Buffer> {
    let ptr = owner.mapping().address(offset, len)?;
    let ptr = core::ptr::NonNull::new(ptr).ok_or_else(|| io::Error::other("a null mapping"))?;
    // SAFETY: `ptr` is valid for `len` bytes (checked by `address`) for as long as the mapping
    // lives, and the `Keep` the buffer holds keeps the mapping's owner, and so the mapping,
    // alive; the bytes are not written while the buffer is in use (the protocol, 17 e.3).
    Ok(unsafe {
        moruna_kernel::arrow::buffer::Buffer::from_custom_allocation(
            ptr,
            len,
            std::sync::Arc::new(Keep(owner)),
        )
    })
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping this value made, unmapped once.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SB-T9: the fields at e.4's offsets, shared through the memfd, and a punched range reads
    /// back as zeros.
    #[test]
    fn sb_t9_counters_page() {
        let a = Mapping::create(3 * PAGE as u64 + 1).unwrap();
        assert_eq!(a.len(), PAGE + 4 * PAGE);
        assert!(!a.is_empty());
        let dup = a.fd().try_clone_to_owned().unwrap();
        let b = Mapping::open(dup, a.len()).unwrap();
        a.counter(field::CEILING).store(123, Ordering::Release);
        assert_eq!(b.counter(field::CEILING).load(Ordering::Acquire), 123);
        b.counter(field::IN_USE).store(7, Ordering::Release);
        assert_eq!(a.counter(field::IN_USE).load(Ordering::Acquire), 7);
        assert_eq!(a.counter(field::MAGIC).load(Ordering::Acquire), MAGIC);
        assert_eq!(a.counter(field::VERSION).load(Ordering::Acquire), VERSION);
        a.bytes_mut(PAGE, 5).unwrap().copy_from_slice(b"hello");
        assert_eq!(b.bytes(PAGE, 5).unwrap(), b"hello");
        assert!(!a.address(PAGE, 5).unwrap().is_null());
        // A range that holds no whole page gives nothing back: its page has neighbours.
        a.punch(PAGE, 5).unwrap();
        assert_eq!(b.bytes(PAGE, 5).unwrap(), b"hello");
        a.bytes_mut(2 * PAGE - 3, 3)
            .unwrap()
            .copy_from_slice(b"end");
        a.punch(PAGE + 5, 2 * PAGE).unwrap();
        assert_eq!(b.bytes(PAGE, 5).unwrap(), b"hello", "a shared page stays");
        assert_eq!(
            b.bytes(2 * PAGE - 3, 3).unwrap(),
            b"end",
            "a shared page stays"
        );
        assert_eq!(b.bytes(2 * PAGE, PAGE).unwrap(), &[0; PAGE][..]);
        a.punch(PAGE, PAGE).unwrap();
        assert_eq!(b.bytes(PAGE, 5).unwrap(), &[0; 5]);
        assert_eq!(
            a.counter(field::CEILING).load(Ordering::Acquire),
            123,
            "the page stays"
        );
        assert!(a.bytes(0, 8).is_err(), "the counters page is not data");
        assert!(a.bytes(PAGE, a.len()).is_err());
        assert!(a.bytes_mut(a.len(), 1).is_err());
        assert!(a.punch(usize::MAX, 2).is_err());
        assert_eq!(a.data_offset(), PAGE);
        assert!(format!("{a:?}").contains("len"));
        // A memfd whose page is not the protocol's is refused.
        let plain = Mapping::create(PAGE as u64).unwrap();
        plain.counter(field::MAGIC).store(0, Ordering::Release);
        let e = Mapping::open(plain.fd().try_clone_to_owned().unwrap(), plain.len()).unwrap_err();
        assert!(e.to_string().contains("counters page"));
        assert!(Mapping::open(plain.fd().try_clone_to_owned().unwrap(), 10).is_err());
        // A length the address space cannot map: `mmap`'s refusal, named.
        let e = Mapping::open(plain.fd().try_clone_to_owned().unwrap(), 1 << 62).unwrap_err();
        assert!(e.to_string().starts_with("mmap"), "{e}");
    }
}
