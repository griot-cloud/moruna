//! The user-space personality's mechanisms (ABI 2.6; `kernel-is-the-machine.md` §4.1,
//! `architecture/data-os.md` §4): stream B1's module.
//!
//! The call numbers (`nr::PERSONALITY_SET` to `nr::THREAD_INTERRUPT`) and the entry frame's
//! layout (`moruna_abi::personality`) are frozen; the arguments are these wrappers'. Stream B2
//! (the port of Asterinas's Linux layer) builds against them. Only stream B1 writes this file.
//!
//! **The entry (§4.1 a).** A process given a personality region with [`personality_set`]
//! before it starts has two parts: the region, which holds the personality's code and
//! state, and everything below and above it, which holds the program. A thread starts in
//! the personality. It hands its CPU to the program with [`personality_return`], which
//! loads the program's registers from a [`Frame`] inside the region and runs the program
//! with the region out of reach. A system call, an exception the core does not serve, or
//! a [`thread_interrupt`] in the program brings the same thread back: `personality_return`
//! returns the reason, with the frame holding the program's registers as they were. Every
//! Linux call of the program comes back. A native call of the program goes to the core when
//! it is on the core's list (`run_granted`'s calls) and is refused otherwise; it resolves
//! only the program's own handles, which [`program_handle`] moves there, places what the
//! core maps in the range `personality_set` reserved, and cannot touch the region, so the
//! program never holds the personality's handles. The kernel switches the general registers and the FS base;
//! the vector and x87 registers stay live across the switch, and the personality saves the
//! program's before it uses its own (as Zircon's restricted mode asks of Starnix).
//!
//! **Snapshots and clones (§4.1 b).** [`memory_snapshot`] makes an object that reads its
//! source's pages until it writes them; [`process_clone`] makes a created process whose
//! address space is a copy-on-write clone of another's, for `fork`.
//!
//! **The pager (§4.1 c).** [`pager_create`] makes an object whose pages a server supplies:
//! a touch of a page it does not hold sends a [`pager_request`] on the server's channel,
//! and the thread waits until [`pager_supply`] gives the page or [`pager_refuse`] refuses
//! it; a refusal is a fault of the touching thread. `process_read` and `process_write` wait
//! for the pager the same way, and answer `Fault` on a refusal.
//!
//! **Thread state and interruption (§4.1 d).** [`thread_state_read`] and
//! [`thread_state_write`] reach the registers of a thread asleep in the core, and
//! [`thread_interrupt`] wakes such a thread: its wait returns `Interrupted`, the way a Linux
//! call returns `EINTR`; a thread running the program enters the personality instead.

use moruna_abi::{nr, personality, read_u64, write_u64};

use crate::{Channel, Error, Handle, Memory, Process, RawHandle, Result, Thread, check, raw};

/// The frame's length in bytes.
pub const FRAME_LEN: usize = personality::LEN;

/// The largest object [`pager_create`] makes: its pages exist only once supplied, so it is
/// bounded by addresses, not by memory (`limits::MEMORY_MAX_BYTES` bounds the others).
pub const PAGER_MAX_BYTES: u64 = 1 << 40;

/// The message the kernel sends on a pager's channel for each page a thread touched that
/// the object does not hold: [`pager_request::SIZE`] bytes, little endian, no handles.
pub mod pager_request {
    /// `u32` the message's kind, [`FAULT`].
    pub const KIND: usize = 0;
    /// `u32` reserved, zero.
    pub const RESERVED: usize = 4;
    /// `u64` the key the object was made with.
    pub const KEY: usize = 8;
    /// `u64` the offset of the page wanted, a multiple of the page size.
    pub const OFFSET: usize = 16;
    /// `u64` the bytes wanted from `OFFSET`: one page.
    pub const LEN: usize = 24;
    pub const SIZE: usize = 32;
    /// A page is wanted.
    pub const FAULT: u32 = 1;
}

/// The program's registers in `moruna_abi::personality`'s layout, as the personality reads
/// and writes them between entries. Eight-byte aligned, as the kernel requires.
#[derive(Clone, Debug, PartialEq, Eq)]
#[repr(C, align(8))]
pub struct Frame(pub [u8; FRAME_LEN]);

impl Default for Frame {
    fn default() -> Frame {
        Frame([0; FRAME_LEN])
    }
}

/// A general register's index in the frame, in `personality::REGISTER_NAMES` order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Register {
    Rax = 0,
    Rbx,
    Rcx,
    Rdx,
    Rsi,
    Rdi,
    Rbp,
    Rsp,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
}

impl Frame {
    fn get(&self, at: usize) -> u64 {
        read_u64(&self.0, at).unwrap_or(0)
    }

    fn set(&mut self, at: usize, value: u64) {
        write_u64(&mut self.0, at, value);
    }

    /// Why the program entered: one of `personality::reason`.
    pub fn reason(&self) -> u64 {
        self.get(personality::REASON)
    }

    pub fn register(&self, r: Register) -> u64 {
        self.get(personality::REGISTERS + 8 * r as usize)
    }

    pub fn set_register(&mut self, r: Register, value: u64) {
        self.set(personality::REGISTERS + 8 * r as usize, value);
    }

    pub fn rip(&self) -> u64 {
        self.get(personality::RIP)
    }

    pub fn set_rip(&mut self, value: u64) {
        self.set(personality::RIP, value);
    }

    pub fn rflags(&self) -> u64 {
        self.get(personality::RFLAGS)
    }

    /// Only the flags a program may change are taken; the rest keep the kernel's values.
    pub fn set_rflags(&mut self, value: u64) {
        self.set(personality::RFLAGS, value);
    }

    pub fn fs_base(&self) -> u64 {
        self.get(personality::FS_BASE)
    }

    pub fn set_fs_base(&mut self, value: u64) {
        self.set(personality::FS_BASE, value);
    }

    pub fn gs_base(&self) -> u64 {
        self.get(personality::GS_BASE)
    }

    /// The exception's vector, for `reason::EXCEPTION` and `reason::PAGE_FAULT`.
    pub fn vector(&self) -> u64 {
        self.get(personality::VECTOR)
    }

    /// The exception's error code.
    pub fn error_code(&self) -> u64 {
        self.get(personality::ERROR_CODE)
    }

    /// The faulting address, for `reason::PAGE_FAULT`.
    pub fn fault_address(&self) -> u64 {
        self.get(personality::FAULT_ADDRESS)
    }
}

/// Gives the created process `process` (a handle with `WRITE`) its personality region:
/// `len` bytes from `base`, whole pages inside user space. Its program can never reach the
/// region. `reserved` (whole pages apart from the region, or empty) is where the core
/// places what its program's native calls map: `memory_map`, `batch_get`, the plane's
/// windows; with it empty those calls find no room (`Limit`). Once per process, before it
/// starts; a process with a forward is refused (`BadState`). Its threads start in the
/// personality.
pub fn personality_set(
    process: &Process,
    base: u64,
    len: u64,
    reserved: core::ops::Range<u64>,
) -> Result<()> {
    set(process, base, len, reserved, 0)
}

fn set(
    process: &Process,
    base: u64,
    len: u64,
    reserved: core::ops::Range<u64>,
    copy: u64,
) -> Result<()> {
    let reserved_len = reserved.end.saturating_sub(reserved.start);
    let a = [
        process.0.raw() as u64,
        base,
        len,
        reserved.start,
        reserved_len,
        copy,
    ];
    check(raw::call6(nr::PERSONALITY_SET, a).0).map(|_| ())
}

/// [`personality_set`], with the personality's fault-tolerant copy routine (B2's `VmReader`
/// and `VmWriter`): a page fault or general protection fault at an instruction of `routine`,
/// while the thread is in the personality, resumes it at `recovery` with the faulting address
/// in `rdx` (zero for a general protection fault) and every other register as it was, where it
/// would otherwise end the process. `routine` and `recovery` lie inside the region.
pub fn personality_set_with_copy(
    process: &Process,
    base: u64,
    len: u64,
    reserved: core::ops::Range<u64>,
    routine: core::ops::Range<u64>,
    recovery: u64,
) -> Result<()> {
    let mut record = [0u8; personality::copy::LEN];
    write_u64(&mut record, personality::copy::START, routine.start);
    write_u64(&mut record, personality::copy::END, routine.end);
    write_u64(&mut record, personality::copy::RECOVERY, recovery);
    // The kernel reads the record's `copy::LEN` bytes during the call.
    set(process, base, len, reserved, record.as_ptr() as u64)
}

/// Moves `handle` from the calling process's table into its program's, where the native
/// calls its program makes resolve; returns its value there, which the personality hands
/// the program. From the personality only. On failure the handle stays in the caller's
/// table and comes back with the error.
pub fn program_handle(handle: Handle) -> core::result::Result<RawHandle, (Error, Handle)> {
    match check(raw::call1(nr::PROGRAM_HANDLE, handle.raw() as u64).0) {
        Ok(v) => {
            core::mem::forget(handle);
            Ok(v as RawHandle)
        }
        Err(e) => Err((e, handle)),
    }
}

/// Runs the program with the registers in `frame`, which must lie inside the calling
/// process's personality region, from the calling thread, which must be in the personality.
/// Returns when the program next enters the personality: the reason (`personality::reason`),
/// with `frame` holding the program's registers then. An interrupt pending on the thread
/// returns `reason::INTERRUPTED` at once, the program not run and `frame` as it was.
/// `BadState` in a process with no personality, `InvalidArgs` for a frame outside the
/// region or registers no program may hold (a non-canonical instruction pointer, stack
/// pointer or FS base).
pub fn personality_return(frame: &mut Frame) -> Result<u64> {
    let at = frame.0.as_mut_ptr() as u64;
    // The kernel reads and writes the frame's `FRAME_LEN` bytes and nothing else, while
    // `frame` is borrowed by this call.
    check(raw::call1(nr::PERSONALITY_RETURN, at).0)
}

/// A snapshot of `len` bytes of `memory` (a handle with `READ`) from `offset`, both whole
/// pages, charged to `account` (the caller's own when `None`): an object that reads the
/// source's pages until it writes them, each write copying the page and charging the copy
/// to the snapshot's account. Writes to the source may show in pages the snapshot has not
/// written, as a Linux `MAP_PRIVATE` mapping of a file allows; [`process_clone`] gives a
/// process's private mappings a full copy-on-write snapshot. A snapshot maps into one
/// process: mapping it into a second, while the first lives, is refused with `BadState`, so a
/// clone reaches every mapping that could write a page it shares.
pub fn memory_snapshot(
    memory: &Memory,
    offset: u64,
    len: u64,
    account: Option<&Handle>,
) -> Result<Memory> {
    let account = account.map_or(0, |a| a.raw() as u64);
    let v = check(
        raw::call4(
            nr::MEMORY_SNAPSHOT,
            memory.0.raw() as u64,
            offset,
            len,
            account,
        )
        .0,
    )?;
    Ok(Memory(Handle::from_raw(v as RawHandle)))
}

/// A created process, named as `process` is (the caller itself when `None`, else a handle
/// with `READ` and `WRITE`) and charged to `account` (the caller's own when `None`), whose
/// address space is a copy-on-write clone of `process`'s, from the process resource
/// `resource` (with `WRITE`), for `fork`. With `moruna_abi::process::SHARE_HANDLES` in
/// `flags`, the clone uses `process`'s own handle table (Linux's `CLONE_FILES`): only a
/// caller with a personality that holds that same table may ask (`AccessDenied`
/// otherwise), and ending the task (`Process::kill` of any of its processes, or the last of
/// them exiting) frees the table and every process in it. Without it, the clone starts with
/// no handles. Any other bit is `InvalidArgs`. Every private page (zero pages, and the pages of an object mapped through a
/// snapshot) is shared until either side writes it, which copies it; every other mapping
/// of a memory object is shared; the data plane's and device mappings are left out. It has
/// no threads, and keeps `process`'s personality region: the caller starts
/// it with [`Process::launch`], its first thread in the personality. Two mappings of one
/// private object stay one object in the clone. The pages it shares stay charged where they
/// were, so it needs room only for its records (64 bytes a page) and its own tables.
pub fn process_clone(
    resource: &Handle,
    process: Option<&Process>,
    account: Option<&Handle>,
    flags: u64,
) -> Result<Process> {
    let account = account.map_or(0, |a| a.raw() as u64);
    let process = process.map_or(0, |p| p.0.raw() as u64);
    let v = check(
        raw::call4(
            nr::PROCESS_CLONE,
            resource.raw() as u64,
            process,
            account,
            flags,
        )
        .0,
    )?;
    Ok(Process(Handle::from_raw(v as RawHandle)))
}

/// An object of `size` bytes (whole pages, at most [`PAGER_MAX_BYTES`]) whose pages the
/// holder of `port`'s peer supplies, charged to `account` (the caller's own when `None`)
/// as they are supplied. `port` (with `READ`, `WRITE` and `TRANSFER`) moves into the
/// kernel, which sends a [`pager_request`] carrying `key` on it for each page a thread
/// touches that the object does not hold; on failure it comes back with the error.
pub fn pager_create(
    port: Channel,
    size: u64,
    key: u64,
    account: Option<&Handle>,
) -> core::result::Result<Memory, (Error, Channel)> {
    let account = account.map_or(0, |a| a.raw() as u64);
    let r = raw::call4(nr::PAGER_CREATE, port.0.raw() as u64, size, key, account).0;
    match check(r) {
        Ok(v) => {
            core::mem::forget(port);
            Ok(Memory(Handle::from_raw(v as RawHandle)))
        }
        Err(e) => Err((e, port)),
    }
}

/// Supplies `len` bytes of the pager's object `memory` (a handle with `WRITE`) from
/// `offset`, both whole pages, with the pages of `source` (a memory object made with
/// `memory_create`, with `READ`) from `source_offset`: the object takes those pages, which
/// the pager should not write again. A page the object already holds keeps its own. Every
/// thread waiting on a page supplied runs on.
pub fn pager_supply(
    memory: &Memory,
    offset: u64,
    len: u64,
    source: &Memory,
    source_offset: u64,
) -> Result<()> {
    let a = [
        memory.0.raw() as u64,
        offset,
        len,
        source.0.raw() as u64,
        source_offset,
        0,
    ];
    check(raw::call6(nr::PAGER_SUPPLY, a).0).map(|_| ())
}

/// Refuses `len` bytes of the pager's object `memory` (a handle with `WRITE`) from `offset`:
/// every thread waiting on a page there faults (a program's enters its personality with
/// `reason::PAGE_FAULT`). A later touch asks again.
pub fn pager_refuse(memory: &Memory, offset: u64, len: u64) -> Result<()> {
    let a = [memory.0.raw() as u64, offset, len, 0, 0, 0];
    check(raw::call6(nr::PAGER_SUPPLY, a).0).map(|_| ())
}

/// Reads the registers of `thread` (a handle with `READ`), asleep in the core, into `frame`
/// in `personality`'s layout: as it will resume, `rax` aside, which will carry its call's
/// result. `BadState` for a thread not asleep in the core.
pub fn thread_state_read(thread: &Thread, frame: &mut [u8; FRAME_LEN]) -> Result<()> {
    check(raw::thread_state_read(thread.0.raw(), frame)).map(|_| ())
}

/// Writes the registers `thread` (a handle with `WRITE`), asleep in the core, resumes with
/// from `frame`: all but `rax`, which carries its call's result, and the frame's reason,
/// vector, error code and fault address, which are only read. `BadState` for a thread not
/// asleep in the core, `InvalidArgs` for registers no thread may hold.
pub fn thread_state_write(thread: &Thread, frame: &[u8; FRAME_LEN]) -> Result<()> {
    check(raw::thread_state_write(thread.0.raw(), frame)).map(|_| ())
}

/// Interrupts `thread` (a handle with `WRITE`): its wait in the core returns
/// `Error::Interrupted`; running the program, it enters its personality with
/// `reason::INTERRUPTED`; otherwise the interrupt waits for its next wait or
/// `personality_return`, which returns at once.
pub fn thread_interrupt(thread: &Thread) -> Result<()> {
    check(raw::call1(nr::THREAD_INTERRUPT, thread.0.raw() as u64).0).map(|_| ())
}

/// The bytes of the hole holding `addr` at the deepest page table this process has there
/// (`plane_probe`'s `TABLE`; a kernel built with its `probe` feature): 4096 where a 4 KiB table
/// holds it, 2 MiB where only the table above does, and so on; zero where a page is mapped.
pub fn table_gap(addr: u64) -> Result<u64> {
    check(raw::call2(nr::PLANE_PROBE, moruna_abi::plane::probe::TABLE, addr).0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_fields_land_at_their_offsets() {
        let mut f = Frame::default();
        f.set_register(Register::Rax, 1);
        f.set_register(Register::R15, 15);
        f.set_rip(0x40_1000);
        f.set_fs_base(0x7000);
        assert_eq!(read_u64(&f.0, personality::REGISTERS), Some(1));
        assert_eq!(read_u64(&f.0, personality::REGISTERS + 15 * 8), Some(15));
        assert_eq!(f.register(Register::R15), 15);
        assert_eq!(f.rip(), 0x40_1000);
        assert_eq!(f.fs_base(), 0x7000);
        assert_eq!(f.reason(), 0);
        assert_eq!(core::mem::align_of::<Frame>(), 8);
        assert_eq!(core::mem::size_of::<Frame>(), FRAME_LEN);
        assert_eq!(personality::REGISTER_NAMES[Register::Rdi as usize], "rdi");
    }

    #[test]
    fn every_frame_field_reads_back() {
        let mut f = Frame::default();
        let all = [
            Register::Rax,
            Register::Rbx,
            Register::Rcx,
            Register::Rdx,
            Register::Rsi,
            Register::Rdi,
            Register::Rbp,
            Register::Rsp,
            Register::R8,
            Register::R9,
            Register::R10,
            Register::R11,
            Register::R12,
            Register::R13,
            Register::R14,
            Register::R15,
        ];
        for (i, r) in all.iter().enumerate() {
            f.set_register(*r, 0x100 + i as u64);
            assert!(personality::REGISTER_NAMES[*r as usize].len() >= 2);
        }
        for (i, r) in all.iter().enumerate() {
            assert_eq!(f.register(*r), 0x100 + i as u64);
        }
        f.set_rflags(0x246);
        assert_eq!(f.rflags(), 0x246);
        let fields = [
            (personality::REASON, 4),
            (personality::GS_BASE, 5),
            (personality::VECTOR, 14),
            (personality::ERROR_CODE, 6),
            (personality::FAULT_ADDRESS, 0xdead_0000),
        ];
        for (at, v) in fields {
            assert!(write_u64(&mut f.0, at, v));
        }
        assert_eq!(f.reason(), personality::reason::INTERRUPTED);
        assert_eq!(f.gs_base(), 5);
        assert_eq!(f.vector(), 14);
        assert_eq!(f.error_code(), 6);
        assert_eq!(f.fault_address(), 0xdead_0000);
        assert_eq!(f.clone(), f);
    }

    #[test]
    fn pager_request_layout() {
        let mut m = [0u8; pager_request::SIZE];
        assert!(moruna_abi::write_u32(
            &mut m,
            pager_request::KIND,
            pager_request::FAULT
        ));
        assert!(write_u64(&mut m, pager_request::KEY, 7));
        assert!(write_u64(&mut m, pager_request::OFFSET, 4096));
        assert!(write_u64(&mut m, pager_request::LEN, 4096));
        assert_eq!(read_u64(&m, pager_request::KEY), Some(7));
        assert_eq!(read_u64(&m, pager_request::LEN), Some(4096));
        assert_eq!(moruna_abi::read_u32(&m, pager_request::RESERVED), Some(0));
    }
}
