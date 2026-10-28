//! The ring (design 21 c to e): a multi-producer, single-consumer queue of encoded records in a
//! byte region the caller provides.
//!
//! The region may be shared memory mapped into several processes, so it holds no pointer: a
//! 64-byte-aligned header of a magic, the data area's capacity and four counters, then the data
//! area. A position is a `u64` byte counter that only grows; the offset into the data area is
//! `pos & (capacity - 1)`. A slot is a `u32` length word, a `u32` state word and the bytes,
//! rounded up to 8.
//!
//! This is the only module of the crate with `unsafe`. It is needed for two things and used for
//! nothing else: reading a header word of the region as an atomic (`Core::counter`,
//! `Core::word`), and copying bytes into or out of a slot a producer or the consumer owns by the
//! protocol of design 21 d.

#![allow(unsafe_code)]

use core::fmt;
use core::marker::PhantomData;
use core::ptr;
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::error::Error;
use crate::record::encode_into;

/// The first eight bytes of a formatted region: "MRNGREC1".
pub const MAGIC: u64 = 0x4D52_4E47_5245_4331;
/// The size of the header; the data area starts here.
pub const HEADER: usize = 320;
/// The smallest data area.
pub const MIN_CAPACITY: u64 = 64;
/// The largest data area, so that a slot's length and a padding's fit the length word.
pub const MAX_CAPACITY: u64 = 1 << 30;

const OFF_MAGIC: usize = 0;
const OFF_CAPACITY: usize = 8;
const OFF_RESERVE: usize = 64;
const OFF_COMMIT: usize = 128;
const OFF_READ: usize = 192;
const OFF_DROPPED: usize = 256;

/// The bytes before a slot's payload: the length word and the state word.
const SLOT_HEAD: u64 = 8;
/// State: the slot is claimed and being written (also what a released slot reads).
const WRITING: u32 = 0;
/// State: the slot holds a record.
const READY: u32 = 1;
/// State: the slot is padding, to the end of the data area or after a failed write.
const PADDING: u32 = 2;
/// Set in the length word by the producer that claims the slot, so a claimed slot never reads
/// as zero (design 21 e.5).
const CLAIMED: u32 = 1 << 31;

/// The bytes a slot of `len` payload bytes takes.
const fn slot_size(len: u64) -> u64 {
    (SLOT_HEAD + len + 7) & !7
}

/// The ring refused a record: no room now, or never (a record bigger than the data area). The
/// refusal was counted in the ring's `dropped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

impl fmt::Display for Full {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the ring has no room for the record")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Full {}

/// Why `push_record` pushed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// The ring had no room (counted in `dropped`).
    Full,
    /// The parts are not a record.
    Record(Error),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PushError::Full => Full.fmt(f),
            PushError::Record(e) => e.fmt(f),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PushError {}

/// The region's address and the data area's size: what every handle holds. All access to the
/// region goes through its methods.
#[derive(Clone, Copy)]
struct Core {
    base: *mut u8,
    capacity: u64,
}

impl Core {
    fn mask(&self) -> u64 {
        self.capacity - 1
    }

    /// The header counter at `off`, as an atomic.
    fn counter(&self, off: usize) -> &AtomicU64 {
        // SAFETY: `off` is one of the OFF_* constants, each a multiple of 8 below HEADER; the
        // region is 64-byte aligned (checked by `init` and `attach`) and at least HEADER bytes
        // long, and lives as long as the handle (`Ring`'s lifetime). Every access to these
        // eight bytes, by every handle and process, is atomic.
        unsafe { AtomicU64::from_ptr(self.base.add(off).cast::<u64>()) }
    }

    /// One of a slot's two words, at the data-area offset `off` plus `word` (0 or 4), as an atomic.
    fn word(&self, off: usize, word: usize) -> &AtomicU32 {
        debug_assert!(off as u64 <= self.capacity - SLOT_HEAD);
        // SAFETY: `off` is `pos & mask` for a position that is a multiple of 8 and at most
        // capacity - 8, so HEADER + off + 8 is inside the region (the data area is `capacity`
        // bytes from HEADER, checked by `init` and `attach`) and the address is 4-aligned (the
        // region is 64-aligned, HEADER and `off` are multiples of 8). Every access to a slot's
        // two words is atomic.
        unsafe { AtomicU32::from_ptr(self.base.add(HEADER + off + word).cast::<u32>()) }
    }

    fn len_word(&self, off: usize) -> &AtomicU32 {
        self.word(off, 0)
    }

    fn state_word(&self, off: usize) -> &AtomicU32 {
        self.word(off, 4)
    }

    fn magic(&self) -> &AtomicU64 {
        self.counter(OFF_MAGIC)
    }

    fn reserve(&self) -> &AtomicU64 {
        self.counter(OFF_RESERVE)
    }

    fn commit(&self) -> &AtomicU64 {
        self.counter(OFF_COMMIT)
    }

    fn read(&self) -> &AtomicU64 {
        self.counter(OFF_READ)
    }

    fn dropped(&self) -> &AtomicU64 {
        self.counter(OFF_DROPPED)
    }

    /// Zero the `size` bytes of the slot at `off`, head and payload: what a released slot leaves
    /// behind, so that every byte outside a claimed slot is zero and a claimed slot's state
    /// reads 0 until its producer publishes (design 21 d.2).
    fn zero(&self, off: usize, size: usize) {
        debug_assert!(off as u64 + size as u64 <= self.capacity);
        // SAFETY: the slot is the consumer's: it was published (or declared dead) and is not
        // released until this call, so no producer writes it and nobody reads it after; `off`
        // and `size` are a slot `extent` proved inside the data area.
        unsafe { ptr::write_bytes(self.base.add(HEADER + off), 0, size) }
    }

    /// The first payload byte of the slot at `off`.
    fn payload(&self, off: usize) -> *mut u8 {
        // SAFETY: the same bound as `word`: the slot's head is inside the region, and the
        // payload starts SLOT_HEAD bytes after it, inside the slot the caller holds.
        unsafe { self.base.add(HEADER + off + SLOT_HEAD as usize) }
    }
}

/// A ring in a region the caller provides.
pub struct Ring<'a> {
    core: Core,
    consumer_taken: AtomicBool,
    _region: PhantomData<&'a mut [u8]>,
}

// SAFETY: a `Ring` is an address and a size; every access through it to the region is atomic or
// confined to a slot by the claim protocol (design 21 d), so handles may be used from any
// thread.
unsafe impl Send for Ring<'_> {}
// SAFETY: as above; the one non-atomic field of a `Ring` is none, `consumer_taken` is atomic.
unsafe impl Sync for Ring<'_> {}

impl<'a> Ring<'a> {
    /// Format `region` as an empty ring. The region must start on a 64-byte boundary and hold
    /// the header and at least `MIN_CAPACITY` bytes; the data area is the largest power of two
    /// (at most `MAX_CAPACITY`) that fits after the header. Everything in the region is zeroed.
    pub fn init(region: &'a mut [u8]) -> Result<Ring<'a>, Error> {
        check_shape(region)?;
        let room = (region.len() - HEADER) as u64;
        let capacity = (1u64 << room.ilog2()).min(MAX_CAPACITY);
        region.fill(0);
        region[OFF_CAPACITY..OFF_CAPACITY + 8].copy_from_slice(&capacity.to_ne_bytes());
        let core = Core {
            base: region.as_mut_ptr(),
            capacity,
        };
        // The magic goes last, with Release, so a handle that sees it sees the rest.
        core.magic().store(MAGIC, Ordering::Release);
        Ok(Ring::new(core))
    }

    /// Join a region `init` formatted, checking its header.
    pub fn attach(region: &'a mut [u8]) -> Result<Ring<'a>, Error> {
        check_shape(region)?;
        let base = region.as_mut_ptr();
        let probe = Core { base, capacity: 1 };
        if probe.magic().load(Ordering::Acquire) != MAGIC {
            return Err(Error::BadMagic);
        }
        let mut word = [0u8; 8];
        word.copy_from_slice(&region[OFF_CAPACITY..OFF_CAPACITY + 8]);
        let capacity = u64::from_ne_bytes(word);
        if !capacity.is_power_of_two() {
            return Err(Error::BadHeader("the capacity is not a power of two"));
        }
        if !(MIN_CAPACITY..=MAX_CAPACITY).contains(&capacity) {
            return Err(Error::BadHeader(
                "the capacity is outside the supported sizes",
            ));
        }
        if capacity > (region.len() - HEADER) as u64 {
            return Err(Error::BadHeader("the capacity is larger than the region"));
        }
        let core = Core { base, capacity };
        let read = core.read().load(Ordering::Acquire);
        let reserve = core.reserve().load(Ordering::Acquire);
        if read > reserve || reserve - read > capacity {
            return Err(Error::BadHeader("the counters do not describe a ring"));
        }
        Ok(Ring::new(core))
    }

    fn new(core: Core) -> Ring<'a> {
        Ring {
            core,
            consumer_taken: AtomicBool::new(false),
            _region: PhantomData,
        }
    }

    /// A producer handle. Any number, in any thread or process, may push at once.
    pub fn producer(&self) -> Producer<'_> {
        Producer {
            core: self.core,
            _ring: PhantomData,
        }
    }

    /// The consumer, resuming at what the ring has released. There is one consumer: a second
    /// call on this handle is refused while the first lives; across handles and processes the
    /// caller keeps the rule (design 21 c.4).
    pub fn consumer(&self) -> Result<Consumer<'_>, Error> {
        if self
            .consumer_taken
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::ConsumerTaken);
        }
        Ok(Consumer {
            core: self.core,
            pos: self.core.read().load(Ordering::Acquire),
            held: None,
            fault: None,
            taken: &self.consumer_taken,
        })
    }

    /// The data area's size in bytes.
    pub fn capacity(&self) -> u64 {
        self.core.capacity
    }

    /// The next byte a producer claims.
    pub fn reserved(&self) -> u64 {
        self.core.reserve().load(Ordering::Acquire)
    }

    /// The end of the last slot the consumer has seen settled, in order.
    pub fn committed(&self) -> u64 {
        self.core.commit().load(Ordering::Acquire)
    }

    /// What the consumer has released.
    pub fn released(&self) -> u64 {
        self.core.read().load(Ordering::Acquire)
    }

    /// The records refused so far.
    pub fn dropped(&self) -> u64 {
        self.core.dropped().load(Ordering::Relaxed)
    }
}

fn check_shape(region: &[u8]) -> Result<(), Error> {
    if !(region.as_ptr() as usize).is_multiple_of(64) {
        return Err(Error::Misaligned);
    }
    let need = HEADER + MIN_CAPACITY as usize;
    if region.len() < need {
        return Err(Error::RegionTooSmall {
            len: region.len(),
            need,
        });
    }
    Ok(())
}

/// A handle that pushes records. Cheap to create; one per thread or process is the intended use,
/// and sharing one is as safe.
pub struct Producer<'r> {
    core: Core,
    _ring: PhantomData<&'r ()>,
}

// SAFETY: a producer is an address and a size, and every access it makes is atomic or to a slot
// it alone has claimed (design 21 d.1).
unsafe impl Send for Producer<'_> {}
// SAFETY: as above; `push` takes `&self` and shares nothing but the region's atomics.
unsafe impl Sync for Producer<'_> {}

/// A slot a producer has claimed: `off` in the data area, `len` payload bytes.
struct Claim {
    off: usize,
    len: usize,
}

impl Producer<'_> {
    /// Push one already-encoded record. Never blocks and never waits for the consumer: when the
    /// ring has no room it counts the refusal in `dropped` and returns `Full`.
    pub fn push(&self, record_bytes: &[u8]) -> Result<(), Full> {
        let claim = self.claim(record_bytes.len())?;
        // SAFETY: the claim gave this producer the `len` payload bytes at `payload(off)`
        // exclusively until it publishes the slot, and they are inside the slot.
        unsafe {
            ptr::copy_nonoverlapping(
                record_bytes.as_ptr(),
                self.core.payload(claim.off),
                claim.len,
            );
        }
        self.publish(&claim, READY);
        Ok(())
    }

    /// Encode a record straight into a claimed slot, so the producer path allocates nothing.
    pub fn push_record(
        &self,
        seq: u64,
        ts: i64,
        subject: u32,
        msg_id: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(), PushError> {
        let len = crate::record::encoded_len(msg_id, headers, body).map_err(PushError::Record)?;
        let claim = self.claim(len).map_err(|_| PushError::Full)?;
        // SAFETY: as in `push`: the claim is this producer's alone until it publishes.
        let slot = unsafe { slice::from_raw_parts_mut(self.core.payload(claim.off), claim.len) };
        match encode_into(slot, seq, ts, subject, msg_id, headers, body) {
            Ok(_) => {
                self.publish(&claim, READY);
                Ok(())
            }
            Err(e) => {
                // Not reachable after `encoded_len` accepted the parts; if it ever is, the slot
                // becomes padding so the ring is not left with a hole.
                self.publish(&claim, PADDING);
                Err(PushError::Record(e))
            }
        }
    }

    /// Claim a slot of `len` payload bytes (design 21 d.1 steps 2 and 3).
    fn claim(&self, len: usize) -> Result<Claim, Full> {
        let core = &self.core;
        let slot = slot_size(len as u64);
        if slot > core.capacity {
            core.dropped().fetch_add(1, Ordering::Relaxed);
            return Err(Full);
        }
        loop {
            // `read` first: it can only be behind `reserve` read after it.
            let read = core.read().load(Ordering::Acquire);
            let reserve = core.reserve().load(Ordering::Acquire);
            let off = reserve & core.mask();
            let tail = core.capacity - off;
            if slot <= tail {
                if reserve - read + slot > core.capacity {
                    core.dropped().fetch_add(1, Ordering::Relaxed);
                    return Err(Full);
                }
                if core
                    .reserve()
                    .compare_exchange_weak(
                        reserve,
                        reserve + slot,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    let claim = Claim {
                        off: off as usize,
                        len,
                    };
                    core.len_word(claim.off)
                        .store(len as u32 | CLAIMED, Ordering::Relaxed);
                    return Ok(claim);
                }
            } else {
                // The slot would cross the end: pad to the end first, then claim again from 0.
                if reserve - read + tail > core.capacity {
                    core.dropped().fetch_add(1, Ordering::Relaxed);
                    return Err(Full);
                }
                if core
                    .reserve()
                    .compare_exchange_weak(
                        reserve,
                        reserve + tail,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    let pad = Claim {
                        off: off as usize,
                        len: (tail - SLOT_HEAD) as usize,
                    };
                    core.len_word(pad.off)
                        .store(pad.len as u32 | CLAIMED, Ordering::Relaxed);
                    self.publish(&pad, PADDING);
                }
            }
        }
    }

    fn publish(&self, claim: &Claim, state: u32) {
        self.core
            .state_word(claim.off)
            .store(state, Ordering::Release);
    }
}

/// The one reader of the ring. A record it returns stays valid until its next `pop`.
pub struct Consumer<'r> {
    core: Core,
    /// The position of the oldest slot not released.
    pos: u64,
    /// The size of the slot last returned, released by the next `pop`.
    held: Option<u64>,
    fault: Option<Error>,
    taken: &'r AtomicBool,
}

// SAFETY: a consumer is an address and a size and a flag reference; it reads only slots a
// producer has published and writes only to release them (design 21 d.2).
unsafe impl Send for Consumer<'_> {}

impl Drop for Consumer<'_> {
    fn drop(&mut self) {
        // A slot held and not released stays in the ring: the next consumer delivers it again.
        self.taken.store(false, Ordering::Release);
    }
}

impl Consumer<'_> {
    /// The oldest record not yet returned, or `None` when the next slot is empty or still being
    /// written (a later slot may be ready; order is kept). Releases the record the last call
    /// returned.
    pub fn pop(&mut self) -> Option<&[u8]> {
        self.step(|_| false)
    }

    /// Like `pop`, and a slot that is still being written at position `p` counts as dead
    /// padding when `is_stale(p)` says its producer is gone (design 21 e). `is_stale` is called
    /// only for a slot that has been claimed.
    pub fn pop_or_skip_stale(&mut self, is_stale: impl Fn(u64) -> bool) -> Option<&[u8]> {
        self.step(is_stale)
    }

    /// The records refused so far.
    pub fn dropped(&self) -> u64 {
        self.core.dropped().load(Ordering::Relaxed)
    }

    /// Why this consumer stopped returning records, if it did: a slot no producer writes, or a
    /// dead slot of unknown extent. A faulted consumer returns `None` for ever.
    pub fn fault(&self) -> Option<Error> {
        self.fault
    }

    /// The position of the oldest slot not released.
    pub fn position(&self) -> u64 {
        self.pos
    }

    fn release(&mut self, size: u64) {
        let off = (self.pos & self.core.mask()) as usize;
        self.core.zero(off, size as usize);
        self.pos += size;
        self.core.read().store(self.pos, Ordering::Release);
    }

    /// The size of the slot at `pos` whose length word is `word`, or the fault.
    fn extent(&self, pos: u64, word: u32) -> Result<u64, Error> {
        if word & CLAIMED == 0 {
            return Err(Error::CorruptSlot { pos });
        }
        let size = slot_size(u64::from(word & !CLAIMED));
        if size > self.core.capacity - (pos & self.core.mask()) {
            return Err(Error::CorruptSlot { pos });
        }
        Ok(size)
    }

    fn step(&mut self, is_stale: impl Fn(u64) -> bool) -> Option<&[u8]> {
        if self.fault.is_some() {
            return None;
        }
        if let Some(size) = self.held.take() {
            self.release(size);
        }
        loop {
            let pos = self.pos;
            let off = (pos & self.core.mask()) as usize;
            let state = self.core.state_word(off).load(Ordering::Acquire);
            let word = self.core.len_word(off).load(Ordering::Relaxed);
            let size = match state {
                READY | PADDING => self.extent(pos, word),
                WRITING => {
                    let claimed = pos < self.core.reserve().load(Ordering::Acquire);
                    if !(claimed && is_stale(pos)) {
                        return None;
                    }
                    if word == 0 {
                        Err(Error::StaleExtentUnknown { pos })
                    } else {
                        self.extent(pos, word)
                    }
                }
                _ => Err(Error::CorruptSlot { pos }),
            };
            let size = match size {
                Ok(size) => size,
                Err(e) => {
                    self.fault = Some(e);
                    return None;
                }
            };
            self.core.commit().store(pos + size, Ordering::Release);
            if state == READY {
                self.held = Some(size);
                let len = (word & !CLAIMED) as usize;
                // SAFETY: the state was READY, read with Acquire, so the producer's writes of the
                // payload happened before; `extent` proved the slot inside the data area; and the
                // slot is not released, so no producer writes it, until the next `step`, which
                // needs `&mut self` and so ends this borrow first.
                return Some(unsafe { slice::from_raw_parts(self.core.payload(off), len) });
            }
            self.release(size);
        }
    }
}
