//! The data plane (ABI 2.1, SDD 29): the engine's slots, and a task's three calls.
//!
//! The engine makes slots, pushes memory objects into them and takes events back
//! ([`Slot`]), and gives a created task process its slots and port ([`attach`]). A task
//! reads its batches in place and writes its records into windows ([`Input`],
//! [`Output`]), and calls ports ([`Port`]). A batch or a window borrows the slot it came
//! from, so it cannot outlive the next call that unmaps it.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use moruna_abi::{
    Error, RawHandle, nr,
    plane::{probe, slot_event, slot_flags},
    read_u32, read_u64,
};

use crate::{Channel, Handle, Memory, Moved, Process, Result, check, raw};

/// The engine's end of a slot.
#[derive(Debug)]
pub struct Slot(pub Handle);

/// What `slot_take` gave back.
#[derive(Debug)]
pub enum SlotEvent {
    /// An input batch the task is done with.
    Released { cookie: u64 },
    /// A record the task put: the window's memory object, where the record starts in
    /// it, and its length.
    Put {
        cookie: u64,
        memory: Memory,
        offset: u64,
        len: u64,
    },
    /// A window the task never put a record in, given back when it ended.
    Returned { cookie: u64, memory: Memory },
}

impl Slot {
    /// An input slot (the engine pushes batches) or an output slot (windows).
    pub fn create(output: bool) -> Result<Slot> {
        let flags = if output {
            slot_flags::OUTPUT
        } else {
            slot_flags::INPUT
        };
        let v = check(raw::slot_create(flags))?;
        Ok(Slot(Handle::from_raw(v as RawHandle)))
    }

    /// Hands the slot `memory` from `offset` for `len` bytes, a batch or a window, with
    /// a cookie its event gives back. The handle moves into the slot; on failure it
    /// comes back with the error.
    pub fn push(&self, memory: Memory, offset: u64, len: u64, cookie: u64) -> Moved<Memory> {
        match check(raw::slot_push(
            self.0.raw(),
            memory.0.raw(),
            offset,
            len,
            cookie,
        )) {
            Ok(_) => {
                core::mem::forget(memory);
                Ok(())
            }
            Err(e) => Err((e, memory)),
        }
    }

    /// The next event: `ShouldWait` with none, `PeerClosed` with none and the task
    /// ended.
    pub fn take(&self) -> Result<SlotEvent> {
        let mut out = [0u8; slot_event::SIZE];
        let v = check(raw::slot_take(self.0.raw(), &mut out))?;
        let word = |at| read_u64(&out, at).unwrap_or(0);
        let cookie = word(slot_event::COOKIE);
        let memory = || Memory(Handle::from_raw(v as RawHandle));
        match read_u32(&out, slot_event::KIND).unwrap_or(0) {
            slot_event::RELEASED => Ok(SlotEvent::Released { cookie }),
            slot_event::PUT => Ok(SlotEvent::Put {
                cookie,
                memory: memory(),
                offset: word(slot_event::OFFSET),
                len: word(slot_event::LEN),
            }),
            slot_event::RETURNED => Ok(SlotEvent::Returned {
                cookie,
                memory: memory(),
            }),
            _ => Err(Error::BadState),
        }
    }

    /// Nothing more will be pushed.
    pub fn close(&self) -> Result<()> {
        check(raw::slot_close(self.0.raw())).map(|_| ())
    }

    /// Ends the task attached to the slot with `reason`: the record taken from it
    /// failed the engine's check.
    pub fn refuse(&self, reason: &str) -> Result<()> {
        let bytes = reason.as_bytes();
        let bytes = &bytes[..bytes.len().min(moruna_abi::limits::REFUSAL_MAX)];
        check(raw::slot_refuse(self.0.raw(), bytes)).map(|_| ())
    }
}

/// Gives the created `process` its plane: `slots`, numbered in order (the engine keeps
/// its handles), and its port over `port`, whose endpoint moves into the kernel. On
/// failure the endpoint comes back with the error.
pub fn attach(process: &Process, slots: &[&Slot], port: Option<Channel>) -> Moved<Option<Channel>> {
    let mut values = [0 as RawHandle; moruna_abi::limits::PLANE_SLOTS];
    if slots.len() > values.len() {
        return Err((Error::Limit, port));
    }
    for (v, s) in values.iter_mut().zip(slots) {
        *v = s.0.raw();
    }
    let end = port.as_ref().map(|c| c.0.raw()).unwrap_or(0);
    match check(raw::process_plane(
        process.0.raw(),
        &values[..slots.len()],
        end,
    )) {
        Ok(_) => {
            if let Some(c) = port {
                core::mem::forget(c);
            }
            Ok(())
        }
        Err(e) => Err((e, port)),
    }
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// A task's own plane, taken once: its slots by number and its port, each of which can
/// be taken once, so no two values can unmap each other's batches.
#[derive(Debug)]
pub struct Plane {
    slots: AtomicU64,
    port: AtomicBool,
}

impl Plane {
    /// The plane, the first time it is asked for; `None` after.
    pub fn take() -> Option<Plane> {
        (!TAKEN.swap(true, Ordering::AcqRel)).then_some(Plane {
            slots: AtomicU64::new(0),
            port: AtomicBool::new(false),
        })
    }

    fn slot(&self, n: u32) -> bool {
        n < 64 && self.slots.fetch_or(1 << n, Ordering::AcqRel) & (1 << n) == 0
    }

    /// Input slot `n`, once; `None` when it was taken (the kernel says whether the
    /// plane has it).
    pub fn input(&self, n: u32) -> Option<Input<'_>> {
        self.slot(n).then_some(Input {
            slot: n,
            _plane: PhantomData,
        })
    }

    /// Output slot `n`, once.
    pub fn output(&self, n: u32) -> Option<Output<'_>> {
        self.slot(n).then_some(Output {
            slot: n,
            _plane: PhantomData,
        })
    }

    /// The port channel, once.
    pub fn port(&self) -> Option<Port<'_>> {
        (!self.port.swap(true, Ordering::AcqRel)).then_some(Port {
            _plane: PhantomData,
        })
    }
}

/// An input slot of the task's plane.
#[derive(Debug)]
pub struct Input<'p> {
    slot: u32,
    _plane: PhantomData<&'p Plane>,
}

/// A batch mapped read only; it leaves the task at the slot's next `batch_get`.
#[derive(Debug)]
pub struct Batch<'s> {
    addr: u64,
    len: u64,
    _slot: PhantomData<&'s mut ()>,
}

fn region(rax: u64, rdx: u64) -> Result<Option<(u64, u64)>> {
    let addr = check(rax)?;
    Ok((addr != 0).then_some((addr, rdx)))
}

impl Input<'_> {
    /// The next batch, the previous one released; `None` at the end. Waits for the
    /// engine while there is none.
    pub fn get(&mut self) -> Result<Option<Batch<'_>>> {
        let (rax, rdx) = raw::batch_get(self.slot as u64);
        Ok(region(rax, rdx)?.map(|(addr, len)| Batch {
            addr,
            len,
            _slot: PhantomData,
        }))
    }
}

impl Batch<'_> {
    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The batch's bytes, in place.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the kernel mapped `len` bytes at `addr` read only into this process
        // and keeps them mapped until this slot's next `batch_get`, which needs the
        // `&mut Input` this batch borrows. No one writes them while they are mapped:
        // the engine never writes a batch it has pushed (SDD 29 DP-I4), and no task
        // holds them writable.
        unsafe { core::slice::from_raw_parts(self.addr as *const u8, self.len as usize) }
    }
}

/// An output slot of the task's plane.
#[derive(Debug)]
pub struct Output<'p> {
    slot: u32,
    _plane: PhantomData<&'p Plane>,
}

/// A window mapped writable; it leaves the task when it is put.
#[derive(Debug)]
pub struct Window<'s> {
    slot: u32,
    addr: u64,
    len: u64,
    _slot: PhantomData<&'s mut ()>,
}

impl Output<'_> {
    /// The slot's window: the one mapped now, or the next the engine gives (waiting for
    /// it); `None` once the engine gives no more.
    pub fn window(&mut self) -> Result<Option<Window<'_>>> {
        let (rax, rdx) = raw::batch_get(self.slot as u64);
        Ok(region(rax, rdx)?.map(|(addr, len)| Window {
            slot: self.slot,
            addr,
            len,
            _slot: PhantomData,
        }))
    }
}

impl Window<'_> {
    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The window's bytes, to write a record into.
    pub fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the kernel mapped `len` bytes at `addr` writable into this process
        // alone and keeps them mapped until `put` consumes this window or the slot's
        // next call, which needs the `&mut Output` it borrows; the engine moved its
        // handle into the slot, so no one else maps them.
        unsafe { core::slice::from_raw_parts_mut(self.addr as *mut u8, self.len as usize) }
    }

    /// Hands the engine the record of `len` bytes at `offset` (a page multiple) of the
    /// window. On a refusal the window stays mapped and comes back with the error.
    pub fn put(self, offset: u64, len: u64) -> core::result::Result<(), (Error, Self)> {
        let addr = self.addr + offset;
        match check(raw::batch_put(self.slot as u64, addr, len)) {
            Ok(_) => Ok(()),
            Err(e) => Err((e, self)),
        }
    }
}

/// The task's port channel.
#[derive(Debug)]
pub struct Port<'p> {
    _plane: PhantomData<&'p Plane>,
}

/// A port call's reply, mapped read only until the next port call.
#[derive(Debug)]
pub struct Reply<'c> {
    addr: u64,
    len: u64,
    _port: PhantomData<&'c mut ()>,
}

impl Port<'_> {
    /// Sends `request` to the engine for `port` and waits for the reply; `None` for an
    /// empty reply. At most `moruna_abi::plane::request::MAX` bytes.
    pub fn call(&mut self, port: u64, request: &[u8]) -> Result<Option<Reply<'_>>> {
        let (rax, rdx) = raw::port_call(port, request);
        Ok(region(rax, rdx)?.map(|(addr, len)| Reply {
            addr,
            len,
            _port: PhantomData,
        }))
    }
}

impl Reply<'_> {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the kernel mapped the reply read only and keeps it until this
        // thread's next port call, which needs the `&mut Port` this reply borrows; the
        // engine never writes a reply it has sent (SDD 29 DP-I4).
        unsafe { core::slice::from_raw_parts(self.addr as *const u8, self.len as usize) }
    }
}

/// `plane_get`, a personality server's `batch_get` on slot `slot` of `process`, mapped
/// into the server: the address and length, `None` at the end. The region stays mapped
/// until the slot's next `plane_get` or `plane_put`, so the server reads and writes it
/// as a [`crate::Mapping`] is, a byte at a time ([`Region`]).
pub fn plane_get(process: &Process, slot: u32) -> Result<Option<Region>> {
    let (rax, rdx) = raw::plane_get(process.0.raw(), slot as u64);
    Ok(region(rax, rdx)?.map(|(addr, len)| Region { addr, len }))
}

/// `plane_put`: the record of `len` bytes at `offset` of the window `region` is, of
/// slot `slot` of `process`.
pub fn plane_put(
    process: &Process,
    slot: u32,
    region: Region,
    offset: u64,
    len: u64,
) -> Result<()> {
    check(raw::plane_put(
        process.0.raw(),
        slot as u64,
        region.addr + offset,
        len,
    ))
    .map(|_| ())
}

/// A batch or a window a server mapped with [`plane_get`]: read and written through
/// volatile accesses, because the server does not own its lifetime.
#[derive(Debug)]
pub struct Region {
    addr: u64,
    len: u64,
}

impl Region {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn range(&self, offset: u64, n: usize) -> Result<u64> {
        let end = offset.checked_add(n as u64).ok_or(Error::OutOfRange)?;
        if end > self.len {
            return Err(Error::OutOfRange);
        }
        Ok(self.addr + offset)
    }

    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let at = self.range(offset, buf.len())?;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = raw::peek(at + i as u64);
        }
        Ok(())
    }

    pub fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        let at = self.range(offset, data.len())?;
        for (i, b) in data.iter().enumerate() {
            raw::poke(at + i as u64, *b);
        }
        Ok(())
    }
}

/// `plane_probe` (the kernel's `probe` feature): the physical address of the page
/// holding `addr` in this process.
pub fn frame_at(addr: u64) -> Result<u64> {
    check(raw::call2(nr::PLANE_PROBE, probe::FRAME, addr).0)
}

/// `plane_probe`: the bytes the kernel has copied into or out of this process.
pub fn copied() -> Result<u64> {
    check(raw::call2(nr::PLANE_PROBE, probe::COPIED, 0).0)
}

/// `plane_probe`: the physical address of page `page` of a memory object.
pub fn object_frame(memory: &Memory, page: u32) -> Result<u64> {
    let arg = memory.0.raw() as u64 | (page as u64) << 32;
    check(raw::call2(nr::PLANE_PROBE, probe::OBJECT_FRAME, arg).0)
}

/// The rings between the engine and a task in ring mode (`architecture/data-os.md` §5): the
/// layout and each side's half of the protocol, the same source the engine's
/// `moruna-plane` builds (and tests) from.
#[path = "../../plane/core/src/ring.rs"]
pub mod ring;

/// Memory shared with another process, read and written by its words and in bulk: the
/// engine's mapping of a region or a rings object, or a task's mapping of what its slots
/// hold. Whole byte ranges are handed from one side to the other by the ring protocol, so
/// neither side touches a range the other may be writing.
#[derive(Debug)]
pub struct Shared {
    addr: u64,
    len: u64,
    _mapping: Option<crate::Mapping<'static>>,
}

impl Shared {
    /// A writable mapping of `len` bytes of `memory`, the engine's own, until dropped.
    pub fn map(memory: &Memory, len: u64) -> Result<Shared> {
        let m = memory.map_detached(0, len, moruna_abi::map::READ | moruna_abi::map::WRITE)?;
        Ok(Shared {
            addr: m.addr(),
            len: m.len(),
            _mapping: Some(m),
        })
    }

    /// What a slot mapped at `addr` for `len` bytes: the slot keeps the mapping, until its
    /// next `batch_get` or `batch_put` or the task's end.
    fn slot(addr: u64, len: u64) -> Shared {
        Shared {
            addr,
            len,
            _mapping: None,
        }
    }

    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn range(&self, at: u64, n: usize) -> Result<u64> {
        let end = at.checked_add(n as u64).ok_or(Error::OutOfRange)?;
        if end > self.len {
            return Err(Error::OutOfRange);
        }
        Ok(self.addr + at)
    }

    /// Copies `buf.len()` bytes at `at` out.
    pub fn read(&self, at: u64, buf: &mut [u8]) -> Result<()> {
        let from = self.range(at, buf.len())?;
        // SAFETY: `from .. from + buf.len()` lies inside the mapping, which lives as long as
        // `self` (or the slot's call that unmaps it, which the caller's borrow precedes); the
        // protocol gives this side the range, so no other process writes it meanwhile.
        unsafe {
            core::ptr::copy_nonoverlapping(from as *const u8, buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    /// Copies `data` in at `at`.
    pub fn write(&self, at: u64, data: &[u8]) -> Result<()> {
        let to = self.range(at, data.len())?;
        // SAFETY: as in `read`; the mapping is writable (a read-only one faults, ending the
        // process, which is the kernel's refusal and never a write elsewhere).
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), to as *mut u8, data.len());
        }
        Ok(())
    }

    fn word(&self, at: usize) -> Option<&core::sync::atomic::AtomicU64> {
        if !at.is_multiple_of(8) {
            return None;
        }
        let addr = self.range(at as u64, 8).ok()?;
        // SAFETY: an aligned word inside the mapping, which outlives the borrow; every
        // access to a shared word, on either side, is atomic.
        Some(unsafe { core::sync::atomic::AtomicU64::from_ptr(addr as *mut u64) })
    }
}

impl ring::Shared for Shared {
    fn load(&self, at: usize) -> u64 {
        self.word(at).map_or(0, |w| w.load(Ordering::Acquire))
    }

    fn store(&self, at: usize, value: u64) {
        if let Some(w) = self.word(at) {
            w.store(value, Ordering::Release);
        }
    }

    fn fence(&self) {
        core::sync::atomic::fence(Ordering::SeqCst);
    }

    /// `futex_wait` on the low half of the word: the kernel keys a word of a shared memory
    /// object by the object and its offset, so the other side's wake meets it.
    fn wait(&self, at: usize, expected: u32, deadline: u64) -> bool {
        let Some(word) = self.bell(at) else {
            return false;
        };
        match check(raw::futex_wait(word, expected, deadline)) {
            Ok(_) | Err(Error::ShouldWait) => true,
            Err(_) => false,
        }
    }

    fn wake(&self, at: usize) {
        if let Some(word) = self.bell(at) {
            raw::futex_wake(word, 1);
        }
    }
}

impl Shared {
    /// The low half of the word at `at`, the futex word of a bell (little endian).
    fn bell(&self, at: usize) -> Option<&core::sync::atomic::AtomicU32> {
        if !at.is_multiple_of(8) {
            return None;
        }
        let addr = self.range(at as u64, 4).ok()?;
        // SAFETY: an aligned word inside the mapping, which outlives the borrow; both sides
        // touch it only atomically.
        Some(unsafe { core::sync::atomic::AtomicU32::from_ptr(addr as *mut u32) })
    }
}

/// A task's side of ring mode: its region and its rings object, each mapped once, from
/// slots 0 and 1 of its plane.
///
/// With no input, the task reads its ring [`Rings::spins`] times, then waits on its bell
/// ([`Rings::wait_input`]): it **lingers** for more while its engine is posting, until a
/// deadline of [`ring::GROUP`] of the engine's posting intervals as the task measures them on
/// its clock page ([`ring::Pace`]), at most [`ring::LINGER_MAX_NS`]; the engine wakes a
/// lingering task once that many posts wait, before it blocks, and when it closes the input,
/// not for each post. Otherwise, and once a linger ends with nothing come, it sleeps until the
/// next post. So the task takes its batches a group at a time even when the scheduler wakes it
/// at once (item 6.3p), and a task whose engine has stopped costs nothing.
#[derive(Debug)]
pub struct Rings {
    region: Shared,
    rings: Shared,
    half: ring::Task,
    /// How many times the task reads its ring before it sleeps: a batch the engine posts
    /// within them costs no call.
    pub spins: u32,
    /// Wakes the task called on the engine's bell.
    wakes: u64,
    /// Waits it took on its own bell, lingers and sleeps: each one futex call.
    sleeps: u64,
    /// How often the engine posts, as the task sees it.
    pace: ring::Pace,
    /// The task's clock.
    clock: Clock,
    /// A batch taken while no window was free, and the part of its answer to write next.
    pending: Option<(ring::InEntry, u32)>,
}

/// What a function given to [`Rings::step_parts`] wrote into a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// A record of this many bytes, the batch's last: the batch is done.
    Last(usize),
    /// A record of this many bytes, and more of the batch's answer to come in the next
    /// window: the answer is bigger than a window.
    More(usize),
}

/// What [`Rings::step`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// It answered a batch with a record in this window.
    Answered(u64),
    /// No batch is there yet: [`Rings::wait_input`].
    NoInput,
    /// No window is free: [`Rings::wait_window`].
    NoWindow,
    /// Every batch is answered and the engine posts no more.
    Ended,
}

/// The task's clock: the clock page, read with no call, or `clock_get` where it has none.
#[derive(Clone, Copy, Debug)]
struct Clock(Option<crate::datapath::ClockPage>);

impl Clock {
    #[cfg(target_arch = "x86_64")]
    fn map() -> Clock {
        Clock(crate::datapath::ClockPage::map().ok())
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn map() -> Clock {
        Clock(None)
    }

    fn now(&self) -> u64 {
        match self.0 {
            #[cfg(target_arch = "x86_64")]
            Some(page) => page.now(),
            _ => crate::now(),
        }
    }
}

/// How many times a task reads its ring before it sleeps on its bell, by default: a few
/// microseconds, then the futex. Under KVM a long run of `pause` traps the vCPU to the host,
/// so 16,384 reads before each sleep cost the gate's waiting tasks more CPU than their
/// functions (Enrich: 270 CPU-s of waits beside 36 of its function, a full year, 2026-11-17).
pub const SPINS: u32 = 1 << 8;

fn broken(_: ring::Bad) -> Error {
    Error::BadState
}

impl Rings {
    fn map(slot: u64) -> Result<Shared> {
        let (rax, rdx) = raw::batch_get(slot);
        let (addr, len) = region(rax, rdx)?.ok_or(Error::PeerClosed)?;
        Ok(Shared::slot(addr, len))
    }

    /// Maps the region (slot 0) and the rings object (slot 1) once and attaches, and pins the
    /// thread on the CPU the engine placed the task on, if it placed it. Takes both slots of
    /// `plane`, so nothing else of the task can unmap them.
    pub fn attach(plane: &Plane) -> Result<Rings> {
        if !(plane.slot(0) && plane.slot(1)) {
            return Err(Error::BadState);
        }
        let region = Rings::map(0)?;
        let rings = Rings::map(1)?;
        let half = ring::Task::attach(&rings).map_err(broken)?;
        // The CPU the engine placed the task on: the kernel keeps a thread where it first
        // ran, so the stage's tasks spread only when each is placed. A CPU the kernel does not
        // have leaves the thread where it is.
        if let Some(cpu) = half.place {
            let _ = crate::datapath::pin_current_thread(Some(cpu));
        }
        let clock = Clock::map();
        let pace = ring::Pace::new(half.head(&rings), clock.now());
        Ok(Rings {
            region,
            rings,
            half,
            spins: SPINS,
            wakes: 0,
            sleeps: 0,
            pace,
            clock,
            pending: None,
        })
    }

    /// The geometry and the mode the engine chose.
    pub fn half(&self) -> &ring::Task {
        &self.half
    }

    /// The next batch, in place in the region; `None` while there is none. It stays the
    /// task's until [`Rings::release`].
    pub fn batch(&mut self) -> Result<Option<(ring::InEntry, &[u8])>> {
        let Some(e) = self.half.next(&self.rings).map_err(broken)? else {
            return Ok(None);
        };
        let at = self.region.range(e.offset, e.len as usize)?;
        // SAFETY: the region is mapped read only for as long as the task lives (slot 0 is
        // never asked again), the engine writes no span it has posted until the task
        // releases it (DP-I4), and the borrow of `self` ends before `release`.
        let bytes = unsafe { core::slice::from_raw_parts(at as *const u8, e.len as usize) };
        Ok(Some((e, bytes)))
    }

    /// Releases the oldest batch taken: the task is done with it, and every record posted
    /// before answers it. Rings the engine's bell, which waits for the release.
    pub fn release(&mut self) -> bool {
        let released = self.half.release(&self.rings);
        if released && self.half.ring(&self.rings) {
            self.wakes += 1;
        }
        released
    }

    /// Whether every batch was taken and the engine posts no more.
    pub fn ended(&self) -> bool {
        self.half.ended(&self.rings)
    }

    /// A window to write a record in, and its bytes; `None` while the engine has given none.
    pub fn window(&mut self) -> Result<Option<(u64, &mut [u8])>> {
        let Some(w) = self.half.window(&self.rings).map_err(broken)? else {
            return Ok(None);
        };
        let g = self.half.geometry;
        let at = self.rings.range(g.window_at(w), g.window_bytes as usize)?;
        // SAFETY: the window is the task's from the free ring until it posts it, mapped
        // writable in the rings object, and the borrow ends before `post`.
        let bytes =
            unsafe { core::slice::from_raw_parts_mut(at as *mut u8, g.window_bytes as usize) };
        Ok(Some((w, bytes)))
    }

    /// Posts the record of `len` bytes at the start of window `w`, answering `cookie`. A batch
    /// may be answered with several records; [`Rings::release`] says it is done.
    pub fn post(&mut self, w: u64, len: u64, cookie: u64) -> bool {
        let posted = self.post_quietly(w, len, cookie);
        // The engine's bell: a wake only when it sleeps.
        if posted && self.half.ring(&self.rings) {
            self.wakes += 1;
        }
        posted
    }

    fn post_quietly(&mut self, w: u64, len: u64, cookie: u64) -> bool {
        self.half.post(
            &self.rings,
            ring::OutEntry {
                window: w,
                offset: 0,
                len,
                cookie,
            },
        )
    }

    /// Says the task's exit code to the engine, which started it and has no other way to know
    /// it, and returns it for `main` to return: the last call a task makes.
    pub fn exit(&mut self, code: i64) -> i64 {
        if self.half.exit(&self.rings, code) {
            self.wakes += 1;
        }
        code
    }

    /// Waits on the task's bell until the next batch or the end (the input ring is empty):
    /// lingers while the engine is posting, otherwise sleeps with no deadline, the engine's wake
    /// reaching the bell of the shared object (see [`Rings`]).
    pub fn wait_input(&mut self) {
        for _ in 0..self.spins {
            if self.half.has_input(&self.rings) {
                return;
            }
            core::hint::spin_loop();
        }
        let clock = self.clock;
        let calls = self.half.wait_input(
            &self.rings,
            &mut self.pace,
            || clock.now(),
            crate::DEADLINE_INFINITE,
        );
        self.sleeps += u64::from(calls);
    }

    /// Sleeps on the task's bell until the engine gives a window (none is free).
    pub fn wait_window(&mut self) {
        for _ in 0..self.spins {
            if self.half.has_window(&self.rings) {
                return;
            }
            core::hint::spin_loop();
        }
        let slept = self.half.rest(
            &self.rings,
            ring::Rest::Sleep(crate::DEADLINE_INFINITE),
            |t| t.has_window(&self.rings),
        );
        self.sleeps += u64::from(slept.called());
    }

    /// Answers the next batch: `f` reads it in place and writes a record into a free window,
    /// returning the record's length (`None` refuses the batch, an error), and the record is
    /// posted and the batch released. No byte moves but what `f` writes.
    pub fn step(&mut self, f: impl FnOnce(&[u8], &mut [u8]) -> Option<usize>) -> Result<Step> {
        let mut f = Some(f);
        self.step_parts(|input, window, _| f.take()?(input, window).map(Part::Last))
    }

    /// [`Rings::step`] for an answer that may be bigger than a window: `f` reads the batch in
    /// place and writes part `part` of its answer (from 0) into a free window, saying whether
    /// more follows. Each part is posted as a record of its own, in a window of its own, and
    /// the batch is released after the last. A step answers one batch, waiting for no window:
    /// on [`Step::NoWindow`] the next step goes on with the same part.
    pub fn step_parts(
        &mut self,
        mut f: impl FnMut(&[u8], &mut [u8], u32) -> Option<Part>,
    ) -> Result<Step> {
        let (entry, mut part) = match self.pending.take() {
            Some(p) => p,
            None => match self.half.next(&self.rings).map_err(broken)? {
                Some(e) => (e, 0),
                None if self.half.ended(&self.rings) => return Ok(Step::Ended),
                None => return Ok(Step::NoInput),
            },
        };
        loop {
            let Some(w) = self.half.window(&self.rings).map_err(broken)? else {
                self.pending = Some((entry, part));
                return Ok(Step::NoWindow);
            };
            let (len, last) = match self.write_part(entry, w, part, &mut f)? {
                Part::Last(len) => (len, true),
                Part::More(len) => (len, false),
            };
            if !last {
                // The engine takes it now, so its window can come back for a later part.
                if !self.post(w, len as u64, entry.cookie) {
                    return Err(Error::ShouldWait);
                }
                part += 1;
                continue;
            }
            if !self.post_quietly(w, len as u64, entry.cookie) {
                return Err(Error::ShouldWait);
            }
            // One ring for the record and the release, which the engine waits for.
            self.release();
            return Ok(Step::Answered(w));
        }
    }

    /// Lets `f` write part `part` of `entry`'s answer into window `w`.
    fn write_part(
        &mut self,
        entry: ring::InEntry,
        w: u64,
        part: u32,
        f: &mut impl FnMut(&[u8], &mut [u8], u32) -> Option<Part>,
    ) -> Result<Part> {
        let g = self.half.geometry;
        let input = self.region.range(entry.offset, entry.len as usize)?;
        let out = self.rings.range(g.window_at(w), g.window_bytes as usize)?;
        // SAFETY: the batch is mapped read only and the engine writes no span it posted until
        // the task releases it (DP-I4); the window is the task's from the free ring until it
        // posts it, in another mapping, so the two slices never overlap; both borrows end
        // when this returns, before the caller posts and releases.
        let (input, window) = unsafe {
            (
                core::slice::from_raw_parts(input as *const u8, entry.len as usize),
                core::slice::from_raw_parts_mut(out as *mut u8, g.window_bytes as usize),
            )
        };
        let wrote = f(input, window, part).ok_or(Error::InvalidArgs)?;
        let (Part::Last(len) | Part::More(len)) = wrote;
        if len == 0 || len as u64 > g.window_bytes {
            return Err(Error::OutOfRange);
        }
        Ok(wrote)
    }

    /// Raises the task's note and rings the engine's bell: call it before a port call and
    /// before the task ends, so an engine asleep on its bell reads the task's port and slots.
    pub fn note(&mut self) {
        if self.half.note(&self.rings) {
            self.wakes += 1;
        }
    }

    /// The task's calls on the bells so far: its waits (lingers and sleeps), and its wakes of
    /// the engine. Each is one futex call; nothing else of the rings makes one.
    pub fn bell_calls(&self) -> (u64, u64) {
        (self.sleeps, self.wakes)
    }

    /// Where window `w` starts in this task: for a test that writes a window it posted, which
    /// must fault once the engine has taken it.
    pub fn window_addr(&self, w: u64) -> u64 {
        self.rings.addr + self.half.geometry.window_at(w)
    }

    /// Unmap mode: hands the rings object back with the records posted on it, which leaves
    /// the task, and maps the next one the engine gives (waiting for it).
    pub fn hand_back(&mut self) -> Result<()> {
        check(raw::batch_put(1, self.rings.addr, self.rings.len))?;
        self.rings = Rings::map(1)?;
        self.half = ring::Task::reattach(&self.rings, &self.half).map_err(broken)?;
        Ok(())
    }
}

/// The most of one line a log record holds; the rest is cut, as the personality cuts a
/// Linux task's line.
#[cfg(feature = "log")]
pub const LOG_LINE_MAX: usize = 4096;
/// Lines a task's log keeps a second: the rate the run's log store keeps for one producer.
#[cfg(feature = "log")]
pub const LOG_RATE_LINES: u64 = 2_000;
/// Lines a task's log keeps at once, its second's worth included.
#[cfg(feature = "log")]
pub const LOG_BURST_LINES: u64 = 10_000;
/// Bytes a task's log keeps a second.
#[cfg(feature = "log")]
pub const LOG_RATE_BYTES: u64 = 1 << 20;
/// Bytes a task's log keeps at once, its second's worth included.
#[cfg(feature = "log")]
pub const LOG_BURST_BYTES: u64 = 4 << 20;

/// A native task's log (SDD 29 f.7): its lines, one `moruna-record` record each, in the ring
/// the engine gave it under the start slot `log`. They are the records the personality
/// writes for a Linux task's descriptor 2, and the engine takes them onto the job's updates
/// the same way.
///
/// Lines are held to [`LOG_RATE_LINES`] and [`LOG_RATE_BYTES`] a second, with bursts. A line
/// past the rate, or one that finds the ring full, is dropped and counted. The next line
/// kept, and the log's end, say how many were dropped.
///
/// `writeln!` works on it: each newline ends a record.
#[cfg(feature = "log")]
pub struct Log {
    writer: LogWriter,
    /// What `write_str` was given since its last newline.
    pending: [u8; LOG_LINE_MAX],
    pending_len: usize,
}

#[cfg(feature = "log")]
struct LogWriter {
    region: Shared,
    /// The ring's memory, held while it is mapped.
    _memory: Option<Memory>,
    name: &'static str,
    clock: fn() -> u64,
    seq: u64,
    lines: Bucket,
    bytes: Bucket,
    dropped: u64,
}

/// A token bucket: `rate` a second, at most `burst` at once, in whole tokens.
#[cfg(feature = "log")]
#[derive(Clone, Copy, Debug)]
struct Bucket {
    rate: u64,
    burst: u64,
    tokens: u64,
    /// The time the tokens are counted to.
    at: u64,
}

#[cfg(feature = "log")]
const SECOND: u128 = 1_000_000_000;

#[cfg(feature = "log")]
impl Bucket {
    fn new(rate: u64, burst: u64, now: u64) -> Bucket {
        Bucket {
            rate,
            burst,
            tokens: burst,
            at: now,
        }
    }

    /// Adds the whole tokens earned since `at`; the time a part token stands for counts at
    /// the next refill.
    fn refill(&mut self, now: u64) {
        if self.tokens >= self.burst {
            self.at = now;
            return;
        }
        let earned = now.saturating_sub(self.at) as u128 * self.rate as u128 / SECOND;
        if earned == 0 {
            return;
        }
        let room = (self.burst - self.tokens) as u128;
        if earned >= room {
            self.tokens = self.burst;
            self.at = now;
        } else {
            self.tokens += earned as u64;
            self.at += (earned * SECOND / self.rate as u128) as u64;
        }
    }
}

/// The header naming the stream a line was written to, and the one a task's log writes:
/// what `linux-protocol`'s `log` module names, which the engine reads.
#[cfg(feature = "log")]
const LOG_STREAM: &str = "stream";
#[cfg(feature = "log")]
const LOG_STDERR: &str = "stderr";
/// A record's subject: the descriptor a Linux task's line was written to.
#[cfg(feature = "log")]
const LOG_SUBJECT: u32 = 2;

#[cfg(feature = "log")]
impl Log {
    /// The task's log, from its start slot `log`; `None` when it has none or the slot does
    /// not hold a ring. Takes the slot.
    pub fn take() -> Option<Log> {
        let memory = Memory(crate::start::take("log")?);
        // The ring's size is in its header, after the magic.
        let head = Shared::map(&memory, crate::PAGE_SIZE).ok()?;
        let mut word = [0u8; 8];
        head.read(0, &mut word).ok()?;
        if u64::from_ne_bytes(word) != moruna_record::ring::MAGIC {
            return None;
        }
        head.read(8, &mut word).ok()?;
        let capacity = u64::from_ne_bytes(word);
        if capacity > moruna_record::ring::MAX_CAPACITY {
            return None;
        }
        drop(head);
        let len = (moruna_record::ring::HEADER as u64 + capacity).div_ceil(crate::PAGE_SIZE)
            * crate::PAGE_SIZE;
        let region = Shared::map(&memory, len).ok()?;
        Some(Log::over(
            region,
            Some(memory),
            crate::start::process_name(),
            crate::now,
        ))
    }

    fn over(region: Shared, memory: Option<Memory>, name: &'static str, clock: fn() -> u64) -> Log {
        let now = clock();
        Log {
            writer: LogWriter {
                region,
                _memory: memory,
                name,
                clock,
                seq: 0,
                lines: Bucket::new(LOG_RATE_LINES, LOG_BURST_LINES, now),
                bytes: Bucket::new(LOG_RATE_BYTES, LOG_BURST_BYTES, now),
                dropped: 0,
            },
            pending: [0; LOG_LINE_MAX],
            pending_len: 0,
        }
    }

    /// Writes `text` as one line (cut at [`LOG_LINE_MAX`]): whether it was kept.
    pub fn line(&mut self, text: &[u8]) -> bool {
        self.writer.line(text)
    }

    /// Lines dropped and not yet said.
    pub fn dropped(&self) -> u64 {
        self.writer.dropped
    }

    /// Writes the line `write_str` left without its newline, and says how many lines were
    /// dropped, if any. Dropping the log does this.
    pub fn finish(&mut self) {
        if self.pending_len > 0 {
            let n = core::mem::take(&mut self.pending_len);
            self.writer.line(&self.pending[..n]);
        }
        if self.writer.dropped > 0 {
            let now = (self.writer.clock)();
            self.writer.say_dropped(now);
        }
    }
}

#[cfg(feature = "log")]
impl Log {
    /// Writes `bytes` as `writeln!` does: each newline ends a line, kept or dropped as
    /// [`Log::line`] says, and what follows the last waits for the next write or
    /// [`Log::finish`]. Bytes need not be UTF-8: std's stdout and stderr write here (item 5.0).
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                let n = core::mem::take(&mut self.pending_len);
                self.writer.line(&self.pending[..n]);
            } else if self.pending_len < LOG_LINE_MAX {
                self.pending[self.pending_len] = b;
                self.pending_len += 1;
            }
        }
    }
}

#[cfg(feature = "log")]
impl core::fmt::Write for Log {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

#[cfg(feature = "log")]
impl Drop for Log {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(feature = "log")]
impl core::fmt::Debug for Log {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Log")
            .field("seq", &self.writer.seq)
            .field("dropped", &self.writer.dropped)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "log")]
impl LogWriter {
    fn line(&mut self, text: &[u8]) -> bool {
        let now = (self.clock)();
        let body = &text[..text.len().min(LOG_LINE_MAX)];
        self.lines.refill(now);
        self.bytes.refill(now);
        if self.lines.tokens == 0 || self.bytes.tokens < body.len() as u64 {
            self.dropped += 1;
            return false;
        }
        self.lines.tokens -= 1;
        self.bytes.tokens -= body.len() as u64;
        if self.dropped > 0 {
            self.say_dropped(now);
        }
        if self.push(now, body) {
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    /// A line saying how many lines were dropped; the count starts again once it is kept.
    fn say_dropped(&mut self, now: u64) {
        let mut text = Text::<96>::default();
        let _ = core::fmt::Write::write_fmt(
            &mut text,
            format_args!(
                "moruna: {} log lines dropped (over the log's rate, or its ring was full)",
                self.dropped
            ),
        );
        if self.push(now, &text.buf[..text.len]) {
            self.dropped = 0;
        }
    }

    fn push(&mut self, now: u64, body: &[u8]) -> bool {
        // SAFETY: the mapping lives as long as `self.region`; the ring's protocol (atomic
        // counters, each slot owned by one side at a time) keeps the engine's reads and these
        // writes apart, as `Mapping::shared_region` says.
        let region = unsafe {
            core::slice::from_raw_parts_mut(self.region.addr as *mut u8, self.region.len as usize)
        };
        let Ok(ring) = moruna_record::ring::Ring::attach(region) else {
            return false;
        };
        self.seq += 1;
        ring.producer()
            .push_record(
                self.seq,
                now as i64,
                LOG_SUBJECT,
                self.name,
                &[(LOG_STREAM, LOG_STDERR)],
                body,
            )
            .is_ok()
    }
}

/// Text formatted into a fixed buffer, cut at its end.
#[cfg(feature = "log")]
struct Text<const N: usize> {
    buf: [u8; N],
    len: usize,
}

#[cfg(feature = "log")]
impl<const N: usize> Default for Text<N> {
    fn default() -> Self {
        Text {
            buf: [0; N],
            len: 0,
        }
    }
}

#[cfg(feature = "log")]
impl<const N: usize> core::fmt::Write for Text<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(N - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[cfg(all(test, feature = "log"))]
mod log_tests {
    extern crate std;

    use super::*;
    use core::fmt::Write;
    use moruna_record::record::{Decoded, decode};
    use moruna_record::ring::Ring;
    use std::cell::Cell;
    use std::string::String;
    use std::vec::Vec;

    std::thread_local! {
        static NOW: Cell<u64> = const { Cell::new(1_000_000_000) };
    }

    fn clock() -> u64 {
        NOW.with(Cell::get)
    }

    fn advance(ns: u64) {
        NOW.with(|n| n.set(n.get() + ns));
    }

    #[repr(C, align(64))]
    #[derive(Clone, Copy)]
    struct Block([u8; 64]);

    /// A formatted ring of `bytes` and a log over it.
    fn log(bytes: usize) -> (Vec<Block>, Log) {
        let mut memory = std::vec![Block([0; 64]); bytes / 64];
        let (addr, len) = (memory.as_mut_ptr() as u64, (memory.len() * 64) as u64);
        // SAFETY: the blocks outlive the log in every test, which drops the log first.
        let region = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len as usize) };
        Ring::init(region).expect("format");
        let log = Log::over(Shared::slot(addr, len), None, "task.t", clock);
        (memory, log)
    }

    /// The lines in the ring, taken out as the engine takes them: stream, subject, text.
    fn drain(memory: &mut [Block]) -> Vec<(String, u32, String)> {
        let len = memory.len() * 64;
        // SAFETY: the test's own buffer; the log writes only between calls.
        let region =
            unsafe { core::slice::from_raw_parts_mut(memory.as_mut_ptr() as *mut u8, len) };
        let ring = Ring::attach(region).expect("attach");
        let mut consumer = ring.consumer().expect("consumer");
        let mut out = Vec::new();
        while let Some(bytes) = consumer.pop() {
            match decode(bytes) {
                Decoded::Record(r, _) => {
                    let stream = r
                        .headers
                        .iter()
                        .find(|(k, _)| *k == LOG_STREAM)
                        .map(|(_, v)| String::from(*v))
                        .unwrap_or_default();
                    assert_eq!(r.msg_id, "task.t");
                    out.push((
                        stream,
                        r.subject,
                        String::from_utf8_lossy(r.body).into_owned(),
                    ));
                }
                other => panic!("a record that does not decode: {other:?}"),
            }
        }
        out
    }

    fn texts(lines: &[(String, u32, String)]) -> Vec<&str> {
        lines.iter().map(|(_, _, t)| t.as_str()).collect()
    }

    #[test]
    fn p_a_line_is_a_standard_error_record_the_engine_reads() {
        let (mut memory, mut log) = log(64 << 10);
        assert!(log.line(b"first"));
        writeln!(log, "batch {} of {}", 3, 4).unwrap();
        write!(log, "no newline").unwrap();
        log.finish();
        drop(log);
        let lines = drain(&mut memory);
        assert_eq!(texts(&lines), ["first", "batch 3 of 4", "no newline"]);
        for (stream, subject, _) in &lines {
            assert_eq!((stream.as_str(), *subject), ("stderr", 2));
        }
    }

    #[test]
    fn p_bytes_written_in_pieces_are_lines_at_their_newlines_utf8_or_not() {
        // std's stdout and stderr (item 5.0) write bytes, a line in several writes or several
        // lines in one, and need not write UTF-8.
        let (mut memory, mut log) = log(64 << 10);
        log.write_bytes(b"sum = ");
        log.write_bytes(b"55\nfilter kept ");
        log.write_bytes(b"3\n\xff\xfe raw\n");
        drop(log);
        let lines = drain(&mut memory);
        assert_eq!(texts(&lines)[..2], ["sum = 55", "filter kept 3"]);
        assert_eq!(lines[2].2.as_bytes()[..6], *"\u{fffd}\u{fffd}".as_bytes());
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn p_a_long_line_is_cut_and_a_line_with_newlines_is_several() {
        let (mut memory, mut log) = log(64 << 10);
        let long = std::vec![b'x'; LOG_LINE_MAX + 100];
        assert!(log.line(&long));
        write!(log, "a\nb\n").unwrap();
        // `write_str` holds at most a line's worth too.
        log.write_str(&"y".repeat(LOG_LINE_MAX + 7)).unwrap();
        drop(log);
        let lines = drain(&mut memory);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].2.len(), LOG_LINE_MAX);
        assert_eq!((lines[1].2.as_str(), lines[2].2.as_str()), ("a", "b"));
        assert_eq!(lines[3].2.len(), LOG_LINE_MAX);
    }

    #[test]
    fn p_lines_past_the_rate_are_dropped_counted_and_said() {
        let (mut memory, mut log) = log(2 << 20);
        let mut kept = 0;
        for i in 0..LOG_BURST_LINES + 5 {
            if log.line(std::format!("line {i}").as_bytes()) {
                kept += 1;
            }
            if i % 1000 == 999 {
                drain(&mut memory);
            }
        }
        assert_eq!(kept, LOG_BURST_LINES, "the burst, at one instant");
        assert_eq!(log.dropped(), 5);
        drain(&mut memory);
        // A millisecond earns two lines at 2,000 a second.
        advance(1_000_000);
        assert!(log.line(b"after"));
        assert!(log.line(b"after, again"));
        assert!(!log.line(b"one too many"));
        drop(log);
        let lines = drain(&mut memory);
        assert_eq!(
            texts(&lines),
            [
                "moruna: 5 log lines dropped (over the log's rate, or its ring was full)",
                "after",
                "after, again",
                "moruna: 1 log lines dropped (over the log's rate, or its ring was full)",
            ]
        );
    }

    #[test]
    fn p_bytes_past_the_rate_are_dropped_with_lines_to_spare() {
        let (mut memory, mut log) = log(1 << 20);
        let line = std::vec![b'z'; LOG_LINE_MAX];
        let per_burst = LOG_BURST_BYTES / LOG_LINE_MAX as u64;
        for i in 0..per_burst {
            assert!(log.line(&line), "line {i} is in the byte burst");
            if i % 32 == 31 {
                drain(&mut memory);
            }
        }
        assert!(!log.line(&line), "the burst's bytes are spent");
        assert!(log.line(b""), "an empty line costs no byte");
        assert_eq!(log.dropped(), 0, "said before the empty line");
        // A second earns a second's bytes back.
        advance(1_000_000_000);
        assert!(log.line(&line));
    }

    #[test]
    fn p_a_full_ring_drops_and_the_next_line_kept_says_how_many() {
        let (mut memory, mut log) = log(1024 + 320);
        let mut sent = 0;
        while log.line(b"filling the ring") {
            sent += 1;
        }
        assert!(sent > 0);
        assert!(!log.line(b"still full"));
        assert_eq!(log.dropped(), 2);
        assert_eq!(drain(&mut memory).len(), sent);
        assert!(log.line(b"room again"));
        drop(log);
        assert_eq!(
            texts(&drain(&mut memory)),
            [
                "moruna: 2 log lines dropped (over the log's rate, or its ring was full)",
                "room again"
            ]
        );
    }

    #[test]
    fn p_a_bucket_keeps_the_time_a_part_token_stands_for() {
        let mut b = Bucket::new(2_000, 3, 0);
        b.tokens = 0;
        b.refill(300_000);
        assert_eq!(b.tokens, 0, "0.3 ms is 0.6 of a token");
        b.refill(600_000);
        assert_eq!(
            (b.tokens, b.at),
            (1, 500_000),
            "1.2 tokens: one, and 0.1 ms kept"
        );
        b.refill(10_000_000_000);
        assert_eq!(
            (b.tokens, b.at),
            (3, 10_000_000_000),
            "never past the burst"
        );
        b.refill(10_000_000_000 + 400_000);
        assert_eq!(
            b.at,
            10_000_000_000 + 400_000,
            "a full bucket earns nothing"
        );
    }
}
