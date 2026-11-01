//! SC-T16: a run killed mid-flight and resumed from its manifest writes what an uninterrupted
//! run writes, re-reads only what it must, restores `Checkpoint` instances and re-inits
//! `Reinit` ones, and refuses to resume a sink that cannot say what it committed. f.13, S17.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use moruna_kernel::{CancelToken, Fingerprint, MorunaError, Placement, ResumePoint, Sink, Source};
use moruna_testkit::{FakePlacement, FakeSink, FakeSource};

use super::common::{HeldSource, Release, RigBuilder, StatefulKernel, manifest_lock, wait_for};

const SPLITS: u32 = 4;
const ROWS: u64 = 60;
/// The first split whose reads the killed run holds pending.
const HELD_FROM: u32 = 2;
/// The sink commits every this many sequence numbers.
const COMMIT_EVERY: u64 = 10;

fn source() -> FakeSource {
    FakeSource::new().splits(SPLITS, ROWS, ROWS * 8)
}

/// The reads of the splits before the held ones: one per morsel, and the lowest sequence
/// numbers, since the splits are read in plan order. Final once a held read is pending.
fn unheld_reads(source: &FakeSource) -> u64 {
    source
        .reads()
        .iter()
        .filter(|(split, _)| *split < HELD_FROM)
        .count() as u64
}

/// True once a held read is pending and the watermark covers every whole block of
/// `commit_every` the unheld morsels make: everything the held run can commit, it has.
fn committed_all_it_can(source: &FakeSource, pending: &AtomicUsize, sink: &FakeSink) -> bool {
    if pending.load(Ordering::SeqCst) == 0 {
        return false;
    }
    let blocks = unheld_reads(source) / COMMIT_EVERY;
    blocks > 0 && Sink::committed_seq(sink) == Some(blocks * COMMIT_EVERY - 1)
}

#[test]
fn sc_t16_resume_equivalence() {
    let _guard = manifest_lock();

    // The run to match: uninterrupted, same pipeline.
    let reference = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
        })
        .source(source())
        .sink(FakeSink::new().resumable(true).commit_every(COMMIT_EVERY))
        .stages(3)
        .go();
    match reference.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let mut want = reference.sink.written();
    want.sort_unstable();

    // The run that is killed. `FakePlacement`'s manifest store is process wide and keyed on one
    // path, so the resumed engine finds this run's manifest.
    let placement = FakePlacement::new().with_manifest_store();
    let sink = FakeSink::new().resumable(true).commit_every(COMMIT_EVERY);
    // The kill lands in the middle of the run because the run cannot finish, not because the
    // host was slow enough: a kernel latency only made a completed first run unlikely, and this
    // test asserted `Cancelled` about it. The reads of the last two splits stay pending until
    // the test lets them go, so the run is held with reads in flight, which the manifest names
    // and the resume reads again (MH 4.7). The first two splits are read in full first, and
    // they are the lowest sequence numbers, so every one of them reaches the sink and the
    // watermark moves whatever order the workers take them in. A latch at the last stage that
    // let through the first calls to arrive did not promise that: a worker the host descheduled
    // holding a low sequence let the other fill the latch's quota with later ones, the low one
    // was then held, the sink never committed, and the test waited on that commit for ever.
    let reads = source();
    let held = HeldSource::new(reads.clone(), HELD_FROM);
    let release = held.release.clone();
    let pending = held.pending.clone();
    let checkpointing = Arc::new(StatefulKernel::new(2).checkpointing());
    let reinit = Arc::new(StatefulKernel::new(2));
    let first = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 10;
        })
        .source_over(Arc::new(held))
        .sink(sink.clone())
        .placement(placement.clone())
        .kernel(checkpointing.clone())
        .kernel(reinit.clone())
        .kernel(Arc::new(StatefulKernel::new(0)))
        .go();
    if let Err(e) = first.scheduler.init_instances() {
        panic!("init_instances: {e}");
    }
    let cancel = CancelToken::new();
    let token = cancel.clone();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| first.scheduler.run(token));
        let _release = Release(|| {
            cancel.cancel();
            release.store(true, Ordering::SeqCst);
        });
        assert!(
            wait_for(Duration::from_secs(30), || {
                committed_all_it_can(&reads, &pending, &first.sink)
                    && !first.fake_placement.manifests_written().is_empty()
            }),
            "the first run never committed the splits before the held ones"
        );
        // Cancelled, and released only once the engine is shut down: the drive lets its reads
        // resolve before it ends (f.10), a read that resolves then is refused by the engine, so
        // it is never pushed and stays issued in the final manifest, which restore hands back.
        // Released any earlier, a read could be pushed and carried to the sink before the
        // shutdown, and the kill would leave nothing to recompute.
        cancel.cancel();
        assert!(
            wait_for(Duration::from_secs(30), || first
                .fake_placement
                .shutdown_calls()
                > 0),
            "the cancelled run never shut its engine down"
        );
        release.store(true, Ordering::SeqCst);
        match handle.join() {
            Ok(Ok(crate::RunOutcome::Cancelled { .. })) => {}
            other => panic!("the first run was meant to be cancelled mid-flight, got {other:?}"),
        }
    });
    let manifest = match first.fake_placement.manifests_written().last().cloned() {
        Some(path) => path,
        None => panic!("the first run wrote no manifest"),
    };
    drop(first);

    // The resumed run, over a restored engine.
    let resumed_placement = FakePlacement::new().with_manifest_store();
    let plan = match Source::plan(&source()) {
        Ok(plan) => plan,
        Err(e) => panic!("plan: {e}"),
    };
    let point: ResumePoint =
        match resumed_placement.restore(&manifest, &[plan], &[] as &[Fingerprint]) {
            Ok(point) => point,
            Err(e) => panic!("restore: {e}"),
        };
    let committed = point.extras.committed[0];
    let point_next_seq = point.extras.source_cursors[0].next_seq;
    let issued_at_kill: Vec<u64> = point.extras.issued[0].iter().map(|(seq, _)| *seq).collect();
    for seq in &issued_at_kill {
        assert!(
            *seq < point_next_seq,
            "an issued read is behind the cursor, which is why the cursor cannot name it"
        );
        assert!(
            committed.is_none_or(|watermark| *seq > watermark)
                == point.to_recompute[0].iter().any(|(s, _)| s == seq),
            "restore hands back every issued read above the watermark, and only those (MH 4.7)"
        );
    }
    let to_recompute = point.to_recompute[0].len();

    assert!(
        to_recompute > 0,
        "the kill left nothing to recompute, so the resume path was not exercised"
    );
    let second_checkpointing = Arc::new(StatefulKernel::new(2).checkpointing());
    let second_reinit = Arc::new(StatefulKernel::new(2));
    let second = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 50;
            cfg.resuming = true;
        })
        .source(source())
        .sink(sink.clone())
        .placement(resumed_placement)
        .kernel(second_checkpointing.clone())
        .kernel(second_reinit.clone())
        .kernel(Arc::new(StatefulKernel::new(0)))
        .go();
    if let Err(e) = second.scheduler.apply_resume_point(point) {
        panic!("apply_resume_point: {e}");
    }
    assert_eq!(
        second.sink.resume_calls(),
        1,
        "the sink was resumed, not opened"
    );
    assert_eq!(
        second.sink.open_calls(),
        1,
        "the sink was opened once, by the first run, and resumed once, by this one"
    );
    assert_eq!(
        second_checkpointing.restore_calls.load(Ordering::SeqCst),
        2,
        "a Checkpoint kernel's instances come back through restore"
    );
    assert_eq!(
        second_reinit.init_calls.load(Ordering::SeqCst),
        2,
        "a Reinit kernel's instances are built again"
    );

    match second.scheduler.run_resumed(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let mut got = second.sink.written();
    got.sort_unstable();
    let unique: std::collections::BTreeSet<u64> = got.iter().copied().collect();
    assert_eq!(
        unique.len(),
        got.len(),
        "no sequence number was written twice"
    );

    let have: std::collections::BTreeSet<u64> = unique;
    let expected: std::collections::BTreeSet<u64> = want.iter().copied().collect();
    let missing: Vec<u64> = expected.difference(&have).copied().collect();
    let watermark = committed.unwrap_or(0);
    // Everything at or below the watermark survived the kill, and everything the manifest named
    // or the cursor pointed at was produced again.
    for seq in 0..=watermark {
        assert!(have.contains(&seq), "committed sequence {seq} is missing");
    }
    // `FakePlacement`'s manifest records the entries its queues hold (d.15); it keeps no
    // lineage index, so a morsel that had already left the last queue and was written but not
    // committed is in neither the manifest nor the resumed sink. The real engine keeps that
    // lineage until `set_committed` (09 f.11, PL-I11), which is why section k states the
    // equality as it does. What this test can prove is that nothing else is missing.
    for seq in &missing {
        assert!(
            *seq > watermark,
            "sequence {seq} is at or below the watermark and was lost"
        );
    }
    // The loss window, exactly. A morsel the resumed run cannot produce is one that, at the
    // moment of the last manifest, was in no queue (so the manifest's lineage does not name it)
    // and had already been issued (so the cursor, which is the next range to issue, does not
    // point at it). Its sequence number therefore lies strictly between the watermark and the
    // cursor's `next_seq`. One thing still puts a morsel there under this fake: one written but
    // not yet committed, which the real engine keeps in its lineage until `set_committed` and
    // `FakePlacement` does not (09 f.11, PL-I11). A read that was still in flight used to be the
    // other, and is no longer: the manifest records it as issued and restore hands it back
    // (MH 4.7, 10 l), which the loop below proves by name.
    let issued = point_next_seq;
    for seq in &missing {
        assert!(
            *seq > watermark && *seq < issued,
            "sequence {seq} is missing from outside the loss window ({watermark}, {issued})"
        );
        assert!(
            !issued_at_kill.contains(seq),
            "sequence {seq} was an issued read at the kill and was not read again"
        );
    }

    // Nothing at or below the watermark was read again, and every recomputed origin was.
    let reads = second.source.reads();
    assert!(
        reads.len() >= to_recompute,
        "every recomputed origin was re-read"
    );
    assert!(
        reads.len() < SPLITS as usize * ROWS as usize,
        "the resumed run re-read the whole input"
    );
    assert!(
        watermark < SPLITS as u64 * ROWS,
        "the watermark is inside the run"
    );
}

/// The interleaving that left SC-T16's first run without a commit, made certain: the worker
/// with sequence 0 is kept in stage 1 until the other has carried every other morsel of the
/// splits before the held ones to the sink. Sequence 0 still gets there, so the watermark moves
/// and covers all of them, and the run is still held by its pending reads. The latch this
/// replaced held the first calls past its quota whatever their sequence, so it held sequence 0
/// here every time and the sink never committed.
#[test]
fn sc_t16_the_lowest_sequence_reaching_the_sink_last_still_commits() {
    let reads = source();
    let held = HeldSource::new(reads.clone(), HELD_FROM);
    let release = held.release.clone();
    let pending = held.pending.clone();
    let sink = FakeSink::new().resumable(true).commit_every(COMMIT_EVERY);
    let (watched, counted, waiting) = (sink.clone(), reads.clone(), pending.clone());
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
        })
        .source_over(Arc::new(held))
        .sink(sink)
        .kernel(Arc::new(StatefulKernel::new(0).stalling(
            0,
            Arc::new(move || {
                waiting.load(Ordering::SeqCst) > 0
                    && watched.written().len() as u64 + 1 >= unheld_reads(&counted)
            }),
        )))
        .kernel(Arc::new(StatefulKernel::new(0)))
        .kernel(Arc::new(StatefulKernel::new(0)))
        .go();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        let _release = Release(|| {
            cancel.cancel();
            release.store(true, Ordering::SeqCst);
        });
        assert!(
            wait_for(Duration::from_secs(30), || committed_all_it_can(
                &reads, &pending, &rig.sink
            )),
            "the sink never committed the splits before the held ones: {:?}",
            Sink::committed_seq(&rig.sink)
        );
        assert!(
            rig.sink.written().contains(&0),
            "sequence 0, kept back behind every other, reached the sink"
        );
        cancel.cancel();
        release.store(true, Ordering::SeqCst);
        match handle.join() {
            Ok(Ok(crate::RunOutcome::Cancelled { .. })) => {}
            other => panic!("the run was meant to be cancelled while held, got {other:?}"),
        }
    });
}

#[test]
fn sc_t16_resume_refuses_a_sink_that_cannot_commit() {
    let _guard = manifest_lock();
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.checkpoint_enabled = true;
            cfg.resuming = true;
        })
        .sink(FakeSink::new().resumable(false))
        .placement(FakePlacement::new().with_manifest_store())
        .stages(1)
        .go();
    assert!(
        !rig.scheduler.checkpoint_enabled(),
        "a sink that cannot say what it committed forces checkpointing off at new"
    );
    let outcome = rig.scheduler.apply_resume_point(ResumePoint::default());
    match outcome {
        Err(MorunaError::Resume(msg)) => {
            assert!(msg.contains("sink"), "the refusal names the sink: {msg}")
        }
        other => panic!("expected Resume naming the sink, got {other:?}"),
    }
    assert_eq!(
        rig.sink.resume_calls(),
        0,
        "it refused before touching the sink"
    );
}
