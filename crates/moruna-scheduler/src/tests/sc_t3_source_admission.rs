//! SC-T3: the source drive issues a read only while Q0 is below high water and read-ahead has
//! room. SC-I3.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use moruna_kernel::{CancelToken, Knob, Knobs, Morsel, Origin, Payload, TierKind};
use moruna_testkit::{FakeAllocator, FakeKernel, FakeSource};

use super::common::{RigBuilder, SlowSource, wait_for};

/// One small table morsel in the allocator's host tier, so a test can fill Q0 by hand.
fn morsel(alloc: &FakeAllocator, seq: u64) -> Morsel {
    let buffer = alloc.arrow_buffer(&[0u8; 8], alloc.host_tier());
    let data = moruna_kernel::arrow::array::ArrayData::builder(
        moruna_kernel::arrow::datatypes::DataType::Int64,
    )
    .len(1)
    .add_buffer(buffer)
    .build();
    let array: moruna_kernel::arrow::array::ArrayRef = match data {
        Ok(data) => moruna_kernel::arrow::array::make_array(data),
        Err(e) => panic!("array: {e}"),
    };
    let schema = std::sync::Arc::new(moruna_kernel::arrow::datatypes::Schema::new(vec![
        moruna_kernel::arrow::datatypes::Field::new(
            "value",
            moruna_kernel::arrow::datatypes::DataType::Int64,
            false,
        ),
    ]));
    let batch = match moruna_kernel::arrow::record_batch::RecordBatch::try_new(schema, vec![array]) {
        Ok(batch) => batch,
        Err(e) => panic!("batch: {e}"),
    };
    let payload = match Payload::table(batch) {
        Ok(payload) => payload,
        Err(e) => panic!("payload: {e}"),
    };
    Morsel::new(
        seq,
        0,
        payload,
        Origin {
            split: 0,
            row_start: seq,
            row_end: seq + 1,
            node: moruna_kernel::LOCAL_NODE,
        },
    )
}

#[test]
fn sc_t3_source_admission_q0_full_stops_reads() {
    // One worker and a kernel slower than the window, so the morsels put into Q0 by hand stay
    // there: Q0 is above its high water mark for the whole test and the drive must issue
    // nothing at all.
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 4;
            cfg.initial_morsel_target = 8;
        })
        .source(FakeSource::new().splits(8, 8, 64))
        .kernel(Arc::new(
            FakeKernel::new().latency(Duration::from_millis(400)),
        ))
        .go();
    rig.scheduler.set(Knob::HighWater {
        stage: 0,
        tier: TierKind::Host,
        bytes: 1,
    });
    for seq in 0..3u64 {
        if let Err(e) =
            moruna_kernel::Placement::push(rig.placement.as_ref(), 0, morsel(&rig.alloc, seq))
        {
            panic!("push: {e}");
        }
    }
    assert!(
        moruna_kernel::Placement::is_full(rig.placement.as_ref(), 0),
        "Q0 is above its high water mark"
    );
    assert!(
        rig.source.reads().is_empty(),
        "no read is issued before run"
    );

    let cancel = CancelToken::new();
    let token = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        token.cancel();
    });
    let _ = rig.scheduler.run(cancel);
    assert!(
        rig.source.reads().is_empty(),
        "the drive read while Q0 was full: {} reads",
        rig.source.reads().len()
    );
}

#[test]
fn sc_t3_source_admission_read_ahead_respected() {
    let slow = SlowSource::new(FakeSource::new().splits(16, 8, 64), 40);
    let peak = Arc::clone(&slow.peak_in_flight);
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.read_ahead = 3)
        .source_over(Arc::new(slow))
        .go();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let watcher = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        token.cancel();
    });
    let _ = rig.scheduler.run(cancel);
    let _ = watcher.join();
    assert!(
        wait_for(Duration::from_millis(100), || peak.load(Ordering::SeqCst)
            > 0),
        "the drive issued no read at all"
    );
    assert!(
        peak.load(Ordering::SeqCst) <= 3,
        "read-ahead 3 was exceeded: {} reads in flight",
        peak.load(Ordering::SeqCst)
    );
}
