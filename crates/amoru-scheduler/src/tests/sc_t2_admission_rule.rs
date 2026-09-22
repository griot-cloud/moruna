//! SC-T2: among the admissible stages a worker takes the one whose output queue holds the
//! fewest bytes, ties to the later stage, and the rule is evaluated at every pick. SC-I2.

use std::sync::Arc;
use std::time::Duration;

use amoru_kernel::{Knobs, Morsel, Origin, Payload};
use amoru_testkit::FakeAllocator;

use super::common::RigBuilder;

fn morsel(alloc: &FakeAllocator, seq: u64, stage: u16, bytes: usize) -> Morsel {
    let buffer = alloc.arrow_buffer(&vec![0u8; bytes], alloc.host_tier());
    let data = amoru_kernel::arrow::array::ArrayData::builder(
        amoru_kernel::arrow::datatypes::DataType::UInt8,
    )
    .len(bytes)
    .add_buffer(buffer)
    .build();
    let array: amoru_kernel::arrow::array::ArrayRef = match data {
        Ok(data) => amoru_kernel::arrow::array::make_array(data),
        Err(e) => panic!("array: {e}"),
    };
    let schema = Arc::new(amoru_kernel::arrow::datatypes::Schema::new(vec![
        amoru_kernel::arrow::datatypes::Field::new(
            "v",
            amoru_kernel::arrow::datatypes::DataType::UInt8,
            false,
        ),
    ]));
    let batch = match amoru_kernel::arrow::record_batch::RecordBatch::try_new(schema, vec![array]) {
        Ok(batch) => batch,
        Err(e) => panic!("batch: {e}"),
    };
    let payload = match Payload::table(batch) {
        Ok(payload) => payload,
        Err(e) => panic!("payload: {e}"),
    };
    Morsel::new(
        seq,
        stage,
        payload,
        Origin {
            split: 0,
            row_start: 0,
            row_end: bytes as u64,
            node: amoru_kernel::LOCAL_NODE,
        },
    )
}

#[test]
fn sc_t2_admission_rule() {
    let rig = RigBuilder::new().stages(3).go();
    let shared = rig.scheduler.shared();
    let alloc = rig.alloc.clone();
    let place = |stage: u16, seq: u64, bytes: usize| {
        if let Err(e) = amoru_kernel::Placement::push(
            rig.placement.as_ref(),
            stage,
            morsel(&alloc, seq, stage, bytes),
        ) {
            panic!("push: {e}");
        }
    };

    // Every stage has a resident head; the last stage's output queue is empty, so it wins.
    place(0, 0, 64);
    place(1, 1, 64);
    place(2, 2, 64);
    std::thread::sleep(Duration::from_millis(2));
    for _ in 0..1_000 {
        assert_eq!(
            crate::pick::pick(shared),
            Some(3),
            "the emptiest output queue is stage 3's"
        );
    }

    // Take stage 3 out of the running with its own high water mark, so stages 1 and 2 hold
    // equal output bytes and the tie must break toward the later one.
    rig.scheduler.set(amoru_kernel::Knob::HighWater {
        stage: 3,
        tier: amoru_kernel::TierKind::Host,
        bytes: 1,
    });
    place(3, 5, 64);
    std::thread::sleep(Duration::from_millis(2));
    assert!(
        rig.placement.is_full(3),
        "stage 3 is above its high water mark"
    );
    for _ in 0..1_000 {
        assert_eq!(
            crate::pick::pick(shared),
            Some(2),
            "ties break to the later stage"
        );
    }

    // Make stage 2's output heavier than stage 1's: the rule now picks stage 1.
    place(2, 6, 512);
    std::thread::sleep(Duration::from_millis(2));
    for _ in 0..1_000 {
        assert_eq!(
            crate::pick::pick(shared),
            Some(1),
            "the fewest output bytes wins"
        );
    }
}
