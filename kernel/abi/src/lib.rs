//! The native ABI of the Moruna microkernel (SDD 20, section d).
//!
//! Everything a process and the kernel must agree on and nothing else: the syscall
//! numbers, the error codes, the rights a handle carries, the signals an object
//! raises, the byte layouts the kernel and a process exchange through memory, the
//! boot manifest format, and the lines the core and the services write on the node's
//! console as it boots. The kernel and `moruna-sys` both depend on this crate, so the
//! two sides cannot drift. It holds no code that touches hardware.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(any(test, feature = "manifest"))]
extern crate alloc;

pub mod boot_line;

/// The boot manifest's parser and renderer; they allocate, so a process that only
/// makes syscalls leaves them out.
#[cfg(feature = "manifest")]
pub mod manifest;

/// The ABI version: the major number changes when a call's meaning changes, the minor
/// number when a call is added. `abi_version` returns `MAJOR << 16 | MINOR`. ABI 2.0
/// (KC1) moved every call to `nr::BASE + n`, so that a process whose foreign system
/// calls are forwarded (SDD 28) keeps the whole low range for them. ABI 2.1 (KD1) added
/// the data plane: slots, the engine's calls on them, and the task's calls in
/// `nr::DATA_PLANE` (SDD 29). ABI 2.2 (KD2) added `process_grant`: handles and a start
/// page for a process its creator launches, so a Linux program can be the engine (SDD 30).
/// ABI 2.3 (KB5) added memory accounts: the object `Account`, `account_create` and
/// `account_info`, the account `process_create` and `memory_create` charge (an argument
/// whose zero is the call ABI 2.2 made), and the error `QuotaExceeded` (SDD 20 f.18).
/// ABI 2.4 adds KE1 contiguous DMA flags; ABI 2.5 adds exact Account/Memory charged-origin comparison.
/// ABI 2.6 (0.7, `architecture/data-os.md`) reserves the data OS's calls, flags and layouts:
/// the data path's mechanisms (stream A) and the user-space personality's (stream B1), each
/// answered `BadSyscall` until its stream implements it (`architecture/data-os-streams.md`).
/// ABI 2.7 (`architecture/moruna-os-system.md`, section 1F; item 1.9c) adds Hyper-V's
/// mechanism for VMBus: the boot capability `root.hyperv` (a resource of kind
/// [`ResourceKind::Hyperv`]), the object [`ObjectType::HvSignal`], and the six calls of
/// [`hyperv`], numbered `nr::HYPERV_POST` to `nr::HYPERV_SIGNAL`.
pub const ABI_MAJOR: u16 = 2;
/// See [`ABI_MAJOR`].
pub const ABI_MINOR: u16 = 7;
/// The packed version `abi_version` returns.
pub const ABI_VERSION: u64 = ((ABI_MAJOR as u64) << 16) | ABI_MINOR as u64;

/// The syscall numbers (SDD 20, d.2). Call `n` of the table is `BASE + n`; a number is
/// never reused for another meaning.
pub mod nr {
    /// The first native number. Every number below it, and every number at or above
    /// `BASE + COUNT` outside `DATA_PLANE`, is foreign: `BadSyscall` for a native
    /// process, forwarded for one with a forward (SDD 28).
    pub const BASE: u64 = 0x1_0000;
    pub const ABI_VERSION: u64 = BASE;
    pub const HANDLE_CLOSE: u64 = BASE + 1;
    pub const HANDLE_DUPLICATE: u64 = BASE + 2;
    pub const HANDLE_INFO: u64 = BASE + 3;
    pub const CHANNEL_CREATE: u64 = BASE + 4;
    pub const CHANNEL_WRITE: u64 = BASE + 5;
    pub const CHANNEL_READ: u64 = BASE + 6;
    /// `memory_create(size, flags, account)`: ABI 1.2 added `flags`, ABI 2.3 the account
    /// the object is charged to (zero: the caller's own).
    pub const MEMORY_CREATE: u64 = BASE + 7;
    pub const MEMORY_MAP: u64 = BASE + 8;
    /// Since 2.0 any whole pages of a mapping `memory_map` made, not only all of it.
    pub const MEMORY_UNMAP: u64 = BASE + 9;
    /// `process_create(resource, name, length, account)`: ABI 2.3 added the account the
    /// process is charged to (zero: the caller's own).
    pub const PROCESS_CREATE: u64 = BASE + 10;
    pub const PROCESS_MAP_IMAGE: u64 = BASE + 11;
    pub const PROCESS_START: u64 = BASE + 12;
    pub const PROCESS_EXIT: u64 = BASE + 13;
    pub const THREAD_CREATE: u64 = BASE + 14;
    pub const THREAD_EXIT: u64 = BASE + 15;
    pub const WAIT: u64 = BASE + 16;
    pub const CLOCK_GET: u64 = BASE + 17;
    pub const TIMER_CREATE: u64 = BASE + 18;
    pub const TIMER_SET: u64 = BASE + 19;
    pub const DEBUG_WRITE: u64 = BASE + 20;
    pub const INTERRUPT_CREATE: u64 = BASE + 21;
    pub const INTERRUPT_ACK: u64 = BASE + 22;
    pub const DEVICE_MEMORY_CREATE: u64 = BASE + 23;
    /// Added in ABI 1.1: a resource over a sub-range of another (SDD 20, f.10).
    pub const RESOURCE_NARROW: u64 = BASE + 24;
    /// Added in ABI 1.2: an interrupt delivered by message (MSI or MSI-X), and the
    /// message a device writes to raise it (SDD 25, d.1).
    pub const INTERRUPT_CREATE_MSI: u64 = BASE + 25;
    /// Added in ABI 1.2: pins a DMA memory object for a device and returns the device
    /// address of each of its pages (SDD 25, d.1).
    pub const MEMORY_PIN: u64 = BASE + 26;
    /// Added in ABI 1.2: releases the pins a DMA resource holds in quarantine.
    pub const DMA_RELEASE: u64 = BASE + 27;
    /// Added in ABI 1.2: the kind and range of a resource.
    pub const RESOURCE_INFO: u64 = BASE + 28;
    /// Added in ABI 1.2: ends a process through a handle to it.
    pub const PROCESS_KILL: u64 = BASE + 29;
    /// Added in ABI 2.0: changes the protection of whole pages of the caller's own
    /// mappings.
    pub const MEMORY_PROTECT: u64 = BASE + 30;
    /// Added in ABI 2.0: gives a created process a forward: its foreign calls go to a
    /// channel, and the fast calls a table names are answered by the kernel (SDD 28).
    pub const PROCESS_FORWARD: u64 = BASE + 31;
    /// Added in ABI 2.0: starts a created process whose address space its creator
    /// built, at an entry and a stack pointer it gives, with no start page.
    pub const PROCESS_LAUNCH: u64 = BASE + 32;
    /// Added in ABI 2.0: maps a memory object, or zero pages filled on first touch,
    /// into a process at an address its caller chooses.
    pub const PROCESS_MAP: u64 = BASE + 33;
    /// Added in ABI 2.0: unmaps whole pages of a process.
    pub const PROCESS_UNMAP: u64 = BASE + 34;
    /// Added in ABI 2.0: changes the protection of whole pages of a process.
    pub const PROCESS_PROTECT: u64 = BASE + 35;
    /// Added in ABI 2.0: copies bytes out of a process's memory.
    pub const PROCESS_READ: u64 = BASE + 36;
    /// Added in ABI 2.0: copies bytes into a process's memory.
    pub const PROCESS_WRITE: u64 = BASE + 37;
    /// Added in ABI 2.0: sleeps while a 32-bit word of the caller's memory holds a
    /// value.
    pub const FUTEX_WAIT: u64 = BASE + 38;
    /// Added in ABI 2.0: wakes threads sleeping on a word of the caller's memory.
    pub const FUTEX_WAKE: u64 = BASE + 39;
    /// Added in ABI 2.0: sets the calling thread's FS base, for thread-local storage.
    pub const THREAD_SET_FS: u64 = BASE + 40;
    /// Added in ABI 2.0: a new thread of a forwarded process, from the registers of a
    /// thread waiting on its forward: `(process, tid, stack, tls, clear_tid, set_tid)`.
    pub const FORWARD_CLONE: u64 = BASE + 41;
    /// Added in ABI 2.0: bytes from the CPU's random source (at most
    /// `limits::RANDOM_MAX` a call).
    pub const RANDOM_READ: u64 = BASE + 42;
    /// Added in ABI 2.1: a slot, the place one batch at a time moves through between
    /// the engine and a task (SDD 29): `(flags)`, `slot_flags::INPUT` or `OUTPUT`. Its
    /// handles raise `READABLE` while an event waits, `WRITABLE` while a push would be
    /// taken, `PEER_CLOSED` once its task has ended, and (ABI 2.2) `SIGNALED` while its
    /// task's `batch_get` would not wait, which a server serving the task's descriptors
    /// waits on (SDD 30 f.10).
    pub const SLOT_CREATE: u64 = BASE + 43;
    /// Added in ABI 2.1: the engine hands a slot a memory object (moved out of its
    /// table): a batch for an input slot, a window for an output slot:
    /// `(slot, memory, offset, len, cookie)`.
    pub const SLOT_PUSH: u64 = BASE + 44;
    /// Added in ABI 2.1: the next event of a slot (`slot_event`), with the memory object
    /// it gives back, if any: `(slot, event)`.
    pub const SLOT_TAKE: u64 = BASE + 45;
    /// Added in ABI 2.1: no more pushes: the task's `batch_get` answers the end once
    /// what is queued is taken.
    pub const SLOT_CLOSE: u64 = BASE + 46;
    /// Added in ABI 2.1: the record taken from a slot failed the engine's check: the
    /// task attached to it ends with the reason: `(slot, reason, len)`.
    pub const SLOT_REFUSE: u64 = BASE + 47;
    /// Added in ABI 2.1: gives a created process its plane: its slots, numbered in the
    /// order given, and the kernel's end of its port channel (none when zero):
    /// `(process, slots, count, port)`.
    pub const PROCESS_PLANE: u64 = BASE + 48;
    /// Added in ABI 2.2: moves handles into a created process its creator will launch,
    /// under slot names, and maps a start page naming them read only into it; returns
    /// the page's address, which the creator hands the program (SDD 30 d.1):
    /// `(process, entries, count)`, `entries` as `process_start`'s.
    pub const PROCESS_GRANT: u64 = BASE + 49;
    /// Added in ABI 2.3: a new account whose ceiling is carved out of a parent's: the
    /// caller's own (zero), an account, or a process's: `(parent, limit, name, length)`;
    /// the limit, whole pages, is held from the parent until the new account is gone.
    pub const ACCOUNT_CREATE: u64 = BASE + 50;
    /// Added in ABI 2.3: an account's limit, use, peak, what it holds for the accounts
    /// carved from it, and its name ([`crate::account_info`]): `(account, out)`, the
    /// account the caller's own (zero), an account or a process's.
    pub const ACCOUNT_INFO: u64 = BASE + 51;
    /// ABI 2.5: exact capability/charged-origin identity, with READ on both:
    /// `(account, object)`, object only Account or Memory. Returns 0 or 1; no
    /// numeric account identity, authority minting, names or ancestor matching.
    pub const ACCOUNT_MATCHES: u64 = BASE + 52;
    // ABI 2.6, reserved for the data OS (`architecture/data-os.md` §2). Each is
    // `BadSyscall` until its stream implements it (`architecture/data-os-streams.md`).
    // Stream A, the data path's mechanisms:
    /// `thread_set_cpu(thread, cpu)`: runs the thread on CPU `cpu` only, or, with
    /// `cpu::ANY`, unpins it: free to run on any CPU; thread handle 0 is the calling thread.
    /// A place to run, never a function to run (data-os §2). The caller placing itself on
    /// a CPU returns on it; any other change takes effect the next time the thread is
    /// queued.
    pub const THREAD_SET_CPU: u64 = BASE + 53;
    /// `clock_page() -> address`: maps the read-only `clock_page::Page` into the caller
    /// once, so time is read with no call (data-os §2).
    pub const CLOCK_PAGE: u64 = BASE + 54;
    /// `windows_protect(process, ranges, count, flags)`: sets `flags` (`map::READ`, or
    /// `map::READ | map::WRITE`) on each of `count` `windows::Range`s of `process`'s
    /// mappings (zero: the caller's own, as `memory_protect`), all or none, with one
    /// flush finished on every CPU when it returns: the grouped revoke before the record
    /// check, and the grant back when a window is used again (data-os §5). The first
    /// argument may instead be a slot handle the caller holds: the ranges are then
    /// offsets within the area that slot is mapped at in the process that mapped it, and
    /// nothing outside that area changes. That is how the engine revokes a task's output
    /// windows, since it holds no handle to the task's process (stream P, 2026-11-16).
    pub const WINDOWS_PROTECT: u64 = BASE + 55;
    // Stream B1, the user-space personality's mechanisms (kernel-is-the-machine §4.1).
    // Their arguments are stream B1's to fix in `moruna-sys`'s `personality` module.
    /// Registers a process's personality region and its entry (§4.1 a).
    pub const PERSONALITY_SET: u64 = BASE + 56;
    /// Returns from the personality's entry to the program, with the frame's registers.
    pub const PERSONALITY_RETURN: u64 = BASE + 57;
    /// A copy-on-write snapshot of a memory object (§4.1 b).
    pub const MEMORY_SNAPSHOT: u64 = BASE + 58;
    /// A copy-on-write clone of a process's address space, for `fork` (§4.1 b):
    /// `(resource, process, account, flags)`, the process the caller's own when zero, and
    /// `flags` from [`crate::process`].
    pub const PROCESS_CLONE: u64 = BASE + 59;
    /// A memory object whose pages a pager supplies (§4.1 c).
    pub const PAGER_CREATE: u64 = BASE + 60;
    /// A pager's answer to a fault: pages supplied, or the fault refused (§4.1 c).
    pub const PAGER_SUPPLY: u64 = BASE + 61;
    /// Reads a thread's saved registers and FS and GS bases (§4.1 d).
    pub const THREAD_STATE_READ: u64 = BASE + 62;
    /// Writes them (§4.1 d).
    pub const THREAD_STATE_WRITE: u64 = BASE + 63;
    /// Interrupts a thread asleep in the core: its wait returns `Interrupted` (§4.1 d).
    pub const THREAD_INTERRUPT: u64 = BASE + 64;
    // ABI 2.7, Hyper-V's mechanism for VMBus (`crate::hyperv`, section 1F). Each is
    // `BadState` on a machine where the core found no Hyper-V, which also has no
    // `root.hyperv` to make it with.
    /// `hyperv_post(root, connection, type, payload, length) -> status`: `HvPostMessage`
    /// of `length` bytes (at most `hyperv::PAYLOAD_MAX`) to a connection ID; answers
    /// Hyper-V's status, `hyperv::SUCCESS`, `hyperv::BUSY` or another.
    pub const HYPERV_POST: u64 = BASE + 65;
    /// `hyperv_take(root, out) -> taken`: copies the message waiting on SINT 2
    /// (`hyperv::MESSAGE_SIZE` bytes) to `out` and frees its slot, writing end-of-message
    /// when another is pending; 1 if one was taken, 0 if none waited. Never waits.
    pub const HYPERV_TAKE: u64 = BASE + 66;
    /// `hyperv_map_events(root) -> address`: maps CPU 0's event-flags page into the
    /// caller, readable and writable so that it can test and clear a flag; the same
    /// address on every later call.
    pub const HYPERV_MAP_EVENTS: u64 = BASE + 67;
    /// `hyperv_interrupt(root) -> handle`: an interrupt object for SINT 2, raised for each
    /// message or event flag Hyper-V gives it, and waited on and acknowledged
    /// (`interrupt_ack`) as a device's is. In `rdx`, the VP index of CPU 0, where SINT 2
    /// arrives (`HV_X64_MSR_VP_INDEX`, read as the SynIC was enabled): the target a VMBus
    /// client names for its contact and every channel, which is not always 0.
    pub const HYPERV_INTERRUPT: u64 = BASE + 68;
    /// `hyperv_signal_create(root, connection) -> handle`: an `HvSignal` that signals that
    /// one connection ID and nothing else, to give to that channel's driver.
    pub const HYPERV_SIGNAL_CREATE: u64 = BASE + 69;
    /// `hyperv_signal(signal) -> status`: `HvSignalEvent` on the signal's connection ID,
    /// event flag 0; answers Hyper-V's status. Needs `WRITE` on the signal.
    pub const HYPERV_SIGNAL: u64 = BASE + 70;
    /// One past the highest call of the table.
    pub const COUNT: u64 = 71;
    /// The data plane's calls (SDD 29): native numbers a task makes on its own plane,
    /// or a personality server on a task's. The rest of the range is reserved.
    pub const DATA_PLANE: core::ops::Range<u64> = BASE + 0x100..BASE + 0x110;
    /// `batch_get(slot) -> (address, length)` (ABI 2.1): an input slot's next batch
    /// mapped read only, or an output slot's window mapped writable; `(0, 0)` at the end.
    pub const BATCH_GET: u64 = BASE + 0x100;
    /// `batch_put(slot, address, length)` (ABI 2.1): a record written in the output
    /// slot's window, handed to the engine; the window leaves the task.
    pub const BATCH_PUT: u64 = BASE + 0x101;
    /// `port_call(port, address, length) -> (address, length)` (ABI 2.1): a request to
    /// the engine on the task's port channel; the reply is mapped in read only.
    pub const PORT_CALL: u64 = BASE + 0x102;
    /// `plane_get(process, slot) -> (address, length)` (ABI 2.1): `batch_get` on a slot
    /// of a process the caller holds with `WRITE`, mapped into the caller.
    pub const PLANE_GET: u64 = BASE + 0x103;
    /// `plane_put(process, slot, address, length)` (ABI 2.1): `batch_put` of a window
    /// `plane_get` mapped into the caller.
    pub const PLANE_PUT: u64 = BASE + 0x104;
    /// `plane_probe(op, argument)` (ABI 2.1): what the data plane's tests measure
    /// (`probe`); `BadSyscall` in a kernel built without its `probe` feature.
    pub const PLANE_PROBE: u64 = BASE + 0x10f;

    /// Whether `number` is a native call: one of the table's, or the data plane's.
    pub const fn is_native(number: u64) -> bool {
        (number >= BASE && number < BASE + COUNT)
            || (number >= DATA_PLANE.start && number < DATA_PLANE.end)
    }
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
    /// (2.3) The account the call charges has no room for it: its use, what it holds for
    /// the accounts carved from it, and the call's charge would pass its limit.
    QuotaExceeded = 15,
    /// (2.6, stream B1) The thread was interrupted (`thread_interrupt`) while it waited.
    Interrupted = 16,
}

impl Error {
    /// All errors, in code order.
    pub const ALL: [Error; 16] = [
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
        Error::QuotaExceeded,
        Error::Interrupted,
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
            Error::QuotaExceeded => "quota exceeded",
            Error::Interrupted => "interrupted",
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
    /// The engine's end of a slot of the data plane (ABI 2.1, SDD 29).
    Slot = 11,
    /// A memory account: a ceiling, and what is charged to it (ABI 2.3, SDD 20 f.18).
    Account = 12,
    /// (2.7) The right to signal one Hyper-V connection ID and nothing else
    /// (`hyperv_signal`), which a VMBus channel's driver holds.
    HvSignal = 13,
}

impl ObjectType {
    pub const ALL: [ObjectType; 13] = [
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
        ObjectType::Slot,
        ObjectType::Account,
        ObjectType::HvSignal,
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
    /// (2.7) Hyper-V's mechanism (`hyperv_post`, `hyperv_take`, `hyperv_map_events`,
    /// `hyperv_interrupt`, `hyperv_signal_create`): `root.hyperv`, which the core makes
    /// only on Hyper-V. It has no range and is never narrowed.
    Hyperv = 6,
}

impl ResourceKind {
    pub const ALL: [ResourceKind; 6] = [
        ResourceKind::Process,
        ResourceKind::DeviceMemory,
        ResourceKind::Interrupt,
        ResourceKind::Msi,
        ResourceKind::Dma,
        ResourceKind::Hyperv,
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

    /// The bytes of one bus's configuration space in an ECAM window (PCI Express Base
    /// Specification, "Enhanced Configuration Access Mechanism"): 32 devices of 8 functions
    /// of 4 KiB.
    pub const ECAM_BUS_BYTES: u64 = 1 << 20;

    /// What `root.ecam` covers on a machine whose firmware describes no PCI configuration
    /// space (no ACPI MCFG), as a Hyper-V Generation 2 machine has none
    /// (`architecture/moruna-os-system.md`, section 1F, "No PCI bus"): nothing. A device
    /// manager given it finds no PCI function.
    pub const NO_ECAM: core::ops::Range<u64> = 0..0;

    /// What `root.ecam` covers on a machine whose firmware describes PCI configuration
    /// space at `base` for buses `bus_start` to `bus_end` (the MCFG's first segment): from
    /// its first bus to the end of its last. `None` when the buses run backwards, or the
    /// window would wrap past the end of a 64-bit address.
    pub fn ecam(base: u64, bus_start: u8, bus_end: u8) -> Option<core::ops::Range<u64>> {
        if bus_end < bus_start {
            return None;
        }
        let start = base.checked_add(u64::from(bus_start) * ECAM_BUS_BYTES)?;
        let end = base.checked_add((u64::from(bus_end) + 1) * ECAM_BUS_BYTES)?;
        Some(start..end)
    }
}

/// Hyper-V's mechanism for VMBus (ABI 2.7; `architecture/moruna-os-system.md`, section
/// 1F, "VMBus, split by the microkernel rule"): what the six calls exchange, and which
/// object each takes. The core enables CPU 0's SynIC only, with SINT 2, so every message
/// and event flag is CPU 0's SINT 2's.
pub mod hyperv {
    use crate::{ObjectType, ResourceKind, Rights, nr};

    /// The SINT VMBus uses (`vmbus_core::VMBUS_SINT`).
    pub const SINT: u32 = 2;
    /// A SynIC message as `hyperv_take` copies it out: its 16-byte header (type, payload
    /// size, flags, sender) and 240 bytes of payload, as Hyper-V wrote them.
    pub const MESSAGE_SIZE: usize = 256;
    /// The most bytes `hyperv_post` carries (TLFS, `HvCallPostMessage`).
    pub const PAYLOAD_MAX: usize = 240;
    /// The size of CPU 0's event-flags page, which `hyperv_map_events` maps.
    pub const EVENT_PAGE_SIZE: usize = 4096;
    /// Where SINT 2's flags start in that page: 256 bytes, 2048 flags, a SINT.
    pub const EVENT_FLAGS_OFFSET: usize = SINT as usize * 256;
    /// The flags a SINT has.
    pub const EVENT_FLAGS: u32 = 2048;

    /// `HV_STATUS_SUCCESS`: posted, or signalled.
    pub const SUCCESS: u64 = 0x0000;
    /// `HV_STATUS_INSUFFICIENT_BUFFERS`: Hyper-V has no room for the message now; post it
    /// again later.
    pub const BUSY: u64 = 0x0013;

    /// The rights an `HvSignal` is made with. It can only signal (`WRITE`), and be given
    /// to a driver (`DUPLICATE`, `TRANSFER`); a duplicate holds fewer rights, never more,
    /// and its connection ID never changes.
    pub const SIGNAL_RIGHTS: Rights = Rights::WRITE
        .union(Rights::DUPLICATE)
        .union(Rights::TRANSFER);

    /// What one of the six calls takes as its first handle, and the right it needs there.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Authority {
        /// `root.hyperv`: a resource of kind [`ResourceKind::Hyperv`], with `WRITE`.
        Root,
        /// An [`ObjectType::HvSignal`], with `WRITE`.
        Signal,
    }

    impl Authority {
        /// The object type of the handle.
        pub const fn object(self) -> ObjectType {
            match self {
                Authority::Root => ObjectType::Resource,
                Authority::Signal => ObjectType::HvSignal,
            }
        }

        /// The resource kind, for a resource.
        pub const fn kind(self) -> Option<ResourceKind> {
            match self {
                Authority::Root => Some(ResourceKind::Hyperv),
                Authority::Signal => None,
            }
        }

        /// The right the handle must carry.
        pub const fn right(self) -> Rights {
            Rights::WRITE
        }
    }

    /// Which handle call `number` takes, if it is one of the six: only the holder of
    /// `root.hyperv` posts, takes, maps, has the interrupt or makes a signal handle; a
    /// signal handle only signals.
    pub const fn authority(number: u64) -> Option<Authority> {
        match number {
            nr::HYPERV_POST
            | nr::HYPERV_TAKE
            | nr::HYPERV_MAP_EVENTS
            | nr::HYPERV_INTERRUPT
            | nr::HYPERV_SIGNAL_CREATE => Some(Authority::Root),
            nr::HYPERV_SIGNAL => Some(Authority::Signal),
            _ => None,
        }
    }

    /// Whether `hyperv_post` takes a payload of `length` bytes.
    pub const fn payload_fits(length: u64) -> bool {
        length <= PAYLOAD_MAX as u64
    }
}

/// The flags of `memory_create` (ABI 1.2; zero is the call of ABI 1.0).
pub mod memory_flags {
    /// The object may be pinned for a device with `memory_pin`.
    pub const DMA: u64 = 1 << 0;
    /// The object's pages are one physically contiguous run, and a pin maps them as
    /// one run of device addresses (ABI 2.4): for a ring a device reads as one block
    /// longer than a page. Only with `DMA`, and at most `CONTIGUOUS_MAX_BYTES`.
    pub const CONTIGUOUS: u64 = 1 << 1;
    pub const ALL: u64 = DMA | CONTIGUOUS;
    /// The largest contiguous object: 4 MiB.
    pub const CONTIGUOUS_MAX_BYTES: u64 = 4 << 20;
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

/// The 64 bytes `account_info` writes (ABI 2.3), little endian.
///
/// ```text
/// offset 0   u64 the limit, bytes
/// offset 8   u64 what is charged to it now, bytes
/// offset 16  u64 the most that was ever charged to it at once, bytes
/// offset 24  u64 what it holds for the accounts carved from it: their limits, bytes
/// offset 32  [u8; 32] its name, zero padded
/// ```
///
/// Room left in it is `limit - used - reserved`.
pub mod account_info {
    pub const SIZE: usize = 64;
    pub const LIMIT: usize = 0;
    pub const USED: usize = 8;
    pub const PEAK: usize = 16;
    pub const RESERVED: usize = 24;
    pub const NAME: usize = 32;
}

/// What the kernel charges an account (ABI 2.3, SDD 20 f.18), in bytes. Every page a
/// process causes to exist is charged to an account: a memory object's pages and the
/// kernel's record of each, a zero page when it is first touched, an address space's
/// page tables, a thread's kernel stack, and each kernel object at a fixed charge. The
/// kernel checks at its build that every fixed charge covers the record it stands for.
pub mod charge {
    /// A page.
    pub const PAGE: u64 = crate::PAGE_SIZE;
    /// A kernel object's record: a timer, an interrupt, a device memory region, a
    /// resource, a pin, an account; and a memory object's, besides its pages.
    pub const OBJECT: u64 = 256;
    /// What the kernel keeps of each page of a memory object besides the page.
    pub const PAGE_RECORD: u64 = 16;
    /// A zero page, once first touched: the page and the kernel's record of it.
    pub const ZERO_PAGE: u64 = PAGE + 32;
    /// An area of an address space: a mapping, or each part of one an unmap or a change
    /// of protection split it into.
    pub const AREA: u64 = 96;
    /// What the kernel keeps of each page a pin hands a device.
    pub const PIN_PAGE: u64 = 128;
    /// A channel: both endpoints and the room of both queues.
    pub const CHANNEL: u64 = 4 * PAGE;
    /// A slot of the data plane, whose queues are reserved whole when it is made.
    pub const SLOT: u64 = 3 * PAGE;
    /// A thread: its kernel stack (OSTD's 128 pages), its registers and its FPU state.
    pub const THREAD: u64 = 130 * PAGE;
    /// A process's record, besides its address space's tables and its threads.
    pub const PROCESS: u64 = PAGE;
    /// An address space's root table and the three tables over the top of user space,
    /// which the hardware layer keeps until the space ends.
    pub const SPACE: u64 = 4 * PAGE;
    /// The page tables of an address space: one page for each 2 MiB, 1 GiB and 512 GiB
    /// region of it an area touches, besides [`SPACE`].
    pub const TABLE_REGIONS: [u64; 3] = [1 << 21, 1 << 30, 1 << 39];
    /// A message queued on a channel: its record and each handle it carries, besides
    /// its bytes; charged to the sender until it is read or dropped.
    pub const MESSAGE: u64 = 128;
    pub const MESSAGE_HANDLE: u64 = 32;
    /// A process's handle table grows by [`HANDLE_SLOTS`] slots at a time, each
    /// [`HANDLE_SLOT`] bytes, and keeps its size until the process ends.
    pub const HANDLE_SLOTS: u64 = 64;
    pub const HANDLE_SLOT: u64 = 96;

    /// A memory object of `pages` pages.
    pub const fn memory(pages: u64) -> u64 {
        OBJECT + pages * (PAGE + PAGE_RECORD)
    }
}

/// The flags of `memory_map`, `memory_protect` and the `process_*` calls on another
/// process's memory.
pub mod map {
    /// The mapping is readable. Every mapping that is not `NONE` is.
    pub const READ: u64 = 1 << 0;
    /// The mapping is writable; needs the `WRITE` right.
    pub const WRITE: u64 = 1 << 1;
    /// The mapping is executable; refused together with `WRITE`.
    pub const EXECUTE: u64 = 1 << 2;
    /// `process_map` only: whatever the range holds is unmapped first.
    pub const FIXED: u64 = 1 << 3;
    /// The protection bits, of which none is no access.
    pub const PROTECTION: u64 = READ | WRITE | EXECUTE;
    /// Every flag `memory_map` takes; device memory takes neither `HUGE` nor `POPULATE`.
    pub const ALL: u64 = READ | WRITE | EXECUTE | HUGE | POPULATE;
    /// Every flag `process_map` takes.
    pub const PROCESS_ALL: u64 = PROTECTION | FIXED | HUGE | POPULATE;
    /// ABI 2.6: map each aligned 2 MiB whose pages are one aligned physical run as one
    /// large page, and place a mapping the kernel chooses where its runs line up. A memory
    /// object's pages are in such runs; zero pages are only with `POPULATE` (data-os §2).
    pub const HUGE: u64 = 1 << 4;
    /// ABI 2.6: every page present at the call, so none faults later: zero pages get
    /// their frames and are charged at once. An object's pages always are (data-os §2).
    pub const POPULATE: u64 = 1 << 5;
}

/// `thread_set_cpu`'s CPU argument (ABI 2.6, stream A).
pub mod cpu {
    /// Any CPU: the thread is not pinned, and is free to run anywhere.
    pub const ANY: u64 = u64::MAX;
}

/// The page `clock_page` maps (ABI 2.6, stream A): a seqlock. Its time is `clock_get`'s
/// monotonic nanoseconds, the clock every deadline in this ABI is measured on. A reader reads `SEQUENCE`,
/// then the fields, then `SEQUENCE` again, and retries if it changed or was odd. Time in
/// nanoseconds is `BASE_NS + ((tsc - BASE_TSC) * MULTIPLIER) >> SHIFT`.
pub mod clock_page {
    /// Byte offset of the `u64` sequence, odd while the kernel writes the page.
    pub const SEQUENCE: usize = 0;
    /// Byte offset of the `u64` monotonic nanoseconds at `BASE_TSC`.
    pub const BASE_NS: usize = 8;
    /// Byte offset of the `u64` time-stamp counter the page was written at.
    pub const BASE_TSC: usize = 16;
    /// Byte offset of the `u64` multiplier from counter ticks to nanoseconds.
    pub const MULTIPLIER: usize = 24;
    /// Byte offset of the `u32` shift applied after the multiplier.
    pub const SHIFT: usize = 32;
    /// The page's length.
    pub const LEN: usize = 4096;
}

/// `process_clone`'s flags (ABI 2.6, stream B1).
pub mod process {
    /// The clone uses the cloned process's own handle table, as Linux's `CLONE_FILES`
    /// does: a handle made, moved or closed in one is so in every process sharing it. Only
    /// a caller with a personality that holds that same table may ask (`AccessDenied`).
    pub const SHARE_HANDLES: u64 = 1 << 0;
    /// Every flag `process_clone` takes; any other bit is `InvalidArgs`.
    pub const ALL: u64 = SHARE_HANDLES;
}

/// `windows_protect`'s entries (ABI 2.6, stream A).
pub mod windows {
    /// One window: a page-aligned range of the target process's mappings. 16 bytes, as two
    /// little-endian `u64`s: the address, then the length.
    pub const RANGE_SIZE: usize = 16;
    /// The most ranges one call takes.
    pub const MAX: usize = 256;
}

/// The frame the personality's entry receives (ABI 2.6, stream B1; kernel-is-the-machine
/// §4.1 a, d): the program's registers when it made a system call or faulted, as little-
/// endian `u64`s at these byte offsets, read and written in place by the personality.
pub mod personality {
    /// Why the program entered the personality: one of `reason`.
    pub const REASON: usize = 0;
    /// The general registers, in this order, from `REGISTERS`, 8 bytes each.
    pub const REGISTERS: usize = 8;
    /// The names of the general registers, in frame order.
    pub const REGISTER_NAMES: [&str; 16] = [
        "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11", "r12",
        "r13", "r14", "r15",
    ];
    /// The instruction pointer.
    pub const RIP: usize = REGISTERS + 16 * 8;
    /// The flags register.
    pub const RFLAGS: usize = RIP + 8;
    /// The FS base.
    pub const FS_BASE: usize = RFLAGS + 8;
    /// The GS base.
    pub const GS_BASE: usize = FS_BASE + 8;
    /// The exception vector, for `reason::EXCEPTION` and `reason::PAGE_FAULT`.
    pub const VECTOR: usize = GS_BASE + 8;
    /// The exception's error code.
    pub const ERROR_CODE: usize = VECTOR + 8;
    /// The faulting address, for `reason::PAGE_FAULT`.
    pub const FAULT_ADDRESS: usize = ERROR_CODE + 8;
    /// The frame's length.
    pub const LEN: usize = FAULT_ADDRESS + 8;

    /// Why the personality was entered.
    pub mod reason {
        /// A system call: its number in `rax`, its arguments in Linux's registers.
        pub const SYSCALL: u64 = 1;
        /// A page fault the core did not serve.
        pub const PAGE_FAULT: u64 = 2;
        /// Any other exception.
        pub const EXCEPTION: u64 = 3;
        /// The thread was interrupted (`thread_interrupt`) while it ran the program.
        pub const INTERRUPTED: u64 = 4;
    }
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
    /// The most bytes one `process_read` or `process_write` copies.
    pub const PROCESS_COPY_MAX: usize = 1024 * 1024;
    /// The most bytes one `random_read` gives.
    pub const RANDOM_MAX: usize = 4096;
    /// The most foreign numbers a forward's table names: numbers `0 .. FORWARD_FAST`.
    pub const FORWARD_FAST: usize = 512;
    /// The most slots one process's plane has (ABI 2.1).
    pub const PLANE_SLOTS: usize = 64;
    /// The most memory objects one slot holds at once: queued, mapped by its task, and
    /// given back and not yet taken (ABI 2.1).
    pub const SLOT_DEPTH: usize = 64;
    /// The longest reason `slot_refuse` takes, in bytes of UTF-8 (ABI 2.1).
    pub const REFUSAL_MAX: usize = 512;
}

/// A process's forward (SDD 28, d.1): how its foreign system calls reach the server
/// that gives it its personality, and which of them the kernel answers itself.
///
/// A forwarded process's thread that makes a call outside the native range sends a
/// [`request`] on the forward's channel and waits; the server answers with a
/// [`reply`] carrying the same token. A call whose number the forward's table names
/// as a [`fast`] call is answered by the kernel with no message: those are the calls
/// on the process's own memory, time and threads that a round trip would make slow.
pub mod forward {
    /// What `process_forward` reads: [`table::SIZE`] bytes.
    pub mod table {
        /// `u32` magic.
        pub const MAGIC: usize = 0;
        /// `u32` reserved, zero.
        pub const RESERVED: usize = 4;
        /// `[u8; FORWARD_FAST]`: entry `n` is the fast call foreign number `n` reaches,
        /// or [`super::fast::NONE`] when it is forwarded.
        pub const CALLS: usize = 8;
        pub const SIZE: usize = CALLS + crate::limits::FORWARD_FAST;
        /// "MKF1".
        pub const MAGIC_VALUE: u32 = 0x3146_4b4d;
    }

    /// The calls the kernel answers for a forwarded process. They take their arguments
    /// and give their results in the x86-64 System V system call convention, the one
    /// the foreign programs they serve were built for: arguments in `rdi`, `rsi`,
    /// `rdx`, `r10`, `r8`, `r9`, and a result in `rax` that is a value or `-errno`.
    /// The kernel holds no personality's call numbers: the table maps them.
    pub mod fast {
        /// Forwarded: not a fast call.
        pub const NONE: u8 = 0;
        /// `futex(uaddr, op, val, timeout, uaddr2, val3)`: wait, wake, their bitset
        /// forms, requeue and compare-requeue, on the process's own words.
        pub const FUTEX: u8 = 1;
        /// `clock_gettime(clock, timespec)`.
        pub const CLOCK_GETTIME: u8 = 2;
        /// `nanosleep(request, remaining)`.
        pub const NANOSLEEP: u8 = 3;
        /// `clock_nanosleep(clock, flags, request, remaining)`.
        pub const CLOCK_NANOSLEEP: u8 = 4;
        /// `mmap(addr, len, prot, flags, fd, offset)` of anonymous memory; a mapping of
        /// a file is forwarded.
        pub const MMAP: u8 = 5;
        /// `munmap(addr, len)`.
        pub const MUNMAP: u8 = 6;
        /// `mprotect(addr, len, prot)`.
        pub const MPROTECT: u8 = 7;
        /// `brk(addr)`.
        pub const BRK: u8 = 8;
        /// `madvise(addr, len, advice)`.
        pub const MADVISE: u8 = 9;
        /// `sched_yield()`.
        pub const SCHED_YIELD: u8 = 10;
        /// `arch_prctl(code, addr)`: setting and reading the FS base; any other code is
        /// forwarded.
        pub const ARCH_PRCTL: u8 = 11;
        /// `set_tid_address(word)`: the word the kernel clears and wakes when the
        /// thread ends.
        pub const SET_TID_ADDRESS: u8 = 12;
        /// `getrandom(buf, len, flags)`, from the CPU's random source.
        pub const GETRANDOM: u8 = 13;
        /// `exit(code)`: the calling thread ends.
        pub const EXIT: u8 = 14;
        /// `gettid()`.
        pub const GETTID: u8 = 15;
        /// `gettimeofday(timeval, timezone)`.
        pub const GETTIMEOFDAY: u8 = 16;
        /// `time(seconds)`.
        pub const TIME: u8 = 17;
        /// `clock_getres(clock, timespec)`.
        pub const CLOCK_GETRES: u8 = 18;
        /// One past the highest.
        pub const COUNT: u8 = 19;
    }

    /// A request, the kernel to the server: [`request::SIZE`] bytes.
    pub mod request {
        /// `u32` the call's kind, [`CALL`].
        pub const KIND: usize = 0;
        /// `u32` the process's live threads when the call was made.
        pub const THREADS: usize = 4;
        /// `u64` the calling thread's token, its thread id; the reply carries it back.
        pub const TOKEN: usize = 8;
        /// `u64` the foreign call number.
        pub const NUMBER: usize = 16;
        /// `[u64; 6]` the arguments.
        pub const ARGS: usize = 24;
        /// `u64` the instruction pointer after the call.
        pub const IP: usize = 72;
        pub const SIZE: usize = 80;
        /// A system call.
        pub const CALL: u32 = 1;
    }

    /// A reply, the server to the kernel: [`reply::SIZE`] bytes.
    pub mod reply {
        /// `u64` the token of the request answered.
        pub const TOKEN: usize = 0;
        /// `u64` the call's result (the value of `rax`), or an exit code.
        pub const VALUE: usize = 8;
        /// `u64` what the thread does: [`RETURN`], [`EXIT_THREAD`], [`EXIT_PROCESS`] or
        /// [`RESTART`].
        pub const ACTION: usize = 16;
        /// `u64` with [`RESTART`], the new stack pointer (`VALUE` is the new entry).
        pub const STACK: usize = 24;
        /// `u64` with [`RESTART`], where the new heap begins (none when zero).
        pub const BRK: usize = 32;
        pub const SIZE: usize = 40;
        /// The thread returns `VALUE` from its call.
        pub const RETURN: u64 = 0;
        /// The thread ends.
        pub const EXIT_THREAD: u64 = 1;
        /// The process ends with code `VALUE`.
        pub const EXIT_PROCESS: u64 = 2;
        /// The thread starts again at entry `VALUE` with stack `STACK`, every other
        /// register zero, its FS base zero and its heap at `BRK`: the server rebuilt
        /// the address space of a process with this one thread (an `execve`).
        pub const RESTART: u64 = 3;
    }
}

/// The data plane (SDD 29): slots, their events, and a task's port calls.
///
/// A slot is made by the engine (`slot_create`) and attached to one task process
/// before it starts (`process_plane`). The engine pushes memory objects into it: an
/// input slot's are batches, each one page-aligned Arrow IPC record the task maps read
/// only with `batch_get`; an output slot's are windows the task maps writable with
/// `batch_get`, writes a record into and hands back with `batch_put`. What comes back
/// to the engine comes as events (`slot_take`).
pub mod plane {
    /// The flags of `slot_create`: exactly one.
    pub mod slot_flags {
        /// The engine pushes batches; the task reads them.
        pub const INPUT: u64 = 1 << 0;
        /// The engine pushes windows; the task writes records into them.
        pub const OUTPUT: u64 = 1 << 1;
    }

    /// The 32 bytes `slot_take` writes, little endian.
    ///
    /// ```text
    /// offset 0   u32 kind
    /// offset 4   u32 reserved, zero
    /// offset 8   u64 the cookie the memory object was pushed with
    /// offset 16  u64 PUT: where the record starts in the memory object
    /// offset 24  u64 PUT: the record's length
    /// ```
    pub mod slot_event {
        pub const SIZE: usize = 32;
        pub const KIND: usize = 0;
        pub const COOKIE: usize = 8;
        pub const OFFSET: usize = 16;
        pub const LEN: usize = 24;
        /// An input batch the task is done with: it took the next one, or it ended.
        /// No handle.
        pub const RELEASED: u32 = 1;
        /// A record the task put in its window: the window's memory object comes back.
        pub const PUT: u32 = 2;
        /// A window the task never put a record in, given back when the task ended.
        pub const RETURNED: u32 = 3;
    }

    /// A port call's request, the kernel to the engine on the task's port channel: a
    /// header of [`request::SIZE`] bytes and then the bytes the task passed.
    pub mod request {
        /// `u32` the message's kind, [`CALL`].
        pub const KIND: usize = 0;
        /// `u64` the calling thread's token; the reply carries it back.
        pub const TOKEN: usize = 8;
        /// `u64` the port the task named.
        pub const PORT: usize = 16;
        /// `u64` the length of the bytes after the header.
        pub const LEN: usize = 24;
        pub const SIZE: usize = 32;
        /// A port call.
        pub const CALL: u32 = 1;
        /// The most bytes a request carries: a channel message less the header.
        pub const MAX: usize = crate::limits::CHANNEL_MAX_BYTES - SIZE;
    }

    /// A port call's reply, the engine to the kernel: [`reply::SIZE`] bytes and, when
    /// `STATUS` is zero and `LEN` is not, one memory object holding the reply, which
    /// the kernel maps into the caller read only.
    pub mod reply {
        /// `u64` the token of the request answered.
        pub const TOKEN: usize = 0;
        /// `u64` zero, or the error code the call returns (`Error::code`).
        pub const STATUS: usize = 8;
        /// `u64` the reply's length in the memory object.
        pub const LEN: usize = 16;
        pub const SIZE: usize = 24;
    }

    /// The operations of `plane_probe`.
    pub mod probe {
        /// The physical address of the page holding the argument, an address of the
        /// caller's (`BadState` where no page is).
        pub const FRAME: u64 = 0;
        /// The bytes the kernel has copied into or out of the caller's memory since it
        /// started.
        pub const COPIED: u64 = 1;
        /// The physical address of page 0 of a memory object the caller holds a handle
        /// to, the argument being `handle | page << 32`.
        pub const OBJECT_FRAME: u64 = 2;
        /// ABI 2.6: the bytes the page table entry mapping the argument, an address of
        /// the caller's, covers: 4096, 2 MiB for a large page, or zero for none.
        pub const MAPPING: u64 = 3;
        /// ABI 2.6 (stream B1): the bytes of the hole holding the argument, an address of
        /// the caller's, at the deepest page table the caller has there: 4096 where a 4 KiB
        /// table holds it, 2 MiB where only the table above does, and so on; zero where a
        /// page is mapped. A table no area needs any more is gone.
        pub const TABLE: u64 = 4;
    }
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
/// offset 12  u32 CPUs online (ABI 2.0; zero before)
/// offset 16  [u8; 32] process name, zero padded
/// offset 48  count entries, each:
///              u32 handle, u32 rights, [u8; 32] slot name, zero padded
/// ```
pub mod start_page {
    pub const MAGIC: u32 = 0x3153_4b4d; // "MKS1"
    pub const MAGIC_OFFSET: usize = 0;
    pub const VERSION_OFFSET: usize = 4;
    pub const COUNT_OFFSET: usize = 8;
    /// Since ABI 2.0: the CPUs online.
    pub const CPUS_OFFSET: usize = 12;
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
        assert_eq!(ObjectType::from_raw(11), Some(ObjectType::Slot));
        assert_eq!(ObjectType::from_raw(12), Some(ObjectType::Account));
        assert_eq!(ObjectType::from_raw(13), Some(ObjectType::HvSignal));
        assert_eq!(ObjectType::from_raw(14), None);
        for k in ResourceKind::ALL {
            assert_eq!(ResourceKind::from_raw(k as u32), Some(k));
        }
        assert_eq!(ResourceKind::from_raw(0), None);
        assert_eq!(ResourceKind::from_raw(6), Some(ResourceKind::Hyperv));
        assert_eq!(ResourceKind::from_raw(7), None);
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
        assert_eq!(ABI_VERSION, 0x2_0007);
        assert_eq!(nr::HYPERV_SIGNAL + 1, nr::BASE + nr::COUNT);
        assert_eq!(nr::HYPERV_POST, nr::THREAD_INTERRUPT + 1);
        assert_eq!(nr::THREAD_SET_CPU, nr::ACCOUNT_MATCHES + 1);
        assert_eq!(nr::PERSONALITY_SET, nr::WINDOWS_PROTECT + 1);
        assert!(nr::is_native(nr::THREAD_SET_CPU) && nr::is_native(nr::THREAD_INTERRUPT));
        assert_eq!(
            map::ALL & (map::HUGE | map::POPULATE),
            map::HUGE | map::POPULATE
        );
        assert_eq!(personality::LEN, 8 + 16 * 8 + 7 * 8);
        const { assert!(clock_page::SHIFT + 4 <= clock_page::LEN) };
        assert_eq!(nr::ACCOUNT_MATCHES, nr::ACCOUNT_INFO + 1);
        assert_eq!(nr::ACCOUNT_CREATE, nr::PROCESS_GRANT + 1);
        assert!(nr::is_native(nr::BATCH_GET) && nr::is_native(nr::PLANE_PROBE));
        assert_eq!(
            plane::request::SIZE + plane::request::MAX,
            limits::CHANNEL_MAX_BYTES
        );
        assert_eq!(plane::slot_event::SIZE, plane::slot_event::LEN + 8);
        assert_eq!(plane::reply::SIZE, plane::reply::LEN + 8);
        assert!(nr::is_native(nr::BASE) && nr::is_native(nr::FORWARD_CLONE));
        assert!(!nr::is_native(nr::BASE - 1) && !nr::is_native(nr::BASE + nr::COUNT));
        assert!(nr::is_native(nr::DATA_PLANE.start) && !nr::is_native(nr::DATA_PLANE.end));
        assert!(!nr::is_native(0) && !nr::is_native(511));
        assert_eq!(map::ALL, 0x37);
        assert_eq!(map::PROCESS_ALL, 0x3f);
        assert_eq!(forward::table::SIZE, 8 + limits::FORWARD_FAST);
        assert_eq!(forward::request::SIZE, forward::request::IP + 8);
        assert_eq!(forward::reply::SIZE, forward::reply::BRK + 8);
        const { assert!(forward::fast::CLOCK_GETRES < forward::fast::COUNT) };
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
        assert_eq!(memory_flags::DMA, 1);
        assert_eq!(memory_flags::CONTIGUOUS, 2);
        assert_eq!(memory_flags::ALL, 3);
        assert_eq!(memory_flags::CONTIGUOUS_MAX_BYTES, 4 << 20);
        assert_eq!(pin_flags::ALL, 1);
        assert_eq!(resource_info::SIZE, 24);
        assert_eq!(limits::PIN_MAX_PAGES, 65536);
        assert_eq!(resource_range::MSI_END, 224);
        assert_eq!(resource_range::DMA_END, 65536);
    }

    #[test]
    fn hyperv_calls_are_numbered_after_the_data_os_and_native() {
        let calls = [
            nr::HYPERV_POST,
            nr::HYPERV_TAKE,
            nr::HYPERV_MAP_EVENTS,
            nr::HYPERV_INTERRUPT,
            nr::HYPERV_SIGNAL_CREATE,
            nr::HYPERV_SIGNAL,
        ];
        for (i, call) in calls.iter().enumerate() {
            assert_eq!(*call, nr::BASE + 65 + i as u64);
            assert!(nr::is_native(*call));
        }
        assert_eq!(ABI_MINOR, 7);
    }

    #[test]
    fn only_root_hyperv_posts_takes_maps_and_makes_signals_and_a_signal_only_signals() {
        use hyperv::{Authority, authority};
        for call in [
            nr::HYPERV_POST,
            nr::HYPERV_TAKE,
            nr::HYPERV_MAP_EVENTS,
            nr::HYPERV_INTERRUPT,
            nr::HYPERV_SIGNAL_CREATE,
        ] {
            let held = authority(call).unwrap();
            assert_eq!(held, Authority::Root);
            assert_eq!(held.object(), ObjectType::Resource);
            assert_eq!(held.kind(), Some(ResourceKind::Hyperv));
            assert_eq!(held.right(), Rights::WRITE);
        }
        let signal = authority(nr::HYPERV_SIGNAL).unwrap();
        assert_eq!(signal, Authority::Signal);
        assert_eq!(signal.object(), ObjectType::HvSignal);
        assert_eq!(signal.kind(), None);
        assert_eq!(signal.right(), Rights::WRITE);
        // No other call of the table is one of them.
        for call in nr::BASE..nr::BASE + nr::COUNT {
            if !(nr::HYPERV_POST..=nr::HYPERV_SIGNAL).contains(&call) {
                assert_eq!(authority(call), None, "{call:#x}");
            }
        }
    }

    #[test]
    fn a_signal_handle_can_be_given_away_and_never_read_mapped_or_waited_on() {
        use hyperv::SIGNAL_RIGHTS;
        assert!(SIGNAL_RIGHTS.contains(Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER));
        for right in [Rights::READ, Rights::MAP, Rights::WAIT] {
            assert!(!SIGNAL_RIGHTS.contains(right));
        }
    }

    #[test]
    fn a_posted_message_is_at_most_240_bytes() {
        assert!(hyperv::payload_fits(0));
        assert!(hyperv::payload_fits(240));
        assert!(!hyperv::payload_fits(241));
        assert!(!hyperv::payload_fits(u64::MAX));
        assert_eq!(hyperv::MESSAGE_SIZE, 16 + hyperv::PAYLOAD_MAX);
        // SINT 2's 2048 flags are the third 256 bytes of the event-flags page.
        assert_eq!(hyperv::EVENT_FLAGS_OFFSET, 512);
        assert_eq!(hyperv::EVENT_FLAGS as usize / 8, 256);
        const { assert!(hyperv::EVENT_FLAGS_OFFSET + 256 <= hyperv::EVENT_PAGE_SIZE) };
        assert_eq!((hyperv::SUCCESS, hyperv::BUSY), (0, 0x13));
    }

    #[test]
    fn root_ecam_covers_the_mcfgs_buses_or_nothing() {
        use resource_range::{ECAM_BUS_BYTES, NO_ECAM, ecam};
        // QEMU's q35: one segment at 0xb000_0000, buses 0 to 255, 256 MiB.
        assert_eq!(ecam(0xb000_0000, 0, 255), Some(0xb000_0000..0xc000_0000));
        // A segment whose first bus is not 0: the window starts at that bus's space.
        assert_eq!(
            ecam(0xe000_0000, 2, 3),
            Some(0xe000_0000 + 2 * ECAM_BUS_BYTES..0xe000_0000 + 4 * ECAM_BUS_BYTES)
        );
        assert_eq!(ecam(0, 7, 7), Some(7 << 20..8 << 20));
        // Buses that run backwards describe nothing to cover.
        assert_eq!(ecam(0xb000_0000, 1, 0), None);
        // A window that would wrap past the end of a 64-bit address.
        assert_eq!(ecam(u64::MAX - ECAM_BUS_BYTES, 0, 1), None);
        assert_eq!(ecam(u64::MAX - ECAM_BUS_BYTES + 1, 0, 0), None);
        assert_eq!(
            ecam(u64::MAX - ECAM_BUS_BYTES, 0, 0),
            Some(u64::MAX - ECAM_BUS_BYTES..u64::MAX)
        );
        // No MCFG: the empty range, which a device manager can claim no window of.
        assert!(NO_ECAM.is_empty());
        assert_eq!(NO_ECAM.end - NO_ECAM.start, 0);
    }

    #[test]
    fn kb5_t1_account_layout_and_charges() {
        assert_eq!(account_info::SIZE, account_info::NAME + limits::NAME_MAX);
        assert_eq!(account_info::RESERVED, account_info::PEAK + 8);
        assert_eq!(Error::QuotaExceeded.code(), 15);
        assert_eq!(Error::from_code(15), Some(Error::QuotaExceeded));
        assert_eq!(Error::QuotaExceeded.name(), "quota exceeded");
        assert_eq!(Error::Interrupted.code(), 16);
        assert_eq!(Error::from_code(16), Some(Error::Interrupted));
        assert_eq!(Error::Interrupted.name(), "interrupted");
        assert_eq!(charge::memory(0), charge::OBJECT);
        assert_eq!(
            charge::memory(3),
            charge::OBJECT + 3 * (PAGE_SIZE + charge::PAGE_RECORD)
        );
        // Each table region is 512 of the one below it, the lowest 512 pages.
        assert_eq!(charge::TABLE_REGIONS[0], 512 * PAGE_SIZE);
        assert_eq!(charge::TABLE_REGIONS[1], 512 * charge::TABLE_REGIONS[0]);
        assert_eq!(charge::TABLE_REGIONS[2], 512 * charge::TABLE_REGIONS[1]);
        const { assert!(charge::THREAD >= 128 * PAGE_SIZE) };
        const { assert!(charge::SPACE.is_multiple_of(PAGE_SIZE)) };
    }
}
