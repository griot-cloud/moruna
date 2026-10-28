//! The start page (SDD 20, d.4): the handles this process started with, by slot name.

use core::sync::atomic::{AtomicU64, Ordering};

use moruna_abi::{RawHandle, Rights, fixed_name, limits, read_u32, start_page};

use crate::Handle;

static PAGE: AtomicU64 = AtomicU64::new(0);
static TAKEN: AtomicU64 = AtomicU64::new(0);

/// Records the start page's address; `main!` calls it before anything else.
pub fn init(addr: u64) {
    PAGE.store(addr, Ordering::Release);
}

fn page() -> Option<&'static [u8; 4096]> {
    let addr = PAGE.load(Ordering::Acquire);
    if addr == 0 || !addr.is_multiple_of(4096) {
        return None;
    }
    // SAFETY: the kernel maps one read-only page at the address it passes to the first
    // thread in `rdi`, before that thread runs, and unmaps it only when the process has
    // ended, so it is readable and never written for as long as this code runs (KK-I9).
    let page = unsafe { &*(addr as *const [u8; 4096]) };
    (read_u32(page, start_page::MAGIC_OFFSET) == Some(start_page::MAGIC)).then_some(page)
}

/// How many handles the process started with.
pub fn count() -> usize {
    page()
        .and_then(|p| read_u32(p, start_page::COUNT_OFFSET))
        .map_or(0, |n| (n as usize).min(limits::START_HANDLES_MAX))
}

/// Entry `i`: the raw value, its rights and its slot name.
pub fn entry(i: usize) -> Option<(RawHandle, Rights, &'static str)> {
    if i >= count() {
        return None;
    }
    let p = page()?;
    let at = start_page::HEADER_SIZE + i * start_page::ENTRY_SIZE;
    let value = read_u32(p, at + start_page::ENTRY_HANDLE)?;
    let rights = Rights::from_bits(read_u32(p, at + start_page::ENTRY_RIGHTS)?)?;
    let name_at = at + start_page::ENTRY_NAME;
    let name = core::str::from_utf8(fixed_name(&p[name_at..name_at + limits::NAME_MAX])).ok()?;
    Some((value, rights, name))
}

fn index(name: &str) -> Option<usize> {
    (0..count()).find(|i| entry(*i).is_some_and(|(_, _, n)| n == name))
}

/// Takes ownership of the handle in slot `name`; a slot is taken once.
pub fn take(name: &str) -> Option<Handle> {
    let i = index(name)?;
    let bit = 1u64 << i;
    if TAKEN.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
        return None;
    }
    entry(i).map(|(v, _, _)| Handle::from_raw(v))
}

/// The raw value in slot `name`, without taking it.
pub fn peek(name: &str) -> Option<RawHandle> {
    index(name).and_then(entry).map(|(v, _, _)| v)
}

/// The process's name, as the manifest or its creator gave it.
pub fn process_name() -> &'static str {
    page()
        .and_then(|p| {
            let field = &p[start_page::NAME_OFFSET..start_page::NAME_OFFSET + limits::NAME_MAX];
            core::str::from_utf8(fixed_name(field)).ok()
        })
        .unwrap_or("?")
}
