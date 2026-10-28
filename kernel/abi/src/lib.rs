//! The native ABI of the Moruna microkernel (SDD 20, section d).
//!
//! Everything a process and the kernel must agree on and nothing else: the syscall
//! numbers, the error codes, the rights a handle carries, the signals an object
//! raises, the byte layouts the kernel and a process exchange through memory, and the
//! boot manifest format. The kernel and `moruna-sys` both depend on this crate, so the
//! two sides cannot drift. It holds no code that touches hardware.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(any(test, feature = "manifest"))]
extern crate alloc;

/// The boot manifest's parser and renderer; they allocate, so a process that only
/// makes syscalls leaves them out.
#[cfg(feature = "manifest")]
pub mod manifest;

/// The ABI version: the major number changes when a call's meaning changes, the minor
/// number when a call is added. `abi_version` returns `MAJOR << 16 | MINOR`.
pub const ABI_MAJOR: u16 = 1;
/// See [`ABI_MAJOR`].
pub const ABI_MINOR: u16 = 2;
/// The packed version `abi_version` returns.
pub const ABI_VERSION: u64 = ((ABI_MAJOR as u64) << 16) | ABI_MINOR as u64;

/// The syscall numbers (SDD 20, d.2). A number is never reused for another meaning.
pub mod nr {
    pub const ABI_VERSION: u64 = 0;
    pub const HANDLE_CLOSE: u64 = 1;
    pub const HANDLE_DUPLICATE: u64 = 2;
    pub const HANDLE_INFO: u64 = 3;
    pub const CHANNEL_CREATE: u64 = 4;
    pub const CHANNEL_WRITE: u64 = 5;
    pub const CHANNEL_READ: u64 = 6;
    pub const MEMORY_CREATE: u64 = 7;
    pub const MEMORY_MAP: u64 = 8;
    pub const MEMORY_UNMAP: u64 = 9;
    pub const PROCESS_CREATE: u64 = 10;
    pub const PROCESS_MAP_IMAGE: u64 = 11;
    pub const PROCESS_START: u64 = 12;
    pub const PROCESS_EXIT: u64 = 13;
    pub const THREAD_CREATE: u64 = 14;
    pub const THREAD_EXIT: u64 = 15;
    pub const WAIT: u64 = 16;
    pub const CLOCK_GET: u64 = 17;
    pub const TIMER_CREATE: u64 = 18;
    pub const TIMER_SET: u64 = 19;
    pub const DEBUG_WRITE: u64 = 20;
    pub const INTERRUPT_CREATE: u64 = 21;
    pub const INTERRUPT_ACK: u64 = 22;
    pub const DEVICE_MEMORY_CREATE: u64 = 23;
    /// Added in ABI 1.1: a resource over a sub-range of another (SDD 20, f.10).
    pub const RESOURCE_NARROW: u64 = 24;
    /// Added in ABI 1.2: an interrupt delivered by message (MSI or MSI-X), and the
    /// message a device writes to raise it (SDD 25, d.1).
    pub const INTERRUPT_CREATE_MSI: u64 = 25;
    /// Added in ABI 1.2: pins a DMA memory object for a device and returns the device
    /// address of each of its pages (SDD 25, d.1).
    pub const MEMORY_PIN: u64 = 26;
    /// Added in ABI 1.2: releases the pins a DMA resource holds in quarantine.
    pub const DMA_RELEASE: u64 = 27;
    /// Added in ABI 1.2: the kind and range of a resource.
    pub const RESOURCE_INFO: u64 = 28;
    /// Added in ABI 1.2: ends a process through a handle to it.
    pub const PROCESS_KILL: u64 = 29;
    /// One past the highest number; any number at or above it is `BadSyscall`.
    pub const COUNT: u64 = 30;
}

/// Every error a syscall returns. A call returns `-(code)` in `rax`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Error {
    /// The value names no handle in the caller's table.
    BadHandle = 1,
    /// The handle names an object of a type the call does not take.
    WrongType = 2,
    /// The handle lacks a right the call needs, or asks for rights it does not hold.
    AccessDenied = 3,
    /// An argument is out of its allowed set.
    InvalidArgs = 4,
    /// The kernel could not allocate what the call needs.
    NoMemory = 5,
    /// The call would block: nothing to read, or the peer's queue is full.
    ShouldWait = 6,
    /// The other end of the channel is closed.
    PeerClosed = 7,
    /// The deadline passed before any awaited signal was raised.
    TimedOut = 8,
    /// The caller's buffer is smaller than the message; `rdx` carries the sizes needed.
    BufferTooSmall = 9,
    /// An offset or length reaches past the end of the object.
    OutOfRange = 10,
    /// No syscall has this number.
    BadSyscall = 11,
    /// A bound was reached: handle table, queue depth, message size, address space.
    Limit = 12,
    /// The object is in a state the call does not accept (a started process, a fired line).
    BadState = 13,
    /// A pointer the caller passed does not name readable (or writable) memory.
    Fault = 14,
}

impl Error {
    /// All errors, in code order.
    pub const ALL: [Error; 14] = [
        Error::BadHandle,
        Error::WrongType,
        Error::AccessDenied,
        Error::InvalidArgs,
        Error::NoMemory,
        Error::ShouldWait,
        Error::PeerClosed,
        Error::TimedOut,
        Error::BufferTooSmall,
        Error::OutOfRange,
        Error::BadSyscall,
        Error::Limit,
        Error::BadState,
        Error::Fault,
    ];

    /// The positive code; `rax` carries its negation.
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The error a negative `rax` names, or `None` for a value that is not an error.
    pub fn from_code(code: u32) -> Option<Error> {
        Error::ALL.iter().copied().find(|e| e.code() == code)
    }

    /// The value the kernel puts in `rax`.
    pub const fn to_raw(self) -> u64 {
        (-(self as i64)) as u64
    }

    /// Splits a raw `rax` into a value or an error. Values are below `2^63`.
    pub fn from_raw(raw: u64) -> Result<u64, Error> {
        let signed = raw as i64;
        if signed >= 0 {
            return Ok(raw);
        }
        let code = signed.checked_neg().and_then(|c| u32::try_from(c).ok());
        Err(code.and_then(Error::from_code).unwrap_or(Error::BadSyscall))
    }

    /// A short name, for diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Error::BadHandle => "bad handle",
            Error::WrongType => "wrong type",
            Error::AccessDenied => "access denied",
            Error::InvalidArgs => "invalid arguments",
            Error::NoMemory => "no memory",
            Error::ShouldWait => "should wait",
            Error::PeerClosed => "peer closed",
            Error::TimedOut => "timed out",
            Error::BufferTooSmall => "buffer too small",
            Error::OutOfRange => "out of range",
            Error::BadSyscall => "bad syscall",
            Error::Limit => "limit reached",
            Error::BadState => "bad state",
            Error::Fault => "fault",
        }
    }
}

/// A handle value as a process sees it. Zero is never a handle.
pub type RawHandle = u32;
/// The value that is never a handle.
pub const HANDLE_INVALID: RawHandle = 0;

/// The rights a handle carries (SDD 20, e.2). Rights only ever shrink: a duplicate
/// carries a subset of its source's, and a transfer carries the sender's exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rights(u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    /// Observe the object: read a channel, read a memory object's pages.
    pub const READ: Rights = Rights(1 << 0);
    /// Change the object: write a channel, write pages, set a timer, start a process.
    pub const WRITE: Rights = Rights(1 << 1);
    /// Map the object's pages into the caller's address space.
    pub const MAP: Rights = Rights(1 << 2);
    /// Make another handle to the object with the same or fewer rights.
    pub const DUPLICATE: Rights = Rights(1 << 3);
    /// Move the handle to another process over a channel.
    pub const TRANSFER: Rights = Rights(1 << 4);
    /// Wait on the object's signals.
    pub const WAIT: Rights = Rights(1 << 5);
    /// Every right this ABI version defines.
    pub const ALL: Rights = Rights(0x3f);

    /// The rights a raw value names, or `None` if it sets a bit this version does not define.
    pub const fn from_bits(bits: u32) -> Option<Rights> {
        if bits & !Rights::ALL.0 != 0 {
            None
        } else {
            Some(Rights(bits))
        }
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    pub const fn intersection(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }

    pub const fn difference(self, other: Rights) -> Rights {
        Rights(self.0 & !other.0)
    }

    /// The name of each right, in bit order, as the manifest spells it.
    pub const NAMES: [(&'static str, Rights); 6] = [
        ("read", Rights::READ),
        ("write", Rights::WRITE),
        ("map", Rights::MAP),
        ("duplicate", Rights::DUPLICATE),
        ("transfer", Rights::TRANSFER),
        ("wait", Rights::WAIT),
    ];

    /// The right a manifest name spells.
    pub fn from_name(name: &str) -> Option<Rights> {
        Rights::NAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, r)| *r)
    }
}

impl core::ops::BitOr for Rights {
    type Output = Rights;
    fn bitor(self, rhs: Rights) -> Rights {
        self.union(rhs)
    }
}

/// The signals an object raises (SDD 20, e.3). Which bits an object can raise depends
/// on its type; a wait for a bit the type never raises is satisfied only by a deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Signals(u32);

impl Signals {
    pub const NONE: Signals = Signals(0);
    /// A channel endpoint has a message to read.
    pub const READABLE: Signals = Signals(1 << 0);
    /// A channel endpoint's peer has room in its queue.
    pub const WRITABLE: Signals = Signals(1 << 1);
    /// A channel endpoint's peer is closed.
    pub const PEER_CLOSED: Signals = Signals(1 << 2);
    /// A process or thread has ended.
    pub const TERMINATED: Signals = Signals(1 << 3);
    /// A timer's deadline passed, or an interrupt line fired and is not yet acknowledged.
    pub const SIGNALED: Signals = Signals(1 << 4);
    /// Every signal this ABI version defines.
    pub const ALL: Signals = Signals(0x1f);

    pub const fn from_bits(bits: u32) -> Option<Signals> {
        if bits & !Signals::ALL.0 != 0 {
            None
        } else {
            Some(Signals(bits))
        }
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Signals) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Signals) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn union(self, other: Signals) -> Signals {
        Signals(self.0 | other.0)
    }

    pub const fn difference(self, other: Signals) -> Signals {
        Signals(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::ops::BitOr for Signals {
    type Output = Signals;
    fn bitor(self, rhs: Signals) -> Signals {
        self.union(rhs)
    }
}

/// The type of the object a handle names, as `handle_info` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ObjectType {
    Process = 1,
    Thread = 2,
    Channel = 3,
    Memory = 4,
    Timer = 5,
    Interrupt = 6,
    DeviceMemory = 7,
    Console = 8,
    Resource = 9,
    /// A memory object pinned for a device (ABI 1.2, `memory_pin`).
    Pin = 10,
}

impl ObjectType {
    pub const ALL: [ObjectType; 10] = [
        ObjectType::Process,
        ObjectType::Thread,
        ObjectType::Channel,
        ObjectType::Memory,
        ObjectType::Timer,
        ObjectType::Interrupt,
        ObjectType::DeviceMemory,
        ObjectType::Console,
        ObjectType::Resource,
        ObjectType::Pin,
    ];

    pub fn from_raw(raw: u32) -> Option<ObjectType> {
        ObjectType::ALL.iter().copied().find(|t| *t as u32 == raw)
    }
}

/// What a resource handle lets its holder create (SDD 20, e.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ResourceKind {
    /// `process_create`.
    Process = 1,
    /// `device_memory_create` over physical ranges.
    DeviceMemory = 2,
    /// `interrupt_create` over interrupt lines.
    Interrupt = 3,
    /// `interrupt_create_msi` over message-signalled interrupt slots (ABI 1.2).
    Msi = 4,
    /// `memory_pin` for the devices whose PCI requester ids lie in its range (ABI 1.2):
    /// the right to hand a device the physical pages of a memory object.
    Dma = 5,
}

impl ResourceKind {
    pub const ALL: [ResourceKind; 5] = [
        ResourceKind::Process,
        ResourceKind::DeviceMemory,
        ResourceKind::Interrupt,
        ResourceKind::Msi,
        ResourceKind::Dma,
    ];

    pub fn from_raw(raw: u32) -> Option<ResourceKind> {
        ResourceKind::ALL.iter().copied().find(|k| *k as u32 == raw)
    }
}

/// The ranges a root resource covers (SDD 20, f.10). `resource_narrow` derives a
/// resource over a sub-range; a process resource has no range and is never narrowed.
pub mod resource_range {
    /// Physical addresses a device memory resource may cover: the 52-bit physical
    /// address space x86_64 defines.
    pub const DEVICE_END: u64 = 1 << 52;
    /// Global system interrupts an interrupt resource may cover: 0 to 255.
    pub const INTERRUPT_END: u64 = 256;
    /// Message-signalled interrupt slots an MSI resource may cover: one per vector the
    /// kernel can route to a process (x86_64 vectors 32 to 255).
    pub const MSI_END: u64 = 224;
    /// PCI requester ids (`bus << 8 | device << 3 | function`) a DMA resource may cover.
    pub const DMA_END: u64 = 1 << 16;
}

/// The flags of `memory_create` (ABI 1.2; zero is the call of ABI 1.0).
pub mod memory_flags {
    /// The object may be pinned for a device with `memory_pin`.
    pub const DMA: u64 = 1 << 0;
    pub const ALL: u64 = DMA;
}

/// The flags of `memory_pin`.
pub mod pin_flags {
    /// The device writes the pages, so the memory handle needs `WRITE` as well as
    /// `READ`.
    pub const DEVICE_WRITES: u64 = 1 << 0;
    pub const ALL: u64 = DEVICE_WRITES;
}

/// What `interrupt_create_msi` returns in `rdx`: the message's data in the high half
/// and the low 32 bits of its address in the low half. On x86_64 the address's high
/// 32 bits are zero.
pub mod msi {
    /// The message address `rdx` carries.
    pub const fn address(rdx: u64) -> u64 {
        rdx & 0xffff_ffff
    }
    /// The message data `rdx` carries.
    pub const fn data(rdx: u64) -> u32 {
        (rdx >> 32) as u32
    }
    /// Packs a message as the kernel returns it.
    pub const fn pack(address: u32, data: u32) -> u64 {
        ((data as u64) << 32) | address as u64
    }
}

/// The 24 bytes `resource_info` writes, little endian.
///
/// ```text
/// offset 0   u32 kind (ResourceKind)
/// offset 4   u32 reserved, zero
/// offset 8   u64 base of the range
/// offset 16  u64 length of the range (zero for a process resource)
/// ```
pub mod resource_info {
    pub const SIZE: usize = 24;
    pub const KIND: usize = 0;
    pub const BASE: usize = 8;
    pub const LEN: usize = 16;
}

/// The flags of `memory_map`.
pub mod map {
    /// The mapping is readable. Every mapping is.
    pub const READ: u64 = 1 << 0;
    /// The mapping is writable; needs the `WRITE` right.
    pub const WRITE: u64 = 1 << 1;
    /// The mapping is executable; refused together with `WRITE`.
    pub const EXECUTE: u64 = 1 << 2;
    pub const ALL: u64 = READ | WRITE | EXECUTE;
}

/// The page size every length and offset in this ABI is a multiple of.
pub const PAGE_SIZE: u64 = 4096;

/// Where a static image's segments may lie (SDD 20, f.2): above the null page, below
/// the area the kernel maps stacks, the start page and `memory_map` into. The boot
/// image tool checks every program against the same range.
pub const IMAGE_RANGE: core::ops::Range<u64> = 0x1000..0x1000_0000_0000;

/// Bounds the kernel enforces, published so a process can size its buffers.
pub mod limits {
    /// The largest message a channel carries, in bytes.
    pub const CHANNEL_MAX_BYTES: usize = 64 * 1024;
    /// The most handles one message carries.
    pub const CHANNEL_MAX_HANDLES: usize = 64;
    /// The most messages queued towards one endpoint.
    pub const CHANNEL_DEPTH: usize = 64;
    /// The most handles one process holds.
    pub const HANDLES_PER_PROCESS: usize = 4096;
    /// The most items one `wait` takes.
    pub const WAIT_MAX_ITEMS: usize = 64;
    /// The largest memory object, in bytes.
    pub const MEMORY_MAX_BYTES: u64 = 256 * 1024 * 1024;
    /// The longest process or handle-slot name, in bytes.
    pub const NAME_MAX: usize = 32;
    /// The most handles a process starts with.
    pub const START_HANDLES_MAX: usize = 64;
    /// The most bytes one `debug_write` takes.
    pub const DEBUG_WRITE_MAX: usize = 4096;
    /// The size of the stack the kernel gives a process's first thread.
    pub const MAIN_STACK_BYTES: u64 = 256 * 1024;
    /// The most pages one `memory_pin` pins: a whole memory object of the largest size.
    pub const PIN_MAX_PAGES: usize = (MEMORY_MAX_BYTES / 4096) as usize;
}

/// The deadline that never passes.
pub const DEADLINE_INFINITE: u64 = u64::MAX;

/// One item of `wait`'s array: 16 bytes, little endian.
///
/// ```text
/// offset 0  u32 handle
/// offset 4  u32 signals awaited
/// offset 8  u32 signals observed (written by the kernel)
/// offset 12 u32 reserved, zero
/// ```
pub mod wait_item {
    pub const SIZE: usize = 16;
    pub const HANDLE: usize = 0;
    pub const AWAITED: usize = 4;
    pub const OBSERVED: usize = 8;
    pub const RESERVED: usize = 12;
}

/// One entry of `process_start`'s array: 40 bytes.
///
/// ```text
/// offset 0  u32 handle, moved to the new process
/// offset 4  u32 reserved, zero
/// offset 8  [u8; 32] slot name, zero padded
/// ```
pub mod start_entry {
    pub const SIZE: usize = 40;
    pub const HANDLE: usize = 0;
    pub const RESERVED: usize = 4;
    pub const NAME: usize = 8;
}

/// The start page: one read-only page the kernel maps into every process before its
/// first thread runs, whose address the thread receives in `rdi`.
///
/// ```text
/// offset 0   u32 magic, START_MAGIC
/// offset 4   u32 ABI version (MAJOR << 16 | MINOR)
/// offset 8   u32 count of entries
/// offset 12  u32 reserved, zero
/// offset 16  [u8; 32] process name, zero padded
/// offset 48  count entries, each:
///              u32 handle, u32 rights, [u8; 32] slot name, zero padded
/// ```
pub mod start_page {
    pub const MAGIC: u32 = 0x3153_4b4d; // "MKS1"
    pub const MAGIC_OFFSET: usize = 0;
    pub const VERSION_OFFSET: usize = 4;
    pub const COUNT_OFFSET: usize = 8;
    pub const NAME_OFFSET: usize = 16;
    pub const HEADER_SIZE: usize = 48;
    pub const ENTRY_SIZE: usize = 40;
    pub const ENTRY_HANDLE: usize = 0;
    pub const ENTRY_RIGHTS: usize = 4;
    pub const ENTRY_NAME: usize = 8;
}

// Every start handle fits on the one start page.
const _: () = assert!(
    start_page::HEADER_SIZE + limits::START_HANDLES_MAX * start_page::ENTRY_SIZE
        <= PAGE_SIZE as usize
);

/// Reads a little-endian `u32` at `offset`, or `None` past the end.
pub fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let b = bytes.get(offset..end)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Writes a little-endian `u32` at `offset`; returns `false` past the end.
pub fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> bool {
    let Some(end) = offset.checked_add(4) else {
        return false;
    };
    match bytes.get_mut(offset..end) {
        Some(b) => {
            b.copy_from_slice(&value.to_le_bytes());
            true
        }
        None => false,
    }
}

/// Reads a little-endian `u64` at `offset`, or `None` past the end.
pub fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    let b = bytes.get(offset..end)?;
    let mut word = [0u8; 8];
    word.copy_from_slice(b);
    Some(u64::from_le_bytes(word))
}

/// Writes a little-endian `u64` at `offset`; returns `false` past the end.
pub fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> bool {
    let Some(end) = offset.checked_add(8) else {
        return false;
    };
    match bytes.get_mut(offset..end) {
        Some(b) => {
            b.copy_from_slice(&value.to_le_bytes());
            true
        }
        None => false,
    }
}

/// A name as stored in a fixed field: the bytes before the first zero.
pub fn fixed_name(field: &[u8]) -> &[u8] {
    let len = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..len]
}

/// Whether `name` is a valid process or slot name: 1 to 32 bytes of `[a-z0-9._-]`.
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= limits::NAME_MAX
        && name
            .iter()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kk_t1_error_codes_round_trip() {
        for e in Error::ALL {
            assert_eq!(Error::from_raw(e.to_raw()), Err(e));
            assert_eq!(Error::from_code(e.code()), Some(e));
        }
        assert_eq!(Error::from_raw(42), Ok(42));
        assert_eq!(Error::from_raw((-999i64) as u64), Err(Error::BadSyscall));
        assert_eq!(Error::from_raw(i64::MIN as u64), Err(Error::BadSyscall));
        let mut codes: alloc::vec::Vec<u32> = Error::ALL.iter().map(|e| e.code()).collect();
        codes.dedup();
        assert_eq!(codes.len(), Error::ALL.len());
        for e in Error::ALL {
            assert!(!e.name().is_empty());
        }
    }

    #[test]
    fn kk_t1_rights_only_defined_bits() {
        assert_eq!(Rights::from_bits(0x40), None);
        assert_eq!(Rights::from_bits(0x3f), Some(Rights::ALL));
        let rw = Rights::READ | Rights::WRITE;
        assert!(rw.contains(Rights::READ));
        assert!(!rw.contains(Rights::MAP));
        assert_eq!(rw.difference(Rights::WRITE), Rights::READ);
        assert_eq!(rw.intersection(Rights::WRITE), Rights::WRITE);
        for (name, r) in Rights::NAMES {
            assert_eq!(Rights::from_name(name), Some(r));
        }
        assert_eq!(Rights::from_name("execute"), None);
        assert_eq!(
            Rights::NAMES.iter().fold(Rights::NONE, |a, (_, r)| a | *r),
            Rights::ALL
        );
    }

    #[test]
    fn kk_t1_signals_only_defined_bits() {
        assert_eq!(Signals::from_bits(0x20), None);
        assert_eq!(Signals::from_bits(0x1f), Some(Signals::ALL));
        let s = Signals::READABLE | Signals::PEER_CLOSED;
        assert!(s.intersects(Signals::PEER_CLOSED));
        assert!(!s.intersects(Signals::TERMINATED));
        assert!(s.contains(Signals::READABLE));
        assert_eq!(s.difference(Signals::READABLE), Signals::PEER_CLOSED);
        assert!(Signals::NONE.is_empty());
        assert_eq!(Signals::from_bits(s.bits()), Some(s));
    }

    #[test]
    fn kk_t1_object_types_round_trip() {
        for t in ObjectType::ALL {
            assert_eq!(ObjectType::from_raw(t as u32), Some(t));
        }
        assert_eq!(ObjectType::from_raw(0), None);
        assert_eq!(ObjectType::from_raw(11), None);
        for k in ResourceKind::ALL {
            assert_eq!(ResourceKind::from_raw(k as u32), Some(k));
        }
        assert_eq!(ResourceKind::from_raw(0), None);
        assert_eq!(ResourceKind::from_raw(6), None);
    }

    #[test]
    fn kk_t1_layout_helpers() {
        let mut page = [0u8; 64];
        assert!(write_u32(&mut page, 0, start_page::MAGIC));
        assert_eq!(read_u32(&page, 0), Some(start_page::MAGIC));
        assert!(!write_u32(&mut page, 61, 1));
        assert_eq!(read_u32(&page, 61), None);
        assert_eq!(read_u32(&page, usize::MAX), None);
        assert!(!write_u32(&mut page, usize::MAX, 1));
        assert_eq!(fixed_name(b"ping\0\0\0"), b"ping");
        assert_eq!(fixed_name(b"full"), b"full");
        assert!(valid_name(b"root.process"));
        assert!(valid_name(b"a-b_c.9"));
        assert!(!valid_name(b""));
        assert!(!valid_name(b"Upper"));
        assert!(!valid_name(&[b'a'; 33]));
        assert_eq!(ABI_VERSION, 0x1_0002);
        assert_eq!(nr::PROCESS_KILL + 1, nr::COUNT);
        assert_eq!(map::ALL, 7);
        let mut word = [0u8; 16];
        assert!(write_u64(&mut word, 8, 0x0123_4567_89ab_cdef));
        assert_eq!(read_u64(&word, 8), Some(0x0123_4567_89ab_cdef));
        assert!(!write_u64(&mut word, 9, 1));
        assert_eq!(read_u64(&word, 9), None);
        assert_eq!(read_u64(&word, usize::MAX), None);
        assert!(!write_u64(&mut word, usize::MAX, 1));
    }

    #[test]
    fn dr_t1_msi_message_packs_and_unpacks() {
        let rdx = msi::pack(0xfee0_0000, 0x41);
        assert_eq!(msi::address(rdx), 0xfee0_0000);
        assert_eq!(msi::data(rdx), 0x41);
        assert_eq!(memory_flags::ALL, 1);
        assert_eq!(pin_flags::ALL, 1);
        assert_eq!(resource_info::SIZE, 24);
        assert_eq!(limits::PIN_MAX_PAGES, 65536);
        assert_eq!(resource_range::MSI_END, 224);
        assert_eq!(resource_range::DMA_END, 65536);
    }
}
