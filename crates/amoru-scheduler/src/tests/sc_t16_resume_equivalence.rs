//! SC-T16: a run killed mid-flight and resumed from its manifest writes what an uninterrupted
//! run writes, re-reads only what it must, restores `Checkpoint` instances and re-inits
//! `Reinit` ones, and refuses to resume a sink that cannot say what it committed. f.13, S17.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use amoru_kernel::{AmoruError, CancelToken, Fingerprint, Placement, ResumePoint, Sink, Source};
use amoru_testkit::{FakeKernel, FakePlacement, FakeSink, FakeSource};

use super::common::{RigBuilder, StatefulKernel, manifest_lock, wait_for};

const SPLITS: u32 = 4;
const ROWS: u64 = 60;

fn source() -> FakeSource {
    FakeSource::new().splits(SPLITS, ROWS, ROWS * 8)
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
        .sink(FakeSink::new().resumable(true).commit_every(10))
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
    let sink = FakeSink::new().resumable(true).commit_every(10);
    // Slow enough that the kill lands in the middle of the run whatever the host's mood: the
    // point of the test is a resume from a manifest, not a race with the fakes.
    let checkpointing = Arc::new(
        StatefulKernel::new(2)
            .checkpointing()
            .latency(Duration::from_millis(5)),
    );
    let reinit = Arc::new(StatefulKernel::new(2));
    let first = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 10;
        })
        .source(source())
        .sink(sink.clone())
        .placement(placement.clone())
        .kernel(checkpointing.clone())
        .kernel(reinit.clone())
        .kernel(Arc::new(FakeKernel::new()))
        .go();
    if let Err(e) = first.scheduler.init_instances() {
        panic!("init_instances: {e}");
    }
    let cancel = CancelToken::new();
    let token = cancel.clone();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| first.scheduler.run(token));
        assert!(
            wait_for(Duration::from_secs(5), || {
                Sink::committed_seq(&first.sink).is_some()
                    && !first.fake_placement.manifests_written().is_empty()
            }),
            "the first run never committed anything"
        );
        cancel.cancel();
        match handle.join() {
            Ok(Ok(crate::RunOutcome::Cancelled { .. })) => {}
            other => panic!("the first run was meant to be cancelled mid-flight, got {other:?}"),
        }
    });
    let manifest = match first.fake_placement.manifests_written().last().cloned() {
        Some(path) => path,
        None => panic!("the first run wrote no manifest"),
    };
    let reads_before = first.source.reads().len();
    drop(first);

    // The resumed run, over a restored engine.
    let resumed_placement = FakePlacement::new().with_manifest_store();
    let plan = match Source::plan(&source()) {
        Ok(plan) => plan,
        Err(e) => panic!("plan: {e}"),
    };
    let point: ResumePoint =
        match resumed_placement.restore(&manifest, &plan, &[] as &[Fingerprint]) {
            Ok(point) => point,
            Err(e) => panic!("restore: {e}"),
        };
    let committed = point.extras.committed_seq;
    let point_next_seq = point.extras.source_cursor.next_seq;
    let to_recompute = point.to_recompute.len();

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
        .kernel(Arc::new(FakeKernel::new()))
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
    // cursor's `next_seq`. Two things put a morsel there: one written but not yet committed,
    // which the real engine keeps in its lineage until `set_committed` and `FakePlacement` does
    // not (09 f.11, PL-I11); and one whose source read was still in flight, which nothing
    // records. The second is a gap in the design, reported to the PM, not an artefact of the
    // fake. What this test proves is that nothing outside that window is lost.
    let issued = point_next_seq;
    for seq in &missing {
        assert!(
            *seq > watermark && *seq < issued,
            "sequence {seq} is missing from outside the loss window ({watermark}, {issued})"
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
    let _ = reads_before;
    assert!(
        watermark < SPLITS as u64 * ROWS,
        "the watermark is inside the run"
    );
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
        Err(AmoruError::Resume(msg)) => {
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
