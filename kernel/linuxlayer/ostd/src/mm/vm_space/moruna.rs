// SPDX-License-Identifier: MPL-2.0

//! The calls on Moruna's core, through `moruna-sys`.
//!
//! Every handle here is the personality's own: its frames' objects, and the processes it
//! shows spaces in.

use moruna_abi::map;
use moruna_sys::{Error, Mapping, Memory, raw};

use super::calls::{Made, ProgramProcess};

/// An object and its mapping in the region, unmapped then closed when dropped.
#[derive(Debug)]
pub(crate) struct Held {
    _mapping: Mapping<'static>,
    _memory: Memory,
}

pub(super) fn object_create(len: usize) -> Result<Made, Error> {
    let memory = Memory::create(len as u64)?;
    let mapping = memory.map_detached(0, len as u64, map::WRITE)?;
    Ok(Made {
        raw: memory.0.raw(),
        addr: mapping.addr() as usize,
        held: Held {
            _mapping: mapping,
            _memory: memory,
        },
    })
}

pub(super) fn map(
    process: ProgramProcess,
    va: usize,
    object: u32,
    offset: usize,
    len: usize,
    prot: u64,
) -> Result<(), Error> {
    Error::from_raw(raw::process_map(
        process.raw(),
        object,
        offset as u64,
        len as u64,
        va as u64,
        prot,
    ))
    .map(|_| ())
}

pub(super) fn unmap(process: ProgramProcess, va: usize, len: usize) -> Result<(), Error> {
    Error::from_raw(raw::process_unmap(process.raw(), va as u64, len as u64)).map(|_| ())
}

pub(super) fn protect(
    process: ProgramProcess,
    va: usize,
    len: usize,
    prot: u64,
) -> Result<(), Error> {
    Error::from_raw(raw::process_protect(
        process.raw(),
        va as u64,
        len as u64,
        prot,
    ))
    .map(|_| ())
}
