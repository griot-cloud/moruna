//! A `split_at` half releases only its own bytes, so a region goes back to the system
//! only when both halves have dropped. Freeing on whichever half held the first byte
//! left the other pointing into freed memory, which the component 9 agent hit for real
//! on 2026-09-22; this test is what stops it coming back.

use moruna_kernel::{Allocator, Tier};
use moruna_testkit::FakeAllocator;

#[test]
fn a_region_survives_until_both_halves_of_a_split_are_released() {
    let alloc = FakeAllocator::new();
    let buffer = alloc.alloc(8192, Tier::Host).expect("alloc");
    let (head, tail) = buffer.split_at(4096);

    // Drop the half holding the region's first byte. The other half must still be usable:
    // if the region went back to the system here, reading `tail` is a use after free and
    // the sanitiser or the allocator's own checks catch it.
    drop(head);
    let ptr = tail.host_ptr().expect("a host buffer has a pointer");
    // SAFETY: `tail` owns these bytes and has not been released.
    unsafe { std::ptr::write_bytes(ptr, 0xA5, tail.len()) };
    assert_eq!(tail.len(), 4096);
    assert_eq!(alloc.in_use(Tier::Host), 4096, "only the head was released");

    drop(tail);
    assert_eq!(alloc.in_use(Tier::Host), 0, "both halves are back");
}
