//! RB5, amended 2026-11-14: a memory wall is met by making room, then by waiting only while
//! something else can still free memory. A fixed count of timed tries is not a rule: it gave up
//! on walls that were about to move and would have waited out walls that never could.
//!
//! Work already under way (a unit's output, a sink's encode buffer) that the arena refuses first
//! asks placement for room: queued batches leave memory in the eviction order, which weighs how
//! soon each is needed against what bringing it back costs per byte freed: a batch that already
//! has a disk copy first, then a source's batch to be read again, then one written to disk
//! (09 f.18, PL-I20). A read at the cursor does not take room from batches queued before it, which
//! are needed sooner; it waits for the consumers, which is back-pressure and not a wall.
//!
//! The wait then lasts while the run can still free memory: room was made and is on its way,
//! progress was made since the last look (a unit taken, a batch freed, spilled, read back or
//! evicted, a read pushed, a sink write landed), or work is in flight elsewhere (a unit running
//! on another worker, a read, a write, a move, a queued encode). A wall that nothing in the run
//! can move for [`QUIET_LIMIT`] ends the run with what it found.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::shared::Shared;

/// How long a wall may stand with nothing in the run able to move it before the run ends.
pub(crate) const QUIET_LIMIT: Duration = Duration::from_secs(5);

/// The longest wait between two looks at a wall.
const MAX_WAIT: Duration = Duration::from_millis(50);

/// Whether the one who met the wall may take room from queued batches.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Room {
    /// Work under way: queued batches, needed later, may leave memory for it.
    Make,
    /// A read at the cursor: it waits for the batches before it to be consumed.
    Wait,
}

/// What a refusal leads to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Look again after this long.
    Retry(Duration),
    /// Nothing in the run can free memory: the run ends, and this says what was found.
    GiveUp(String),
}

/// One wall, held by whoever met it, for as long as it stands.
#[derive(Debug)]
pub(crate) struct Wall {
    looks: u32,
    signature: u64,
    quiet_since: Option<Instant>,
}

impl Wall {
    /// A wall met now.
    pub(crate) fn new(shared: &Shared) -> Wall {
        Wall {
            looks: 0,
            signature: progress(shared),
            quiet_since: None,
        }
    }

    /// A refusal of `bytes`. `own_slots` is how many busy slots the one who met it holds itself
    /// (a worker its own; a drive none), so that it does not count as work in flight elsewhere.
    pub(crate) fn refused(
        &mut self,
        shared: &Shared,
        bytes: u64,
        room: Room,
        own_slots: usize,
    ) -> Next {
        self.looks = self.looks.saturating_add(1);
        let made = room == Room::Make && make_room(shared, bytes);
        let now = progress(shared);
        let moved = now != self.signature;
        self.signature = now;
        if made || moved || others_working(shared, own_slots) {
            self.quiet_since = None;
            return Next::Retry(wait(self.looks, made));
        }
        let since = *self.quiet_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= QUIET_LIMIT {
            let stats = shared.placement_stats();
            return Next::GiveUp(format!(
                "a request for {bytes} bytes met a memory wall that nothing in the run could move \
                 for {} s: no queued batch could leave memory ({} bytes queued in memory, disk \
                 tier {}), no other unit, read, write or move was in flight, and nothing \
                 progressed",
                QUIET_LIMIT.as_secs(),
                stats.catalog.resident_by_tier.iter().sum::<u64>(),
                if stats.catalog.budget_by_tier[moruna_kernel::TierKind::Disk.index()] > 0 {
                    "on"
                } else {
                    "off"
                },
            ));
        }
        Next::Retry(wait(self.looks, false))
    }
}

/// The wait before the next look: a millisecond when room is on its way, else doubling from one
/// millisecond to [`MAX_WAIT`].
fn wait(looks: u32, made: bool) -> Duration {
    if made {
        return Duration::from_millis(1);
    }
    Duration::from_millis(1u64 << looks.min(6)).min(MAX_WAIT)
}

/// Ask placement for `bytes` of room in the host tier (09 f.18). True when a batch left memory or
/// is on its way out.
pub(crate) fn make_room(shared: &Shared, bytes: u64) -> bool {
    let tier = crate::decide::host_tier(shared).kind();
    let made = shared.placement.shed(tier, bytes.max(1));
    if made {
        shared.walls_room_made.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(target: "sched.wall", bytes, "room made for work under way");
    }
    made
}

/// A figure that moves whenever the run moves: the catalog's releases, frees, spills, reloads,
/// evictions and overflows, every stage's takes, every source's pushes and every sink's writes.
fn progress(shared: &Shared) -> u64 {
    let stats = shared.placement_stats();
    let c = &stats.catalog;
    let mut sum = c
        .released
        .wrapping_add(c.freed)
        .wrapping_add(c.spills)
        .wrapping_add(c.reloads)
        .wrapping_add(c.evictions)
        .wrapping_add(c.overflows);
    for stage in &shared.stages {
        sum = sum.wrapping_add(stage.taken.load(Ordering::SeqCst));
    }
    for source in &shared.sources {
        sum = sum.wrapping_add(source.out.morsels.load(Ordering::SeqCst));
    }
    for sink in &shared.sinks {
        sum = sum.wrapping_add(sink.input.morsels.load(Ordering::SeqCst));
    }
    sum
}

/// True while work elsewhere may still free memory: a slot busy beyond those held by walls, a
/// read, a write or a move in flight, or an offloaded job queued (the one waiting runs those).
fn others_working(shared: &Shared, own_slots: usize) -> bool {
    let at_walls = shared.wall_waiters.load(Ordering::SeqCst).max(own_slots);
    usize::from(shared.knobs.slots.busy()) > at_walls
        || shared.reads_in_flight.load(Ordering::SeqCst) > 0
        || shared.writes_in_flight.load(Ordering::SeqCst) > 0
        || shared.offload.queued() > 0
        || shared.placement_stats().in_flight_bytes > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_to_its_cap_and_is_short_when_room_is_coming() {
        assert_eq!(wait(1, true), Duration::from_millis(1));
        assert_eq!(wait(1, false), Duration::from_millis(2));
        assert_eq!(wait(3, false), Duration::from_millis(8));
        assert_eq!(wait(30, false), MAX_WAIT);
    }
}
