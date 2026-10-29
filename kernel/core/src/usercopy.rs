//! Copies between the kernel and the calling process's memory.
//!
//! Every copy goes through OSTD's fallible user readers and writers, which check the
//! range is user space and turn a fault into an error, so a bad pointer from a process
//! is `Fault`, never a kernel crash. A copy that faults on zero pages the process has
//! not touched yet populates them (SDD 20, f.12) and is tried once more. Every length
//! is bounded by the caller before it gets here, and buffers are reserved fallibly.

use alloc::vec::Vec;

use moruna_abi::Error;
use ostd::mm::{FallibleVmRead, FallibleVmWrite, VmReader, VmWriter};

use crate::vm::Space;

fn read_into(space: &Space, ptr: u64, buf: &mut [u8]) -> Result<(), Error> {
    let addr = usize::try_from(ptr).map_err(|_| Error::Fault)?;
    let mut reader = space.vm.reader(addr, buf.len()).map_err(|_| Error::Fault)?;
    let len = buf.len();
    let mut writer = VmWriter::from(&mut buf[..]);
    match reader.read_fallible(&mut writer) {
        Ok(n) if n == len => Ok(()),
        _ => Err(Error::Fault),
    }
}

fn write_from(space: &Space, ptr: u64, data: &[u8]) -> Result<(), Error> {
    let addr = usize::try_from(ptr).map_err(|_| Error::Fault)?;
    let mut writer = space
        .vm
        .writer(addr, data.len())
        .map_err(|_| Error::Fault)?;
    let mut reader = VmReader::from(data);
    match writer.write_fallible(&mut reader) {
        Ok(n) if n == data.len() => Ok(()),
        _ => Err(Error::Fault),
    }
}

/// Copies `buf.len()` bytes from `ptr` of the active space.
pub fn read_exact(space: &Space, ptr: u64, buf: &mut [u8]) -> Result<(), Error> {
    if buf.is_empty() {
        return Ok(());
    }
    if read_into(space, ptr, buf).is_ok() {
        return Ok(());
    }
    space.populate(ptr, buf.len() as u64, false)?;
    read_into(space, ptr, buf)
}

pub fn read(space: &Space, ptr: u64, len: usize) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    if len == 0 {
        return Ok(buf);
    }
    buf.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
    buf.resize(len, 0);
    read_exact(space, ptr, &mut buf)?;
    Ok(buf)
}

pub fn write(space: &Space, ptr: u64, data: &[u8]) -> Result<(), Error> {
    if data.is_empty() {
        return Ok(());
    }
    if write_from(space, ptr, data).is_ok() {
        return Ok(());
    }
    space.populate(ptr, data.len() as u64, true)?;
    write_from(space, ptr, data)
}

/// The `u32` at `ptr`, which must already be populated: this is the read a futex makes
/// under its lock, which must not take the space's.
pub fn read_u32(space: &Space, ptr: u64) -> Result<u32, Error> {
    let mut b = [0u8; 4];
    read_into(space, ptr, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

pub fn read_u64(space: &Space, ptr: u64) -> Result<u64, Error> {
    let mut b = [0u8; 8];
    read_exact(space, ptr, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

pub fn write_u32(space: &Space, ptr: u64, value: u32) -> Result<(), Error> {
    write(space, ptr, &value.to_le_bytes())
}
