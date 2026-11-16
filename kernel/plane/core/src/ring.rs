//! The rings between the engine and one task (`architecture/data-os.md` §5): the layout of
//! the shared object, and each side's half of the protocol.
//!
//! A task in ring mode has two objects, each mapped into it once:
//!
//! - its **input region**, read only, on slot 0: the records of the batches it is given,
//!   which the engine writes there and names on the input ring;
//! - its **rings object**, writable, on slot 1: a control page, the entries of three rings,
//!   and the output windows the task writes its records into.
//!
//! The three rings are single producer, single consumer, with indices that only grow:
//!
//! - **input** (the engine posts a batch, the task releases it);
//! - **output** (the task posts a record it wrote in a window, the engine takes it);
//! - **free** (the engine gives a window, the task takes it).
//!
//! The task can write every byte of the rings object, so the engine never reads back a word
//! it wrote: it keeps its own copies, and checks every word the task writes against them
//! before it believes it ([`Bad`] when a word could not be true). No word the task writes can
//! make the engine read or write outside its own mappings.
//!
//! This file is plain `core`, with no allocation and no unsafe code: `moruna-sys` includes it
//! for the task's half, and the Python shim speaks the same layout.

/// The layout of the rings object, in bytes from its start.
pub mod layout {
    /// "MORUNARG" as a little-endian `u64`.
    pub const MAGIC_VALUE: u64 = u64::from_le_bytes(*b"MORUNARG");
    /// The protocol's version.
    pub const VERSION_VALUE: u64 = 1;

    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 8;
    /// The input region's size.
    pub const REGION_BYTES: usize = 16;
    pub const IN_CAP: usize = 24;
    pub const OUT_CAP: usize = 32;
    pub const FREE_CAP: usize = 40;
    pub const WINDOWS: usize = 48;
    pub const WINDOW_BYTES: usize = 56;
    /// Where window 0 starts in the rings object.
    pub const WINDOWS_AT: usize = 64;
    /// How a record leaves the task's reach: [`super::Mode`].
    pub const MODE: usize = 72;
    /// Raised each time the engine hands the task a new rings object (unmap mode).
    pub const GENERATION: usize = 80;

    // The moving words, a cache line each.
    /// Input entries the engine has posted.
    pub const IN_HEAD: usize = 128;
    /// Input entries the task has released.
    pub const IN_TAIL: usize = 192;
    /// Output entries the task has posted.
    pub const OUT_HEAD: usize = 256;
    /// Output entries the engine has taken.
    pub const OUT_TAIL: usize = 320;
    /// Windows the engine has given.
    pub const FREE_HEAD: usize = 384;
    /// Windows the task has taken.
    pub const FREE_TAIL: usize = 448;
    /// Nonzero while the engine sleeps waiting for an output.
    pub const ENGINE_WAITS: usize = 512;
    /// Nonzero while the task sleeps waiting for an input or a window.
    pub const TASK_WAITS: usize = 576;
    /// Nonzero once the engine posts no more input.
    pub const CLOSED: usize = 640;
    /// The protocol version the task attached with.
    pub const ATTACHED: usize = 704;

    /// The entries start on the second page.
    pub const ENTRIES_AT: usize = 4096;
    /// An input entry: offset in the region, length, cookie, rows.
    pub const IN_ENTRY: usize = 32;
    /// An output entry: window, offset in it, length, cookie of the input it answers.
    pub const OUT_ENTRY: usize = 32;
    /// A free entry: a window's number.
    pub const FREE_ENTRY: usize = 8;

    /// One page.
    pub const PAGE: u64 = 4096;
    /// The most entries a ring holds.
    pub const CAP_MAX: u64 = 1 << 16;
    /// The most windows a rings object holds.
    pub const WINDOWS_MAX: u64 = 1 << 12;

    /// The port number a task calls to wait (`port_call`): real ports are numbered from 1.
    pub const WAIT_PORT: u64 = 0;
    /// A wait's request byte: for an input, or the end.
    pub const WAIT_INPUT: u8 = b'I';
    /// A wait's request byte: for a free window.
    pub const WAIT_WINDOW: u8 = b'W';
    /// A request byte that only wakes the engine, answered at once.
    pub const WAKE: u8 = b'N';
}

use layout::*;

/// How a record the task posted leaves its reach before the engine checks it (DP1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The engine takes write access to the posted windows back itself, a group at a time
    /// (`windows_protect`); the task keeps its mapping.
    Protect = 1,
    /// The task hands the whole rings object back with `batch_put`, which unmaps it; the
    /// engine checks what it posted and gives it a new rings object (today's path).
    Unmap = 2,
}

impl Mode {
    pub fn from_word(w: u64) -> Option<Mode> {
        match w {
            1 => Some(Mode::Protect),
            2 => Some(Mode::Unmap),
            _ => None,
        }
    }
}

/// A protocol word that could not be true: the side that wrote it is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bad(pub &'static str);

/// The words of a shared object, each an aligned `u64`.
pub trait Shared {
    /// The word at byte `at`, with acquire ordering.
    fn load(&self, at: usize) -> u64;
    /// Sets the word at byte `at`, with release ordering.
    fn store(&self, at: usize, value: u64);
    /// A full fence: stores before it are seen before loads after it.
    fn fence(&self);
}

/// The sizes of a task's rings and windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub region_bytes: u64,
    pub in_cap: u64,
    pub out_cap: u64,
    pub free_cap: u64,
    pub windows: u64,
    pub window_bytes: u64,
}

fn pages(bytes: u64) -> Option<u64> {
    bytes.checked_add(PAGE - 1).map(|b| b & !(PAGE - 1))
}

impl Geometry {
    /// A geometry of `windows` windows of `window_bytes` and an input region of
    /// `region_bytes`, with rings sized for them.
    pub fn new(region_bytes: u64, windows: u64, window_bytes: u64) -> Result<Geometry, Bad> {
        let free_cap = windows
            .max(1)
            .checked_next_power_of_two()
            .ok_or(Bad("windows"))?;
        let g = Geometry {
            region_bytes,
            in_cap: 64,
            out_cap: free_cap.max(64),
            free_cap,
            windows,
            window_bytes,
        };
        g.check()?;
        Ok(g)
    }

    /// Whether the sizes can be laid out.
    pub fn check(&self) -> Result<(), Bad> {
        let cap = |c: u64| c.is_power_of_two() && c <= CAP_MAX;
        if !cap(self.in_cap) || !cap(self.out_cap) || !cap(self.free_cap) {
            return Err(Bad("a ring's capacity is not a power of two within bounds"));
        }
        if self.windows == 0 || self.windows > WINDOWS_MAX || self.windows > self.free_cap {
            return Err(Bad("the windows do not fit the free ring"));
        }
        if self.window_bytes == 0 || !self.window_bytes.is_multiple_of(PAGE) {
            return Err(Bad("a window is not whole pages"));
        }
        if self.region_bytes == 0 || !self.region_bytes.is_multiple_of(PAGE) {
            return Err(Bad("the region is not whole pages"));
        }
        self.object_bytes()
            .map(|_| ())
            .ok_or(Bad("the rings object is past u64"))
    }

    /// Where the input entries start.
    pub fn in_entries(&self) -> usize {
        ENTRIES_AT
    }

    /// Where the output entries start.
    pub fn out_entries(&self) -> usize {
        ENTRIES_AT + self.in_cap as usize * IN_ENTRY
    }

    /// Where the free entries start.
    pub fn free_entries(&self) -> usize {
        self.out_entries() + self.out_cap as usize * OUT_ENTRY
    }

    /// Where window 0 starts: the page after the entries.
    pub fn windows_at(&self) -> u64 {
        let end = self.free_entries() as u64 + self.free_cap * FREE_ENTRY as u64;
        pages(end).unwrap_or(u64::MAX)
    }

    /// Where window `w` starts in the rings object.
    pub fn window_at(&self, w: u64) -> u64 {
        self.windows_at() + w * self.window_bytes
    }

    /// The rings object's size.
    pub fn object_bytes(&self) -> Option<u64> {
        self.windows
            .checked_mul(self.window_bytes)?
            .checked_add(self.windows_at())
    }

    /// Writes the header of a rings object.
    pub fn write(&self, s: &impl Shared, mode: Mode, generation: u64) {
        s.store(VERSION, VERSION_VALUE);
        s.store(REGION_BYTES, self.region_bytes);
        s.store(IN_CAP, self.in_cap);
        s.store(OUT_CAP, self.out_cap);
        s.store(FREE_CAP, self.free_cap);
        s.store(WINDOWS, self.windows);
        s.store(WINDOW_BYTES, self.window_bytes);
        s.store(WINDOWS_AT, self.windows_at());
        s.store(MODE, mode as u64);
        s.store(GENERATION, generation);
        // The magic last: a task that sees it sees the rest.
        s.store(MAGIC, MAGIC_VALUE);
    }

    /// The header of a rings object, as the task reads it.
    pub fn read(s: &impl Shared) -> Result<(Geometry, Mode), Bad> {
        if s.load(MAGIC) != MAGIC_VALUE {
            return Err(Bad("no rings object"));
        }
        if s.load(VERSION) != VERSION_VALUE {
            return Err(Bad("another version of the rings"));
        }
        let g = Geometry {
            region_bytes: s.load(REGION_BYTES),
            in_cap: s.load(IN_CAP),
            out_cap: s.load(OUT_CAP),
            free_cap: s.load(FREE_CAP),
            windows: s.load(WINDOWS),
            window_bytes: s.load(WINDOW_BYTES),
        };
        g.check()?;
        if s.load(WINDOWS_AT) != g.windows_at() {
            return Err(Bad("the windows are not where the sizes put them"));
        }
        let mode = Mode::from_word(s.load(MODE)).ok_or(Bad("an unknown mode"))?;
        Ok((g, mode))
    }
}

/// A batch on the input ring: its record at `offset` of the region, `len` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InEntry {
    pub offset: u64,
    pub len: u64,
    pub cookie: u64,
    pub rows: u64,
}

/// A record on the output ring: `len` bytes at `offset` of window `window`, answering the
/// input with `cookie`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutEntry {
    pub window: u64,
    pub offset: u64,
    pub len: u64,
    pub cookie: u64,
}

/// The engine's half: it posts inputs and gives windows, and takes outputs.
#[derive(Clone, Debug)]
pub struct Engine {
    pub geometry: Geometry,
    in_head: u64,
    in_tail: u64,
    out_tail: u64,
    out_head: u64,
    free_head: u64,
    free_tail: u64,
    closed: bool,
}

impl Engine {
    /// The engine's half of a new rings object, its header written.
    pub fn format(s: &impl Shared, geometry: Geometry, mode: Mode, generation: u64) -> Engine {
        for at in [
            IN_HEAD,
            IN_TAIL,
            OUT_HEAD,
            OUT_TAIL,
            FREE_HEAD,
            FREE_TAIL,
            ENGINE_WAITS,
            TASK_WAITS,
            CLOSED,
            ATTACHED,
        ] {
            s.store(at, 0);
        }
        geometry.write(s, mode, generation);
        Engine {
            geometry,
            in_head: 0,
            in_tail: 0,
            out_tail: 0,
            out_head: 0,
            free_head: 0,
            free_tail: 0,
            closed: false,
        }
    }

    /// The same half on a new rings object (unmap mode): the input ring as it stands, with
    /// `unreleased` (the engine's own copies of the entries from the tail on) written again,
    /// and the output and free rings empty.
    pub fn carry(
        &self,
        to: &impl Shared,
        mode: Mode,
        generation: u64,
        unreleased: impl IntoIterator<Item = InEntry>,
    ) -> Engine {
        let mut next = Engine::format(to, self.geometry, mode, generation);
        let g = &self.geometry;
        let flight = unreleased.into_iter().take(self.in_flight() as usize);
        for (i, e) in (self.in_tail..).zip(flight) {
            let at = g.in_entries() + (i % g.in_cap) as usize * IN_ENTRY;
            to.store(at, e.offset);
            to.store(at + 8, e.len);
            to.store(at + 16, e.cookie);
            to.store(at + 24, e.rows);
        }
        next.in_head = self.in_head;
        next.in_tail = self.in_tail;
        next.closed = self.closed;
        to.store(IN_TAIL, self.in_tail);
        to.store(CLOSED, u64::from(self.closed));
        to.store(IN_HEAD, self.in_head);
        next
    }

    /// Inputs posted and not yet released.
    pub fn in_flight(&self) -> u64 {
        self.in_head - self.in_tail
    }

    /// Whether the input ring has room.
    pub fn can_post(&self) -> bool {
        self.in_flight() < self.geometry.in_cap
    }

    /// Posts `entry`; `false` when the ring is full or closed.
    pub fn post(&mut self, s: &impl Shared, entry: InEntry) -> bool {
        if !self.can_post() || self.closed {
            return false;
        }
        let g = &self.geometry;
        let at = g.in_entries() + (self.in_head % g.in_cap) as usize * IN_ENTRY;
        s.store(at, entry.offset);
        s.store(at + 8, entry.len);
        s.store(at + 16, entry.cookie);
        s.store(at + 24, entry.rows);
        self.in_head += 1;
        s.store(IN_HEAD, self.in_head);
        true
    }

    /// No more inputs.
    pub fn close(&mut self, s: &impl Shared) {
        self.closed = true;
        s.store(CLOSED, 1);
    }

    /// How many more inputs the task has released since the last call, in posting order.
    pub fn released(&mut self, s: &impl Shared) -> Result<u64, Bad> {
        let t = s.load(IN_TAIL);
        if t < self.in_tail || t > self.in_head {
            return Err(Bad("the task released an input it was not given"));
        }
        let n = t - self.in_tail;
        self.in_tail = t;
        Ok(n)
    }

    /// Gives window `w`; `false` when the free ring is full.
    pub fn give(&mut self, s: &impl Shared, w: u64) -> Result<bool, Bad> {
        self.given(s)?;
        let g = &self.geometry;
        if self.free_head - self.free_tail >= g.free_cap || w >= g.windows {
            return Ok(false);
        }
        let at = g.free_entries() + (self.free_head % g.free_cap) as usize * FREE_ENTRY;
        s.store(at, w);
        self.free_head += 1;
        s.store(FREE_HEAD, self.free_head);
        Ok(true)
    }

    /// Windows given and not yet taken by the task.
    pub fn given(&mut self, s: &impl Shared) -> Result<u64, Bad> {
        let t = s.load(FREE_TAIL);
        if t < self.free_tail || t > self.free_head {
            return Err(Bad("the task took a window it was not given"));
        }
        self.free_tail = t;
        Ok(self.free_head - t)
    }

    /// Outputs the task has posted and the engine has not taken.
    pub fn posted(&mut self, s: &impl Shared) -> Result<u64, Bad> {
        let h = s.load(OUT_HEAD);
        if h < self.out_head || h - self.out_tail > self.geometry.out_cap {
            return Err(Bad("the task posted outputs past its ring"));
        }
        self.out_head = h;
        Ok(h - self.out_tail)
    }

    /// The next output the task posted, its window and extent checked against the
    /// geometry (whether the task held that window is the caller's to know).
    pub fn take(&mut self, s: &impl Shared) -> Result<Option<OutEntry>, Bad> {
        if self.out_tail == self.out_head && self.posted(s)? == 0 {
            return Ok(None);
        }
        let g = &self.geometry;
        let at = g.out_entries() + (self.out_tail % g.out_cap) as usize * OUT_ENTRY;
        let e = OutEntry {
            window: s.load(at),
            offset: s.load(at + 8),
            len: s.load(at + 16),
            cookie: s.load(at + 24),
        };
        self.out_tail += 1;
        s.store(OUT_TAIL, self.out_tail);
        if e.window >= g.windows {
            return Err(Bad("the task posted a record in a window that is not one"));
        }
        if !e.offset.is_multiple_of(PAGE) {
            return Err(Bad(
                "the task posted a record that does not start on a page",
            ));
        }
        if e.len == 0
            || e.offset
                .checked_add(e.len)
                .is_none_or(|end| end > g.window_bytes)
        {
            return Err(Bad("the task posted a record past its window"));
        }
        Ok(Some(e))
    }

    /// Says whether the engine is about to sleep; going to sleep, it reads the ring again
    /// after the fence, so a post the task made before seeing the word is not missed.
    pub fn set_waiting(&mut self, s: &impl Shared, waiting: bool) -> Result<u64, Bad> {
        s.store(ENGINE_WAITS, u64::from(waiting));
        s.fence();
        self.posted(s)
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The input entries not yet released, oldest first, as (index, entry).
    pub fn unreleased(&self) -> core::ops::Range<u64> {
        self.in_tail..self.in_head
    }
}

/// The task's half: it takes inputs and windows, and posts outputs.
#[derive(Clone, Debug)]
pub struct Task {
    pub geometry: Geometry,
    pub mode: Mode,
    pub generation: u64,
    in_tail: u64,
    in_next: u64,
    out_head: u64,
    free_tail: u64,
}

impl Task {
    /// The task's half of the rings object `s`, attached.
    pub fn attach(s: &impl Shared) -> Result<Task, Bad> {
        let (geometry, mode) = Geometry::read(s)?;
        let t = Task {
            geometry,
            mode,
            generation: s.load(GENERATION),
            in_tail: s.load(IN_TAIL),
            in_next: s.load(IN_TAIL),
            out_head: s.load(OUT_HEAD),
            free_tail: s.load(FREE_TAIL),
        };
        s.store(ATTACHED, VERSION_VALUE);
        Ok(t)
    }

    /// The next input, which stays the task's until it releases it; `None` while there is
    /// none.
    pub fn next(&mut self, s: &impl Shared) -> Result<Option<InEntry>, Bad> {
        let head = s.load(IN_HEAD);
        if head < self.in_next || head - self.in_tail > self.geometry.in_cap {
            return Err(Bad("the engine's input ring is not one"));
        }
        if self.in_next == head {
            return Ok(None);
        }
        let g = &self.geometry;
        let at = g.in_entries() + (self.in_next % g.in_cap) as usize * IN_ENTRY;
        let e = InEntry {
            offset: s.load(at),
            len: s.load(at + 8),
            cookie: s.load(at + 16),
            rows: s.load(at + 24),
        };
        if e.offset
            .checked_add(e.len)
            .is_none_or(|end| end > g.region_bytes)
        {
            return Err(Bad("an input past the region"));
        }
        self.in_next += 1;
        Ok(Some(e))
    }

    /// Releases the oldest input taken.
    pub fn release(&mut self, s: &impl Shared) -> bool {
        if self.in_tail == self.in_next {
            return false;
        }
        self.in_tail += 1;
        s.store(IN_TAIL, self.in_tail);
        true
    }

    /// Whether the engine posts no more and every input was taken.
    pub fn ended(&self, s: &impl Shared) -> bool {
        s.load(CLOSED) != 0 && s.load(IN_HEAD) == self.in_next
    }

    /// A window to write the next record in; `None` while the engine has given none.
    pub fn window(&mut self, s: &impl Shared) -> Result<Option<u64>, Bad> {
        let head = s.load(FREE_HEAD);
        if head < self.free_tail || head - self.free_tail > self.geometry.free_cap {
            return Err(Bad("the engine's free ring is not one"));
        }
        if head == self.free_tail {
            return Ok(None);
        }
        let g = &self.geometry;
        let at = g.free_entries() + (self.free_tail % g.free_cap) as usize * FREE_ENTRY;
        let w = s.load(at);
        if w >= g.windows {
            return Err(Bad("the engine gave a window that is not one"));
        }
        self.free_tail += 1;
        s.store(FREE_TAIL, self.free_tail);
        Ok(Some(w))
    }

    /// Posts a record; `false` when the output ring is full.
    pub fn post(&mut self, s: &impl Shared, e: OutEntry) -> bool {
        let g = &self.geometry;
        if self.out_head - s.load(OUT_TAIL) >= g.out_cap {
            return false;
        }
        let at = g.out_entries() + (self.out_head % g.out_cap) as usize * OUT_ENTRY;
        s.store(at, e.window);
        s.store(at + 8, e.offset);
        s.store(at + 16, e.len);
        s.store(at + 24, e.cookie);
        self.out_head += 1;
        s.store(OUT_HEAD, self.out_head);
        true
    }

    /// Whether the engine sleeps waiting for an output, read after a fence so the post just
    /// made is ordered before it.
    pub fn engine_waits(&self, s: &impl Shared) -> bool {
        s.fence();
        s.load(ENGINE_WAITS) != 0
    }

    /// Says whether the task is about to sleep.
    pub fn set_waiting(&self, s: &impl Shared, waiting: bool) {
        s.store(TASK_WAITS, u64::from(waiting));
        s.fence();
    }

    /// Records posted on this rings object.
    pub fn posted(&self) -> u64 {
        self.out_head
    }
}
