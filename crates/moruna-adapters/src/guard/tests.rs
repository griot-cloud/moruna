//! AD-T13 gate_decides_on_a_fresh_measurement, and the frame and counting rules the hooks rely
//! on (f.9 to f.12, AD-I8, AD-I9, AD-I11). No interpreter: the "allocator underneath" is a
//! closure returning a pointer.

use std::sync::Arc;

use moruna_kernel::{Sample, Sampler};
use moruna_testkit::FakeSampler;

use super::*;

const MIB: u64 = 1024 * 1024;

fn sample(anon: u64, ceiling: u64) -> Sample {
    Sample {
        anon_bytes: anon,
        ceiling_bytes: ceiling,
        ..Sample::default()
    }
}

fn gate_over(samples: Vec<Sample>, ceiling: u64) -> (Arc<MemoryGate>, FakeSampler) {
    let sampler = FakeSampler::new().scripted(samples);
    let gate = MemoryGate::new(Arc::new(sampler.clone()) as Arc<dyn Sampler>, ceiling);
    (gate, sampler)
}

/// A pointer that is never dereferenced: what the allocator underneath "returned".
fn some() -> *mut u8 {
    core::ptr::NonNull::<u8>::dangling().as_ptr()
}

#[test]
fn ad_t13_gate_admits_within_the_estimate_without_measuring() {
    let (gate, sampler) = gate_over(vec![sample(100 * MIB, 0)], 200 * MIB);
    assert_eq!(sampler.samples_taken(), 1, "one measurement at creation");
    assert_eq!(gate.admit(50 * MIB), Ok(()));
    assert_eq!(gate.admit(50 * MIB), Ok(()));
    assert_eq!(sampler.samples_taken(), 1, "the fast path takes no sample");
    assert_eq!(gate.measurements(), 1);
    assert!(format!("{gate:?}").contains("since"));
}

#[test]
fn ad_t13_gate_measures_before_it_refuses() {
    // Created at 100 MiB; then the process has given memory back (60 MiB), then it has not.
    let (gate, sampler) = gate_over(
        vec![
            sample(100 * MIB, 0),
            sample(60 * MIB, 0),
            sample(150 * MIB, 0),
        ],
        200 * MIB,
    );
    assert_eq!(gate.admit(80 * MIB), Ok(()));
    // 100 + 80 + 40 > 200 on the estimate; the fresh measurement (60) leaves room.
    assert_eq!(gate.admit(40 * MIB), Ok(()));
    assert_eq!(sampler.samples_taken(), 2);
    // 60 + 40 since + 120 > 200: measured again; the fresh 150 replaces what was admitted
    // before it (it is resident now or never will be, f.9), and 150 + 120 > 200.
    let refused = gate.admit(120 * MIB);
    assert_eq!(
        refused,
        Err(Refusal {
            requested: 120 * MIB,
            in_use: 150 * MIB,
            ceiling: 200 * MIB,
        })
    );
    assert_eq!(gate.refusals(), 1);
    assert_eq!(gate.measurements(), 3);
    // The refused bytes were not kept: a small request fits again on the estimate.
    assert_eq!(gate.admit(5 * MIB), Ok(()));
}

#[test]
fn ad_t13_gate_takes_the_ceiling_of_the_sample() {
    // The sample's ceiling replaces the run's; zero keeps it.
    let (gate, _) = gate_over(
        vec![sample(10 * MIB, 0), sample(10 * MIB, 50 * MIB)],
        1000 * MIB,
    );
    assert_eq!(
        gate.admit(100 * MIB),
        Ok(()),
        "110 under 1000 on the estimate"
    );
    let refused = gate.admit(900 * MIB).expect_err("past the sample's 50 MiB");
    assert_eq!(refused.ceiling, 50 * MIB);
    let (named, _) = gate_over(vec![sample(10 * MIB, 30 * MIB)], 1000 * MIB);
    assert!(
        named.admit(25 * MIB).is_err(),
        "the first sample's ceiling is in force"
    );
}

#[test]
fn ad_t13_no_ceiling_admits_everything_and_credit_makes_room() {
    let (open, _) = gate_over(vec![sample(10 * MIB, 0)], 0);
    assert_eq!(open.admit(u64::MAX), Ok(()));
    let (gate, sampler) = gate_over(vec![sample(100 * MIB, 0)], 200 * MIB);
    assert_eq!(gate.admit(90 * MIB), Ok(()));
    gate.credit(90 * MIB);
    assert_eq!(gate.admit(90 * MIB), Ok(()));
    assert_eq!(
        sampler.samples_taken(),
        1,
        "the credit kept it on the fast path"
    );
}

#[test]
fn ad_t13_no_frame_calls_through_uncounted() {
    let mut called = false;
    let p = allocate(10 * MIB, || {
        called = true;
        some()
    });
    assert!(called && !p.is_null());
}

#[test]
fn ad_t13_frame_counts_python_requests_and_refuses_guarded_ones() {
    let (gate, _) = gate_over(vec![sample(100 * MIB, 0)], 110 * MIB);
    let kernel = KernelMemory::new(true);
    kernel.bind(gate.clone());
    kernel.bind(gate.clone());
    assert!(kernel.guarded());
    let (calls, end) = kernel.frame(|| {
        let mut calls = 0;
        // Small: counted, never examined.
        assert!(
            !allocate(100, || {
                calls += 1;
                some()
            })
            .is_null()
        );
        // Guarded and admitted.
        assert!(
            !allocate(5 * MIB, || {
                calls += 1;
                some()
            })
            .is_null()
        );
        // Guarded and refused: the allocator underneath is never called.
        assert!(
            allocate(50 * MIB, || {
                calls += 1;
                some()
            })
            .is_null()
        );
        // A second refusal is counted; the first is the one kept.
        assert!(allocate(60 * MIB, some).is_null());
        calls
    });
    assert_eq!(calls, 2);
    let python = end.counts.python;
    assert_eq!(python.requests, 4);
    assert_eq!(python.bytes, 100 + 5 * MIB + 50 * MIB + 60 * MIB);
    assert_eq!(python.largest, 60 * MIB);
    assert_eq!(python.refused, 2);
    assert_eq!(python.peak, 0, "Python objects' peak is not measured");
    assert!(end.counts.measured && end.counts.refusal_on);
    assert_eq!(end.refusal.map(|r| r.requested), Some(50 * MIB));
    assert_eq!(kernel.live().refusals, 2);
}

#[test]
fn ad_t13_failed_underlying_is_credited_and_reentrancy_is_not_counted() {
    let (gate, sampler) = gate_over(vec![sample(100 * MIB, 0)], 200 * MIB);
    let kernel = KernelMemory::new(true);
    kernel.bind(gate.clone());
    let (_, end) = kernel.frame(|| {
        // The allocator underneath fails: the admitted bytes go back to the gate.
        assert!(allocate(90 * MIB, core::ptr::null_mut::<u8>).is_null());
        // A hooked allocator calling another hooked allocator underneath: counted once.
        allocate(MIB, || allocate(MIB, some))
    });
    assert_eq!(end.counts.python.requests, 2);
    assert_eq!(end.counts.python.bytes, 91 * MIB);
    assert_eq!(
        gate.admit(99 * MIB),
        Ok(()),
        "90 credited back, 1 held: fits"
    );
    assert_eq!(sampler.samples_taken(), 1);
}

#[test]
fn ad_t13_unguarded_or_unbound_is_counted_and_never_refused() {
    let (gate, _) = gate_over(vec![sample(100 * MIB, 0)], 110 * MIB);
    let off = KernelMemory::new(false);
    off.bind(gate);
    let unbound = KernelMemory::new(true);
    for kernel in [off, unbound] {
        let (p, end) = kernel.frame(|| allocate(500 * MIB, some));
        assert!(!p.is_null(), "not refused");
        assert_eq!(end.counts.python.bytes, 500 * MIB, "but counted");
        assert!(!end.counts.refusal_on);
        assert!(end.refusal.is_none());
    }
}

#[test]
fn ad_t13_frames_nest_and_restore() {
    let outer = KernelMemory::new(false);
    let inner = KernelMemory::new(false);
    let (_, end) = outer.frame(|| {
        allocate(10, some);
        let (_, inner_end) = inner.frame(|| allocate(20, some));
        assert_eq!(inner_end.counts.python.bytes, 20);
        allocate(30, some);
    });
    assert_eq!(
        end.counts.python.bytes, 40,
        "the outer frame came back after the inner"
    );
    assert!(allocate(1, some) == some(), "and no frame is left behind");
}

#[test]
fn ad_t13_frame_is_restored_on_unwind() {
    let kernel = KernelMemory::new(false);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        kernel.frame(|| panic!("inside the frame"));
    }));
    assert!(unwound.is_err());
    let (_, end) = KernelMemory::new(false).frame(|| ());
    assert_eq!(end.counts.python.requests, 0);
    // A request now is outside any frame: nothing panics, nothing is counted anywhere.
    assert!(!allocate(1, some).is_null());
}

#[test]
fn ad_t13_numpy_blocks_are_exact() {
    let (gate, _) = gate_over(vec![sample(100 * MIB, 0)], 120 * MIB);
    let owner = KernelMemory::new(true);
    owner.bind(gate.clone());
    let (_, end) = owner.frame(|| {
        assert!(!allocate_owned(&owner, 8 * MIB, some).is_null());
        assert!(!allocate_owned(&owner, MIB, some).is_null());
        assert!(
            !allocate_owned(&owner, 100, some).is_null(),
            "small: counted, not live"
        );
        release_owned(&owner, MIB);
        // Grow the 8 MiB block to 12: the growth is the request.
        assert!(!resize_owned(&owner, 8 * MIB, 12 * MIB, some).is_null());
        // A growth past the room is refused and the block stays as it was.
        assert!(resize_owned(&owner, 12 * MIB, 40 * MIB, some).is_null());
        // A fresh block past the room is refused.
        assert!(allocate_owned(&owner, 30 * MIB, some).is_null());
        // The allocator underneath fails: nothing is held.
        assert!(allocate_owned(&owner, 2 * MIB, core::ptr::null_mut::<u8>).is_null());
        assert!(resize_owned(&owner, 12 * MIB, 13 * MIB, core::ptr::null_mut::<u8>).is_null());
        // Shrinking needs nothing from the gate.
        assert!(!resize_owned(&owner, 12 * MIB, 10 * MIB, some).is_null());
    });
    let numpy = end.counts.numpy;
    assert_eq!(numpy.requests, 9);
    assert_eq!(numpy.largest, 30 * MIB);
    assert_eq!(numpy.refused, 2);
    // Held: 8 + 1 + small, then -1, then +4: the peak is 12 MiB and the small block.
    assert_eq!(numpy.peak, 12 * MIB + 100);
    let live = owner.live();
    assert_eq!(
        live.live_bytes,
        10 * MIB,
        "the 10 MiB block; the small one is not live"
    );
    assert_eq!(live.peak_bytes, 12 * MIB);
    assert_eq!(live.refusals, 2);
    // A free outside any frame, on another thread, still leaves `live`.
    let owner2 = Arc::clone(&owner);
    std::thread::spawn(move || release_owned(&owner2, 10 * MIB))
        .join()
        .expect("freed");
    assert_eq!(owner.live().live_bytes, 0);
}

#[test]
fn ad_t13_numpy_outside_its_own_frame() {
    let owner = KernelMemory::new(false);
    let other = KernelMemory::new(false);
    // Another kernel's frame: the block is live for its owner, the call's counts are untouched.
    let (_, end) = other.frame(|| allocate_owned(&owner, 4 * MIB, some));
    assert_eq!(end.counts.numpy.requests, 0);
    assert_eq!(owner.live().live_bytes, 4 * MIB);
    // No frame at all: the same.
    allocate_owned(&owner, 4 * MIB, some);
    resize_owned(&owner, 4 * MIB, 5 * MIB, some);
    assert_eq!(owner.live().live_bytes, 9 * MIB);
}

#[test]
fn ad_t13_arrow_from_the_pool() {
    // Bytes allocated rose 3, the high-water mark rose to 10 above the start: the peak is 10.
    let c = arrow_counts((100, 105, 1000, 10), (103, 110, 1020, 13));
    assert_eq!((c.bytes, c.requests, c.peak), (20, 3, 10));
    assert_eq!((c.largest, c.refused), (0, 0));
    // No new mark: the peak is what is still held.
    let c = arrow_counts((100, 500, 1000, 10), (130, 500, 1030, 11));
    assert_eq!(c.peak, 30);
    // Memory given back: never negative.
    let c = arrow_counts((100, 500, 1000, 10), (50, 500, 1000, 10));
    assert_eq!((c.bytes, c.requests, c.peak), (0, 0, 0));
}
