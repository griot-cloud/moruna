//! Releases are attributed to the allocation they came from, not to whatever lives at the
//! address now (02 h, AR-I5).
//!
//! The defect these are written against: `release` found its block with a range query over
//! the live blocks, keyed by base offset and guarded by the block's charged range, and took
//! the released length off that block's outstanding count with a `saturating_sub`. Offsets
//! recycle, because a freed block goes on a free list and is carved up again, and the arena
//! never unmaps mid-run (AR-I3). So a release that arrived late or twice could land inside a
//! *different* live block, take its count to zero and put it back on the free list while a
//! `Buffer` still pointed at those bytes. The next allocation then handed the same bytes to a
//! second owner, silently: no crash, no counter, nothing in the report. A buffer now carries
//! a token naming its allocation and its incarnation, so a release of bytes that have already
//! gone back matches nothing and is refused.

mod common;

use std::collections::BTreeMap;

use moruna_kernel::{Allocator, Buffer, NO_TOKEN, Tier};

/// Every live buffer's byte range, so the test can say out loud what "two owners of the same
/// bytes" would look like. Ranges rather than bytes: a 512 MiB block is one entry, and the
/// overlap question is answered against the two neighbours in the map.
#[derive(Default)]
struct Claims {
    /// Start address to (end address, the allocation that claimed it).
    ranges: BTreeMap<usize, (usize, u64)>,
}

impl Claims {
    /// Claim `buffer`'s bytes for `id`; panics naming both owners if the range meets one that
    /// is already claimed.
    fn claim(&mut self, id: u64, buffer: &Buffer) {
        let start = buffer.host_ptr().expect("a host buffer") as usize;
        let end = start + buffer.len();
        if start == end {
            return;
        }
        if let Some((before, (before_end, other))) = self.ranges.range(..=start).next_back()
            && *before_end > start
        {
            panic!(
                "allocation {id} at {start:#x}..{end:#x} overlaps allocation {other} at \
                 {before:#x}..{before_end:#x}: the arena handed the same bytes to two owners"
            );
        }
        if let Some((after, (after_end, other))) = self.ranges.range(start..).next()
            && *after < end
        {
            panic!(
                "allocation {id} at {start:#x}..{end:#x} overlaps allocation {other} at \
                 {after:#x}..{after_end:#x}: the arena handed the same bytes to two owners"
            );
        }
        self.ranges.insert(start, (end, id));
    }

    /// Give `buffer`'s bytes back.
    fn release(&mut self, buffer: &Buffer) {
        let start = buffer.host_ptr().expect("a host buffer") as usize;
        if !buffer.is_empty() {
            self.ranges.remove(&start);
        }
    }
}

/// Allocate, split and drop across every size class until offsets have recycled many times
/// over, checking after every allocation that no byte belongs to two live buffers.
///
/// This is the test that would have caught the defect if the release path had been wrong in
/// a way the arena itself could notice; it passes on the old code too, because the old code's
/// misattribution needed a release the safe API cannot produce. That release is the second
/// test. Both are here because this one is what a future change to the free lists will trip
/// over, and that is the failure mode worth a permanent test.
#[test]
fn ar_offsets_recycle_without_two_owners_of_one_byte() {
    let arena = common::host_arena(256 << 20);
    let mut rng = common::Rng::new(0x5EED_0F17);
    let mut claims = Claims::default();
    let mut live: Vec<(u64, Buffer)> = Vec::new();
    let mut next_id = 0u64;
    let mut allocations = 0u64;

    for round in 0..3_000u32 {
        let bytes = 1 + rng.upto(3 << 20) as usize;
        let Ok(buffer) = arena.alloc(bytes, Tier::Host) else {
            // The region is full; drop the oldest half and carry on. Filling and emptying is
            // what makes offsets recycle, which is the point.
            for (_, buffer) in live.drain(..live.len() / 2 + 1) {
                claims.release(&buffer);
            }
            continue;
        };
        allocations += 1;
        // Half of them are split, so two buffers share one allocation and the arena must not
        // return it until both have gone (e.3).
        if round % 2 == 0 && buffer.len() > 1 {
            let mid = buffer.len() / 2;
            let (head, tail) = buffer.split_at(mid);
            next_id += 1;
            claims.claim(next_id, &head);
            live.push((next_id, head));
            next_id += 1;
            claims.claim(next_id, &tail);
            live.push((next_id, tail));
        } else {
            next_id += 1;
            claims.claim(next_id, &buffer);
            live.push((next_id, buffer));
        }
        // Drop something already live, at a position the generator picks, so the free lists
        // and the large area are both exercised out of order.
        if live.len() > 24 {
            let at = rng.upto(live.len() as u64) as usize;
            let (_, buffer) = live.remove(at.min(live.len() - 1));
            claims.release(&buffer);
        }
    }
    for (_, buffer) in live.drain(..) {
        claims.release(&buffer);
    }
    assert!(
        allocations > 1_000,
        "the soak has to actually allocate; it made {allocations}"
    );
    assert_eq!(
        arena.arena_stats().double_release,
        0,
        "no release in a correct run is refused"
    );
    assert_eq!(arena.stats().host_in_use, 0, "every byte came back");
}

/// A release that arrives after its allocation has gone back is refused, even when the bytes
/// it names now belong to somebody else.
///
/// This is the one that fails on the old code. The stale release names bytes that a live
/// buffer owns, and the old `release_large`/`release_class` pair credited it by address: the
/// block's outstanding count went to zero, it went back on the free list, and the next
/// allocation was served from bytes the live buffer still held. Here the token names an
/// incarnation that is over, so the release is refused and counted.
#[test]
fn ar_a_stale_release_is_refused_rather_than_credited_elsewhere() {
    let arena = common::host_arena(64 << 20);
    let before = arena.arena_stats().double_release;

    let victim = arena.alloc(1 << 20, Tier::Host).expect("a buffer");
    let base = victim.host_ptr().expect("a host buffer");
    let token = victim.token();
    assert_ne!(token, NO_TOKEN, "the arena tags every buffer it hands out");
    let handle = victim.arena().clone();
    drop(victim);

    // The same bytes, handed out again. On the old code the stale release below would have
    // put this one back on the free list under us.
    let heir = arena.alloc(1 << 20, Tier::Host).expect("the bytes again");
    assert_eq!(
        heir.host_ptr().expect("a host buffer"),
        base,
        "the fixture needs the address to be reused, which is what makes the bug possible"
    );
    assert_ne!(heir.token(), token, "and a new incarnation of it");

    handle.release_token(base, 1 << 20, Tier::Host, token);
    assert_eq!(
        arena.arena_stats().double_release,
        before + 1,
        "the stale release is counted"
    );
    assert_eq!(
        arena.stats().host_in_use,
        1 << 20,
        "and credited to nothing: the live buffer still holds its megabyte"
    );

    // A release with no token is refused for the same reason: nothing says which allocation
    // it belongs to, and guessing is what this change removes.
    handle.release(base, 1 << 20, Tier::Host);
    assert_eq!(arena.arena_stats().double_release, before + 2);
    assert_eq!(arena.stats().host_in_use, 1 << 20);

    drop(heir);
    assert_eq!(arena.stats().host_in_use, 0);
}
