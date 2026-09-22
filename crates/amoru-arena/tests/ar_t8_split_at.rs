//! AR-T8 split_at (02 k): split, release one half, allocate again: the slot is not reused
//! until both halves have been released. e.3.

mod common;

use amoru_kernel::{Allocator, Tier};

#[test]
fn ar_t8_split_at() {
    let arena = common::host_arena(4 << 20);
    let b = arena
        .alloc(64 * 1024, Tier::Host)
        .expect("one class-0 slot");
    let slot = b.host_ptr().expect("host pointer");
    assert_eq!(arena.stats().host_in_use, 64 * 1024);

    let (head, tail) = b.split_at(16 * 1024);
    assert_eq!((head.len(), tail.len()), (16 * 1024, 48 * 1024));
    assert_eq!(
        arena.stats().host_in_use,
        64 * 1024,
        "one slot, two buffers"
    );

    drop(head);
    assert_eq!(
        arena.stats().host_in_use,
        64 * 1024,
        "half the slot is still live"
    );
    let other = arena.alloc(64 * 1024, Tier::Host).expect("another slot");
    assert_ne!(
        other.host_ptr().expect("host pointer"),
        slot,
        "the slot must not be reused while a half of it is live (e.3)"
    );
    drop(other);

    drop(tail);
    assert_eq!(arena.stats().host_in_use, 0, "both halves are back");
    let again = arena.alloc(64 * 1024, Tier::Host).expect("the slot again");
    assert_eq!(
        again.host_ptr().expect("host pointer"),
        slot,
        "the slot is reusable once every byte of it is back"
    );
    assert_eq!(arena.arena_stats().double_release, 0);
}
