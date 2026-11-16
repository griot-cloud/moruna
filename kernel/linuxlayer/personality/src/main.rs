// SPDX-License-Identifier: MPL-2.0

//! The Linux personality's image. The launcher (`linuxlayer_launch::launcher`) maps it in
//! a program's process and starts its first thread at `_start`, with the stack pointer at
//! the start page's address. On the host there is nothing to run.

#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(target_os = "none")]
mod image {
    extern crate alloc;

    use alloc::{boxed::Box, string::String, vec::Vec};
    use core::arch::global_asm;

    use linuxlayer_launch::{launcher::object_len, slot};
    use moruna_linuxlayer::start::{ProgramGrant, TaskStart, start_task};
    use moruna_sys::{Account, Channel, Memory, Usage, start};

    /// The scratch's limit when the launcher grants no scratch account.
    const SCRATCH_DEFAULT: usize = 64 << 20;

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

    /// The handles granted to the task's program: every entry of the start page whose name is
    /// not one of the personality's own slots.
    fn program_grants() -> Vec<ProgramGrant> {
        let names: Vec<(&'static str, moruna_sys::Rights)> = (0..start::count())
            .filter_map(start::entry)
            .filter(|(_, _, name)| slot::is_program(name))
            .map(|(_, rights, name)| (name, rights))
            .collect();
        names
            .into_iter()
            .filter_map(|(name, rights)| {
                Some(ProgramGrant {
                    name: String::from(name),
                    handle: start::take(name)?,
                    rights,
                })
            })
            .collect()
    }

    extern "C" fn entry(start_page: u64) -> ! {
        start::init(start_page);
        // SAFETY: the launcher gave the first thread a stack of `STACK_SIZE` bytes aligned to
        // its size, and nothing else uses its block.
        unsafe { ostd::backend::start_thread_block() };
        // SAFETY: once, on the only thread, before anything reads a registry.
        unsafe { run_constructors() };
        if let Some(own) = start::take(slot::SELF) {
            let own = own.into_raw();
            ostd::backend::set_self_process(own);
            // The heap's memory comes from the task's shared memory, before anything is
            // allocated: one address in every process of the task.
            if let Err(e) = ostd::backend::start_task_memory(own) {
                ostd::early_println!("linux: the heap stays the process's own: {:?}", e);
            }
        }
        // Frames' objects come from the task's shared memory: one address in every process
        // of the task (`ostd::backend::shared_map`).
        ostd::mm::vm_space::inject_object_source(ostd::mm::vm_space::ObjectSource {
            create: shared_object,
            release: |_, addr, _| {
                let _ = ostd::backend::shared_unmap(addr);
            },
        });
        if let Some(processes) = start::take(slot::PROCESSES) {
            ostd::backend::set_process_resource(processes.into_raw());
        }
        let cmdline = start::take(slot::CMDLINE)
            .map(|m| linuxlayer_personality::cmdline(mapped(Memory(m))))
            .unwrap_or_default();
        let initramfs = start::take(slot::INITRAMFS).map(|m| mapped(Memory(m)));
        // A task: its image, its scratch, its plane and its log; the layer runs the program
        // its command names on that image.
        if let Some(filesystem) = start::take(slot::FILESYSTEM) {
            let scratch_quota = start::take(slot::SCRATCH)
                .and_then(|account| Account(account).usage().ok())
                .map_or(SCRATCH_DEFAULT, |usage| usage.limit as usize);
            let task = TaskStart {
                filesystem: Channel(filesystem),
                scratch_quota,
                input: start::take(slot::INPUT),
                output: start::take(slot::OUTPUT),
                log: start::take(slot::LOG).map(|log| {
                    let log = Memory(log);
                    let len = object_len(&log);
                    (log, len)
                }),
                name: String::from(start::process_name()),
                program: program_grants(),
                report: start::take(slot::REPORT).map(Memory),
                command: start::take(slot::COMMAND).and_then(|c| {
                    let c = linuxlayer_launch::command::decode(mapped(Memory(c)))?;
                    Some(moruna_linuxlayer::start::TaskCommand {
                        path: c.path,
                        argv: c.argv,
                        env: c.env,
                    })
                }),
            };
            if let Err(e) = start_task(task) {
                ostd::early_println!("linux: FAIL: the task cannot start: {:?}", e);
                moruna_sys::exit(1);
            }
        }
        let limit = Usage::own().ok().map(|usage| usage.limit);
        let memory = linuxlayer_personality::memory_of(limit);
        let info = linuxlayer_personality::boot_info(cmdline, initramfs, memory);
        linuxlayer_personality::start(info, moruna_linuxlayer::boot);
        // The layer runs on the threads its boot started; the idle thread ends the process
        // when the init process ends.
        moruna_sys::thread_exit()
    }

    /// An object for frames from the task's shared memory: its handle and address.
    fn shared_object(len: usize) -> ostd::Result<(u32, usize)> {
        let addr = ostd::backend::shared_map(len)?;
        let (_, object, _) = ostd::backend::shared_chunk(addr).ok_or(ostd::Error::NoMemory)?;
        Ok((object, addr))
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
