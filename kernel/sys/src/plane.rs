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
}

/// A task's side of ring mode: its region and its rings object, each mapped once, from
/// slots 0 and 1 of its plane.
#[derive(Debug)]
pub struct Rings {
    region: Shared,
    rings: Shared,
    half: ring::Task,
}

fn broken(_: ring::Bad) -> Error {
    Error::BadState
}

impl Rings {
    fn map(slot: u64) -> Result<Shared> {
        let (rax, rdx) = raw::batch_get(slot);
        let (addr, len) = region(rax, rdx)?.ok_or(Error::PeerClosed)?;
        Ok(Shared::slot(addr, len))
    }

    /// Maps the region (slot 0) and the rings object (slot 1) once and attaches. Takes both
    /// slots of `plane`, so nothing else of the task can unmap them.
    pub fn attach(plane: &Plane) -> Result<Rings> {
        if !(plane.slot(0) && plane.slot(1)) {
            return Err(Error::BadState);
        }
        let region = Rings::map(0)?;
        let rings = Rings::map(1)?;
        let half = ring::Task::attach(&rings).map_err(broken)?;
        Ok(Rings {
            region,
            rings,
            half,
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

    /// Releases the oldest batch taken.
    pub fn release(&mut self) -> bool {
        self.half.release(&self.rings)
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

    /// Posts the record of `len` bytes at the start of window `w`, answering `cookie`.
    pub fn post(&mut self, w: u64, len: u64, cookie: u64) -> bool {
        let posted = self.half.post(
            &self.rings,
            ring::OutEntry {
                window: w,
                offset: 0,
                len,
                cookie,
            },
        );
        if posted && self.half.mode == ring::Mode::Protect && self.half.engine_waits(&self.rings) {
            let _ = self.call(ring::layout::WAKE, 0);
        }
        posted
    }

    fn call(&mut self, want: u8, seen: u64) -> Result<()> {
        let mut body = [0u8; 16];
        body[0] = want;
        body[8..].copy_from_slice(&seen.to_le_bytes());
        self.half
            .set_waiting(&self.rings, want != ring::layout::WAKE);
        let (rax, _) = raw::port_call(ring::layout::WAIT_PORT, &body);
        self.half.set_waiting(&self.rings, false);
        check(rax).map(|_| ())
    }

    /// Waits for the next batch or the end (the input ring is empty).
    pub fn wait_input(&mut self) -> Result<()> {
        use ring::Shared as _;
        let seen = self.rings.load(ring::layout::IN_HEAD);
        self.call(ring::layout::WAIT_INPUT, seen)
    }

    /// Waits for a window (none is free).
    pub fn wait_window(&mut self) -> Result<()> {
        use ring::Shared as _;
        let seen = self.rings.load(ring::layout::FREE_HEAD);
        self.call(ring::layout::WAIT_WINDOW, seen)
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
