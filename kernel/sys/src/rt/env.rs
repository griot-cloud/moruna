//! The process's environment (item 5.5): its start slot `env` ([`crate::env`]), read once by the
//! program's start ([`super::start`]) and kept for the process's life. std's `sys::env` answers
//! from it; the C library builds its `environ` from it ([`c_entries`]). Setting a variable is
//! not supported: the environment is what the process was started with.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use moruna_abi::PAGE_SIZE;

use super::os;
use crate::env::{Entries, SLOT, end_from, entries as parse};

const PAGE: usize = PAGE_SIZE as usize;

/// The entries the start read, each with its zero byte: their address and length. Written once,
/// by the first thread before `main`, and read after.
static BYTES: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static LEN: AtomicUsize = AtomicUsize::new(0);

/// Reads the slot `env`, if the process was given one, and keeps its entries.
pub(super) fn load() {
    if let Some(slot) = crate::start::take(SLOT) {
        let memory = crate::Memory(slot);
        keep(read(|offset, page| os::env_page(&memory, offset, page)));
    }
}

/// The environment's bytes, through `page`, which fills its buffer with the object's page at an
/// offset and says whether the object has one: page after page until one holds the empty entry
/// that ends the entries, or the object ends. What it gives ends in a zero byte, so every entry
/// in it is a C string.
pub(super) fn read(mut page: impl FnMut(u64, &mut [u8]) -> bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; PAGE];
    let mut offset = 0u64;
    while page(offset, &mut buf) {
        let from = bytes.len();
        bytes.extend_from_slice(&buf);
        if let Some(end) = end_from(&bytes, from) {
            bytes.truncate(end);
            break;
        }
        offset += PAGE_SIZE;
    }
    if bytes.last().is_some_and(|b| *b != 0) {
        bytes.push(0);
    }
    bytes
}

/// Keeps `bytes` as the environment, for the process's life.
fn keep(bytes: Vec<u8>) {
    let kept: &'static mut [u8] = bytes.leak();
    LEN.store(kept.len(), Ordering::Relaxed);
    BYTES.store(kept.as_mut_ptr(), Ordering::Release);
}

/// The environment's bytes: empty until the start read some.
fn bytes() -> &'static [u8] {
    let ptr = BYTES.load(Ordering::Acquire);
    if ptr.is_null() {
        return &[];
    }
    // SAFETY: `keep` leaked these bytes, and stored their address after their length; nothing
    // writes them again.
    unsafe { core::slice::from_raw_parts(ptr, LEN.load(Ordering::Relaxed)) }
}

/// The environment's entries, each `(NAME, VALUE)`, in the order they were given.
pub fn entries() -> Entries<'static> {
    parse(bytes())
}

/// The value of the variable `name`: its first entry's.
pub fn get(name: &[u8]) -> Option<&'static [u8]> {
    entries().find(|(n, _)| *n == name).map(|(_, v)| v)
}

/// Each entry as a C string, `NAME=VALUE` and its zero byte, for the C library's `environ`:
/// its address, which stays valid for the process's life.
pub fn c_entries() -> impl Iterator<Item = *const u8> {
    entries().map(|(name, _)| name.as_ptr())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec;

    use super::*;

    /// An object of `pages` pages holding `text` from its start, read through `read`.
    fn read_object(text: &[u8], pages: usize) -> Vec<u8> {
        let mut object = vec![0u8; pages * PAGE];
        object[..text.len()].copy_from_slice(text);
        read(|offset, page| {
            let at = offset as usize;
            match object.get(at..at + PAGE) {
                Some(src) => {
                    page.copy_from_slice(src);
                    true
                }
                None => false,
            }
        })
    }

    #[test]
    fn the_entries_are_read_to_their_end_and_kept_with_their_zero_bytes() {
        let bytes = read_object(b"A=1\0B=two\0", 1);
        assert_eq!(bytes, b"A=1\0B=two\0");
        // Entries that run over a page are read on to the next.
        let long = [b'x'; PAGE];
        let mut text = b"LONG=".to_vec();
        text.extend_from_slice(&long);
        text.push(0);
        let bytes = read_object(&text, 2);
        assert_eq!(bytes, text);
        // An object with no room left for the empty entry ends where it ends.
        let mut full = vec![b'y'; PAGE];
        full[..2].copy_from_slice(b"F=");
        let bytes = read_object(&full, 1);
        assert_eq!(bytes.len(), PAGE + 1);
        assert_eq!(bytes.last(), Some(&0));
        // No object, or an empty one: no entries.
        assert!(read(|_, _| false).is_empty());
        assert!(read_object(b"", 1).is_empty());
    }

    #[test]
    fn the_kept_environment_answers_by_name_and_as_c_strings() {
        // Before the start keeps any: none.
        assert_eq!(get(b"MORUNA_PARAMS"), None);
        keep(b"MORUNA_ROLE=function\0MORUNA_PARAMS={\"threshold\":0.5}\0A=1\0A=2\0".to_vec());
        assert_eq!(get(b"MORUNA_PARAMS"), Some(&b"{\"threshold\":0.5}"[..]));
        assert_eq!(get(b"MORUNA_ROLE"), Some(&b"function"[..]));
        assert_eq!(get(b"A"), Some(&b"1"[..]), "the first entry of a name");
        assert_eq!(get(b"MORUNA_ARGS"), None);
        assert_eq!(entries().count(), 4);
        let c: Vec<&core::ffi::CStr> = c_entries()
            // SAFETY: each is an entry of the kept bytes, a zero byte after it.
            .map(|p| unsafe { core::ffi::CStr::from_ptr(p.cast()) })
            .collect();
        assert_eq!(c[0].to_bytes(), b"MORUNA_ROLE=function");
        assert_eq!(c[1].to_bytes(), b"MORUNA_PARAMS={\"threshold\":0.5}");
        assert_eq!(c.len(), 4);
        // On a host the start page holds no slot: loading keeps what was there.
        load();
        assert_eq!(entries().count(), 4);
    }
}
