//! PL-T32 to PL-T36 (f.18): the buffer catalog over the engine.
//!
//! PL-T32 a_fanned_out_batch_moves_once: a batch pushed into two queues is one batch, spilled
//! once, in one state for both queues, and each consumer gets its bytes. PL-T33
//! known_future_chooses_what_leaves: under pressure the batch whose next use is furthest leaves
//! first, one with a disk copy before one that must be written, and none a consumer is about to
//! take, whichever queue's tail it is at. PL-T34 held_batches_stay: a batch a running unit took
//! never leaves memory, and leaves once released. PL-T35 events_and_reservations: the catalog's
//! events, and a claim that never passes the budget. PL-T36 evicted_once_and_resumed_once: a
//! source's evicted batch is listed and replaced once for every queue; a spilled fan-out comes
//! back from a manifest as one batch with both its consumers.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moruna_kernel::{
    CheckpointExtras, Consumer, Link, Locality, Morsel, Placement, Producer, SourceCursor, Tier,
    TierKind, Topology,
};
use moruna_kernel::{Reserve, Residency};
use moruna_placement::catalog::{CatalogEvent, CatalogObserver};
use moruna_placement::state::State;
use moruna_placement::{PlacementConfig, PlacementEngine};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

const ROWS: usize = 256;
const HOST: usize = 2;

/// One source fanning out to two stages, each to its own sink: queues 0 and 1 from the source,
/// 2 and 3 to the sinks.
fn fan_out() -> Topology {
    let k = |stage| Consumer::Kernel { stage, port: 0 };
    Topology::new(
        1,
        2,
        2,
        vec![
            Link {
                from: Producer::Source(0),
                to: k(1),
            },
            Link {
                from: Producer::Source(0),
                to: k(2),
            },
            Link {
                from: Producer::Kernel(1),
                to: Consumer::Sink(0),
            },
            Link {
                from: Producer::Kernel(2),
                to: Consumer::Sink(1),
            },
        ],
    )
    .expect("a fan-out")
}

fn config(dir: &std::path::Path, host: u64) -> PlacementConfig {
    let mut cfg = common::config(
        1,
        Some(dir.to_path_buf()),
        common::budgets(host, 0, 1 << 30),
    );
    cfg.topology = fan_out();
    cfg.fingerprints = vec![
        moruna_kernel::Fingerprint::compute("a", b"1"),
        moruna_kernel::Fingerprint::compute("b", b"1"),
    ];
    cfg.resume_policy = vec![moruna_kernel::ResumePolicy::Reinit; 2];
    cfg
}

/// The values batch `seq` carries, distinct per batch.
fn values(seq: u64) -> Vec<i32> {
    (0..ROWS as i32).map(|i| seq as i32 * 10_000 + i).collect()
}

fn batch(alloc: &FakeAllocator, seq: u64) -> Morsel {
    common::valued_morsel(alloc, seq, 0, &values(seq))
}

/// Both source queues stage, with a window of one.
fn staging(engine: &PlacementEngine) {
    for q in [0, 1] {
        engine.set_staging(q, true);
        engine.set_promotion_window(q, 1);
    }
}

/// Take every batch of `queue` in order, check its bytes, release it.
fn drain(engine: &PlacementEngine, reactor: &FakeReactor, queue: u16, expect: &[u64]) {
    let mut got = Vec::new();
    while got.len() < expect.len() {
        match engine
            .pop(queue, common::want_host(), Locality::Any)
            .expect("pop")
        {
            Some(morsel) => {
                assert_eq!(
                    common::values_of(&morsel),
                    values(morsel.seq),
                    "queue {queue} got the bytes of batch {}",
                    morsel.seq
                );
                engine.release(queue, morsel.seq).expect("release");
                got.push(morsel.seq);
            }
            None => common::settle(reactor),
        }
    }
    assert_eq!(got, expect, "queue {queue} in order");
}

fn state(engine: &PlacementEngine, queue: u16, seq: u64) -> State {
    engine
        .entry_states(queue)
        .into_iter()
        .find(|(s, _)| *s == seq)
        .map(|(_, state)| state)
        .unwrap_or(State::Consumed)
}

fn writes(reactor: &FakeReactor) -> usize {
    reactor
        .ops()
        .iter()
        .filter(|op| op.kind == OpKind::WriteFile)
        .count()
}

#[test]
fn pl_t32_a_fanned_out_batch_moves_once() {
    let scratch = common::Scratch::new("t32");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let engine = common::engine(config(scratch.path(), 1 << 30), &alloc, &reactor);
    staging(&engine);
    let heard = Arc::new(Heard::default());
    engine.observe(heard.clone());
    let b = batch(&alloc, 0).bytes;
    for q in [0, 1] {
        engine.set_water(q, TierKind::Host, 2 * b, 3 * b);
    }
    let pushed: Vec<u64> = (0..12).collect();
    for &seq in &pushed {
        engine
            .push_to(&[0, 1], batch(&alloc, seq))
            .expect("fans out");
    }
    common::settle(&reactor);
    let stats = engine.stats();
    assert_eq!(stats.catalog.buffers, 12, "twelve batches, not twenty-four");
    for &seq in &pushed {
        let a = engine.buffer_of(0, seq);
        assert!(a.is_some(), "batch {seq} is in the catalog");
        assert_eq!(a, engine.buffer_of(1, seq), "both entries name one batch");
    }
    let states = |q| {
        engine
            .entry_states(q)
            .into_iter()
            .map(|(seq, state)| (seq, format!("{state:?}")))
            .collect::<Vec<_>>()
    };
    assert_eq!(states(0), states(1), "one batch, one state, in both queues");
    let spilled = stats.catalog.spills;
    assert!(spilled > 0, "the pressure must spill");
    assert!(
        spilled <= 12,
        "each batch is written at most once: {spilled}"
    );
    assert_eq!(stats.catalog.spilled_bytes, spilled * b);
    assert!(
        stats.queues[0].bytes_by_tier[HOST] <= 3 * b,
        "queue 0 within its share"
    );
    assert_eq!(
        stats.queues[0].bytes_by_tier[TierKind::Disk.index()],
        stats.catalog.resident_by_tier[TierKind::Disk.index()],
        "what is on disk is the same batches for both queues"
    );

    drain(&engine, &reactor, 0, &pushed);
    drain(&engine, &reactor, 1, &pushed);
    let done = engine.stats().catalog;
    assert_eq!((done.buffers, done.freed, done.released), (0, 12, 24));
    assert_eq!(done.resident_by_tier, [0; moruna_kernel::TIER_COUNT]);
    assert_eq!(done.reserved_by_tier, [0; moruna_kernel::TIER_COUNT]);
    let mut written: Vec<u64> = heard
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|event| match event {
            CatalogEvent::Spilled { batch, .. } => Some(batch.0),
            _ => None,
        })
        .collect();
    let spills = written.len() as u64;
    written.sort_unstable();
    written.dedup();
    assert_eq!(written.len() as u64, spills, "no batch is written twice");
    assert_eq!(done.spills, spills);
    assert!(spills >= spilled);
    assert!(done.reloads >= 1 && done.reloaded_bytes == done.reloads * b);
    // Closed, the queues append no more, so a segment whose records are all released is
    // reclaimable.
    engine.close(0);
    engine.close(1);
    let detail = engine.detailed_stats();
    assert!(detail.segments_live > 0);
    assert_eq!(
        detail.segments_reclaimable, detail.segments_live,
        "every record is released once its batch is freed (f.7)"
    );
    assert_eq!(moruna_placement::locks::violations(), 0, "lock order (4.2)");
}

#[test]
fn pl_t33_known_future_chooses_what_leaves() {
    let scratch = common::Scratch::new("t33");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let engine = common::engine(config(scratch.path(), 1 << 30), &alloc, &reactor);
    staging(&engine);
    let b = batch(&alloc, 0).bytes;
    // X0..X4 wait in queue 0 alone; Y is the tail of queue 0 and the head of queue 1.
    for seq in 0..5 {
        engine.push(0, batch(&alloc, seq)).expect("push");
    }
    let y = 5;
    engine.push_to(&[0, 1], batch(&alloc, y)).expect("fans out");
    // X3 gets a disk copy: spilled and read back, it is `Resident + OnDisk`.
    let x3 = engine.buffer_of(0, 3).expect("X3");
    engine.spill(x3).expect("spill");
    assert!(matches!(state(&engine, 0, 3), State::OnDisk(_)));
    engine.reload(x3).expect("reload");
    assert!(matches!(state(&engine, 0, 3), State::ResidentOnDisk(_)));
    assert!(
        engine.reload(x3).is_err(),
        "only a batch on disk is read back"
    );
    let before = writes(&reactor);

    // Shed one batch from queue 0. X3 (next use 3, nothing to write: 4 x b / b) goes before X4
    // (next use 4, written: 5 x b / 2b), and Y, queue 0's tail, is queue 1's head.
    engine.set_water(0, TierKind::Host, 5 * b, 5 * b);
    assert!(matches!(state(&engine, 0, 3), State::OnDisk(_)), "X3 left");
    assert_eq!(writes(&reactor), before, "by dropping its copy: no IO");
    assert!(matches!(state(&engine, 0, 4), State::Resident(_)));
    assert!(
        matches!(state(&engine, 0, y), State::Resident(_)),
        "Y stays"
    );
    // One more: X4, the furthest of those left, is written.
    engine.set_water(0, TierKind::Host, 4 * b, 4 * b);
    assert!(matches!(state(&engine, 0, 4), State::OnDisk(_)), "X4 left");
    assert!(writes(&reactor) > before, "by a spill");
    assert!(
        matches!(state(&engine, 0, y), State::Resident(_)),
        "Y stays"
    );
    // Down to one: X1 and X2 go; the heads X0 and Y cannot, and the queue is full.
    engine.set_water(0, TierKind::Host, b, b);
    for seq in [1, 2] {
        assert!(
            matches!(state(&engine, 0, seq), State::OnDisk(_)),
            "X{seq} left"
        );
    }
    for seq in [0, y] {
        assert!(
            matches!(state(&engine, 0, seq), State::Resident(_)),
            "{seq} stays"
        );
    }
    // Still above the mark with only the two heads left: nothing more can leave, and the
    // queue reports full until a consumer takes one (f.14).
    assert!(
        engine.is_full(0),
        "nothing can leave: the queue reports full (f.14)"
    );
    let catalog = engine.stats().catalog;
    assert_eq!(
        catalog.evictions, 5,
        "X3 by hand, then X3 again, X4, X1, X2"
    );
    assert_eq!(catalog.spills, 4, "X3 by hand, then X4, X1, X2");

    // The snapshot a policy reads: every batch with its future.
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.batches.len(), 6);
    let view = snapshot
        .batches
        .iter()
        .find(|v| Some(v.id) == engine.buffer_of(1, y))
        .expect("Y");
    assert_eq!(view.consumers.len(), 2);
    let future = view.future.expect("a future");
    assert_eq!(
        (future.next_use, future.hot),
        (0, true),
        "Y is queue 1's head"
    );
    assert_eq!(view.reload_cost, b);
    assert_eq!(engine.eviction_order(), "known_future");

    engine.set_water(0, TierKind::Host, 1 << 30, 1 << 30);
    drain(&engine, &reactor, 0, &[0, 1, 2, 3, 4, 5]);
    drain(&engine, &reactor, 1, &[y]);
    let done = engine.stats().catalog;
    assert_eq!((done.buffers, done.freed), (0, 6));
    assert!(done.misses >= 4 && done.miss_reload_bytes == done.misses * b);
}

#[test]
fn pl_t34_held_batches_stay() {
    let scratch = common::Scratch::new("t34");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let engine = common::engine(config(scratch.path(), 1 << 30), &alloc, &reactor);
    staging(&engine);
    let b = batch(&alloc, 0).bytes;
    // Queue 1 holds Z then Y; queue 0 holds Y alone, and its consumer takes it.
    engine.push(1, batch(&alloc, 0)).expect("Z");
    engine.push_to(&[0, 1], batch(&alloc, 1)).expect("Y");
    let y = engine
        .pop(0, common::want_host(), Locality::Any)
        .expect("pop")
        .expect("Y");
    let id = engine.buffer_of(1, 1).expect("Y");
    assert_eq!(engine.stats().catalog.held_bytes, b);
    // Queue 1 must shed a batch: Z is its head and Y is held, so nothing can leave.
    engine.set_water(1, TierKind::Host, b, b);
    assert!(matches!(state(&engine, 1, 1), State::Resident(_)), "held");
    assert!(engine.is_full(1), "nothing could leave");
    let refused = engine.spill(id).expect_err("refused").to_string();
    assert!(refused.contains("held by a running unit"), "{refused}");
    // Released, Y may leave.
    engine.release(0, y.seq).expect("release");
    drop(y);
    engine.set_water(1, TierKind::Host, b, b);
    assert!(matches!(state(&engine, 1, 1), State::OnDisk(_)), "Y left");
    assert!(!engine.is_full(1));
    let refused = engine.reload(id);
    assert!(refused.is_ok(), "a batch on disk is read back on request");
    drain(&engine, &reactor, 1, &[0, 1]);
    assert!(
        engine.spill(id).is_err(),
        "a freed batch is not in the catalog"
    );
    assert!(engine.release(1, 1).is_err(), "released once");
    assert!(
        engine.release(7, 1).is_err(),
        "a queue the run does not have"
    );
    let done = engine.stats().catalog;
    assert_eq!((done.buffers, done.freed, done.held_bytes), (0, 2, 0));
}

#[derive(Default)]
struct Heard(Mutex<Vec<CatalogEvent>>);

impl CatalogObserver for Heard {
    fn event(&self, event: &CatalogEvent) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.clone());
    }
}

#[test]
fn pl_t35_events_and_reservations() {
    let scratch = common::Scratch::new("t35");
    let alloc = FakeAllocator::new();
    let b = batch(&alloc, 0).bytes;
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(5));
    let engine = common::engine(config(scratch.path(), 4 * b), &alloc, &reactor);
    staging(&engine);
    let heard = Arc::new(Heard::default());
    engine.observe(heard.clone());

    // A claim never passes the budget; one larger than the budget is refused for ever.
    let first = match engine.reserve(Tier::Host, 3 * b).expect("reserve") {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    assert_eq!((first.bytes(), first.slot()), (3 * b, HOST));
    match engine.reserve(Tier::Host, 2 * b).expect("reserve") {
        Reserve::Refused { need, budget } => assert_eq!((need, budget), (b, 4 * b)),
        other => panic!("refused, got {other:?}"),
    }
    drop(first);
    assert_eq!(
        engine.stats().catalog.reserved_by_tier[HOST],
        0,
        "dropped, given back"
    );
    assert!(matches!(
        engine.reserve(Tier::Host, 5 * b).expect("reserve"),
        Reserve::Refused { .. }
    ));
    assert!(
        engine
            .reserve(
                Tier::Disk(moruna_kernel::SegmentRef {
                    segment: 0,
                    offset: 0,
                    len: 0,
                }),
                b
            )
            .is_err()
    );
    assert!(
        engine.reserve(Tier::PinnedHost, b).is_err(),
        "not this run's host tier"
    );

    // A claim becomes the batch's residency.
    let claim = match engine.reserve(Tier::Host, 2 * b).expect("reserve") {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    engine
        .push_reserved(&[0], batch(&alloc, 0), claim)
        .expect("push");
    let catalog = engine.stats().catalog;
    assert_eq!(
        (
            catalog.resident_by_tier[HOST],
            catalog.reserved_by_tier[HOST]
        ),
        (b, 0)
    );
    // A batch larger than its claim is an overrun, counted.
    let small = match engine.reserve(Tier::Host, b / 2).expect("reserve") {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    engine
        .push_reserved(&[0], batch(&alloc, 1), small)
        .expect("push");
    assert_eq!(engine.stats().catalog.overrun_bytes, b - b / 2);
    // Memory holds 2b; a claim of 3b needs b freed, which a spill of batch 1 gives when it
    // lands: pending until then.
    match engine.reserve(Tier::Host, 3 * b).expect("reserve") {
        Reserve::Pending { need } => assert_eq!(need, b),
        other => panic!("pending, got {other:?}"),
    }
    common::settle(&reactor);
    let claim = match engine.reserve(Tier::Host, 3 * b).expect("reserve") {
        Reserve::Granted(claim) => claim,
        other => panic!("granted once the spill landed, got {other:?}"),
    };
    let catalog = engine.stats().catalog;
    assert!(
        catalog.resident_by_tier[HOST] + catalog.reserved_by_tier[HOST] <= 4 * b,
        "never past the budget"
    );
    // A claim whose push is refused is given back.
    engine.close(1);
    assert!(
        engine
            .push_reserved(&[0, 1], batch(&alloc, 2), claim)
            .is_err()
    );
    assert_eq!(engine.stats().catalog.reserved_by_tier[HOST], 0);
    let claim = match engine.reserve(Tier::Host, b).expect("reserve") {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    assert!(engine.push_reserved(&[9], batch(&alloc, 2), claim).is_err());
    assert_eq!(engine.stats().catalog.reserved_by_tier[HOST], 0);

    drain(&engine, &reactor, 0, &[0, 1]);
    let id0 = 1;
    let events = heard.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            CatalogEvent::Released { .. } => "released",
            CatalogEvent::Freed { .. } => "freed",
            CatalogEvent::Spilled { .. } => "spilled",
            CatalogEvent::Reloaded { .. } => "reloaded",
            CatalogEvent::Dropped { .. } => "dropped",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "spilled", "released", "freed", "reloaded", "released", "freed"
        ],
        "batch 1 spilled; batch 0 taken and freed; batch 1 read back, taken and freed"
    );
    assert!(matches!(
        events[2],
        CatalogEvent::Freed { batch, bytes } if batch.0 == id0 && bytes == b
    ));
    assert_eq!(moruna_placement::locks::violations(), 0, "lock order (4.2)");
}

#[test]
fn pl_t36_evicted_once_and_resumed_once() {
    // A source's batch, dropped under pressure with staging off, is listed once and one
    // `replace` brings it back for both queues.
    let scratch = common::Scratch::new("t36a");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let engine = common::engine(config(scratch.path(), 1 << 30), &alloc, &reactor);
    for q in [0, 1] {
        engine.set_promotion_window(q, 1);
    }
    let b = batch(&alloc, 0).bytes;
    for seq in 0..3 {
        engine
            .push_to(&[0, 1], batch(&alloc, seq))
            .expect("fans out");
    }
    engine.set_water(0, TierKind::Host, 2 * b, 2 * b);
    assert!(
        matches!(state(&engine, 0, 2), State::Evicted),
        "the furthest left"
    );
    assert!(
        matches!(state(&engine, 1, 2), State::Evicted),
        "for both queues"
    );
    let listed: Vec<u64> = engine.evicted(0).iter().map(|(seq, _)| *seq).collect();
    assert_eq!(listed, vec![2], "listed under the lowest queue");
    assert!(engine.evicted(1).is_empty(), "and only there");
    assert_eq!(engine.detailed_stats().per_queue[0].evictions, 1);
    // The pressure is lifted first, or the same pressure would drop it again at once.
    engine.set_water(0, TierKind::Host, 1 << 30, 1 << 30);
    engine.replace(0, batch(&alloc, 2)).expect("replace");
    assert!(
        matches!(state(&engine, 1, 2), State::Resident(_)),
        "back for queue 1 too"
    );
    assert!(
        engine.replace(1, batch(&alloc, 2)).is_err(),
        "nothing left to replace"
    );
    engine.set_water(0, TierKind::Host, 1 << 30, 1 << 30);
    drain(&engine, &reactor, 0, &[0, 1, 2]);
    drain(&engine, &reactor, 1, &[0, 1, 2]);
    assert_eq!(engine.stats().catalog.freed, 3);

    // A fan-out spilled once comes back from a manifest as one batch with both consumers.
    let scratch = common::Scratch::new("t36b");
    let reactor = FakeReactor::new();
    let cfg = config(scratch.path(), 1 << 30);
    let engine = common::engine(cfg.clone(), &alloc, &reactor);
    staging(&engine);
    for seq in 0..6 {
        engine
            .push_to(&[0, 1], batch(&alloc, seq))
            .expect("fans out");
    }
    engine.set_water(0, TierKind::Host, b, b);
    common::settle(&reactor);
    let on_disk: Vec<u64> = engine
        .entry_states(0)
        .into_iter()
        .filter(|(_, state)| matches!(state, State::OnDisk(_)))
        .map(|(seq, _)| seq)
        .collect();
    assert_eq!(on_disk, vec![1, 2, 3, 4, 5], "all but the head");
    let extras = CheckpointExtras {
        kernel_states: Vec::new(),
        sink_states: vec![None, None],
        committed: vec![None, None],
        source_cursors: vec![SourceCursor {
            split_index: 0,
            row_offset: 6,
            next_seq: 6,
        }],
        issued: vec![Vec::new()],
    };
    let path = engine.checkpoint(&extras).expect("a manifest");
    engine.shutdown();
    let fresh = common::engine(cfg.clone(), &alloc, &reactor);
    let point = fresh
        .restore(&path, &[common::plan()], &cfg.fingerprints)
        .expect("restores");
    let recompute: Vec<u64> = point.to_recompute[0].iter().map(|(s, _)| *s).collect();
    assert_eq!(recompute, vec![0], "the resident head is read again");
    assert_eq!(fresh.stats().catalog.buffers, 5, "five batches, not ten");
    for seq in 1..6 {
        let a = fresh.buffer_of(0, seq);
        assert!(a.is_some());
        assert_eq!(a, fresh.buffer_of(1, seq), "batch {seq} is one batch again");
    }
    staging(&fresh);
    drain(&fresh, &reactor, 0, &[1, 2, 3, 4, 5]);
    drain(&fresh, &reactor, 1, &[1, 2, 3, 4, 5]);
    let done = fresh.stats().catalog;
    assert_eq!((done.buffers, done.freed, done.released), (0, 5, 10));
    let detail = fresh.detailed_stats();
    assert_eq!(detail.segments_reclaimable, detail.segments_live);
    assert_eq!(moruna_placement::locks::violations(), 0, "lock order (4.2)");
}

/// PL-T35, the port's refusals: every action a policy may ask for that would break a rule of
/// f.18 is refused with an error naming the batch and the rule, and changes nothing.
#[test]
fn pl_t35_the_port_refuses_by_name() {
    let scratch = common::Scratch::new("t35b");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let b = batch(&alloc, 0).bytes;
    let mut cfg = config(scratch.path(), 2 * b);
    cfg.budgets.device[0] = 4 * b;
    let engine = common::engine(cfg, &alloc, &reactor);
    staging(&engine);
    // Queue 0 may hold the whole budget: only what this test asks for moves.
    engine.set_water(0, TierKind::Host, 1 << 30, 1 << 30);
    let unknown = moruna_placement::catalog::BufferId(4242);
    let message = |r: moruna_kernel::Result<()>| r.expect_err("refused").to_string();
    assert!(message(engine.spill(unknown)).contains("no batch 4242"));
    assert!(message(engine.reload(unknown)).contains("no batch 4242"));

    // A claim in one tier does not admit a batch in another.
    let device = match engine
        .reserve(Tier::Device(moruna_kernel::DeviceId(0)), b)
        .expect("reserve")
    {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    let refused = message(engine.push_reserved(&[0], batch(&alloc, 0), device));
    assert!(
        refused.contains("its reservation is in tier slot 0"),
        "{refused}"
    );
    let device = match engine
        .reserve(Tier::Device(moruna_kernel::DeviceId(0)), b)
        .expect("reserve")
    {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    let refused = message(engine.replace_reserved(0, batch(&alloc, 0), device));
    assert!(
        refused.contains("its reservation is in tier slot 0"),
        "{refused}"
    );
    let host = match engine.reserve(Tier::Host, b).expect("reserve") {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    assert!(message(engine.replace_reserved(9, batch(&alloc, 0), host)).contains("outside"));
    let host = match engine.reserve(Tier::Host, b).expect("reserve") {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    assert!(
        message(engine.replace_reserved(0, batch(&alloc, 0), host)).contains("no evicted entry")
    );
    let catalog = engine.stats().catalog;
    assert_eq!(
        catalog.reserved_by_tier,
        [0; moruna_kernel::TIER_COUNT],
        "all given back"
    );

    // Only a host-resident batch spills, and only a batch on disk is read back, into room.
    engine.push(0, batch(&alloc, 0)).expect("push");
    engine.push(0, batch(&alloc, 1)).expect("push");
    let second = engine.buffer_of(0, 1).expect("batch 1");
    engine.spill(second).expect("spill");
    assert!(message(engine.spill(second)).contains("not resident in the host tier alone"));
    let blocker = match engine.reserve(Tier::Host, b).expect("reserve") {
        Reserve::Granted(claim) => claim,
        other => panic!("granted, got {other:?}"),
    };
    assert!(
        message(engine.reload(second)).contains("are not free"),
        "memory is full"
    );
    drop(blocker);
    engine.reload(second).expect("room now");
    assert!(
        engine.shed(TierKind::Host, b),
        "batch 1 leaves again, by its disk copy"
    );
    assert!(matches!(state(&engine, 0, 1), State::OnDisk(_)));
    assert!(
        !engine.shed(TierKind::Host, b),
        "the head alone is left, and it is hot"
    );

    // A run with no disk tier spills nothing.
    let alloc = FakeAllocator::new();
    let mut cfg = common::config(1, None, common::budgets(1 << 20, 0, 0));
    cfg.topology = fan_out();
    cfg.fingerprints = vec![
        moruna_kernel::Fingerprint::compute("a", b"1"),
        moruna_kernel::Fingerprint::compute("b", b"1"),
    ];
    let bare = common::engine(cfg, &alloc, &FakeReactor::new());
    bare.push(0, batch(&alloc, 0)).expect("push");
    let id = bare.buffer_of(0, 0).expect("batch 0");
    assert!(message(bare.spill(id)).contains("no disk tier"));
}

/// PL-T38 what_memory_cannot_admit (KD7b, f.18 rule 12): a batch pushed straight to disk counts
/// in no tier while its record is written, lands `OnDisk`, and its consumer reads it back under a
/// claim with its own bytes; `head` names a queue's batch and whether the take is its last; a run
/// with no disk tier refuses the push by name.
#[test]
fn pl_t38_what_memory_cannot_admit() {
    let scratch = common::Scratch::new("t38");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(20));
    let engine = common::engine(config(scratch.path(), 1 << 30), &alloc, &reactor);
    staging(&engine);
    let b = batch(&alloc, 0).bytes;
    engine
        .push_spilled(&[0, 1], batch(&alloc, 0))
        .expect("straight to disk");
    let catalog = engine.stats().catalog;
    assert_eq!(catalog.resident_by_tier[HOST], 0, "admitted to no tier");
    assert_eq!(
        (
            catalog.outside_bytes,
            catalog.overflows,
            catalog.overflow_bytes
        ),
        (b, 1, b)
    );
    let head = engine.head(0).expect("a head");
    assert_eq!(
        (head.seq, head.bytes, head.last),
        (0, b, false),
        "queue 1 waits for it too"
    );
    assert!(engine.head(7).is_none(), "a queue the run does not have");
    // The record lands and, as queue 0's head, the batch is read back under a claim with its
    // disk copy kept.
    common::until("written and read back", || {
        let catalog = engine.stats().catalog;
        catalog.spills == 1 && catalog.reloads == 1
    });
    common::settle(&reactor);
    assert_eq!(engine.stats().catalog.outside_bytes, 0);
    assert!(matches!(state(&engine, 0, 0), State::ResidentOnDisk(_)));
    drain(&engine, &reactor, 0, &[0]);
    assert_eq!(
        engine.head(1).map(|h| h.last),
        Some(true),
        "queue 1's take is the last"
    );
    drain(&engine, &reactor, 1, &[0]);
    let done = engine.stats().catalog;
    assert_eq!((done.buffers, done.freed, done.outside_bytes), (0, 1, 0));
    assert!(
        engine.push_spilled(&[9], batch(&alloc, 1)).is_err(),
        "a queue outside the run"
    );

    let alloc = FakeAllocator::new();
    let mut cfg = common::config(1, None, common::budgets(1 << 20, 0, 0));
    cfg.topology = fan_out();
    cfg.fingerprints = vec![
        moruna_kernel::Fingerprint::compute("a", b"1"),
        moruna_kernel::Fingerprint::compute("b", b"1"),
    ];
    let bare = common::engine(cfg, &alloc, &FakeReactor::new());
    let refused = bare
        .push_spilled(&[0], batch(&alloc, 0))
        .expect_err("no disk")
        .to_string();
    assert!(refused.contains("no disk tier"), "{refused}");
}

/// PL-T39 a_head_on_disk_comes_back (f.18 rule 13): memory is full of batches inside queue 1's
/// window, every one hot, and queue 0's head is on disk. Its reload takes the room of the one
/// whose next use is furthest, by the known-future order, and never passes the budget.
#[test]
fn pl_t39_a_head_on_disk_comes_back() {
    let scratch = common::Scratch::new("t39");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let b = batch(&alloc, 0).bytes;
    let engine = common::engine(config(scratch.path(), 3 * b), &alloc, &reactor);
    staging(&engine);
    for q in [0, 1] {
        engine.set_water(q, TierKind::Host, 1 << 30, 1 << 30);
    }
    engine.set_promotion_window(1, 3);
    for seq in 0..3 {
        engine.push(1, batch(&alloc, seq)).expect("push");
    }
    assert_eq!(
        engine.stats().catalog.resident_by_tier[HOST],
        3 * b,
        "full, all hot"
    );
    // Queue 0's batch does not fit, so it goes straight to disk and lands there; as its queue's
    // head it must come back.
    engine
        .push_spilled(&[0], batch(&alloc, 9))
        .expect("to disk");
    common::settle(&reactor);
    assert!(
        matches!(
            state(&engine, 0, 9),
            State::Resident(_) | State::ResidentOnDisk(_)
        ),
        "queue 0's head came back: {:?}",
        state(&engine, 0, 9)
    );
    assert!(
        matches!(state(&engine, 1, 2), State::OnDisk(_)),
        "by taking the room of the batch used last"
    );
    for seq in [0, 1] {
        assert!(
            matches!(state(&engine, 1, seq), State::Resident(_)),
            "{seq} stays"
        );
    }
    let catalog = engine.stats().catalog;
    assert!(catalog.peak_by_tier[HOST] <= 3 * b, "never past the budget");
    assert_eq!(catalog.misses, 1, "one miss, its reload");
    drain(&engine, &reactor, 0, &[9]);
    drain(&engine, &reactor, 1, &[0, 1, 2]);
    assert_eq!(engine.stats().catalog.freed, 4);
}
