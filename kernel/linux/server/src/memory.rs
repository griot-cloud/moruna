//! A task's memory, as the server reads and writes it: through `process_read` and
//! `process_write` on the task's process handle, never by mapping it (SDD 28, f.1). A
//! bad address is `EFAULT`, as Linux answers it.

use alloc::vec::Vec;

use moruna_abi::limits::PROCESS_COPY_MAX;
use moruna_linux::types::errno;
use moruna_svc::Process;

const PAGE: u64 = 4096;

/// `len` bytes at `addr`.
pub fn read(p: &Process, addr: u64, len: usize) -> Result<Vec<u8>, u64> {
    let mut out = alloc::vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(PROCESS_COPY_MAX);
        p.read(addr + done as u64, &mut out[done..done + n])
            .map_err(|_| errno::EFAULT)?;
        done += n;
    }
    Ok(out)
}

/// Writes `data` at `addr`.
pub fn write(p: &Process, addr: u64, data: &[u8]) -> Result<(), u64> {
    for (i, chunk) in data.chunks(PROCESS_COPY_MAX).enumerate() {
        p.write(addr + (i * PROCESS_COPY_MAX) as u64, chunk)
            .map_err(|_| errno::EFAULT)?;
    }
    Ok(())
}

pub fn read_u64(p: &Process, addr: u64) -> Result<u64, u64> {
    let b = read(p, addr, 8)?;
    let mut w = [0u8; 8];
    w.copy_from_slice(&b);
    Ok(u64::from_le_bytes(w))
}

pub fn write_u32(p: &Process, addr: u64, v: u32) -> Result<(), u64> {
    write(p, addr, &v.to_le_bytes())
}

/// A NUL-terminated string at `addr`, of at most `max` bytes before its NUL
/// (`ENAMETOOLONG` past it). Read a page at a time, so a string that ends just before
/// an unmapped page is read.
pub fn string(p: &Process, addr: u64, max: usize) -> Result<Vec<u8>, u64> {
    if addr == 0 {
        return Err(errno::EFAULT);
    }
    let mut out = Vec::new();
    let mut at = addr;
    loop {
        let room = (PAGE - at % PAGE) as usize;
        let chunk = read(p, at, room.min(256))?;
        if let Some(end) = chunk.iter().position(|b| *b == 0) {
            out.extend_from_slice(&chunk[..end]);
            return Ok(out);
        }
        out.extend_from_slice(&chunk);
        if out.len() > max {
            return Err(errno::ENAMETOOLONG);
        }
        at += chunk.len() as u64;
    }
}

/// A null-terminated array of string pointers at `addr` (`argv`, `envp`): at most
/// `count` strings and `total` bytes (`E2BIG` past either).
pub fn strings(p: &Process, addr: u64, count: usize, total: usize) -> Result<Vec<Vec<u8>>, u64> {
    let mut out = Vec::new();
    if addr == 0 {
        return Ok(out);
    }
    let mut size = 0;
    for i in 0.. {
        let ptr = read_u64(p, addr + 8 * i as u64)?;
        if ptr == 0 {
            break;
        }
        if out.len() >= count {
            return Err(errno::E2BIG);
        }
        let s = string(p, ptr, total)?;
        size += s.len() + 1;
        if size > total {
            return Err(errno::E2BIG);
        }
        out.push(s);
    }
    Ok(out)
}
