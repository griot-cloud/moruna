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
//! Each side sleeps only when it has nothing to do, on a futex word of the rings object: its
//! **bell**, which the other side raises after each post and wakes only when the sleeper has
//! said it waits. The kernel keys a futex on a shared memory object by the object and the
//! offset, so the wake meets the waiter in the other process.
//!
//! **The plane batches by design** (item 6.3p). A task that runs out of input while its engine
//! is posting does not sleep until the next post: it **lingers** ([`layout::WAITS_LINGER`] in
//! [`layout::TASK_WAITS`]) until a deadline of [`GROUP`] posting intervals, which it measures
//! itself ([`Pace`]), and lingers only when that is within [`LINGER_MAX_NS`]. The engine does
//! not wake a lingering task for each post: it wakes it once [`GROUP`] posts wait, before it
//! blocks waiting on the plane ([`Engine::flush`]), and when it closes the input
//! ([`Engine::ring_now`]). A task that sleeps ([`layout::WAITS_SLEEP`]), its engine posting too
//! slowly for a linger, is woken a group at a time too when the engine reads a clock
//! ([`Engine::ring_at`], rule (d)): once [`GROUP`] posts wait, or a group of the engine's own
//! posting intervals after the first, before the engine blocks, and when it leaves the plane for
//! other work ([`Engine::leave`]). So a task woken at once, by a free CPU or a prompt scheduler,
//! still takes its batches a group at a time, and an engine never waits out a linger or a group:
//! it wakes the task before it blocks. The value is an extension of the word's meaning with no change of layout: an
//! engine of before treats any nonzero word as a wait and wakes the task at each post, and a
//! task of before never lingers.
//!
//! The three rings are single producer, single consumer, with indices that only grow:
//!
//! - **input** (the engine posts a batch, the task releases it once it is done with it);
//! - **output** (the task posts a record it wrote in a window, the engine takes it);
//! - **free** (the engine gives a window, the task takes it).
//!
//! A task answers a batch with one record or several, each in a window of its own, and then
//! releases the batch: its release says the batch is done, and the engine takes every record
//! posted before it as the batch's answer. A window is sized from the plane's budget, never
//! from a guess of how much bigger a stage's output is than its input.
//!
//! The task says its exit code in the control page before it ends ([`Task::exit`]): a task the
//! engine starts itself has no other way to tell it.
//!
//! **A release carries the piece's time** (item 6.3z). A task that attached at
//! [`layout::ATTACHED_TIMED`] writes the function time it spent on a batch ([`PieceTimes`]) into
//! the batch's input entry, in its `rows` word ([`layout::IN_PIECE_NS`]), before it stores the
//! release: no layout change, since the engine never reads an entry back (it keeps its own
//! copies) and posts into the entry's slot again only after the release has passed it. The engine
//! reads that word only from a task that attached so, and only for an input it has seen released
//! and not yet posted over ([`Engine::piece_ns`]); every other task's release has no time. An
//! engine of before never reads the word, and a task of before (the Python shim's) attaches at
//! [`layout::VERSION_VALUE`].
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
    /// The protocol's version: 4 since a task answers a batch with several records and says
    /// its exit code.
    pub const VERSION_VALUE: u64 = 4;

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
    /// The CPU the engine places the task on, plus one; zero leaves it where the kernel put
    /// it. The task pins its thread there as it attaches (`moruna_sys::plane::Rings`): the
    /// kernel keeps a thread on the CPU it first ran on, and a stage's tasks, each started
    /// while the others slept, all went to one CPU (the NYC gate, 2026-11-17). A word of
    /// protocol 4 that a task of an older engine reads as zero.
    pub const PLACE: usize = 88;

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
    /// Nonzero while the task sleeps waiting for an input or a window: [`WAITS_SLEEP`] or
    /// [`WAITS_LINGER`]. The engine clears it when it wakes the task, which sets it again the
    /// next time it waits.
    pub const TASK_WAITS: usize = 576;
    /// Nonzero once the engine posts no more input.
    pub const CLOSED: usize = 640;
    /// The protocol version the task attached with: [`VERSION_VALUE`] for a task of before,
    /// [`ATTACHED_TIMED`] for one whose releases carry its piece's time.
    pub const ATTACHED: usize = 704;
    /// The task's bell: a 32-bit futex word (the low half of this word) the engine raises
    /// after it posts, gives a window or closes, and wakes when the task waits on it.
    pub const TASK_BELL: usize = 768;
    /// The engine's bell: raised by the task after each record it posts.
    pub const ENGINE_BELL: usize = 832;
    /// Raised by the task before a port call and before it ends: the engine reads the task's
    /// port and slots, which a sleep on its bell does not watch.
    pub const TASK_NOTE: usize = 896;
    /// One once the task said its exit code, in [`TASK_EXIT_CODE`], just before it ends.
    pub const TASK_EXIT: usize = 960;
    /// The exit code the task said, an `i64`.
    pub const TASK_EXIT_CODE: usize = 968;

    /// The entries start on the second page.
    pub const ENTRIES_AT: usize = 4096;
    /// An input entry: offset in the region, length, cookie, rows.
    pub const IN_ENTRY: usize = 32;
    /// An output entry: window, offset in it, length, cookie of the input it answers.
    pub const OUT_ENTRY: usize = 32;
    /// A free entry: a window's number.
    pub const FREE_ENTRY: usize = 8;

    /// [`ATTACHED`] of a task whose releases carry its piece's time (item 6.3z): the header's
    /// [`VERSION_VALUE`] stays 4, so a task of before still attaches, and only the version the
    /// task says it attached with rises.
    pub const ATTACHED_TIMED: u64 = 5;
    /// The word of an input entry, from the entry's start, that a task attached at
    /// [`ATTACHED_TIMED`] writes as it releases the entry: the piece's function time in
    /// nanoseconds, or [`NO_TIME`]. It is the entry's `rows`, which neither side needs once the
    /// task has taken the entry.
    pub const IN_PIECE_NS: usize = 24;
    /// [`IN_PIECE_NS`] of a release the task has no time for.
    pub const NO_TIME: u64 = u64::MAX;

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

    /// [`TASK_WAITS`]: the task sleeps until the engine posts, gives a window or closes, and the
    /// engine wakes it at the first of these.
    pub const WAITS_SLEEP: u64 = 1;
    /// [`TASK_WAITS`]: the task lingers for more input until a deadline of its own. The engine
    /// wakes it once [`super::GROUP`] posts wait for it, before the engine blocks waiting on the
    /// plane, and when it closes the input; not for each post. An engine that does not know the
    /// value takes it as [`WAITS_SLEEP`].
    pub const WAITS_LINGER: u64 = 2;
}

/// The posts a lingering task takes at once: the engine wakes it when this many wait for it, and
/// the task's deadline is this many of the engine's posting intervals. Two halves the bells' calls
/// a batch, and keeps a lingering task working beside an engine that holds three batches in flight
/// for it (a stage's tasks, `moruna-micro`'s `layout::IN_FLIGHT`): with a bigger group such an
/// engine blocks on its own lingering task, waking it only then, and waits out a batch's compute
/// it would otherwise have overlapped (`batching.rs`'s table, a CPU each: with three in flight a
/// group of 2 costs the engine 1.08 calls a batch and the task 0.51, against 1.58 and 1.00 when
/// the task sleeps; a group of 4 or 8 costs the engine 2.43).
pub const GROUP: u64 = 2;

/// The longest a task lingers, in nanoseconds: a task whose engine posts more slowly than
/// [`GROUP`] posts in this time sleeps until the next post, as before lingering, because a call
/// each batch is small beside such intervals, and a batch posted while its engine goes on to other
/// work never waits longer than this for its task. It is the scheduler's slice (`moruna-sched`'s
/// `SLICE_TICKS`, 10 ticks of 1 ms): how long a woken task already waited behind the running one
/// before item 1.11e, the plane's batches with it.
pub const LINGER_MAX_NS: u64 = 10_000_000;

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
    /// Sleeps while the 32-bit word at `at` holds `expected`, until a wake on it or
    /// `deadline` (the kernel's clock; `u64::MAX` for none): a futex on the shared object,
    /// which the other side's wake meets from its own process. Whether it returned before
    /// the deadline. Without a futex it returns `false` at once.
    fn wait(&self, at: usize, expected: u32, deadline: u64) -> bool {
        let _ = (at, expected, deadline);
        false
    }
    /// Wakes the other side's waiter on the word at `at`.
    fn wake(&self, at: usize) {
        let _ = at;
    }
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
    /// The task's bell as the engine last raised it.
    task_bell: u32,
    /// The task's note as the engine last read it.
    note: u64,
    /// The engine's bell as it stood when the engine last looked at the rings.
    bell_seen: u32,
    /// Posts made while the task waited that the engine has not woken it for.
    owed: u64,
    /// When the first of them was posted, on the engine's clock.
    owed_since: u64,
    /// The engine's measure of how often it posts to this task, on its clock ([`Engine::ring_at`]).
    pace: Option<Pace>,
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
            TASK_BELL,
            ENGINE_BELL,
            TASK_NOTE,
            TASK_EXIT,
            TASK_EXIT_CODE,
            PLACE,
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
            task_bell: 0,
            note: 0,
            bell_seen: 0,
            owed: 0,
            owed_since: 0,
            pace: None,
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
        next.task_bell = self.task_bell;
        next.pace = self.pace;
        to.store(TASK_BELL, u64::from(self.task_bell));
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

    /// The function time, in nanoseconds, the task gave input `index` (its posting order, from 0)
    /// as it released it (item 6.3z): its entry's [`IN_PIECE_NS`] word, read only from a task that
    /// attached at [`ATTACHED_TIMED`] or above, and only once [`Engine::released`] has seen the
    /// release pass the entry and before the engine posts into its slot again. `None` for any other
    /// task, any other input, and a release the task gave no time.
    pub fn piece_ns(&self, s: &impl Shared, index: u64) -> Option<u64> {
        let g = &self.geometry;
        if index >= self.in_tail || self.in_head > index.saturating_add(g.in_cap) {
            return None;
        }
        if s.load(ATTACHED) < ATTACHED_TIMED {
            return None;
        }
        let at = g.in_entries() + (index % g.in_cap) as usize * IN_ENTRY + IN_PIECE_NS;
        match s.load(at) {
            NO_TIME => None,
            ns => Some(ns),
        }
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

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Raises the task's bell after a post, and wakes the task if it sleeps, or if it lingers and
    /// [`GROUP`] posts now wait for it: whether it called the wake. One wake a wait: the engine
    /// clears the word, which the task sets again the next time it waits. The engine never reads
    /// the bell back, which the task could have written.
    pub fn ring(&mut self, s: &impl Shared) -> bool {
        self.ring_in(s, GROUP)
    }

    /// [`Engine::ring`] with a group of `group` posts: 1 wakes a lingering task at each post, as
    /// an engine of protocol 4 did.
    pub fn ring_in(&mut self, s: &impl Shared, group: u64) -> bool {
        self.raise(s);
        match s.load(TASK_WAITS) {
            0 => {
                self.owed = 0;
                false
            }
            WAITS_LINGER => {
                self.owed += 1;
                self.owed >= group && self.wake(s)
            }
            _ => self.wake(s),
        }
    }

    /// Raises the task's bell after a post at `now` on the engine's clock, measuring how often the
    /// engine posts to this task ([`Engine::posting_interval`]): whether it called the wake.
    ///
    /// - A task awake is not woken; nothing is owed it.
    /// - A lingering task is woken once `group` posts wait for it ([`Engine::ring_in`]).
    /// - A sleeping task is woken once `group` posts wait for it, or once a group of the engine's
    ///   posting intervals has passed since the first of them; and at once while the engine has
    ///   no clock or no interval measured yet. Before the engine blocks ([`Engine::flush`]) or
    ///   leaves the plane for other work ([`Engine::leave`]) it wakes it for what it owes, and
    ///   [`Engine::due`] wakes it at that deadline when the engine looks at the plane again.
    pub fn ring_at(&mut self, s: &impl Shared, group: u64, now: Option<u64>) -> bool {
        self.raise(s);
        if let Some(now) = now {
            match &mut self.pace {
                Some(p) => p.saw(self.in_head, now),
                None => self.pace = Some(Pace::new(self.in_head, now)),
            }
        }
        match s.load(TASK_WAITS) {
            0 => {
                self.owed = 0;
                false
            }
            WAITS_LINGER => {
                self.owe(now.unwrap_or(0));
                self.owed >= group && self.wake(s)
            }
            _ => {
                let (Some(now), Some(interval)) = (now, self.posting_interval()) else {
                    return self.wake(s);
                };
                self.owe(now);
                (self.owed >= group || self.overdue(now, group, interval)) && self.wake(s)
            }
        }
    }

    /// The engine's measured interval between its posts to this task, in nanoseconds, if any.
    pub fn posting_interval(&self) -> Option<u64> {
        self.pace.and_then(|p| p.interval())
    }

    fn owe(&mut self, now: u64) {
        if self.owed == 0 {
            self.owed_since = now;
        }
        self.owed += 1;
    }

    fn overdue(&self, now: u64, group: u64, interval: u64) -> bool {
        now.saturating_sub(self.owed_since) >= interval.saturating_mul(group)
    }

    /// The engine looks at the plane at `now`: a sleeping task owed posts since a group of its
    /// posting intervals is woken (whether it called the wake). A lingering task answers them
    /// by its own deadline.
    pub fn due(&mut self, s: &impl Shared, group: u64, now: u64) -> bool {
        if self.owed == 0 {
            return false;
        }
        match s.load(TASK_WAITS) {
            0 => {
                self.owed = 0;
                false
            }
            WAITS_LINGER => false,
            _ => match self.posting_interval() {
                Some(i) if self.overdue(now, group, i) => self.wake(s),
                _ => false,
            },
        }
    }

    /// The engine leaves the plane for other work (a stage unit returns its worker): wakes a
    /// sleeping task it owes posts, which has no deadline of its own. A lingering task answers
    /// them by its deadline. Whether it called the wake.
    pub fn leave(&mut self, s: &impl Shared) -> bool {
        self.wake_owed(s, false)
    }

    /// Raises the task's bell after a window given: a task that sleeps is woken (it may wait for
    /// the window), a lingering one is not, because it waits for input, which a window does not
    /// give it.
    pub fn ring_given(&mut self, s: &impl Shared) -> bool {
        self.raise(s);
        match s.load(TASK_WAITS) {
            0 | WAITS_LINGER => false,
            _ => self.wake(s),
        }
    }

    /// Raises the task's bell after the close, and wakes the task however it waits: the end is
    /// what a lingering task waits for too.
    pub fn ring_now(&mut self, s: &impl Shared) -> bool {
        self.raise(s);
        s.load(TASK_WAITS) != 0 && self.wake(s)
    }

    /// The engine is about to block waiting on the plane: wakes a task that posts wait for,
    /// lingering or asleep, which would otherwise take them only at its deadline or the engine's
    /// next post. Whether it called the wake; no call when nothing is owed, or when the task,
    /// waiting again, has released every input it was posted (it took the owed posts itself).
    /// The release count is the task's word: a task that lies about it only delays itself.
    pub fn flush(&mut self, s: &impl Shared) -> bool {
        self.wake_owed(s, true)
    }

    fn wake_owed(&mut self, s: &impl Shared, lingering: bool) -> bool {
        if self.owed == 0 {
            return false;
        }
        let waits = s.load(TASK_WAITS);
        if waits == WAITS_LINGER && !lingering {
            return false;
        }
        if waits != 0 && s.load(IN_TAIL) < self.in_head {
            return self.wake(s);
        }
        self.owed = 0;
        false
    }

    /// Posts made while the task lingered that it has not been woken for.
    pub fn owed(&self) -> u64 {
        self.owed
    }

    fn raise(&mut self, s: &impl Shared) {
        self.task_bell = self.task_bell.wrapping_add(1);
        s.store(TASK_BELL, u64::from(self.task_bell));
        s.fence();
    }

    fn wake(&mut self, s: &impl Shared) -> bool {
        self.owed = 0;
        s.store(TASK_WAITS, 0);
        s.wake(TASK_BELL);
        true
    }

    /// The engine starts a look at the rings: the bell as it stands now. A ring after this
    /// is one the look may not have seen, so the engine will not sleep through it.
    pub fn look(&mut self, s: &impl Shared) {
        self.bell_seen = s.load(ENGINE_BELL) as u32;
    }

    /// Going to sleep on the engine's bell: says so, then reads the bell and the output ring
    /// again after the fence. The bell's value to wait on, or `None` when the task rang it
    /// since the engine's last look (a record, its note) or an output came meanwhile.
    pub fn sleep(&mut self, s: &impl Shared) -> Result<Option<u32>, Bad> {
        s.store(ENGINE_WAITS, 1);
        s.fence();
        let seq = s.load(ENGINE_BELL) as u32;
        if seq != self.bell_seen || self.posted(s)? > 0 {
            s.store(ENGINE_WAITS, 0);
            return Ok(None);
        }
        Ok(Some(seq))
    }

    /// Whether the task raised its note since the engine last looked: it is about to call a
    /// port, or to end.
    pub fn noted(&mut self, s: &impl Shared) -> bool {
        let note = s.load(TASK_NOTE);
        let changed = note != self.note;
        self.note = note;
        changed
    }

    /// Places the task on `cpu`, before it starts, or nowhere in particular with `None`
    /// ([`PLACE`]).
    pub fn place(&self, s: &impl Shared, cpu: Option<u32>) {
        s.store(PLACE, cpu.map_or(0, |c| u64::from(c) + 1));
    }

    /// The exit code the task said, if it said one ([`Task::exit`]). The task wrote it, so
    /// it is only the task's word: the engine judges the task by what it put.
    pub fn exit(&self, s: &impl Shared) -> Option<i64> {
        (s.load(TASK_EXIT) == 1).then(|| s.load(TASK_EXIT_CODE) as i64)
    }

    /// Awake again.
    pub fn awake(&self, s: &impl Shared) {
        s.store(ENGINE_WAITS, 0);
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
    /// The engine's bell as the task last raised it.
    engine_bell: u32,
    /// The task's note as it last raised it.
    note: u64,
    /// The CPU the engine placed the task on ([`PLACE`]), as the task attached.
    pub place: Option<u32>,
}

impl Task {
    /// The task's half of the rings object `s`, attached.
    pub fn attach(s: &impl Shared) -> Result<Task, Bad> {
        let (geometry, mode) = Geometry::read(s)?;
        let t = Task {
            place: match s.load(PLACE) {
                0 => None,
                v => u32::try_from(v - 1).ok(),
            },
            geometry,
            mode,
            generation: s.load(GENERATION),
            in_tail: s.load(IN_TAIL),
            in_next: s.load(IN_TAIL),
            out_head: s.load(OUT_HEAD),
            free_tail: s.load(FREE_TAIL),
            engine_bell: s.load(ENGINE_BELL) as u32,
            note: s.load(TASK_NOTE),
        };
        // Its releases carry its pieces' times (item 6.3z).
        s.store(ATTACHED, ATTACHED_TIMED);
        Ok(t)
    }

    /// The task's half of the next rings object the engine gave (unmap mode), the inputs it
    /// took and has not released kept as they were: the engine carried the input ring over.
    pub fn reattach(s: &impl Shared, previous: &Task) -> Result<Task, Bad> {
        let mut t = Task::attach(s)?;
        if t.in_tail != previous.in_tail || t.geometry != previous.geometry {
            return Err(Bad("the next rings object does not carry the input ring"));
        }
        t.in_next = previous.in_next;
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

    /// Releases the oldest input taken: the task is done with it, and every record it posted
    /// before answers it. The release carries no time ([`Task::release_timed`]).
    pub fn release(&mut self, s: &impl Shared) -> bool {
        self.release_timed(s, None)
    }

    /// [`Task::release`], with the piece's function time in nanoseconds, if the task has one
    /// ([`PieceTimes`]): written into the entry's [`IN_PIECE_NS`] word before the release is
    /// stored, so the engine that reads the release reads the time with it. A time of
    /// [`NO_TIME`] or more is given as `NO_TIME - 1`.
    pub fn release_timed(&mut self, s: &impl Shared, piece_ns: Option<u64>) -> bool {
        if self.in_tail == self.in_next {
            return false;
        }
        let g = &self.geometry;
        let at = g.in_entries() + (self.in_tail % g.in_cap) as usize * IN_ENTRY + IN_PIECE_NS;
        s.store(at, piece_ns.map_or(NO_TIME, |ns| ns.min(NO_TIME - 1)));
        self.in_tail += 1;
        s.store(IN_TAIL, self.in_tail);
        true
    }

    /// Inputs taken and not yet released.
    pub fn held(&self) -> u64 {
        self.in_next - self.in_tail
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

    /// Records posted on this rings object.
    pub fn posted(&self) -> u64 {
        self.out_head
    }

    /// Raises the engine's bell after a post, and wakes the engine if it said it waits:
    /// whether it called the wake.
    pub fn ring(&mut self, s: &impl Shared) -> bool {
        self.engine_bell = self.engine_bell.wrapping_add(1);
        s.store(ENGINE_BELL, u64::from(self.engine_bell));
        s.fence();
        if s.load(ENGINE_WAITS) == 0 {
            return false;
        }
        s.store(ENGINE_WAITS, 0);
        s.wake(ENGINE_BELL);
        true
    }

    /// Raises the task's note and rings the engine's bell: before a port call, and before the
    /// task ends, so an engine asleep on its bell reads the task's port and slots.
    pub fn note(&mut self, s: &impl Shared) -> bool {
        self.note = self.note.wrapping_add(1);
        s.store(TASK_NOTE, self.note);
        self.ring(s)
    }

    /// Says the task's exit code, then raises its note: the last thing a task does before it
    /// ends.
    pub fn exit(&mut self, s: &impl Shared, code: i64) -> bool {
        s.store(TASK_EXIT_CODE, code as u64);
        s.store(TASK_EXIT, 1);
        self.note(s)
    }

    /// Whether an input, or the end, is there for the task.
    pub fn has_input(&self, s: &impl Shared) -> bool {
        s.load(IN_HEAD) != self.in_next || s.load(CLOSED) != 0
    }

    /// Whether a window is there for the task.
    pub fn has_window(&self, s: &impl Shared) -> bool {
        s.load(FREE_HEAD) != self.free_tail
    }

    /// Sleeps on the task's bell until the engine posts, gives a window or closes, or the
    /// deadline: says it waits, reads the bell, and sleeps only if `ready` still says no.
    /// Whether it returned before the deadline.
    pub fn sleep(&self, s: &impl Shared, deadline: u64, ready: impl Fn(&Task) -> bool) -> bool {
        self.rest(s, Rest::Sleep(deadline), ready) != Slept::TimedOut
    }

    /// Waits on the task's bell as `how` says, unless `ready` says, once the wait is said, that
    /// there is no need: [`Task::arm`], the futex, [`Task::disarm`].
    pub fn rest(&self, s: &impl Shared, how: Rest, ready: impl Fn(&Task) -> bool) -> Slept {
        let Some(seq) = self.arm(s, how, ready) else {
            return Slept::Ready;
        };
        let woke = s.wait(TASK_BELL, seq, how.deadline());
        Task::disarm(s);
        if woke { Slept::Woken } else { Slept::TimedOut }
    }

    /// Says the task waits as `how` says, and reads its bell after the fence: the bell's value to
    /// wait on, or `None` (the word cleared again) when `ready` says what it waits for came
    /// meanwhile. A post after this changes the bell, so the futex does not sleep through it.
    pub fn arm(&self, s: &impl Shared, how: Rest, ready: impl Fn(&Task) -> bool) -> Option<u32> {
        s.store(TASK_WAITS, how.word());
        s.fence();
        let seq = s.load(TASK_BELL) as u32;
        if ready(self) {
            Task::disarm(s);
            return None;
        }
        Some(seq)
    }

    /// Awake again: the task no longer waits.
    pub fn disarm(s: &impl Shared) {
        s.store(TASK_WAITS, 0);
    }

    /// The input ring's head: how many batches the engine has posted on it.
    pub fn head(&self, s: &impl Shared) -> u64 {
        s.load(IN_HEAD)
    }

    /// Waits for an input or the end once the task has found none (after its spin, if it
    /// spins), measuring its engine's posts on `pace` with the clock `now`: it lingers or sleeps
    /// as [`Pace::rest`] says, and a linger that ends at its deadline with nothing come sleeps
    /// until the next post, so a task whose engine stopped costs one call more and then nothing.
    /// `forever` is the deadline of a sleep. The futex calls it made: each copies the bell's word.
    pub fn wait_input(
        &self,
        s: &impl Shared,
        pace: &mut Pace,
        now: impl Fn() -> u64,
        forever: u64,
    ) -> u32 {
        let at = now();
        pace.saw(self.head(s), at);
        let how = pace.rest(at, forever);
        let slept = self.rest(s, how, |t| t.has_input(s));
        let mut calls = u32::from(slept.called());
        if matches!(how, Rest::Linger(_)) && slept == Slept::TimedOut {
            let again = self.rest(s, Rest::Sleep(forever), |t| t.has_input(s));
            calls += u32::from(again.called());
        }
        pace.saw(self.head(s), now());
        calls
    }
}

/// How a task waits on its bell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rest {
    /// Until the engine's next post, window or close, or this deadline ([`layout::WAITS_SLEEP`]).
    Sleep(u64),
    /// For more input, until this deadline ([`layout::WAITS_LINGER`]): the engine wakes it for a
    /// group of posts, before it blocks, or for the close.
    Linger(u64),
}

impl Rest {
    /// The deadline, on the kernel's clock.
    pub fn deadline(self) -> u64 {
        match self {
            Rest::Sleep(d) | Rest::Linger(d) => d,
        }
    }

    /// What [`layout::TASK_WAITS`] says while the task waits so.
    pub fn word(self) -> u64 {
        match self {
            Rest::Sleep(_) => WAITS_SLEEP,
            Rest::Linger(_) => WAITS_LINGER,
        }
    }
}

/// How a wait on the bell ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slept {
    /// What it waited for had come before it slept: no futex call.
    Ready,
    /// It slept, and returned before its deadline: a wake, or the bell moved as it went to sleep.
    Woken,
    /// It slept until its deadline.
    TimedOut,
}

impl Slept {
    /// Whether the wait called the futex.
    pub fn called(self) -> bool {
        self != Slept::Ready
    }
}

/// A task's measure of how often its engine posts, from the input ring's head against the task's
/// clock (the clock page: no call), and how it waits from it ([`Pace::rest`]).
///
/// Each look that finds posts the last look did not see is a sample of the interval between
/// posts: the time since the last look over the posts since. A shorter sample is taken at once and
/// a longer one by half, so a pause of the engine (a long sample) lengthens the interval only
/// gradually and a task back in a stream lingers again at its next look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pace {
    /// The time of the last look that saw new posts.
    at: u64,
    /// The head it saw.
    head: u64,
    /// The interval between posts, in nanoseconds; zero until one is measured.
    interval: u64,
}

impl Pace {
    /// A measure that starts at `now`, with the head at `head`.
    pub fn new(head: u64, now: u64) -> Pace {
        Pace {
            at: now,
            head,
            interval: 0,
        }
    }

    /// The input ring's head stood at `head` at `now`.
    pub fn saw(&mut self, head: u64, now: u64) {
        if head <= self.head {
            if head < self.head {
                // A head that went back is no ring's: start again from it.
                *self = Pace::new(head, now);
            }
            return;
        }
        let sample = (now.saturating_sub(self.at) / (head - self.head)).max(1);
        self.interval = match self.interval {
            0 => sample,
            i if sample <= i => sample,
            i => i + (sample - i) / 2,
        };
        self.at = now;
        self.head = head;
    }

    /// The measured interval between the engine's posts, if any.
    pub fn interval(&self) -> Option<u64> {
        (self.interval > 0).then_some(self.interval)
    }

    /// How to wait at `now` for input that has not come: linger until [`GROUP`] intervals from
    /// now when that is within [`LINGER_MAX_NS`]; otherwise, and before any interval is measured,
    /// sleep until the next post (or `forever`, the deadline of a wait without one).
    pub fn rest(&self, now: u64, forever: u64) -> Rest {
        self.rest_in(now, GROUP, forever)
    }

    /// [`Pace::rest`] for a group of `group` posts.
    pub fn rest_in(&self, now: u64, group: u64, forever: u64) -> Rest {
        match self.interval() {
            Some(i) if i.saturating_mul(group) <= LINGER_MAX_NS => {
                Rest::Linger(now.saturating_add(i * group))
            }
            _ => Rest::Sleep(forever),
        }
    }
}

/// The most batches a task times at once ([`PieceTimes`]): the input ring's capacity as
/// [`Geometry::new`] makes it. A batch taken while this many are held goes untimed.
pub const TIMED_MAX: usize = 64;

/// A task's measure of each piece's function time (item 6.3z), on its own clock: from its take
/// of a batch off the input ring to its release of it, less its waits for the engine (for a free
/// window, or for input while it holds batches), which are the engine's time.
///
/// The task's time is shared among the batches it holds: between two events (a take, a release,
/// the start or the end of a wait for the engine) the time elapsed is split evenly among the
/// batches held then, the odd nanoseconds to the oldest, and a wait adds nothing to any. So the
/// times of the batches a task held sum to the time it was busy holding them, never its waiting,
/// however many it held at once, and a task that holds one batch at a time gives each its own
/// time.
///
/// It keeps [`TIMED_MAX`] batches' times at most: a batch taken past them goes untimed, as does
/// every batch behind it until the oldest timed are released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PieceTimes {
    /// When the time since was last shared out.
    at: u64,
    /// In a wait for the engine: the time since is no batch's.
    waiting: bool,
    /// The held batches' times, oldest first, as a ring from `first`: `None` for one untimed.
    times: [Option<u64>; TIMED_MAX],
    first: usize,
    /// Batches held with a place in `times`.
    kept: usize,
    /// Batches held past them, the newest, all untimed.
    past: u64,
}

impl PieceTimes {
    /// A measure that starts at `now`, holding no batch.
    pub fn new(now: u64) -> PieceTimes {
        PieceTimes {
            at: now,
            waiting: false,
            times: [None; TIMED_MAX],
            first: 0,
            kept: 0,
            past: 0,
        }
    }

    /// Batches held.
    pub fn held(&self) -> u64 {
        self.kept as u64 + self.past
    }

    /// Shares the time from the last event to `now` among the batches held, unless the task waits
    /// for the engine.
    fn share(&mut self, now: u64) {
        let elapsed = now.saturating_sub(self.at);
        self.at = self.at.max(now);
        let held = self.held();
        if self.waiting || held == 0 || elapsed == 0 {
            return;
        }
        let (each, odd) = (elapsed / held, elapsed % held);
        for k in 0..self.kept {
            let slot = &mut self.times[(self.first + k) % TIMED_MAX];
            let add = if k == 0 { each + odd } else { each };
            *slot = slot.map(|t| t.saturating_add(add));
        }
    }

    /// The task took a batch off its input ring at `now`.
    pub fn took(&mut self, now: u64) {
        self.share(now);
        // Batches are held past the kept ones only while every place is taken.
        if self.kept < TIMED_MAX {
            self.times[(self.first + self.kept) % TIMED_MAX] = Some(0);
            self.kept += 1;
        } else {
            self.past += 1;
        }
    }

    /// The task releases its oldest batch at `now`: its time, `None` when it went untimed or no
    /// batch is held.
    pub fn released(&mut self, now: u64) -> Option<u64> {
        self.share(now);
        if self.kept == 0 {
            return None;
        }
        let time = self.times[self.first].take();
        self.first = (self.first + 1) % TIMED_MAX;
        self.kept -= 1;
        if self.past > 0 {
            // The oldest of those past takes the place freed, untimed as it came.
            self.times[(self.first + self.kept) % TIMED_MAX] = None;
            self.kept += 1;
            self.past -= 1;
        }
        time
    }

    /// The task starts waiting for the engine at `now`, for a free window or for input: until
    /// [`PieceTimes::wait_ends`], its time is no batch's.
    pub fn wait_begins(&mut self, now: u64) {
        self.share(now);
        self.waiting = true;
    }

    /// The task's wait for the engine ends at `now`.
    pub fn wait_ends(&mut self, now: u64) {
        self.at = self.at.max(now);
        self.waiting = false;
    }
}
