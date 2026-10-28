//! The `syscall` instruction, and one safe function per call that passes memory.
//!
//! Every call that reads or writes the caller's memory takes a slice here, so the
//! kernel only ever sees pointers Rust vouches for. Calls that take raw handle values
//! are safe whatever the value is: the kernel answers `BadHandle` for one that names
//! nothing in this process. The calls that could break this process's own memory
//! (unmapping, starting a thread at an address) stay private to the crate.

use core::arch::asm;

use moruna_abi::{RawHandle, nr, wait_item};

use crate::WaitItem;

/// Issues syscall `n`.
///
/// # Safety
///
/// Each argument the call reads as a pointer must name memory the call may read or
/// write for the length the call is given, per SDD 20 d.2.
#[inline(always)]
unsafe fn syscall(n: u64, a: [u64; 6]) -> (u64, u64) {
    let rax: u64;
    let rdx: u64;
    // SAFETY: the `syscall` instruction enters the kernel, which preserves every
    // register but rax, rdx (results), rcx and r11 (clobbered by the instruction).
    // Memory safety of the arguments is the caller's obligation, stated above.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => rax,
            in("rdi") a[0],
            in("rsi") a[1],
            inlateout("rdx") a[2] => rdx,
            in("r10") a[3],
            in("r8") a[4],
            in("r9") a[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    (rax, rdx)
}

/// Calls that take no pointer.
fn plain(n: u64, a: [u64; 6]) -> (u64, u64) {
    debug_assert!(!matches!(
        n,
        nr::CHANNEL_WRITE
            | nr::CHANNEL_READ
            | nr::PROCESS_CREATE
            | nr::PROCESS_START
            | nr::WAIT
            | nr::DEBUG_WRITE
    ));
    // SAFETY: only calls that read no memory through their arguments reach here: the
    // crate's own wrappers call `plain` with those numbers and no other.
    unsafe { syscall(n, a) }
}

pub(crate) fn call0(n: u64) -> (u64, u64) {
    plain(n, [0; 6])
}

pub(crate) fn call1(n: u64, a: u64) -> (u64, u64) {
    plain(n, [a, 0, 0, 0, 0, 0])
}

pub(crate) fn call2(n: u64, a: u64, b: u64) -> (u64, u64) {
    plain(n, [a, b, 0, 0, 0, 0])
}

pub(crate) fn call3(n: u64, a: u64, b: u64, c: u64) -> (u64, u64) {
    plain(n, [a, b, c, 0, 0, 0])
}

pub(crate) fn call4(n: u64, a: u64, b: u64, c: u64, d: u64) -> (u64, u64) {
    plain(n, [a, b, c, d, 0, 0])
}

pub fn handle_close(v: RawHandle) -> u64 {
    call1(nr::HANDLE_CLOSE, v as u64).0
}

pub fn handle_duplicate(v: RawHandle, rights: u32) -> u64 {
    call2(nr::HANDLE_DUPLICATE, v as u64, rights as u64).0
}

pub fn handle_info(v: RawHandle) -> u64 {
    call1(nr::HANDLE_INFO, v as u64).0
}

pub fn channel_write(v: RawHandle, bytes: &[u8], handles: &[RawHandle]) -> u64 {
    let a = [
        v as u64,
        bytes.as_ptr() as u64,
        bytes.len() as u64,
        handles.as_ptr() as u64,
        handles.len() as u64,
        0,
    ];
    // SAFETY: the kernel reads `bytes.len()` bytes and `handles.len()` values from the
    // two slices, which are valid for those reads.
    unsafe { syscall(nr::CHANNEL_WRITE, a) }.0
}

pub fn channel_read(v: RawHandle, bytes: &mut [u8], handles: &mut [RawHandle]) -> (u64, u64) {
    let a = [
        v as u64,
        bytes.as_mut_ptr() as u64,
        bytes.len() as u64,
        handles.as_mut_ptr() as u64,
        handles.len() as u64,
        0,
    ];
    // SAFETY: the kernel writes at most `bytes.len()` bytes and `handles.len()` values
    // into the two slices, which are valid for those writes.
    unsafe { syscall(nr::CHANNEL_READ, a) }
}

/// `memory_map` on a raw value. Safe: it adds a mapping at an address the kernel picks
/// and never touches an existing one. The mapping is not unmapped by this function.
pub fn memory_map(v: RawHandle, offset: u64, len: u64, flags: u64) -> u64 {
    call4(nr::MEMORY_MAP, v as u64, offset, len, flags).0
}

pub fn process_create(resource: RawHandle, name: &[u8]) -> u64 {
    let a = [
        resource as u64,
        name.as_ptr() as u64,
        name.len() as u64,
        0,
        0,
        0,
    ];
    // SAFETY: the kernel reads `name.len()` bytes from `name`.
    unsafe { syscall(nr::PROCESS_CREATE, a) }.0
}

pub fn process_map_image(process: RawHandle, image: RawHandle) -> u64 {
    call2(nr::PROCESS_MAP_IMAGE, process as u64, image as u64).0
}

/// `entries` is a whole number of 40-byte start entries (SDD 20 d.3).
pub fn process_start(process: RawHandle, entry: u64, entries: &[u8]) -> u64 {
    let count = (entries.len() / moruna_abi::start_entry::SIZE) as u64;
    let a = [process as u64, entry, entries.as_ptr() as u64, count, 0, 0];
    // SAFETY: the kernel reads `count * 40` bytes, no more than `entries.len()`.
    unsafe { syscall(nr::PROCESS_START, a) }.0
}

pub fn wait(items: &mut [WaitItem], deadline: u64) -> u64 {
    let a = [
        items.as_mut_ptr() as u64,
        items.len() as u64,
        deadline,
        0,
        0,
        0,
    ];
    const { assert!(core::mem::size_of::<WaitItem>() == wait_item::SIZE) };
    // SAFETY: the kernel reads and writes `items.len()` 16-byte items, the layout of
    // `WaitItem`, in the slice.
    unsafe { syscall(nr::WAIT, a) }.0
}

pub fn timer_set(v: RawHandle, deadline: u64) -> u64 {
    call2(nr::TIMER_SET, v as u64, deadline).0
}

pub fn debug_write(console: RawHandle, bytes: &[u8]) -> u64 {
    let a = [
        console as u64,
        bytes.as_ptr() as u64,
        bytes.len() as u64,
        0,
        0,
        0,
    ];
    // SAFETY: the kernel reads `bytes.len()` bytes from `bytes`.
    unsafe { syscall(nr::DEBUG_WRITE, a) }.0
}

pub fn interrupt_create(resource: RawHandle, gsi: u64) -> u64 {
    call2(nr::INTERRUPT_CREATE, resource as u64, gsi).0
}

pub fn interrupt_ack(v: RawHandle) -> u64 {
    call1(nr::INTERRUPT_ACK, v as u64).0
}

pub fn device_memory_create(resource: RawHandle, paddr: u64, len: u64) -> u64 {
    call3(nr::DEVICE_MEMORY_CREATE, resource as u64, paddr, len).0
}

/// Reads one byte of a live mapping.
pub(crate) fn peek(addr: u64) -> u8 {
    // SAFETY: callers pass an address inside a `Mapping` they hold, which the kernel
    // keeps mapped readable until the `Mapping` is dropped. Volatile, because another
    // process may write the same page.
    unsafe { core::ptr::read_volatile(addr as *const u8) }
}

/// Writes one byte of a live, writable mapping.
pub(crate) fn poke(addr: u64, value: u8) {
    // SAFETY: callers pass an address inside a writable `Mapping` they hold. Volatile,
    // because another process may read the same page.
    unsafe { core::ptr::write_volatile(addr as *mut u8, value) }
}
