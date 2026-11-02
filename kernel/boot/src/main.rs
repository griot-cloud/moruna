//! The kernel image: OSTD's entry, its panic handler and its allocators, wired to the
//! Moruna microkernel. This is the crate `cargo osdk` would generate as its "base
//! crate"; it is written out here so the kernel builds with plain cargo. It holds the
//! only attributes in the kernel that must be unsafe (exported symbols), and no logic.

#![no_std]
#![no_main]
#![feature(linkage)]

#[ostd::main]
fn main() {
    moruna_microkernel::main();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    unsafe extern "Rust" {
        fn __ostd_panic_handler(info: &core::panic::PanicInfo) -> !;
    }
    // SAFETY: OSTD defines `__ostd_panic_handler` with exactly this signature; it prints
    // the panic and a backtrace and powers the machine off, so it never returns.
    unsafe { __ostd_panic_handler(info) }
}

mod frame_allocator {
    use osdk_frame_allocator::FrameAllocator;
    use ostd::mm::frame::GlobalFrameAllocator;

    static FRAME_ALLOCATOR: FrameAllocator = FrameAllocator;

    // SAFETY: OSTD looks this symbol up by name and no other crate in the image defines it.
    #[unsafe(no_mangle)]
    #[linkage = "weak"]
    static __GLOBAL_FRAME_ALLOCATOR_REF: &'static dyn GlobalFrameAllocator = &FRAME_ALLOCATOR;
}

mod heap_allocator {
    use osdk_heap_allocator::{HeapAllocator, type_from_layout};
    use ostd::mm::heap::{GlobalHeapAllocator, SlotInfo};

    static HEAP_ALLOCATOR: HeapAllocator = HeapAllocator;

    // SAFETY: OSTD looks this symbol up by name and no other crate in the image defines it.
    #[unsafe(no_mangle)]
    #[linkage = "weak"]
    static __GLOBAL_HEAP_ALLOCATOR_REF: &'static dyn GlobalHeapAllocator = &HEAP_ALLOCATOR;

    // SAFETY: OSTD looks this symbol up by name and no other crate in the image defines it.
    #[unsafe(no_mangle)]
    #[linkage = "weak"]
    fn __global_heap_slot_info_from_layout(layout: core::alloc::Layout) -> Option<SlotInfo> {
        type_from_layout(layout)
    }
}
