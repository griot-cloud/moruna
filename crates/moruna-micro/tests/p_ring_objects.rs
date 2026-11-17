//! Stream P: a ring task's region and rings object, as `layout::ring_sizes` sizes them for any
//! plane budget, are each at most the kernel's largest memory object, its rings laid out as the
//! plane lays them (`moruna_plane::ring::Geometry`). Past that the kernel refuses the object and
//! the task's plane does not open (`Kernel(Limit)`, the NYC gate at 8 GiB, 2026-11-17).

use moruna_micro::layout::{OBJECT_MAX, PAGE, RINGS_HEAD_MAX, ring_sizes};
use moruna_plane::ring::Geometry;

#[test]
fn p_a_ring_task_s_objects_fit_the_kernel_at_any_budget() {
    let budgets = (20..=46).map(|shift| 1u64 << shift).chain([
        (1 << 30) - PAGE,
        (1 << 30) + PAGE,
        20_992 << 20,
        u64::MAX,
    ]);
    for budget in budgets {
        let s = ring_sizes(budget);
        let g =
            Geometry::new(s.region, s.windows, s.window).expect("a geometry the plane lays out");
        assert!(
            g.windows_at() <= RINGS_HEAD_MAX,
            "the rings' head at {budget}"
        );
        let object = g.object_bytes().expect("in u64");
        assert!(
            object <= OBJECT_MAX,
            "rings object of {object} bytes at {budget}"
        );
        assert!(s.region <= OBJECT_MAX, "region at {budget}");
    }
}

#[test]
fn p_the_sizes_the_gate_was_refused_at_were_past_the_kernel_s_largest_object() {
    // What a 1 GiB plane gave before: two windows of an eighth each, 256 MiB with the head.
    let before = Geometry::new(256 << 20, 2, 128 << 20).expect("a geometry");
    assert!(before.object_bytes().expect("in u64") > OBJECT_MAX);
}
