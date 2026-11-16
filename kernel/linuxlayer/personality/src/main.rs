// SPDX-License-Identifier: MPL-2.0

//! The Linux personality's image. The launcher (`linuxlayer_launch::launcher`) maps it in
//! a program's process and starts its first thread at `_start`, with the stack pointer at
//! the start page's address. On the host there is nothing to run.

#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(target_os = "none")]
mod image {
    extern crate alloc;

    use alloc::boxed::Box;
    use core::arch::global_asm;

    use linuxlayer_launch::launcher::object_len;
    use moruna_sys::{Memory, start};

    // The first thread: the start page's address is at the stack pointer.
    global_asm!(
        ".globl _start",
        "_start:",
        "mov rdi, [rsp]",
        "and rsp, -16",
        "call {entry}",
        "ud2",
        entry = sym entry,
    );

    extern "C" fn entry(start_page: u64) -> ! {
        start::init(start_page);
        // SAFETY: the launcher gave the first thread a stack of `STACK_SIZE` bytes aligned to
        // its size, and nothing else uses its block.
        unsafe { ostd::backend::start_thread_block() };
        // SAFETY: once, on the only thread, before anything reads a registry.
        unsafe { run_constructors() };
        if let Some(own) = start::take("self") {
            ostd::backend::set_self_process(own.into_raw());
        }
        let cmdline = start::take("cmdline")
            .map(|m| linuxlayer_personality::cmdline(mapped(Memory(m))))
            .unwrap_or_default();
        let initramfs = start::take("initramfs").map(|m| mapped(Memory(m)));
        let info = linuxlayer_personality::boot_info(cmdline, initramfs);
        linuxlayer_personality::start(info, moruna_linuxlayer::boot);
        // The layer runs on the threads its boot started; the idle thread ends the process
        // when the init process ends.
        moruna_sys::thread_exit()
    }

    /// The whole memory object `object`, mapped read-only for as long as the process runs.
    fn mapped(object: Memory) -> &'static [u8] {
        let object: &'static Memory = Box::leak(Box::new(object));
        match object.map(0, object_len(object), 0) {
            Ok(mapping) => Box::leak(Box::new(mapping)).bytes(),
            Err(_) => &[],
        }
    }

    /// Runs the constructors `inventory` registers with (`personality.ld`'s `.init_array`).
    ///
    /// # Safety
    ///
    /// Once, before anything reads what they register.
    unsafe fn run_constructors() {
        unsafe extern "C" {
            static __init_array_start: [unsafe extern "C" fn(); 0];
            static __init_array_end: [unsafe extern "C" fn(); 0];
        }
        let mut at = (&raw const __init_array_start).cast::<unsafe extern "C" fn()>();
        let end = (&raw const __init_array_end).cast::<unsafe extern "C" fn()>();
        while at < end {
            // SAFETY: the linker fills the table with constructors, which run once each.
            unsafe { (*at)() };
            // SAFETY: `at` stays within the table or one past its end.
            at = unsafe { at.add(1) };
        }
    }
}

#[cfg(not(target_os = "none"))]
fn main() {}
