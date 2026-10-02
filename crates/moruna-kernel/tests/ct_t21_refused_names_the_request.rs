//! CT-T21 refused_names_the_request (E13): `MorunaError::Refused` displays exactly the d.14
//! text; `KernelAlloc::default()` is all zero and unmeasured; the thread-local hand-off returns
//! what was set on the same thread once, and nothing set on another thread.

use moruna_kernel::{AllocCounts, KernelAlloc, MorunaError, set_kernel_alloc, take_kernel_alloc};

#[test]
fn ct_t21_refused_names_the_request() {
    let e = MorunaError::Refused {
        stage: 2,
        seq: 41,
        kernel: "jobs.greedy".into(),
        requested: 1 << 30,
        in_use: 400,
        ceiling: 512,
        features: None,
    };
    assert_eq!(
        e.to_string(),
        "budget: kernel jobs.greedy stage 2 morsel 41 requested 1073741824 bytes with 400 in use, \
         which would pass the ceiling of 512 bytes; refused before the memory existed"
    );
}

#[test]
fn ct_t21_kernel_alloc_hand_off() {
    assert_eq!(KernelAlloc::default(), KernelAlloc::NONE);
    assert!(!KernelAlloc::default().measured);
    assert_eq!(take_kernel_alloc(), KernelAlloc::NONE);
    let counts = KernelAlloc {
        measured: true,
        refusal_on: true,
        numpy: AllocCounts {
            bytes: 10,
            requests: 1,
            largest: 10,
            peak: 10,
            refused: 0,
        },
        ..KernelAlloc::default()
    };
    set_kernel_alloc(counts);
    std::thread::spawn(|| assert_eq!(take_kernel_alloc(), KernelAlloc::NONE))
        .join()
        .expect("the other thread saw nothing");
    assert_eq!(take_kernel_alloc(), counts);
    assert_eq!(take_kernel_alloc(), KernelAlloc::NONE);
    let mut c = counts;
    for source in 0..3 {
        for count in 0..5 {
            *c.source_mut(source).count_mut(count) = (source * 5 + count) as u64;
        }
    }
    assert_eq!(c.arrow.refused, 14);
    assert_eq!(c.source(1).count(2), 7);
    assert_eq!(c.python.count(0), 0);
}
