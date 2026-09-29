//! SC-T22: a copy (no kernel stage) whose source is faster than its sink never has a read
//! refused by the arena. `is_full(0)` does not keep reads inside the arena (a queue over its
//! mark that can evict is draining, one waiting on an evicted head is not planned, 09 f.14 and
//! f.2), so here no mark is set at all and the source drive must stop issuing before the arena
//! is full rather than let a read find it full; and while an evicted entry waits for its
//! re-read, it issues no fresh read. f.5, SC-I3.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moruna_kernel::{
    Allocator, BoxFuture, CancelToken, Payload, Placement, Result, RowRange, Source, SourceSchema,
    Split, Tier,
};
use moruna_testkit::{FakeAllocator, FakePlacement, FakeSink, FakeSource};

use super::common::RigBuilder;

/// A morsel: 128 rows of the fake source's eight bytes.
const MORSEL: u64 = 1024;

/// The host tier holds eight morsels; the run moves forty-eight.
const ARENA: u64 = 8 * MORSEL;

fn copy_rig(alloc: FakeAllocator, read_ahead: u16) -> super::common::Rig {
    RigBuilder::new()
        .cfg(|cfg| {
            cfg.read_ahead = read_ahead;
            cfg.sink_concurrency = 2;
            cfg.initial_morsel_target = MORSEL;
            cfg.morsel_min = 8;
        })
        .source(FakeSource::new().splits(48, 128, MORSEL))
        .sink(FakeSink::new().latency(Duration::from_millis(4)))
        .alloc(alloc)
        .go()
}

#[test]
fn sc_t22_copy_behind_a_slow_sink_never_meets_a_full_arena() {
    for read_ahead in [1u16, 2, 4] {
        let alloc = FakeAllocator::new().with_limit(Tier::Host, ARENA);
        let rig = copy_rig(alloc.clone(), read_ahead);
        match rig.scheduler.run(CancelToken::new()) {
            Ok(crate::RunOutcome::Completed { .. }) => {}
            other => panic!(
                "read-ahead {read_ahead}: a copy behind a slow sink ended with {other:?}; the \
                 arena refused a read the drive should not have issued"
            ),
        }
        let mut written = rig.sink.written();
        written.sort_unstable();
        assert_eq!(
            written,
            (0..48).collect::<Vec<u64>>(),
            "read-ahead {read_ahead}: every morsel reached the sink once"
        );
        assert_eq!(
            alloc.in_use(Tier::Host),
            0,
            "read-ahead {read_ahead}: every read was released"
        );
    }
}

#[test]
fn sc_t22_copy_behind_a_slow_sink_fills_the_arena_rather_than_idling() {
    // The bound stops the drive at the arena, not at one morsel: with room for eight, the
    // queue behind the slow sink holds more than the read-ahead alone would put there.
    let alloc = FakeAllocator::new().with_limit(Tier::Host, ARENA);
    let rig = copy_rig(alloc.clone(), 2);
    let watcher = {
        let alloc = alloc.clone();
        std::thread::spawn(move || {
            let mut peak = 0;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                peak = peak.max(alloc.in_use(Tier::Host));
                if peak > 2 * MORSEL {
                    break;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            peak
        })
    };
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let peak = watcher.join().unwrap_or(0);
    assert!(
        peak > 2 * MORSEL && peak <= ARENA,
        "the queue behind the sink used the arena: peak {peak} of {ARENA}"
    );
}

/// A test-local `Source` (k allows one for a behaviour no fake knob provides): the fake source,
/// counting every read of a range it has not read before that was issued while Q0 held an
/// evicted entry.
struct Watching {
    inner: FakeSource,
    placement: FakePlacement,
    seen: Mutex<HashSet<(u32, u64)>>,
    fresh_behind_evicted: AtomicUsize,
}

impl Source for Watching {
    fn schema(&self) -> SourceSchema {
        Source::schema(&self.inner)
    }

    fn plan(&self) -> Result<Vec<Split>> {
        self.inner.plan()
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        let start = rows.map_or(0, |r| r.start);
        let fresh = self
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((split.id, start));
        if fresh && !Placement::evicted(&self.placement, 0).is_empty() {
            self.fresh_behind_evicted.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.read(split, rows, alloc, tier)
    }
}

#[test]
fn sc_t22_copy_behind_a_slow_sink_waits_for_its_replacements() {
    // Q0 evicts past two morsels. An evicted entry is re-read into the room its eviction gave,
    // so no fresh read is issued while one is waiting; the run still delivers every morsel.
    let placement = FakePlacement::new().with_pressure(0, 2 * MORSEL);
    let source = Arc::new(Watching {
        inner: FakeSource::new().splits(48, 128, MORSEL),
        placement: placement.clone(),
        seen: Mutex::new(HashSet::new()),
        fresh_behind_evicted: AtomicUsize::new(0),
    });
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.read_ahead = 2;
            cfg.initial_morsel_target = MORSEL;
            cfg.morsel_min = 8;
        })
        .source_over(Arc::clone(&source) as Arc<dyn Source>)
        .placement(placement)
        .sink(FakeSink::new().latency(Duration::from_millis(2)))
        .alloc(FakeAllocator::new().with_limit(Tier::Host, ARENA))
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let mut written = rig.sink.written();
    written.sort_unstable();
    assert_eq!(written, (0..48).collect::<Vec<u64>>(), "every morsel, once");
    assert_eq!(
        source.fresh_behind_evicted.load(Ordering::SeqCst),
        0,
        "a fresh read was issued while an evicted entry waited for its re-read"
    );
}
