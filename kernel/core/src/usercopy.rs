//! Copies between the kernel and the calling process's memory.
//!
//! Every copy goes through OSTD's fallible user readers and writers, which check the
//! range is user space and turn a fault into an error, so a bad pointer from a process
//! is `Fault`, never a kernel crash. Every length is bounded by the caller before it
//! gets here, and buffers are reserved fallibly.

use alloc::vec::Vec;

use moruna_abi::Error;
use ostd::mm::{FallibleVmRead, FallibleVmWrite, VmReader, VmSpace, VmWriter};

pub fn read(vm: &VmSpace, ptr: u64, len: usize) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    if len == 0 {
        return Ok(buf);
    }
    buf.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
    buf.resize(len, 0);
    let addr = usize::try_from(ptr).map_err(|_| Error::Fault)?;
    let mut reader = vm.reader(addr, len).map_err(|_| Error::Fault)?;
    let mut writer = VmWriter::from(&mut buf[..]);
    match reader.read_fallible(&mut writer) {
        Ok(n) if n == len => Ok(buf),
        _ => Err(Error::Fault),
    }
}

pub fn write(vm: &VmSpace, ptr: u64, data: &[u8]) -> Result<(), Error> {
    if data.is_empty() {
        return Ok(());
    }
    let addr = usize::try_from(ptr).map_err(|_| Error::Fault)?;
    let mut writer = vm.writer(addr, data.len()).map_err(|_| Error::Fault)?;
    let mut reader = VmReader::from(data);
    match writer.write_fallible(&mut reader) {
        Ok(n) if n == data.len() => Ok(()),
        _ => Err(Error::Fault),
    }
}
