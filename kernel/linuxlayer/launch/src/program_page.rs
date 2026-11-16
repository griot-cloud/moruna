// SPDX-License-Identifier: MPL-2.0

//! The program's start page: the page the personality makes for a program it runs with
//! handles granted (`run_granted`, `run_plane`), in the core's start page layout
//! (`moruna_abi::start_page`). The program's auxiliary vector carries its address under
//! `AT_MORUNA_START`, as the old server's did, and its entries name the program's handles by
//! their values in the program's own handle table (`architecture/data-os.md` section 4).

use alloc::{vec, vec::Vec};

use moruna_abi::{ABI_VERSION, PAGE_SIZE, limits, start_page as page, write_u32};

/// The auxiliary vector entry whose value is the page's address, as
/// `linux_protocol::AT_MORUNA_START`: outside every type Linux defines.
pub const AT_MORUNA_START: u64 = 0x4d4f_5255;

/// A handle on the page: its value in the program's table, its rights and its name.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub handle: u32,
    pub rights: u32,
    pub name: &'a str,
}

/// The page, `PAGE_SIZE` bytes, for the process named `process` (cut at
/// `limits::NAME_MAX` bytes) on `cpus` CPUs; `None` for more than
/// `limits::START_HANDLES_MAX` entries, or a name past `limits::NAME_MAX` bytes.
pub fn encode(process: &str, cpus: u32, entries: &[Entry<'_>]) -> Option<Vec<u8>> {
    if entries.len() > limits::START_HANDLES_MAX
        || entries.iter().any(|e| e.name.len() > limits::NAME_MAX)
    {
        return None;
    }
    let mut out = vec![0u8; PAGE_SIZE as usize];
    write_u32(&mut out, page::MAGIC_OFFSET, page::MAGIC);
    write_u32(&mut out, page::VERSION_OFFSET, ABI_VERSION as u32);
    write_u32(&mut out, page::COUNT_OFFSET, entries.len() as u32);
    write_u32(&mut out, page::CPUS_OFFSET, cpus);
    let name = &process.as_bytes()[..process.len().min(limits::NAME_MAX)];
    out[page::NAME_OFFSET..][..name.len()].copy_from_slice(name);
    for (i, e) in entries.iter().enumerate() {
        let at = page::HEADER_SIZE + i * page::ENTRY_SIZE;
        write_u32(&mut out, at + page::ENTRY_HANDLE, e.handle);
        write_u32(&mut out, at + page::ENTRY_RIGHTS, e.rights);
        out[at + page::ENTRY_NAME..][..e.name.len()].copy_from_slice(e.name.as_bytes());
    }
    Some(out)
}
