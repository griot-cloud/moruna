//! A native process's environment (item 5.5, system design 5A0): its start slot `env`, a
//! read-only memory object of entries `NAME=VALUE`, each ending in a zero byte. An empty entry
//! (a zero byte where an entry would begin) or the object's end ends them; a process given no
//! `env` slot has no environment. The kernel's start page is unchanged: the environment is one
//! more handle on it.
//!
//! Its maker gives it with [`give`]: the engine's stage, for a native task (`moruna-micro`), and
//! the boot's harness. std's start reads it once (`rt::env`); `std::env` answers from that, and
//! the C library builds its `environ` from std's.

use moruna_abi::{PAGE_SIZE, Rights, map};

use crate::{Error, Handle, Memory, Result};

/// The start slot a native process's environment is given under.
pub const SLOT: &str = "env";

/// Whether `entry` can be one: `NAME=VALUE` with a name of at least one byte, and no zero byte.
pub fn is_entry(entry: &[u8]) -> bool {
    !entry.contains(&0)
        && entry
            .iter()
            .position(|b| *b == b'=')
            .is_some_and(|eq| eq > 0)
}

/// The bytes `entries` take, each with its zero byte; `None` when one cannot be an entry
/// ([`is_entry`]).
pub fn encoded_len<E: AsRef<[u8]>>(entries: &[E]) -> Option<usize> {
    entries.iter().try_fold(0usize, |len, e| {
        let e = e.as_ref();
        is_entry(e).then(|| len.checked_add(e.len() + 1)).flatten()
    })
}

/// The size of the memory object that holds `len` bytes of entries: whole pages, with at least
/// one zero byte after them, the empty entry that ends them.
pub fn object_size(len: usize) -> Option<u64> {
    let len = u64::try_from(len).ok()?.checked_add(1)?;
    len.div_ceil(PAGE_SIZE).checked_mul(PAGE_SIZE)
}

/// Where each of `entries` begins in its object: one after another, a zero byte after each.
pub fn offsets<E: AsRef<[u8]>>(entries: &[E]) -> impl Iterator<Item = (usize, &[u8])> {
    entries.iter().scan(0usize, |at, e| {
        let e = e.as_ref();
        let here = *at;
        *at += e.len() + 1;
        Some((here, e))
    })
}

/// A new memory object holding `entries`, as a handle to give under [`SLOT`] that reads and
/// maps it and moves: no process it is given to can write it. It is charged to the caller's
/// account. `InvalidArgs` when an entry is not `NAME=VALUE` or holds a zero byte.
pub fn give<E: AsRef<[u8]>>(entries: &[E]) -> Result<Handle> {
    let len = encoded_len(entries).ok_or(Error::InvalidArgs)?;
    let size = object_size(len).ok_or(Error::OutOfRange)?;
    let memory = Memory::create(size)?;
    {
        // A new object's pages are zero: each entry's zero byte, and the end, are there.
        let mapping = memory.map(0, size, map::READ | map::WRITE)?;
        for (at, e) in offsets(entries) {
            mapping.write(at, e)?;
        }
    }
    memory
        .0
        .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
}

/// Where the entries in `bytes` end: the empty entry that ends them, looked for from `from`
/// (bytes before it were looked at already); `None` while `bytes` holds no such entry.
pub fn end_from(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len()).find(|&p| bytes[p] == 0 && (p == 0 || bytes[p - 1] == 0))
}

/// The entries of an encoded environment, each `(NAME, VALUE)`, in order, up to the first empty
/// entry or the end of the bytes. An entry with no `=`, or an empty name, is passed over, as a C
/// library passes over a malformed one.
#[derive(Clone, Debug)]
pub struct Entries<'a> {
    rest: &'a [u8],
}

/// The entries `bytes` holds.
pub fn entries(bytes: &[u8]) -> Entries<'_> {
    Entries { rest: bytes }
}

impl<'a> Iterator for Entries<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let end = self
                .rest
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(self.rest.len());
            if end == 0 {
                self.rest = &[];
                return None;
            }
            let entry = &self.rest[..end];
            self.rest = self.rest.get(end + 1..).unwrap_or(&[]);
            if let Some(eq) = entry.iter().position(|b| *b == b'=')
                && eq > 0
            {
                return Some((&entry[..eq], &entry[eq + 1..]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    /// `entries` encoded as [`give`] lays them in their object.
    fn encoded(entries: &[&str]) -> Vec<u8> {
        let len = encoded_len(entries).unwrap();
        let mut out = std::vec![0u8; object_size(len).unwrap() as usize];
        for (at, e) in offsets(entries) {
            out[at..at + e.len()].copy_from_slice(e);
        }
        out
    }

    #[test]
    fn an_entry_is_a_name_an_equals_sign_and_a_value_with_no_zero_byte() {
        assert!(is_entry(b"MORUNA_PARAMS={\"threshold\":0.5}"));
        assert!(is_entry(b"EMPTY="));
        assert!(is_entry(b"A=b=c"));
        assert!(!is_entry(b"=value"));
        assert!(!is_entry(b"NAME"));
        assert!(!is_entry(b""));
        assert!(!is_entry(b"A=b\0c"));
    }

    #[test]
    fn entries_are_laid_one_after_another_each_with_its_zero_byte() {
        let given = ["MORUNA_ROLE=function", "MORUNA_PARAMS={}"];
        assert_eq!(encoded_len(&given), Some(21 + 17));
        assert_eq!(
            offsets(&given).map(|(at, _)| at).collect::<Vec<_>>(),
            [0, 21]
        );
        let bytes = encoded(&given);
        assert_eq!(bytes.len(), 4096);
        assert_eq!(&bytes[..38], b"MORUNA_ROLE=function\0MORUNA_PARAMS={}\0");
        assert_eq!(end_from(&bytes, 0), Some(38));
        let read: Vec<(&[u8], &[u8])> = entries(&bytes).collect();
        assert_eq!(
            read,
            [
                (&b"MORUNA_ROLE"[..], &b"function"[..]),
                (b"MORUNA_PARAMS", b"{}")
            ]
        );
    }

    #[test]
    fn an_entry_that_cannot_be_one_is_refused_whole() {
        assert_eq!(encoded_len(&["A=1", "B"]), None);
        assert_eq!(encoded_len(&["A=1", "B=\0"]), None);
        assert_eq!(encoded_len::<&str>(&[]), Some(0));
    }

    #[test]
    fn the_object_keeps_a_zero_byte_after_the_entries() {
        assert_eq!(object_size(0), Some(4096));
        assert_eq!(object_size(4095), Some(4096));
        assert_eq!(object_size(4096), Some(8192));
        assert_eq!(object_size(usize::MAX), None);
        // No environment: the object is one empty entry.
        assert_eq!(entries(&encoded(&[])).count(), 0);
        assert_eq!(end_from(&encoded(&[]), 0), Some(0));
    }

    #[test]
    fn the_end_is_found_from_where_the_last_look_stopped() {
        assert_eq!(end_from(b"A=1\0", 0), None);
        assert_eq!(end_from(b"A=1\0\0", 0), Some(4));
        // A look from 4 sees the zero byte at 3 before it.
        assert_eq!(end_from(b"A=1\0\0", 4), Some(4));
        assert_eq!(end_from(b"A=1\0B=2\0\0", 5), Some(8));
        assert_eq!(end_from(b"", 0), None);
    }

    #[test]
    fn a_malformed_entry_is_passed_over_and_the_last_may_lack_its_zero_byte() {
        let read: Vec<(&[u8], &[u8])> = entries(b"=x\0NOEQ\0A=1\0B=").collect();
        assert_eq!(read, [(&b"A"[..], &b"1"[..]), (b"B", b"")]);
        assert_eq!(entries(b"").count(), 0);
        assert_eq!(entries(b"\0A=1\0").count(), 0);
        let it = entries(b"A=1\0");
        assert!(std::format!("{it:?}").contains("Entries"));
    }
}
