// SPDX-License-Identifier: MPL-2.0

//! Moruna policy: all free buddy blocks share one coalescing domain.
//! The bounded one-to-four-frame caches remain in `cache`; large free pools
//! must not retain opposite physical buddies on different processors.

use super::set::BuddySet;
use crate::chunk::{greater_order_of, size_of_order, split_to_chunks};
use core::alloc::Layout;
use ostd::{
    irq::DisabledLocalIrqGuard,
    mm::Paddr,
    sync::{LocalIrqDisabled, SpinLock},
};

const MAX_BUDDY_ORDER: usize = 32;
static GLOBAL_POOL: SpinLock<BuddySet<MAX_BUDDY_ORDER>, LocalIrqDisabled> =
    SpinLock::new(BuddySet::new_empty());

fn alloc_from(pool: &mut BuddySet<MAX_BUDDY_ORDER>, layout: Layout) -> Option<Paddr> {
    let order = greater_order_of(layout.size()).max(greater_order_of(layout.align()));
    let available = pool.total_size();
    let addr = pool.alloc_chunk(order)?;
    let allocated_size = size_of_order(order);
    debug_assert!(available >= allocated_size);
    if allocated_size > layout.size() {
        insert_into(
            pool,
            [(addr + layout.size(), allocated_size - layout.size())].into_iter(),
        );
    }
    Some(addr)
}

fn insert_into(
    pool: &mut BuddySet<MAX_BUDDY_ORDER>,
    segments: impl Iterator<Item = (Paddr, usize)>,
) {
    for (addr, size) in segments {
        for (addr, order) in split_to_chunks(addr, size) {
            pool.insert_chunk(addr, order);
        }
    }
}

pub(super) fn alloc(_guard: &DisabledLocalIrqGuard, layout: Layout) -> Option<Paddr> {
    alloc_from(&mut GLOBAL_POOL.lock(), layout)
}

pub(super) fn dealloc(
    _guard: &DisabledLocalIrqGuard,
    segments: impl Iterator<Item = (Paddr, usize)>,
) {
    insert_into(&mut GLOBAL_POOL.lock(), segments);
}

pub(super) fn add_free_memory(_guard: &DisabledLocalIrqGuard, addr: Paddr, size: usize) {
    insert_into(&mut GLOBAL_POOL.lock(), [(addr, size)].into_iter());
}

#[cfg(feature = "shared-pool-probe")]
mod probe {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use ostd::{
        cpu::{CpuSet, PinCurrentCpu, num_cpus},
        irq,
        mm::{FrameAllocOptions, HasPaddr, PAGE_SIZE, Segment, UniqueFrame},
        smp::inter_processor_call,
    };

    static POOL: SpinLock<BuddySet<MAX_BUDDY_ORDER>, LocalIrqDisabled> =
        SpinLock::new(BuddySet::new_empty());
    static ADDRESS: AtomicUsize = AtomicUsize::new(0);
    const PAGES: usize = 64;
    const CHUNK_PAGES: usize = 8; // Bypasses the bounded one-to-four-frame cache.

    fn free_assigned_chunks() {
        let guard = irq::disable_local();
        let cpu = u32::from(guard.current_cpu()) as usize;
        let address = ADDRESS.load(Ordering::Acquire);
        let mut pool = POOL.lock();
        for index in 0..PAGES / CHUNK_PAGES {
            if index % num_cpus() == cpu {
                insert_into(
                    &mut pool,
                    [(
                        address + index * CHUNK_PAGES * PAGE_SIZE,
                        CHUNK_PAGES * PAGE_SIZE,
                    )]
                    .into_iter(),
                );
            }
        }
    }

    pub(super) fn run() {
        // An isolated real physical region prevents unrelated free blocks from
        // falsely satisfying the recombination assertion.
        let segment = FrameAllocOptions::new()
            .alloc_segment(PAGES)
            .expect("shared-pool probe region");
        let address = segment.paddr();
        for frame in segment {
            UniqueFrame::try_from(frame)
                .expect("probe frame ownership")
                .reset_as_unused();
        }
        ADDRESS.store(address, Ordering::Release);
        let mut targets = CpuSet::new_empty();
        for cpu in ostd::cpu::all_cpus() {
            targets.add(cpu);
        }
        inter_processor_call(&targets, free_assigned_chunks).wait();
        let mut pool = POOL.lock();
        assert_eq!(pool.total_size(), PAGES * PAGE_SIZE);
        let joined = alloc_from(
            &mut pool,
            Layout::from_size_align(PAGES * PAGE_SIZE, PAGES * PAGE_SIZE).unwrap(),
        )
        .expect("cross-CPU buddies must coalesce");
        assert_eq!(joined, address);
        assert_eq!(pool.total_size(), 0);
        drop(pool);
        // Restore physical ownership before returning the region to the real allocator.
        drop(Segment::from_unused(address..address + PAGES * PAGE_SIZE, |_| ()).unwrap());
        ostd::early_println!(
            "kk: shared frame pool coalesced {} pages across {} CPUs",
            PAGES,
            num_cpus()
        );
    }
}

#[cfg(feature = "shared-pool-probe")]
pub(crate) fn probe_shared_pool() {
    probe::run();
}
