//! `moruna-sys`: safe wrappers over the Moruna microkernel's native syscalls, for a
//! `no_std` process (SDD 20, section d). Handles are owned values that close on drop;
//! a transfer moves them. The base the SDK (KB2) is built on.
//!
//! A program built on it declares its entry with [`main!`]:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//! moruna_sys::main!(run);
//! fn run() -> i64 { 0 }
//! ```

//!
//! A Linux program on the Moruna kernel that was granted handles (SDD 30) uses the same
//! wrappers: it is built for its Linux target, where the crate leaves out the entry, the
//! panic handler and the heap, which its own runtime gives it, and it finds its start
//! page with [`start::init`] from the address its auxiliary vector carries.

#![no_std]
#![cfg_attr(target_os = "none", feature(allow_internal_unsafe))]
// `main!` exports `_start` from the program that invokes it, which is the one unsafe
// attribute a program needs; the macro carries it so the program can forbid unsafe code.
#![cfg_attr(target_os = "none", expect(internal_features))]

#[cfg(feature = "heap")]
extern crate alloc;

#[cfg(feature = "heap")]
mod heap;
pub mod plane;
mod private_memory;
pub use private_memory::PrivateMemory;
pub mod raw;
pub mod start;

#[cfg(target_os = "none")]
use core::sync::atomic::{AtomicU8, Ordering};
use core::{
    fmt::{self, Write as _},
    marker::PhantomData,
};

pub use moruna_abi::{
    ABI_VERSION, DEADLINE_INFINITE, Error, ObjectType, PAGE_SIZE, RawHandle, ResourceKind, Rights,
    Signals, charge, forward, limits, map, memory_flags, pin_flags,
};
use moruna_abi::{msi, nr, start_entry, wait_item};

pub type Result<T> = core::result::Result<T, Error>;

/// The result of a call that moves handles: on failure they come back with the error.
pub type Moved<T> = core::result::Result<(), (Error, T)>;

fn check(raw: u64) -> Result<u64> {
    Error::from_raw(raw)
}

/// The ABI version the kernel speaks, `MAJOR << 16 | MINOR`.
pub fn abi_version() -> u64 {
    raw::call0(nr::ABI_VERSION).0
}

/// Ends the calling process with `code`.
pub fn exit(code: i64) -> ! {
    raw::call1(nr::PROCESS_EXIT, code as u64);
    // The kernel never returns from `process_exit`.
    loop {
        core::hint::spin_loop();
    }
}

/// Ends the calling thread. A process whose last thread ends exits with code 0.
pub fn thread_exit() -> ! {
    raw::call0(nr::THREAD_EXIT);
    loop {
        core::hint::spin_loop();
    }
}

/// Nanoseconds on the kernel's monotonic clock.
pub fn now() -> u64 {
    raw::call0(nr::CLOCK_GET).0
}

/// Sleeps while `word` holds `expected`, until a [`futex_wake`] on it or the deadline
/// (ABI 2.0): `ShouldWait` when it holds another value, `TimedOut` at the deadline.
pub fn futex_wait(
    word: &core::sync::atomic::AtomicU32,
    expected: u32,
    deadline: u64,
) -> Result<()> {
    check(raw::futex_wait(word, expected, deadline)).map(|_| ())
}

/// Wakes at most `count` threads sleeping on `word`; returns how many (ABI 2.0).
pub fn futex_wake(word: &core::sync::atomic::AtomicU32, count: u32) -> Result<u32> {
    check(raw::futex_wake(word, count)).map(|n| n as u32)
}

/// Fills `buf` from the CPU's random source (ABI 2.0), `limits::RANDOM_MAX` bytes at a
/// time.
pub fn random(buf: &mut [u8]) -> Result<()> {
    for chunk in buf.chunks_mut(limits::RANDOM_MAX) {
        check(raw::random_read(chunk))?;
    }
    Ok(())
}

/// Sets the calling thread's FS base (ABI 2.0); this crate keeps nothing there.
pub fn set_fs(base: u64) -> Result<()> {
    check(raw::thread_set_fs(base)).map(|_| ())
}

/// An owned handle. Dropping it closes it.
#[derive(Debug)]
pub struct Handle(RawHandle);

impl Handle {
    /// Takes ownership of a raw value, for example one received in a message.
    pub fn from_raw(value: RawHandle) -> Handle {
        Handle(value)
    }

    /// The raw value, still owned by `self`.
    pub fn raw(&self) -> RawHandle {
        self.0
    }

    /// Gives up ownership without closing.
    pub fn into_raw(self) -> RawHandle {
        let v = self.0;
        core::mem::forget(self);
        v
    }

    /// A new handle to the same object with `rights`, a subset of this handle's.
    pub fn duplicate(&self, rights: Rights) -> Result<Handle> {
        let (v, _) = raw::call2(nr::HANDLE_DUPLICATE, self.0 as u64, rights.bits() as u64);
        check(v).map(|v| Handle(v as RawHandle))
    }

    /// The type of the object and the rights this handle carries.
    pub fn info(&self) -> Result<(ObjectType, Rights)> {
        info(self.0)
    }

    pub fn close(self) -> Result<()> {
        let v = self.into_raw();
        check(raw::call1(nr::HANDLE_CLOSE, v as u64).0).map(|_| ())
    }

    /// A resource over `base .. base + len`, inside this resource's range, with this
    /// handle's rights (`resource_narrow`, ABI 1.1): physical addresses, page aligned,
    /// for a device memory resource; GSIs for an interrupt resource; slots for an MSI
    /// resource; PCI requester ids for a DMA resource.
    pub fn narrow(&self, base: u64, len: u64) -> Result<Handle> {
        check(raw::resource_narrow(self.0, base, len)).map(|v| Handle(v as RawHandle))
    }

    /// A resource's kind and range (`resource_info`, ABI 1.2).
    pub fn resource_info(&self) -> Result<ResourceInfo> {
        let mut out = [0u8; moruna_abi::resource_info::SIZE];
        check(raw::resource_info(self.0, &mut out))?;
        let kind = moruna_abi::read_u32(&out, moruna_abi::resource_info::KIND)
            .and_then(ResourceKind::from_raw)
            .ok_or(Error::InvalidArgs)?;
        let base = moruna_abi::read_u64(&out, moruna_abi::resource_info::BASE);
        let len = moruna_abi::read_u64(&out, moruna_abi::resource_info::LEN);
        match (base, len) {
            (Some(base), Some(len)) => Ok(ResourceInfo { kind, base, len }),
            _ => Err(Error::InvalidArgs),
        }
    }
}

/// What a resource lets its holder create, and over which range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceInfo {
    pub kind: ResourceKind,
    pub base: u64,
    pub len: u64,
}

impl ResourceInfo {
    /// The first value past the range.
    pub fn end(&self) -> u64 {
        self.base + self.len
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        raw::call1(nr::HANDLE_CLOSE, self.0 as u64);
    }
}

/// `handle_info` on any value, owned or not: the call a test of isolation makes.
pub fn info(value: RawHandle) -> Result<(ObjectType, Rights)> {
    let packed = check(raw::call1(nr::HANDLE_INFO, value as u64).0)?;
    let kind = ObjectType::from_raw((packed >> 32) as u32).ok_or(Error::InvalidArgs)?;
    let rights = Rights::from_bits(packed as u32).ok_or(Error::InvalidArgs)?;
    Ok((kind, rights))
}

/// One end of a channel.
#[derive(Debug)]
pub struct Channel(pub Handle);

impl Channel {
    pub fn create() -> Result<(Channel, Channel)> {
        let (a, b) = raw::call0(nr::CHANNEL_CREATE);
        let a = check(a)?;
        Ok((
            Channel(Handle(a as RawHandle)),
            Channel(Handle(b as RawHandle)),
        ))
    }

    /// Sends `bytes` and moves `handles` to the peer. On failure the handles come back.
    pub fn write<const N: usize>(&self, bytes: &[u8], handles: [Handle; N]) -> Moved<[Handle; N]> {
        let values: [RawHandle; N] = core::array::from_fn(|i| handles[i].raw());
        let r = raw::channel_write(self.0.raw(), bytes, &values);
        match check(r) {
            Ok(_) => {
                for h in handles {
                    core::mem::forget(h);
                }
                Ok(())
            }
            Err(e) => Err((e, handles)),
        }
    }

    /// Sends `bytes` and moves every handle in `handles` to the peer: on success each
    /// slot is left `None`; on failure every handle stays where it was. A `None` slot
    /// is `InvalidArgs`, as are more than `limits::CHANNEL_MAX_HANDLES` slots.
    pub fn write_with(&self, bytes: &[u8], handles: &mut [Option<Handle>]) -> Result<()> {
        if handles.len() > limits::CHANNEL_MAX_HANDLES {
            return Err(Error::Limit);
        }
        let mut values = [0 as RawHandle; limits::CHANNEL_MAX_HANDLES];
        for (value, slot) in values.iter_mut().zip(handles.iter()) {
            *value = slot.as_ref().ok_or(Error::InvalidArgs)?.raw();
        }
        check(raw::channel_write(
            self.0.raw(),
            bytes,
            &values[..handles.len()],
        ))?;
        for slot in handles.iter_mut() {
            if let Some(h) = slot.take() {
                core::mem::forget(h);
            }
        }
        Ok(())
    }

    /// Reads one message into `bytes`; the handles it carries land in `handles`.
    /// Returns the byte count and the handle count. `BufferTooSmall` leaves the message
    /// queued; [`Channel::needed`] tells what it needs.
    pub fn read(&self, bytes: &mut [u8], handles: &mut [Option<Handle>]) -> Result<(usize, usize)> {
        let mut values = [0 as RawHandle; limits::CHANNEL_MAX_HANDLES];
        let cap = handles.len().min(values.len());
        let (r, d) = raw::channel_read(self.0.raw(), bytes, &mut values[..cap]);
        let nbytes = check(r)? as usize;
        let nhandles = d as usize;
        for (slot, v) in handles.iter_mut().zip(values.iter()).take(nhandles) {
            *slot = Some(Handle(*v));
        }
        Ok((nbytes, nhandles))
    }

    /// The sizes the next message needs: bytes and handles. `None` if nothing is queued.
    pub fn needed(&self) -> Result<Option<(usize, usize)>> {
        let (r, d) = raw::channel_read(self.0.raw(), &mut [], &mut []);
        match check(r) {
            Ok(_) => Ok(Some((0, 0))),
            Err(Error::BufferTooSmall) => {
                Ok(Some(((d & 0xffff_ffff) as usize, (d >> 32) as usize)))
            }
            Err(Error::ShouldWait) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// One entry of a [`wait`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WaitItem {
    pub handle: RawHandle,
    pub awaited: u32,
    pub observed: u32,
    reserved: u32,
}

const _: () = assert!(core::mem::size_of::<WaitItem>() == wait_item::SIZE);

impl WaitItem {
    pub fn new(handle: &Handle, awaited: Signals) -> WaitItem {
        WaitItem::raw(handle.raw(), awaited)
    }

    /// An item for a raw value the caller may not own (a test of isolation).
    pub fn raw(handle: RawHandle, awaited: Signals) -> WaitItem {
        WaitItem {
            handle,
            awaited: awaited.bits(),
            observed: 0,
            reserved: 0,
        }
    }

    pub fn observed(&self) -> Signals {
        Signals::from_bits(self.observed).unwrap_or(Signals::NONE)
    }
}

/// Waits until one of `items` raises a signal it awaits, or `deadline` (on [`now`]'s
/// clock) passes. Returns how many items are satisfied; every item's `observed` is set.
pub fn wait(items: &mut [WaitItem], deadline: u64) -> Result<usize> {
    check(raw::wait(items, deadline)).map(|n| n as usize)
}

/// Waits for `signals` on one handle; returns what it observed.
pub fn wait_one(handle: &Handle, signals: Signals, deadline: u64) -> Result<Signals> {
    let mut items = [WaitItem::new(handle, signals)];
    wait(&mut items, deadline)?;
    Ok(items[0].observed())
}

/// A memory object.
#[derive(Debug)]
pub struct Memory(pub Handle);

impl Memory {
    /// A memory object of `size` bytes, whole pages, charged to the caller's account.
    pub fn create(size: u64) -> Result<Memory> {
        check(raw::memory_create(size, 0, 0)).map(|v| Memory(Handle(v as RawHandle)))
    }

    /// A memory object charged to `account`: an [`Account`]'s handle, or a process's,
    /// either with `WRITE` (ABI 2.3).
    pub fn create_in(size: u64, account: &Handle) -> Result<Memory> {
        check(raw::memory_create(size, 0, account.raw())).map(|v| Memory(Handle(v as RawHandle)))
    }

    /// A memory object a holder of a DMA resource may pin for a device (ABI 1.2).
    pub fn create_dma(size: u64) -> Result<Memory> {
        check(raw::memory_create(size, memory_flags::DMA, 0))
            .map(|v| Memory(Handle(v as RawHandle)))
    }

    /// DMA memory charged to the supplied actual account, never a service pool.
    pub fn create_dma_in(size: u64, account: &Handle) -> Result<Memory> {
        check(raw::memory_create(size, memory_flags::DMA, account.raw()))
            .map(|v| Memory(Handle(v as RawHandle)))
    }
    /// KE1 ABI 2.4: one bounded contiguous DMA run charged to this process.
    pub fn create_dma_contiguous(size: u64) -> Result<Memory> {
        check(raw::memory_create(
            size,
            memory_flags::DMA | memory_flags::CONTIGUOUS,
            0,
        ))
        .map(|v| Memory(Handle(v as RawHandle)))
    }

    /// One bounded contiguous DMA run charged to the explicit original account.
    pub fn create_dma_contiguous_in(size: u64, account: &Handle) -> Result<Memory> {
        check(raw::memory_create(
            size,
            memory_flags::DMA | memory_flags::CONTIGUOUS,
            account.raw(),
        ))
        .map(|v| Memory(Handle(v as RawHandle)))
    }

    /// Maps `len` bytes from `offset` with `flags` (`map::WRITE`, `map::EXECUTE`).
    pub fn map(&self, offset: u64, len: u64, flags: u64) -> Result<Mapping<'_>> {
        mapping(&self.0, offset, len, flags)
    }

    /// Maps as [`Memory::map`] does, with a mapping that does not borrow the handle:
    /// a mapping keeps its pages whatever becomes of the handle it was made from.
    pub fn map_detached(&self, offset: u64, len: u64, flags: u64) -> Result<Mapping<'static>> {
        mapping(&self.0, offset, len, flags)
    }
}

/// A memory account (ABI 2.3, SDD 20 f.18): a limit, and what is charged to it. Its
/// limit was carved out of its parent's, which holds it until the last handle to the
/// account and the last thing charged to it are gone.
#[derive(Debug)]
pub struct Account(pub Handle);

impl Account {
    /// An account named `name` (1 to 32 bytes of `[a-z0-9._-]`) of `limit` bytes, whole
    /// pages, carved out of the caller's own account: `QuotaExceeded` when it has no
    /// room for the whole limit.
    pub fn carve(limit: u64, name: &str) -> Result<Account> {
        check(raw::account_create(0, limit, name.as_bytes()))
            .map(|v| Account(Handle(v as RawHandle)))
    }

    /// An account carved out of this one.
    pub fn carve_from(&self, limit: u64, name: &str) -> Result<Account> {
        check(raw::account_create(self.0.raw(), limit, name.as_bytes()))
            .map(|v| Account(Handle(v as RawHandle)))
    }

    /// Compare an actual Account capability, or a Memory object's immutable
    /// charged owner, to this exact Account. READ is required on both handles.
    /// Foreign accounts return false; names, usage and ancestry have no meaning.
    pub fn matches(&self, object: &Handle) -> Result<bool> {
        match check(raw::account_matches(self.0.raw(), object.raw()))? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::BadState),
        }
    }

    /// Its figures now.
    pub fn usage(&self) -> Result<Usage> {
        Usage::of(&self.0)
    }
}

/// An account's figures (`account_info`), in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub limit: u64,
    /// Charged to it now.
    pub used: u64,
    /// The most ever charged to it at once.
    pub peak: u64,
    /// Held for the accounts carved out of it: the sum of their limits.
    pub reserved: u64,
    name: [u8; limits::NAME_MAX],
}

impl Usage {
    fn read(account: RawHandle) -> Result<Usage> {
        use moruna_abi::account_info as info;
        let mut out = [0u8; info::SIZE];
        check(raw::account_info(account, &mut out))?;
        let word = |at: usize| moruna_abi::read_u64(&out, at).unwrap_or(0);
        let mut name = [0u8; limits::NAME_MAX];
        name.copy_from_slice(&out[info::NAME..info::NAME + limits::NAME_MAX]);
        Ok(Usage {
            limit: word(info::LIMIT),
            used: word(info::USED),
            peak: word(info::PEAK),
            reserved: word(info::RESERVED),
            name,
        })
    }

    /// The caller's own account's figures.
    pub fn own() -> Result<Usage> {
        Usage::read(0)
    }

    /// The figures of the account `handle` names: an account, or a process's, with
    /// `READ`.
    pub fn of(handle: &Handle) -> Result<Usage> {
        Usage::read(handle.raw())
    }

    /// What can still be charged to it, or carved out of it.
    pub fn room(&self) -> u64 {
        self.limit
            .saturating_sub(self.used)
            .saturating_sub(self.reserved)
    }

    /// Its name.
    pub fn name(&self) -> &str {
        let name = moruna_abi::fixed_name(&self.name);
        core::str::from_utf8(name).unwrap_or("")
    }
}

/// A memory object pinned for a device (`memory_pin`, ABI 1.2). Dropping it closes
/// the pin, whose pages wait in its DMA resource's quarantine until [`dma_release`].
#[derive(Debug)]
pub struct Pin(pub Handle);

impl Pin {
    /// Pins `memory` (made with [`Memory::create_dma`]) for the devices `dma` covers,
    /// writing the device address of each page to `addrs`; returns the pin and the
    /// page count. `device_writes` needs `WRITE` on `memory`. `BufferTooSmall` when
    /// `addrs` has room for fewer addresses than the object has pages.
    pub fn create(
        dma: &Handle,
        memory: &Handle,
        device_writes: bool,
        addrs: &mut [u64],
    ) -> Result<(Pin, usize)> {
        let flags = if device_writes {
            pin_flags::DEVICE_WRITES
        } else {
            0
        };
        let (v, pages) = raw::memory_pin(dma.raw(), memory.raw(), flags, addrs);
        check(v).map(|v| (Pin(Handle(v as RawHandle)), pages as usize))
    }

    /// How many page addresses pinning `memory` writes: every check of
    /// [`Pin::create`] is made, and nothing is pinned.
    pub fn pages(dma: &Handle, memory: &Handle, device_writes: bool) -> Result<usize> {
        let flags = if device_writes {
            pin_flags::DEVICE_WRITES
        } else {
            0
        };
        match raw::memory_pin(dma.raw(), memory.raw(), flags, &mut []) {
            (v, needed) if Error::from_raw(v) == Err(Error::BufferTooSmall) => Ok(needed as usize),
            (v, _) => match check(v) {
                // A pin with no room for an address cannot succeed: an object has a page.
                Ok(_) => Err(Error::BadState),
                Err(e) => Err(e),
            },
        }
    }
}

/// Drops the pins `dma` holds in quarantine (`dma_release`, ABI 1.2): its holder says
/// the devices it covers no longer use their pages. Returns how many there were.
pub fn dma_release(dma: &Handle) -> Result<u64> {
    check(raw::dma_release(dma.raw()))
}

/// A device memory region (MMIO pages), created from the device root resource.
#[derive(Debug)]
pub struct DeviceMemory(pub Handle);

impl DeviceMemory {
    pub fn create(resource: &Handle, paddr: u64, len: u64) -> Result<DeviceMemory> {
        let (v, _) = raw::call3(nr::DEVICE_MEMORY_CREATE, resource.raw() as u64, paddr, len);
        check(v).map(|v| DeviceMemory(Handle(v as RawHandle)))
    }

    pub fn map(&self, offset: u64, len: u64, flags: u64) -> Result<Mapping<'_>> {
        mapping(&self.0, offset, len, flags)
    }

    /// Maps as [`DeviceMemory::map`] does, with a mapping that does not borrow the
    /// handle.
    pub fn map_detached(&self, offset: u64, len: u64, flags: u64) -> Result<Mapping<'static>> {
        mapping(&self.0, offset, len, flags)
    }
}

/// Maps `handle`'s object. The mapping's lifetime is the caller's choice: the kernel
/// keeps a mapping's pages for as long as it exists, whatever becomes of the handle.
fn mapping<'a>(handle: &Handle, offset: u64, len: u64, flags: u64) -> Result<Mapping<'a>> {
    let (v, _) = raw::call4(
        nr::MEMORY_MAP,
        handle.raw() as u64,
        offset,
        len,
        flags | map::READ,
    );
    let addr = check(v)?;
    Ok(Mapping {
        addr,
        len,
        writable: flags & map::WRITE != 0,
        made_writable: flags & map::WRITE != 0,
        _object: PhantomData,
    })
}

/// Pages mapped into this process; unmapped on drop. The bytes are shared with every
/// other mapping of the same object, so they are read and written one at a time as
/// volatile accesses, never through a Rust reference.
#[derive(Debug)]
pub struct Mapping<'a> {
    addr: u64,
    len: u64,
    writable: bool,
    made_writable: bool,
    _object: PhantomData<&'a Handle>,
}

impl Mapping<'_> {
    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The first address above the mapping: a stack top for [`Thread::spawn`].
    pub fn end(&self) -> u64 {
        self.addr + self.len
    }

    /// Makes the whole mapping read only, or writable again if it was made writable
    /// (`memory_protect`, ABI 2.0).
    pub fn protect(&mut self, writable: bool) -> Result<()> {
        if writable && !self.made_writable {
            return Err(Error::AccessDenied);
        }
        let flags = if writable {
            map::READ | map::WRITE
        } else {
            map::READ
        };
        check(raw::memory_protect(self.addr, self.len, flags))?;
        self.writable = writable;
        Ok(())
    }

    /// Unmaps every page from page `keep` on, keeping the first `keep` pages (a partial
    /// `memory_unmap`, ABI 2.0).
    pub fn shrink(&mut self, keep: u64) -> Result<()> {
        let kept = keep.checked_mul(PAGE_SIZE).ok_or(Error::InvalidArgs)?;
        if keep == 0 || kept >= self.len {
            return Err(Error::InvalidArgs);
        }
        check(raw::call2(nr::MEMORY_UNMAP, self.addr + kept, self.len - kept).0)?;
        self.len = kept;
        Ok(())
    }

    fn range(&self, offset: usize, n: usize) -> Result<u64> {
        let end = offset.checked_add(n).ok_or(Error::OutOfRange)?;
        if end as u64 > self.len {
            return Err(Error::OutOfRange);
        }
        Ok(self.addr + offset as u64)
    }

    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        let at = self.range(offset, buf.len())?;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = raw::peek(at + i as u64);
        }
        Ok(())
    }

    pub fn write(&self, offset: usize, data: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::AccessDenied);
        }
        let at = self.range(offset, data.len())?;
        for (i, b) in data.iter().enumerate() {
            raw::poke(at + i as u64, *b);
        }
        Ok(())
    }

    /// The mapping's bytes as one region shared with other processes, for a protocol
    /// whose every access is an atomic or a hand-over of bytes from one side to the
    /// other, as a `moruna-record` ring's is (SDD 29 f.7). Writable mappings only.
    pub fn shared_region(&mut self) -> Result<&mut [u8]> {
        if !self.writable {
            return Err(Error::AccessDenied);
        }
        // SAFETY: the kernel keeps `len` bytes at `addr` mapped writable for as long as
        // the mapping lives, which the borrow of `self` outlives. Other processes may
        // map the same pages; the caller's protocol (atomics for the shared words, and
        // each slot of bytes owned by one side at a time) is what keeps their accesses
        // from racing, as this method's contract says.
        Ok(unsafe { core::slice::from_raw_parts_mut(self.addr as *mut u8, self.len as usize) })
    }

    /// The mapping's bytes, read in place: for memory no process writes while the borrow
    /// lives, which is what a data-plane batch is (SDD 29 DP-I4: no task maps a batch
    /// writable, and the engine writes none it has pushed). The engine decodes a record a
    /// task put this way, with no copy (SDD 30 f.6).
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the kernel keeps `len` bytes at `addr` mapped readable for as long as
        // the mapping lives, which the borrow of `self` outlives. The caller's contract,
        // as this method's says, is that nothing writes them meanwhile.
        unsafe { core::slice::from_raw_parts(self.addr as *const u8, self.len as usize) }
    }

    pub fn read_u32(&self, offset: usize) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read(offset, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// The address of an access of `n` bytes at `offset`, which must be aligned to `n`.
    fn aligned(&self, offset: usize, n: usize) -> Result<u64> {
        if !offset.is_multiple_of(n) {
            return Err(Error::InvalidArgs);
        }
        self.range(offset, n)
    }

    fn writable(&self, offset: usize, n: usize) -> Result<u64> {
        if !self.writable {
            return Err(Error::AccessDenied);
        }
        self.aligned(offset, n)
    }

    /// One 8-bit access.
    pub fn read8(&self, offset: usize) -> Result<u8> {
        Ok(raw::peek(self.aligned(offset, 1)?))
    }

    /// One 16-bit access at an offset aligned to 2.
    pub fn read16(&self, offset: usize) -> Result<u16> {
        Ok(raw::load16(self.aligned(offset, 2)?))
    }

    /// One 32-bit access at an offset aligned to 4.
    pub fn read32(&self, offset: usize) -> Result<u32> {
        Ok(raw::load32(self.aligned(offset, 4)?))
    }

    /// One 64-bit access at an offset aligned to 8.
    pub fn read64(&self, offset: usize) -> Result<u64> {
        Ok(raw::load64(self.aligned(offset, 8)?))
    }

    pub fn write8(&self, offset: usize, value: u8) -> Result<()> {
        raw::poke(self.writable(offset, 1)?, value);
        Ok(())
    }

    pub fn write16(&self, offset: usize, value: u16) -> Result<()> {
        raw::store16(self.writable(offset, 2)?, value);
        Ok(())
    }

    pub fn write32(&self, offset: usize, value: u32) -> Result<()> {
        raw::store32(self.writable(offset, 4)?, value);
        Ok(())
    }

    pub fn write64(&self, offset: usize, value: u64) -> Result<()> {
        raw::store64(self.writable(offset, 8)?, value);
        Ok(())
    }
}

impl Drop for Mapping<'_> {
    fn drop(&mut self) {
        raw::call2(nr::MEMORY_UNMAP, self.addr, self.len);
    }
}

/// The start entries (`moruna_abi::start_entry`) naming `handles`, and how many.
fn start_entries(
    handles: &[(Option<Handle>, &str)],
) -> Result<([[u8; start_entry::SIZE]; limits::START_HANDLES_MAX], usize)> {
    if handles.len() > limits::START_HANDLES_MAX {
        return Err(Error::Limit);
    }
    let mut entries = [[0u8; start_entry::SIZE]; limits::START_HANDLES_MAX];
    for (e, (h, name)) in entries.iter_mut().zip(handles.iter()) {
        let h = h.as_ref().ok_or(Error::InvalidArgs)?;
        e[start_entry::HANDLE..start_entry::HANDLE + 4].copy_from_slice(&h.raw().to_le_bytes());
        let n = name.len().min(limits::NAME_MAX);
        e[start_entry::NAME..start_entry::NAME + n].copy_from_slice(&name.as_bytes()[..n]);
    }
    Ok((entries, handles.len()))
}

/// The handles a call moved into another process are that process's now: their values
/// here are not closed.
fn forget_moved(handles: &mut [(Option<Handle>, &str)]) {
    for (h, _) in handles.iter_mut() {
        if let Some(h) = h.take() {
            core::mem::forget(h);
        }
    }
}

/// A process handle.
#[derive(Debug)]
pub struct Process(pub Handle);

impl Process {
    /// Creates an empty process from the process root resource, charged to the caller's
    /// own account: it and everything it causes to exist.
    pub fn create(resource: &Handle, name: &str) -> Result<Process> {
        let r = raw::process_create(resource.raw(), name.as_bytes(), 0);
        check(r).map(|v| Process(Handle(v as RawHandle)))
    }

    /// Creates an empty process charged to `account`: an [`Account`]'s handle, or a
    /// process's, either with `WRITE` (ABI 2.3).
    pub fn create_in(resource: &Handle, name: &str, account: &Handle) -> Result<Process> {
        let r = raw::process_create(resource.raw(), name.as_bytes(), account.raw());
        check(r).map(|v| Process(Handle(v as RawHandle)))
    }

    /// The figures of the account this process is charged to.
    pub fn usage(&self) -> Result<Usage> {
        Usage::of(&self.0)
    }

    /// Loads the static ELF image in `image`; returns its entry point.
    pub fn map_image(&self, image: &Handle) -> Result<u64> {
        check(
            raw::call2(
                nr::PROCESS_MAP_IMAGE,
                self.0.raw() as u64,
                image.raw() as u64,
            )
            .0,
        )
    }

    /// Starts the process at `entry`, moving the handles of `handles` into it under
    /// their slot names: on success every slot is left `None`, on failure every handle
    /// stays where it was. A `None` slot is `InvalidArgs`.
    pub fn start_with(&self, entry: u64, handles: &mut [(Option<Handle>, &str)]) -> Result<()> {
        let (entries, count) = start_entries(handles)?;
        check(raw::process_start(
            self.0.raw(),
            entry,
            entries[..count].as_flattened(),
        ))?;
        forget_moved(handles);
        Ok(())
    }

    /// Moves the handles of `handles` into this created process under their slot names
    /// and maps a start page naming them into it (`process_grant`, ABI 2.2), for a
    /// process its creator launches with [`Process::launch`]; returns the page's address
    /// in the process, which the creator hands the program (a Linux program reads it
    /// from its auxiliary vector, SDD 30 d.2). On success every slot is left `None`, on
    /// failure every handle stays where it was.
    pub fn grant(&self, handles: &mut [(Option<Handle>, &str)]) -> Result<u64> {
        let (entries, count) = start_entries(handles)?;
        let page = check(raw::process_grant(
            self.0.raw(),
            entries[..count].as_flattened(),
        ))?;
        forget_moved(handles);
        Ok(page)
    }

    /// Ends the running process (`process_kill`, ABI 1.2).
    pub fn kill(&self) -> Result<()> {
        check(raw::process_kill(self.0.raw())).map(|_| ())
    }

    /// Gives the created process a forward over `endpoint` with `table` (ABI 2.0,
    /// SDD 28 d.1). The endpoint moves into the kernel; on failure it is given back.
    pub fn forward(&self, endpoint: Channel, table: &[u8]) -> Moved<Channel> {
        match check(raw::process_forward(self.0.raw(), endpoint.0.raw(), table)) {
            Ok(_) => {
                core::mem::forget(endpoint);
                Ok(())
            }
            Err(e) => Err((e, endpoint)),
        }
    }

    /// Starts the created process at `entry` with its stack pointer at `stack` and its
    /// heap beginning at `brk` (none when zero); returns its first thread's id.
    pub fn launch(&self, entry: u64, stack: u64, brk: u64) -> Result<u64> {
        check(raw::process_launch(self.0.raw(), entry, stack, brk))
    }

    /// Maps `len` bytes of `memory` from `offset` (zero pages filled on first touch when
    /// `None`) into the process at `addr` (where the kernel chooses when zero), with
    /// `map` flags; returns where.
    pub fn map(
        &self,
        memory: Option<&Handle>,
        offset: u64,
        len: u64,
        addr: u64,
        flags: u64,
    ) -> Result<u64> {
        let m = memory.map(|h| h.raw()).unwrap_or(0);
        check(raw::process_map(self.0.raw(), m, offset, len, addr, flags))
    }

    /// Unmaps whole pages of the process.
    pub fn unmap(&self, addr: u64, len: u64) -> Result<()> {
        check(raw::process_unmap(self.0.raw(), addr, len)).map(|_| ())
    }

    /// Sets the protection (`map::PROTECTION` bits) of whole pages of the process.
    pub fn protect(&self, addr: u64, len: u64, flags: u64) -> Result<()> {
        check(raw::process_protect(self.0.raw(), addr, len, flags)).map(|_| ())
    }

    /// Copies bytes at `addr` of the process into `buf`.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        check(raw::process_read(self.0.raw(), addr, buf)).map(|_| ())
    }

    /// Copies `data` to `addr` of the process.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<()> {
        check(raw::process_write(self.0.raw(), addr, data)).map(|_| ())
    }

    /// A new thread of the forwarded process from the registers of its thread `tid`,
    /// which waits on the forward (`forward_clone`, SDD 28 f.4): its stack pointer
    /// `stack`, its FS base `tls` (the parent's when zero), the word `clear_tid` cleared
    /// and woken when it ends, and its id written to `set_tid` before it runs (neither
    /// when zero). Returns its id.
    pub fn clone_thread(
        &self,
        tid: u64,
        stack: u64,
        tls: u64,
        clear_tid: u64,
        set_tid: u64,
    ) -> Result<u64> {
        check(raw::forward_clone(
            self.0.raw(),
            tid,
            stack,
            tls,
            clear_tid,
            set_tid,
        ))
    }

    /// Starts the process at `entry`, moving `handles` into it under their slot names.
    pub fn start<'n, const N: usize>(
        &self,
        entry: u64,
        handles: [(Handle, &'n str); N],
    ) -> Moved<[(Handle, &'n str); N]> {
        let mut entries = [[0u8; start_entry::SIZE]; N];
        for (e, (h, name)) in entries.iter_mut().zip(handles.iter()) {
            e[start_entry::HANDLE..start_entry::HANDLE + 4].copy_from_slice(&h.raw().to_le_bytes());
            let n = name.len().min(limits::NAME_MAX);
            e[start_entry::NAME..start_entry::NAME + n].copy_from_slice(&name.as_bytes()[..n]);
        }
        let r = raw::process_start(self.0.raw(), entry, entries.as_flattened());
        match check(r) {
            Ok(_) => {
                for (h, _) in handles {
                    core::mem::forget(h);
                }
                Ok(())
            }
            Err(e) => Err((e, handles)),
        }
    }
}

/// A thread handle.
#[derive(Debug)]
pub struct Thread(pub Handle);

impl Thread {
    /// Starts a thread of this process at `entry(arg)` on the stack whose top is
    /// `stack_top` (16-byte aligned; the thread starts as if called from there).
    pub fn spawn(entry: extern "C" fn(u64) -> !, stack_top: u64, arg: u64) -> Result<Thread> {
        let (v, _) = raw::call3(nr::THREAD_CREATE, entry as usize as u64, stack_top, arg);
        check(v).map(|v| Thread(Handle(v as RawHandle)))
    }
}

/// A timer: raises `SIGNALED` at its deadline.
#[derive(Debug)]
pub struct Timer(pub Handle);

impl Timer {
    pub fn create() -> Result<Timer> {
        check(raw::call0(nr::TIMER_CREATE).0).map(|v| Timer(Handle(v as RawHandle)))
    }

    /// Clears `SIGNALED` and arms the timer for `deadline` on [`now`]'s clock; zero disarms.
    pub fn set(&self, deadline: u64) -> Result<()> {
        check(raw::call2(nr::TIMER_SET, self.0.raw() as u64, deadline).0).map(|_| ())
    }
}

/// An interrupt line, created from the interrupt root resource.
#[derive(Debug)]
pub struct Interrupt(pub Handle);

/// The message a device writes to raise an interrupt made by
/// [`Interrupt::create_msi`]: the address and data of an MSI or MSI-X table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msi {
    pub address: u64,
    pub data: u32,
}

impl Interrupt {
    pub fn create(resource: &Handle, gsi: u32) -> Result<Interrupt> {
        let (v, _) = raw::call2(nr::INTERRUPT_CREATE, resource.raw() as u64, gsi as u64);
        check(v).map(|v| Interrupt(Handle(v as RawHandle)))
    }

    /// An interrupt raised by a message, in slot `slot` of an MSI resource
    /// (`interrupt_create_msi`, ABI 1.2), and the message.
    pub fn create_msi(resource: &Handle, slot: u64) -> Result<(Interrupt, Msi)> {
        let (v, rdx) = raw::interrupt_create_msi(resource.raw(), slot);
        let v = check(v)?;
        Ok((
            Interrupt(Handle(v as RawHandle)),
            Msi {
                address: msi::address(rdx),
                data: msi::data(rdx),
            },
        ))
    }

    /// Clears `SIGNALED` once the device is serviced.
    pub fn ack(&self) -> Result<()> {
        check(raw::call1(nr::INTERRUPT_ACK, self.0.raw() as u64).0).map(|_| ())
    }
}

/// The debug console: write only, and only for processes the manifest gives it to.
#[derive(Debug)]
pub struct Console(pub Handle);

impl Console {
    pub fn write_str(&self, text: &str) -> Result<()> {
        for chunk in text.as_bytes().chunks(limits::DEBUG_WRITE_MAX) {
            check(raw::debug_write(self.0.raw(), chunk))?;
        }
        Ok(())
    }

    /// Formats into a line buffer and writes it in one call, so lines from different
    /// processes do not interleave.
    pub fn print(&self, args: fmt::Arguments<'_>) {
        let mut line = Line::new();
        let _ = line.write_fmt(args);
        let _ = self.write_str(line.as_str());
    }
}

/// Prints a line to a [`Console`].
#[macro_export]
macro_rules! println {
    ($console:expr, $($arg:tt)*) => {
        $console.print(format_args!("{}\n", format_args!($($arg)*)))
    };
}

const LINE: usize = 512;

/// A fixed buffer formatted into; output past its end is cut.
pub struct Line {
    buf: [u8; LINE],
    len: usize,
}

impl Line {
    pub fn new() -> Line {
        Line {
            buf: [0; LINE],
            len: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl Default for Line {
    fn default() -> Line {
        Line::new()
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = LINE - self.len;
        let mut n = s.len().min(room);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[cfg(target_os = "none")]
static PANICKING: AtomicU8 = AtomicU8::new(0);

/// A panic prints its message on the console slot, if the process has one, and ends
/// the process with code 101.
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    if PANICKING.swap(1, Ordering::AcqRel) == 0
        && let Some(console) = start::peek("console")
    {
        let mut line = Line::new();
        let _ = writeln!(
            line,
            "{}: panicked: {}",
            start::process_name(),
            info.message()
        );
        let _ = raw::debug_write(console, line.as_str().as_bytes());
    }
    exit(101)
}

/// Declares a process's entry: `main!(run)` runs `fn run() -> i64` and exits with
/// what it returns.
#[cfg(target_os = "none")]
#[macro_export]
#[allow_internal_unsafe]
macro_rules! main {
    ($run:path) => {
        // SAFETY: `_start` is the symbol the image's entry point names, and nothing
        // else in the image defines it.
        #[unsafe(no_mangle)]
        pub extern "C" fn _start(start_page: u64) -> ! {
            $crate::start::init(start_page);
            $crate::exit($run())
        }
    };
}
