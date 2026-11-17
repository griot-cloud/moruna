//! A scratch file's bytes as a run of chunks, each one memory object (SDD 27 f.7): how the
//! chunks are sized, the budget of handles they hold, and a [`Store`] over any [`Backing`]
//! that makes, copies and zeros chunks. The `imagefs` server's backing is memory objects
//! charged to the directory's account; the tests' is the host's memory.
//!
//! A file's chunks grow with it. Its first chunk is what its first growth needs, at least
//! a page. Each later one is the largest of [`MIN_CHUNK`], an eighth of what the file
//! holds, and what the growth needs, at most [`MAX_CHUNK`], in whole pages. No chunk is
//! larger than its account has room for, so a file fills its account up to the last
//! growth that does not fit. So:
//!
//! - what the account pays for past a file's end is less than
//!   `max(MIN_CHUNK, size / 8 + PAGE)`, and less than `MAX_CHUNK`, besides the kernel's
//!   record of each chunk (256 bytes, and 16 a page);
//! - a file holds a handle per 16 KiB up to 128 KiB, then grows by an eighth a chunk up to
//!   128 MiB (about 59 chunks), then a chunk per 16 MiB: 4 GiB written in 32 KiB pieces
//!   is 309 chunks, 16 GiB is 1,077, and 3,072 handles hold one file of 47 GiB.
//!
//! The chunks of every scratch directory a server keeps share one [`Budget`] of handles,
//! since they share its handle table: the rest of the table is the server's own, for its
//! heap, its connections and its messages. A growth the budget or the account has no room
//! for is `NoSpace`, and nothing is made.

use alloc::{collections::BTreeMap, rc::Rc, vec::Vec};
use core::cell::Cell;

use crate::{
    error::{Error, Result},
    scratch::{Id, Store},
};

/// The unit every chunk is a multiple of.
pub const PAGE: u64 = 4096;
/// The least chunk after a file's first.
pub const MIN_CHUNK: u64 = 16 * 1024;
/// The largest chunk.
pub const MAX_CHUNK: u64 = 16 << 20;
/// Each chunk after the first adds at least this fraction of what the file holds.
const GROWTH: u64 = 8;

/// The next chunk of a file whose chunks hold `capacity` bytes, growing to `size`.
pub fn next_len(capacity: u64, size: u64) -> u64 {
    let need = size.saturating_sub(capacity);
    let least = if capacity == 0 {
        PAGE
    } else {
        MIN_CHUNK.max(capacity / GROWTH)
    };
    need.max(least).min(MAX_CHUNK).next_multiple_of(PAGE)
}

/// What an account is charged for a chunk: `object` for each, and `page` for each page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cost {
    pub object: u64,
    pub page: u64,
}

impl Cost {
    /// The charge for a chunk of `len` bytes.
    pub fn of(self, len: u64) -> u64 {
        self.object
            .saturating_add((len / PAGE).saturating_mul(self.page))
    }

    /// The largest chunk `room` pays for.
    pub fn fits(self, room: u64) -> u64 {
        room.saturating_sub(self.object) / self.page.max(1) * PAGE
    }
}

/// Where a [`Chunked`] store keeps its chunks.
pub trait Backing {
    /// One chunk's memory; dropping it gives its pages and its handle back.
    type Chunk;
    /// What the account is charged for a chunk.
    fn cost(&self) -> Cost;
    /// What the account can still be charged, when it can be read.
    fn room(&mut self) -> Result<Option<u64>>;
    /// A chunk of `len` bytes, whole pages, all zero.
    fn create(&mut self, len: u64) -> Result<Self::Chunk>;
    /// `buf.len()` bytes of `chunk` from `at`, at least one, within it.
    fn read(&self, chunk: &Self::Chunk, at: u64, buf: &mut [u8]) -> Result<()>;
    /// `bytes` into `chunk` at `at`, at least one, within it.
    fn write(&self, chunk: &Self::Chunk, at: u64, bytes: &[u8]) -> Result<()>;
    /// Zeros `from .. to` of `chunk`, a range within it of at least one byte.
    fn zero(&self, chunk: &Self::Chunk, from: u64, to: u64) -> Result<()>;
}

/// The handles every chunk of a server's scratch directories may hold together, shared
/// by its stores.
#[derive(Clone, Debug)]
pub struct Budget {
    limit: usize,
    held: Rc<Cell<usize>>,
}

impl Budget {
    pub fn new(limit: usize) -> Budget {
        Budget {
            limit,
            held: Rc::new(Cell::new(0)),
        }
    }

    /// The handles chunks hold now.
    pub fn held(&self) -> usize {
        self.held.get()
    }

    /// How many more chunks may be made.
    pub fn room(&self) -> usize {
        self.limit.saturating_sub(self.held.get())
    }

    fn hold(&self) -> Option<Held> {
        if self.room() == 0 {
            return None;
        }
        self.held.set(self.held.get() + 1);
        Some(Held(self.held.clone()))
    }
}

/// One chunk's handle in its budget, given back when the chunk goes.
struct Held(Rc<Cell<usize>>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

struct Chunk<C> {
    start: u64,
    len: u64,
    // Dropped before `_held`: the handle is closed before the budget has it back.
    memory: C,
    _held: Held,
}

struct File<C> {
    size: u64,
    chunks: Vec<Chunk<C>>,
}

impl<C> File<C> {
    fn new() -> File<C> {
        File {
            size: 0,
            chunks: Vec::new(),
        }
    }

    fn capacity(&self) -> u64 {
        self.chunks.last().map_or(0, |c| c.start + c.len)
    }

    /// Each chunk `start .. end` touches, with the range of it, and where in the
    /// caller's buffer that range begins. The range is within the file's size.
    fn spans(&self, start: u64, end: u64) -> impl Iterator<Item = (&C, u64, u64, usize)> {
        let first = self.chunks.partition_point(|c| c.start + c.len <= start);
        self.chunks[first..]
            .iter()
            .take_while(move |c| c.start < end)
            .map(move |c| {
                let from = start.max(c.start);
                let to = end.min(c.start + c.len);
                (
                    &c.memory,
                    from - c.start,
                    to - c.start,
                    (from - start) as usize,
                )
            })
    }
}

/// The chunks a file whose chunks hold `capacity` bytes adds to hold `size`, each held to
/// what `room` has left when it is known; `NoSpace` once one does not fit.
struct Plan {
    capacity: u64,
    size: u64,
    room: Option<u64>,
    cost: Cost,
}

impl Iterator for Plan {
    type Item = Result<u64>;

    fn next(&mut self) -> Option<Result<u64>> {
        if self.capacity >= self.size {
            return None;
        }
        let mut len = next_len(self.capacity, self.size);
        if let Some(room) = self.room {
            let least = (self.size - self.capacity)
                .min(MAX_CHUNK)
                .next_multiple_of(PAGE);
            let fits = self.cost.fits(room);
            if fits < least {
                self.capacity = self.size;
                return Some(Err(Error::NoSpace));
            }
            len = len.min(fits);
            self.room = Some(room - self.cost.of(len));
        }
        self.capacity = self.capacity.saturating_add(len);
        Some(Ok(len))
    }
}

/// A [`Store`] keeping each file as a run of chunks of `B`.
pub struct Chunked<B: Backing> {
    backing: B,
    budget: Budget,
    files: BTreeMap<Id, File<B::Chunk>>,
}

impl<B: Backing> Chunked<B> {
    pub fn new(backing: B, budget: Budget) -> Chunked<B> {
        Chunked {
            backing,
            budget,
            files: BTreeMap::new(),
        }
    }

    /// The chunks it holds.
    pub fn chunks(&self) -> usize {
        self.files.values().map(|f| f.chunks.len()).sum()
    }

    /// Adds the chunks file `id`, whose chunks hold `capacity` bytes, needs to hold
    /// `size`. All are counted against the budget and the account before any is made.
    fn grow(&mut self, id: Id, capacity: u64, size: u64) -> Result<()> {
        let room = self.backing.room()?;
        let cost = self.backing.cost();
        let plan = || Plan {
            capacity,
            size,
            room,
            cost,
        };
        let mut count = 0usize;
        for len in plan() {
            len?;
            count += 1;
            if count > self.budget.room() {
                return Err(Error::NoSpace);
            }
        }
        let Chunked {
            backing,
            budget,
            files,
        } = self;
        let file = files.entry(id).or_insert_with(File::new);
        file.chunks.try_reserve(count).map_err(|_| Error::NoSpace)?;
        let kept = file.chunks.len();
        let mut start = capacity;
        for len in plan() {
            let made = len.and_then(|len| {
                let held = budget.hold().ok_or(Error::NoSpace)?;
                let memory = backing.create(len)?;
                Ok(Chunk {
                    start,
                    len,
                    memory,
                    _held: held,
                })
            });
            match made {
                Ok(chunk) => {
                    start += chunk.len;
                    file.chunks.push(chunk);
                }
                Err(e) => {
                    // A refusal drops what this call made: nothing about the file changes.
                    file.chunks.truncate(kept);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Cuts file `id` to `size`, less than its size: zeros its bytes from `size` in the
    /// chunk that holds it, so a later growth reads zeros there, and drops every chunk
    /// past it.
    fn cut(&mut self, id: Id, size: u64) -> Result<()> {
        let Some(file) = self.files.get_mut(&id) else {
            return Ok(());
        };
        let keep = file.chunks.partition_point(|c| c.start < size);
        if let Some(last) = keep.checked_sub(1).map(|i| &file.chunks[i]) {
            // Past the old size the chunk is zero already.
            let end = (last.start + last.len).min(file.size);
            if size < end {
                self.backing
                    .zero(&last.memory, size - last.start, end - last.start)?;
            }
        }
        // Only once the zeroing is done do the chunks past the end go.
        file.chunks.truncate(keep);
        Ok(())
    }
}

/// The end of `len` bytes from `offset` of `file`, which must lie within its size; a file
/// the store has not seen has size zero.
fn within<C>(file: Option<&File<C>>, offset: u64, len: usize) -> Result<u64> {
    offset
        .checked_add(len as u64)
        .filter(|end| *end <= file.map_or(0, |f| f.size))
        .ok_or(Error::BadRange)
}

impl<B: Backing> Store for Chunked<B> {
    fn resize(&mut self, id: Id, size: u64) -> Result<()> {
        let (old, capacity) = self
            .files
            .get(&id)
            .map_or((0, 0), |f| (f.size, f.capacity()));
        if size > capacity {
            self.grow(id, capacity, size)?;
        } else if size < old {
            self.cut(id, size)?;
        }
        // Between the old size and the capacity the chunks are zero already.
        self.files.entry(id).or_insert_with(File::new).size = size;
        Ok(())
    }

    fn read(&self, id: Id, offset: u64, buf: &mut [u8]) -> Result<()> {
        let file = self.files.get(&id);
        let end = within(file, offset, buf.len())?;
        let Some(file) = file.filter(|_| !buf.is_empty()) else {
            return Ok(());
        };
        for (chunk, from, to, done) in file.spans(offset, end) {
            let n = (to - from) as usize;
            self.backing.read(chunk, from, &mut buf[done..done + n])?;
        }
        Ok(())
    }

    fn write(&mut self, id: Id, offset: u64, bytes: &[u8]) -> Result<()> {
        let file = self.files.get(&id);
        let end = within(file, offset, bytes.len())?;
        let Some(file) = file.filter(|_| !bytes.is_empty()) else {
            return Ok(());
        };
        for (chunk, from, to, done) in file.spans(offset, end) {
            let n = (to - from) as usize;
            self.backing.write(chunk, from, &bytes[done..done + n])?;
        }
        Ok(())
    }

    fn free(&mut self, id: Id) {
        self.files.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, vec};
    use core::cell::RefCell;

    use super::*;
    use crate::scratch::Scratch;

    /// The host's memory under an optional limit, which counts what it makes and can
    /// refuse its `fail`th chunk.
    struct Fake {
        limit: Option<u64>,
        cost: Cost,
        used: Rc<Cell<u64>>,
        made: Rc<Cell<usize>>,
        fail: Option<usize>,
    }

    /// Its bytes are made when it is first written, so a test may hold GiB of chunks.
    struct FakeChunk {
        len: usize,
        bytes: RefCell<Vec<u8>>,
        charge: u64,
        used: Rc<Cell<u64>>,
    }

    impl Drop for FakeChunk {
        fn drop(&mut self) {
            self.used.set(self.used.get() - self.charge);
        }
    }

    const BYTES: Cost = Cost {
        object: 0,
        page: PAGE,
    };
    /// The kernel's charges (`moruna_abi::charge`).
    const KERNEL: Cost = Cost {
        object: 256,
        page: PAGE + 16,
    };

    impl Fake {
        fn new(limit: Option<u64>, cost: Cost) -> Fake {
            Fake {
                limit,
                cost,
                used: Rc::new(Cell::new(0)),
                made: Rc::new(Cell::new(0)),
                fail: None,
            }
        }
    }

    impl Backing for Fake {
        type Chunk = FakeChunk;

        fn cost(&self) -> Cost {
            self.cost
        }

        fn room(&mut self) -> Result<Option<u64>> {
            Ok(self.limit.map(|l| l - self.used.get()))
        }

        fn create(&mut self, len: u64) -> Result<FakeChunk> {
            assert!(
                len > 0 && len.is_multiple_of(PAGE) && len <= MAX_CHUNK,
                "{len}"
            );
            if self.fail == Some(self.made.get()) {
                return Err(Error::NoSpace);
            }
            let charge = self.cost.of(len);
            if self.limit.is_some_and(|l| self.used.get() + charge > l) {
                return Err(Error::NoSpace);
            }
            self.used.set(self.used.get() + charge);
            self.made.set(self.made.get() + 1);
            Ok(FakeChunk {
                len: len as usize,
                bytes: RefCell::new(Vec::new()),
                charge,
                used: self.used.clone(),
            })
        }

        fn read(&self, chunk: &FakeChunk, at: u64, buf: &mut [u8]) -> Result<()> {
            let (at, end) = (at as usize, at as usize + buf.len());
            assert!(at < end && end <= chunk.len);
            match chunk.bytes.borrow().get(at..end) {
                Some(bytes) => buf.copy_from_slice(bytes),
                None => buf.fill(0),
            }
            Ok(())
        }

        fn write(&self, chunk: &FakeChunk, at: u64, bytes: &[u8]) -> Result<()> {
            let (at, end) = (at as usize, at as usize + bytes.len());
            assert!(at < end && end <= chunk.len);
            let mut held = chunk.bytes.borrow_mut();
            held.resize(chunk.len, 0);
            held[at..end].copy_from_slice(bytes);
            Ok(())
        }

        fn zero(&self, chunk: &FakeChunk, from: u64, to: u64) -> Result<()> {
            assert!(from < to && to as usize <= chunk.len);
            if let Some(bytes) = chunk.bytes.borrow_mut().get_mut(from as usize..to as usize) {
                bytes.fill(0);
            }
            Ok(())
        }
    }

    fn store(limit: Option<u64>, handles: usize) -> Chunked<Fake> {
        Chunked::new(Fake::new(limit, BYTES), Budget::new(handles))
    }

    /// The chunks a file grown by `piece` bytes a write to `size` ends with, checking at
    /// each growth what its account pays for past its end.
    fn layout(size: u64, piece: u64) -> Vec<u64> {
        let mut lens = Vec::new();
        let (mut capacity, mut at) = (0u64, 0u64);
        while at < size {
            at = (at + piece).min(size);
            let plan = Plan {
                capacity,
                size: at,
                room: None,
                cost: BYTES,
            };
            for len in plan {
                let len = len.unwrap();
                capacity += len;
                lens.push(len);
            }
            let waste = capacity - at;
            assert!(waste < MAX_CHUNK, "{waste} past {at}");
            assert!(
                waste < MIN_CHUNK.max(at / GROWTH + PAGE),
                "{waste} past {at}"
            );
        }
        lens
    }

    /// Grows file `id` to `size` by `piece` bytes a resize, as appends do.
    fn grow(s: &mut Chunked<Fake>, id: Id, size: u64, piece: u64) {
        let mut at = 0;
        while at < size {
            at = (at + piece).min(size);
            s.resize(id, at).unwrap();
        }
    }

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn chunks_grow_by_an_eighth_between_their_bounds() {
        assert_eq!(next_len(0, 1), PAGE);
        assert_eq!(next_len(0, 8192), 8192);
        assert_eq!(next_len(0, 8193), 3 * PAGE);
        assert_eq!(next_len(8192, 8193), MIN_CHUNK);
        assert_eq!(next_len(128 << 10, (128 << 10) + 1), MIN_CHUNK);
        assert_eq!(next_len(1 << 20, (1 << 20) + 1), 128 << 10);
        assert_eq!(next_len(1 << 20, 3 << 20), 2 << 20);
        assert_eq!(next_len(0, 1 << 30), MAX_CHUNK);
        assert_eq!(next_len(1 << 30, (1 << 30) + 1), MAX_CHUNK);
        assert_eq!(next_len(0, u64::MAX), MAX_CHUNK);
        assert_eq!(next_len(u64::MAX - PAGE, u64::MAX), MAX_CHUNK);
    }

    #[test]
    fn several_gib_fit_the_handles_with_room_to_spare() {
        // A file written in the pieces the Linux server sends (32 KiB) and in the pieces
        // a `BufWriter` flushes (8 KiB).
        let gib = 1u64 << 30;
        assert_eq!(layout(4 * gib, 32 << 10).len(), 309);
        assert_eq!(layout(4 * gib, 8 << 10).len(), 315);
        assert_eq!(layout(27_000_000, 8 << 10).len(), 53);
        // In one growth: the largest chunks from the first.
        assert_eq!(layout(4 * gib, 4 * gib).len(), 256);
        // Many small files: each is a page or what its first write needed.
        assert_eq!(layout(100, 100), [PAGE]);
        assert_eq!(layout(8192, 8192), [8192]);
        // A file of 16 KiB pieces takes no more handles than 16 KiB chunks did.
        let small = layout(1 << 20, MIN_CHUNK);
        assert!(
            small.len() <= (1 << 20) / MIN_CHUNK as usize,
            "{}",
            small.len()
        );
        // What a budget of 3,072 handles holds as one file.
        let (mut capacity, mut made) = (0u64, 0);
        while made < 3072 {
            capacity += next_len(capacity, capacity + (32 << 10));
            made += 1;
        }
        assert!(capacity > 47 * gib, "{capacity}");
    }

    #[test]
    fn bytes_cross_the_chunks_boundaries() {
        let mut s = store(None, 4096);
        let size = 3 << 20;
        grow(&mut s, 1, size, 64 << 10);
        let mut reference = vec![0u8; size as usize];
        // Writes of odd lengths at odd offsets, every one across at least one boundary
        // once the chunks are larger than the writes.
        let mut at = 7u64;
        let mut seed = 1u8;
        while at < size - 70_000 {
            let n = 65_521usize;
            let bytes = pattern(n, seed);
            s.write(1, at, &bytes).unwrap();
            reference[at as usize..at as usize + n].copy_from_slice(&bytes);
            at += 61_123;
            seed = seed.wrapping_add(1);
        }
        let mut whole = vec![0u8; size as usize];
        s.read(1, 0, &mut whole).unwrap();
        assert!(whole == reference);
        // Reads of every chunk's last byte and the next one's first.
        let file = &s.files[&1];
        assert!(file.chunks.len() > 2);
        for c in &file.chunks[..file.chunks.len() - 1] {
            let edge = c.start + c.len;
            let mut two = [0u8; 2];
            s.read(1, edge - 1, &mut two).unwrap();
            assert_eq!(two, reference[edge as usize - 1..edge as usize + 1]);
        }
        // Empty reads and writes, at the end and inside.
        s.read(1, size, &mut []).unwrap();
        s.write(1, 5, &[]).unwrap();
        assert_eq!(s.read(1, size, &mut [0]), Err(Error::BadRange));
        assert_eq!(s.write(1, size - 1, &[1, 2]), Err(Error::BadRange));
        assert_eq!(s.read(1, u64::MAX, &mut [0]), Err(Error::BadRange));
        assert_eq!(s.write(1, u64::MAX, &[0]), Err(Error::BadRange));
        // A file the store has not seen has length zero.
        s.read(9, 0, &mut []).unwrap();
        s.write(9, 0, &[]).unwrap();
        assert_eq!(s.read(9, 0, &mut [0]), Err(Error::BadRange));
        assert_eq!(s.write(9, 0, &[0]), Err(Error::BadRange));
    }

    #[test]
    fn resizes_up_and_down_and_cut_bytes_read_back_as_zero() {
        let mut s = store(None, 4096);
        let size = 2 << 20;
        grow(&mut s, 1, size, 32 << 10);
        s.write(1, 0, &vec![0xab; size as usize]).unwrap();
        let chunks = s.chunks();
        let starts: Vec<u64> = s.files[&1].chunks.iter().map(|c| c.start).collect();

        // Down into the middle of a chunk: the chunks past it go, the rest of it is zero.
        let cut = starts[starts.len() / 2] + 4097;
        s.resize(1, cut).unwrap();
        assert!(s.chunks() < chunks);
        let mut kept = vec![0u8; cut as usize];
        s.read(1, 0, &mut kept).unwrap();
        assert!(kept.iter().all(|b| *b == 0xab));
        s.resize(1, size).unwrap();
        let mut grown = vec![0u8; (size - cut) as usize];
        s.read(1, cut, &mut grown).unwrap();
        assert!(grown.iter().all(|b| *b == 0));

        // Up within the capacity it has: no chunk made, and zeros.
        let held = s.chunks();
        let capacity = s.files[&1].capacity();
        s.resize(1, cut + 1).unwrap();
        s.resize(1, capacity).unwrap();
        assert_eq!(s.chunks(), held);
        let mut tail = [1u8; 1];
        s.read(1, capacity - 1, &mut tail).unwrap();
        assert_eq!(tail, [0]);

        // Down onto a chunk's start: that chunk goes and nothing is zeroed.
        let boundary = starts[3];
        s.resize(1, boundary).unwrap();
        assert_eq!(s.files[&1].chunks.len(), 3);
        assert_eq!(s.files[&1].capacity(), boundary);

        // Down to nothing and up again.
        s.resize(1, 0).unwrap();
        assert_eq!(s.chunks(), 0);
        s.resize(1, 10).unwrap();
        let mut ten = [1u8; 10];
        s.read(1, 0, &mut ten).unwrap();
        assert_eq!(ten, [0; 10]);
        // An unchanged size, and a file the store has not seen cut to nothing.
        s.resize(1, 10).unwrap();
        s.resize(7, 0).unwrap();
        assert_eq!(s.files[&7].chunks.len(), 0);
    }

    #[test]
    fn the_budget_refuses_and_nothing_is_made() {
        let budget = Budget::new(10);
        let mut a = Chunked::new(Fake::new(None, BYTES), budget.clone());
        let mut b = Chunked::new(Fake::new(None, BYTES), budget.clone());
        // Ten 16 KiB chunks, then the budget is spent.
        a.resize(1, 8192).unwrap();
        for i in 1..10 {
            a.resize(1, 8192 + i * MIN_CHUNK).unwrap();
        }
        assert_eq!((budget.held(), budget.room()), (10, 0));
        let made = a.backing.made.get();
        assert_eq!(a.resize(1, 1 << 20), Err(Error::NoSpace));
        assert_eq!(b.resize(1, 1), Err(Error::NoSpace));
        assert_eq!(a.backing.made.get(), made);
        assert_eq!(b.backing.made.get(), 0);
        assert_eq!(a.files[&1].size, 8192 + 9 * MIN_CHUNK);
        assert!(!b.files.contains_key(&1));
        // A growth that needs more chunks than are left makes none of them.
        a.resize(1, MIN_CHUNK).unwrap();
        assert_eq!(budget.held(), 2);
        assert_eq!(b.resize(2, 9 * MAX_CHUNK), Err(Error::NoSpace));
        assert_eq!(b.backing.made.get(), 0);
        assert_eq!(budget.held(), 2);
        // A huge size is counted, not made.
        assert_eq!(b.resize(3, u64::MAX), Err(Error::NoSpace));
        assert_eq!(b.resize(3, 1 << 62), Err(Error::NoSpace));
        // Freed files, and dropped stores, give their handles back.
        b.resize(4, 8 * MAX_CHUNK).unwrap();
        assert_eq!(budget.held(), 10);
        b.free(4);
        assert_eq!(budget.held(), 2);
        drop(a);
        assert_eq!(budget.held(), 0);
    }

    #[test]
    fn the_account_is_filled_to_the_last_growth_that_fits() {
        // The boot test's account, written 16 KiB at a time, with the kernel's charges.
        let quota = 256 << 10;
        let mut s = Chunked::new(Fake::new(Some(quota), KERNEL), Budget::new(4096));
        let mut written = 0;
        let refused = loop {
            match s.resize(1, written + MIN_CHUNK) {
                Ok(()) => written += MIN_CHUNK,
                Err(e) => break e,
            }
        };
        assert_eq!(refused, Error::NoSpace);
        let used = s.backing.used.get();
        assert!(
            used <= quota && written + 2 * MIN_CHUNK > quota,
            "{written} {used}"
        );
        assert_eq!(s.files[&1].size, written);
        // A large account takes a large file in a few chunks.
        let gib = 1u64 << 30;
        let mut big = Chunked::new(Fake::new(Some(gib), KERNEL), Budget::new(4096));
        let mut at = 0;
        while big.resize(1, at + (1 << 20)).is_ok() {
            at += 1 << 20;
        }
        assert!(at >= gib - (8 << 20), "{at}");
        assert!(big.chunks() < 120, "{}", big.chunks());
        // A growth past the account makes nothing.
        let made = big.backing.made.get();
        assert_eq!(big.resize(2, gib), Err(Error::NoSpace));
        assert_eq!(big.backing.made.get(), made);
    }

    #[test]
    fn a_refused_chunk_drops_what_the_growth_made() {
        let budget = Budget::new(4096);
        let mut s = Chunked::new(Fake::new(None, BYTES), budget.clone());
        s.resize(1, 100).unwrap();
        s.write(1, 0, &[7; 100]).unwrap();
        s.backing.fail = Some(3);
        assert_eq!(s.resize(1, 3 * MAX_CHUNK), Err(Error::NoSpace));
        assert_eq!((s.chunks(), budget.held()), (1, 1));
        assert_eq!(s.backing.used.get(), PAGE);
        let mut back = [0u8; 100];
        s.read(1, 0, &mut back).unwrap();
        assert_eq!(back, [7; 100]);
        // Cost arithmetic at its edges.
        assert_eq!(KERNEL.fits(255), 0);
        assert_eq!(KERNEL.fits(256 + 4112), PAGE);
        assert_eq!(KERNEL.of(PAGE), 256 + 4112);
        assert_eq!(KERNEL.of(u64::MAX), u64::MAX);
    }

    #[test]
    fn a_scratch_directory_runs_on_chunks() {
        let budget = Budget::new(64);
        let mut s = Scratch::with_store(Box::new(Chunked::new(
            Fake::new(Some(1 << 20), KERNEL),
            budget.clone(),
        )));
        let f = s.create(0, b"f", 0o644).unwrap();
        let bytes = pattern(100_000, 3);
        assert_eq!(s.write(f, 50, &bytes), Ok(100_000));
        assert_eq!(s.read(f, 50, 100_000).unwrap(), bytes);
        assert_eq!(s.read(f, 0, 50).unwrap(), [0; 50]);
        s.truncate(f, 60).unwrap();
        s.truncate(f, 200).unwrap();
        assert_eq!(&s.read(f, 50, 150).unwrap()[..10], &bytes[..10]);
        assert_eq!(s.read(f, 60, 140).unwrap(), [0; 140]);
        assert_eq!(s.write(f, 0, &[1; 2 << 20]), Err(Error::NoSpace));
        assert_eq!(s.info(f).unwrap().size, 200);
        s.remove(0, b"f").unwrap();
        assert_eq!(budget.held(), 0);
    }
}
