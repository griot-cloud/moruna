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
            | nr::MEMORY_PIN
            | nr::RESOURCE_INFO
            | nr::PROCESS_FORWARD
            | nr::PROCESS_READ
            | nr::PROCESS_WRITE
            | nr::FUTEX_WAIT
            | nr::FUTEX_WAKE
            | nr::RANDOM_READ
            | nr::SLOT_TAKE
            | nr::SLOT_REFUSE
            | nr::PROCESS_PLANE
            | nr::PROCESS_GRANT
            | nr::PORT_CALL
            | nr::ACCOUNT_CREATE
            | nr::ACCOUNT_INFO
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

pub(crate) fn call6(n: u64, a: [u64; 6]) -> (u64, u64) {
    plain(n, a)
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

/// `process_create` (ABI 2.3): the process charged to `account`, zero for the caller's
/// own account.
pub fn process_create(resource: RawHandle, name: &[u8], account: RawHandle) -> u64 {
    let a = [
        resource as u64,
        name.as_ptr() as u64,
        name.len() as u64,
        account as u64,
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

/// `entries` is a whole number of 40-byte start entries, as `process_start`'s (ABI 2.2).
pub fn process_grant(process: RawHandle, entries: &[u8]) -> u64 {
    let count = (entries.len() / moruna_abi::start_entry::SIZE) as u64;
    let a = [process as u64, entries.as_ptr() as u64, count, 0, 0, 0];
    // SAFETY: the kernel reads `count * 40` bytes, no more than `entries.len()`.
    unsafe { syscall(nr::PROCESS_GRANT, a) }.0
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

pub fn resource_narrow(resource: RawHandle, base: u64, len: u64) -> u64 {
    call3(nr::RESOURCE_NARROW, resource as u64, base, len).0
}

/// `memory_create` with its ABI 1.2 flags (`moruna_abi::memory_flags`), charged to
/// `account` (ABI 2.3), zero for the caller's own account.
pub fn memory_create(size: u64, flags: u64, account: RawHandle) -> u64 {
    call3(nr::MEMORY_CREATE, size, flags, account as u64).0
}

/// `account_create` (ABI 2.3): an account named `name` of `limit` bytes carved out of
/// `parent`, zero for the caller's own account.
pub fn account_create(parent: RawHandle, limit: u64, name: &[u8]) -> u64 {
    let a = [
        parent as u64,
        limit,
        name.as_ptr() as u64,
        name.len() as u64,
        0,
        0,
    ];
    // SAFETY: the kernel reads `name.len()` bytes from `name`.
    unsafe { syscall(nr::ACCOUNT_CREATE, a) }.0
}

/// ABI 2.5: exact Account/Memory charged-owner comparison, no user pointers.
pub fn account_matches(account: RawHandle, object: RawHandle) -> u64 {
    call2(nr::ACCOUNT_MATCHES, account as u64, object as u64).0
}

/// `account_info` (ABI 2.3) of `account`, zero for the caller's own.
pub fn account_info(account: RawHandle, out: &mut [u8; moruna_abi::account_info::SIZE]) -> u64 {
    let a = [account as u64, out.as_mut_ptr() as u64, 0, 0, 0, 0];
    // SAFETY: the kernel writes `account_info::SIZE` bytes to `out`, which holds them.
    unsafe { syscall(nr::ACCOUNT_INFO, a) }.0
}

/// `interrupt_create_msi`: the interrupt in `rax`, the message in `rdx`
/// (`moruna_abi::msi`).
pub fn interrupt_create_msi(resource: RawHandle, slot: u64) -> (u64, u64) {
    call2(nr::INTERRUPT_CREATE_MSI, resource as u64, slot)
}

/// `memory_pin`: the pin in `rax`, the page count in `rdx`; the device address of
/// each page lands in `addrs`.
pub fn memory_pin(dma: RawHandle, memory: RawHandle, flags: u64, addrs: &mut [u64]) -> (u64, u64) {
    let a = [
        dma as u64,
        memory as u64,
        flags,
        addrs.as_mut_ptr() as u64,
        addrs.len() as u64,
        0,
    ];
    // SAFETY: the kernel writes at most `addrs.len()` 8-byte values into the slice.
    unsafe { syscall(nr::MEMORY_PIN, a) }
}

pub fn dma_release(dma: RawHandle) -> u64 {
    call1(nr::DMA_RELEASE, dma as u64).0
}

/// `resource_info`: the kind and range land in `out` (`moruna_abi::resource_info`).
pub fn resource_info(resource: RawHandle, out: &mut [u8; moruna_abi::resource_info::SIZE]) -> u64 {
    let a = [resource as u64, out.as_mut_ptr() as u64, 0, 0, 0, 0];
    // SAFETY: the kernel writes exactly `resource_info::SIZE` bytes into `out`.
    unsafe { syscall(nr::RESOURCE_INFO, a) }.0
}

pub fn process_kill(process: RawHandle) -> u64 {
    call1(nr::PROCESS_KILL, process as u64).0
}

/// `process_forward` (ABI 2.0): the table is `moruna_abi::forward::table::SIZE` bytes.
pub fn process_forward(process: RawHandle, endpoint: RawHandle, table: &[u8]) -> u64 {
    let a = [
        process as u64,
        endpoint as u64,
        table.as_ptr() as u64,
        table.len() as u64,
        0,
        0,
    ];
    // SAFETY: the kernel reads `table.len()` bytes from `table`.
    unsafe { syscall(nr::PROCESS_FORWARD, a) }.0
}

/// `process_launch` (ABI 2.0).
pub fn process_launch(process: RawHandle, entry: u64, stack: u64, brk: u64) -> u64 {
    call4(nr::PROCESS_LAUNCH, process as u64, entry, stack, brk).0
}

/// `process_map` (ABI 2.0), of a memory object or, with `memory` zero, zero pages.
pub fn process_map(
    process: RawHandle,
    memory: RawHandle,
    offset: u64,
    len: u64,
    addr: u64,
    flags: u64,
) -> u64 {
    call6(
        nr::PROCESS_MAP,
        [process as u64, memory as u64, offset, len, addr, flags],
    )
    .0
}

/// `process_unmap` (ABI 2.0).
pub fn process_unmap(process: RawHandle, addr: u64, len: u64) -> u64 {
    call3(nr::PROCESS_UNMAP, process as u64, addr, len).0
}

/// `process_protect` (ABI 2.0).
pub fn process_protect(process: RawHandle, addr: u64, len: u64, flags: u64) -> u64 {
    call4(nr::PROCESS_PROTECT, process as u64, addr, len, flags).0
}

/// `forward_clone` (ABI 2.0).
pub fn forward_clone(
    process: RawHandle,
    tid: u64,
    stack: u64,
    tls: u64,
    clear: u64,
    set: u64,
) -> u64 {
    call6(
        nr::FORWARD_CLONE,
        [process as u64, tid, stack, tls, clear, set],
    )
    .0
}

/// `process_read` (ABI 2.0): bytes of another process's memory into `buf`.
pub fn process_read(process: RawHandle, addr: u64, buf: &mut [u8]) -> u64 {
    let a = [
        process as u64,
        addr,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
    ];
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
    unsafe { syscall(nr::PROCESS_READ, a) }.0
}

/// `process_write` (ABI 2.0): `data` into another process's memory.
pub fn process_write(process: RawHandle, addr: u64, data: &[u8]) -> u64 {
    let a = [
        process as u64,
        addr,
        data.as_ptr() as u64,
        data.len() as u64,
        0,
        0,
    ];
    // SAFETY: the kernel reads `data.len()` bytes from `data`.
    unsafe { syscall(nr::PROCESS_WRITE, a) }.0
}

/// `futex_wait` (ABI 2.0) on a word this process owns.
pub fn futex_wait(word: &core::sync::atomic::AtomicU32, expected: u32, deadline: u64) -> u64 {
    let a = [word.as_ptr() as u64, expected as u64, deadline, 0, 0, 0];
    // SAFETY: the kernel reads the four bytes of `word`, which the reference keeps
    // alive for the call.
    unsafe { syscall(nr::FUTEX_WAIT, a) }.0
}

/// `futex_wake` (ABI 2.0) on a word this process owns.
pub fn futex_wake(word: &core::sync::atomic::AtomicU32, count: u32) -> u64 {
    let a = [word.as_ptr() as u64, count as u64, 0, 0, 0, 0];
    // SAFETY: the kernel only uses the word's address as a key; it reads nothing.
    unsafe { syscall(nr::FUTEX_WAKE, a) }.0
}

/// `random_read` (ABI 2.0): fills `buf` from the CPU's random source.
pub fn random_read(buf: &mut [u8]) -> u64 {
    let a = [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0];
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
    unsafe { syscall(nr::RANDOM_READ, a) }.0
}

/// `batch_get` (ABI 2.1): an address and a length, or an error in `rax`. Safe: it adds a
/// mapping where the kernel chooses, and unmaps only the slot's previous batch, which
/// the plane's `Batch` borrows keep out of reach.
pub fn batch_get(slot: u64) -> (u64, u64) {
    call1(nr::BATCH_GET, slot)
}

/// `batch_put` (ABI 2.1): unmaps only the slot's window.
pub fn batch_put(slot: u64, addr: u64, len: u64) -> u64 {
    call3(nr::BATCH_PUT, slot, addr, len).0
}

/// `plane_get` (ABI 2.1).
pub fn plane_get(process: RawHandle, slot: u64) -> (u64, u64) {
    call2(nr::PLANE_GET, process as u64, slot)
}

/// `plane_put` (ABI 2.1).
pub fn plane_put(process: RawHandle, slot: u64, addr: u64, len: u64) -> u64 {
    call4(nr::PLANE_PUT, process as u64, slot, addr, len).0
}

/// `slot_create` (ABI 2.1).
pub fn slot_create(flags: u64) -> u64 {
    call1(nr::SLOT_CREATE, flags).0
}

/// `slot_push` (ABI 2.1).
pub fn slot_push(slot: RawHandle, memory: RawHandle, offset: u64, len: u64, cookie: u64) -> u64 {
    call6(
        nr::SLOT_PUSH,
        [slot as u64, memory as u64, offset, len, cookie, 0],
    )
    .0
}

/// `slot_close` (ABI 2.1).
pub fn slot_close(slot: RawHandle) -> u64 {
    call1(nr::SLOT_CLOSE, slot as u64).0
}

/// `slot_take` (ABI 2.1): the event's 32 bytes land in `out`; the handle it gives
/// back, or zero, in `rax`.
pub fn slot_take(slot: RawHandle, out: &mut [u8; moruna_abi::plane::slot_event::SIZE]) -> u64 {
    let a = [slot as u64, out.as_mut_ptr() as u64, 0, 0, 0, 0];
    // SAFETY: the kernel writes exactly `slot_event::SIZE` bytes into `out`.
    unsafe { syscall(nr::SLOT_TAKE, a) }.0
}

/// `slot_refuse` (ABI 2.1): the reason is read from `reason`.
pub fn slot_refuse(slot: RawHandle, reason: &[u8]) -> u64 {
    let a = [
        slot as u64,
        reason.as_ptr() as u64,
        reason.len() as u64,
        0,
        0,
        0,
    ];
    // SAFETY: the kernel reads `reason.len()` bytes from `reason`.
    unsafe { syscall(nr::SLOT_REFUSE, a) }.0
}

/// `process_plane` (ABI 2.1): the slots' handle values are read from `slots`.
pub fn process_plane(process: RawHandle, slots: &[RawHandle], port: RawHandle) -> u64 {
    let a = [
        process as u64,
        slots.as_ptr() as u64,
        slots.len() as u64,
        port as u64,
        0,
        0,
    ];
    // SAFETY: the kernel reads `slots.len()` four-byte values from `slots`.
    unsafe { syscall(nr::PROCESS_PLANE, a) }.0
}

/// `port_call` (ABI 2.1): the request is read from `bytes`; the reply's address and
/// length come back in `rax` and `rdx`.
pub fn port_call(port: u64, bytes: &[u8]) -> (u64, u64) {
    let a = [port, bytes.as_ptr() as u64, bytes.len() as u64, 0, 0, 0];
    // SAFETY: the kernel reads `bytes.len()` bytes from `bytes`.
    unsafe { syscall(nr::PORT_CALL, a) }
}

/// `thread_set_fs` (ABI 2.0). Safe for this crate's programs, which keep nothing at
/// the FS base.
pub fn thread_set_fs(base: u64) -> u64 {
    call1(nr::THREAD_SET_FS, base).0
}

/// `memory_protect` (ABI 2.0), for whole pages of a mapping the caller holds.
pub(crate) fn memory_protect(addr: u64, len: u64, flags: u64) -> u64 {
    call3(nr::MEMORY_PROTECT, addr, len, flags).0
}

/// `memory_unmap`, for the heap, which unmaps only mappings it made itself.
#[cfg(feature = "heap")]
pub(crate) fn memory_unmap(addr: u64, len: u64) -> u64 {
    call2(nr::MEMORY_UNMAP, addr, len).0
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

macro_rules! sized {
    ($load:ident, $store:ident, $t:ty) => {
        /// One access of exactly this width to a live mapping: a device register is
        /// read and written at its own width, never byte by byte.
        pub(crate) fn $load(addr: u64) -> $t {
            // SAFETY: callers pass an address inside a `Mapping` they hold, aligned to
            // the type's size (checked by `Mapping`), which the kernel keeps mapped
            // readable until the `Mapping` is dropped.
            unsafe { core::ptr::read_volatile(addr as *const $t) }
        }

        pub(crate) fn $store(addr: u64, value: $t) {
            // SAFETY: callers pass an aligned address inside a writable `Mapping` they
            // hold, which the kernel keeps mapped writable until it is dropped.
            unsafe { core::ptr::write_volatile(addr as *mut $t, value) }
        }
    };
}

sized!(load16, store16, u16);
sized!(load32, store32, u32);
sized!(load64, store64, u64);
