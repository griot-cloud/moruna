// SPDX-License-Identifier: MPL-2.0

//! The Linux personality's start: what it gives Asterinas's Linux layer in place of a
//! bootloader, and the console the layer's `ttyS0` writes to.
//!
//! The image's entry (`main.rs`) records the start page and the first thread's block, runs
//! the constructors, reads its slots (`self`, `console`, `initramfs`, `cmdline`), then calls
//! [`start`] with the layer's boot.

#![no_std]

extern crate alloc;

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{fmt, time::Duration};

use aster_console::{AnyConsoleDevice, ConsoleCallback};
use component::{ComponentInitError, init_component};
use ostd::{
    boot::{
        BootInfo,
        memory_region::{MemoryRegion, MemoryRegionType},
    },
    mm::VmReader,
    sync::SpinLock,
    task::{Task, TaskOptions},
};

/// The memory the personality reports as its machine's (`/proc/meminfo`, `sysinfo`) when
/// its account cannot be read. Its frames come from memory objects as it needs them, so
/// this is a budget, not a reservation.
pub const MEMORY_BUDGET: usize = 1 << 30;

/// The memory a task is told its machine has: its process's account's limit, which the
/// core holds it to, or [`MEMORY_BUDGET`] when that cannot be read.
pub fn memory_of(account_limit: Option<u64>) -> usize {
    account_limit.map_or(MEMORY_BUDGET, |limit| limit as usize)
}

/// The command line in `bytes`, a memory object the launcher filled: up to the first zero
/// byte, trimmed, with invalid UTF-8 replaced.
pub fn cmdline(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().into()
}

/// What the layer starts with: the command line, the initramfs, `memory` bytes of memory,
/// and the clocks at this moment.
pub fn boot_info(cmdline: String, initramfs: Option<&'static [u8]>, memory: usize) -> BootInfo {
    BootInfo {
        bootloader_name: "moruna".into(),
        kernel_cmdline: cmdline,
        initramfs,
        memory_regions: alloc::vec![MemoryRegion::new(0, memory, MemoryRegionType::Usable,)],
        // No wall clock reaches the personality yet, so its time starts at the epoch.
        wall_clock_at_start: Duration::ZERO,
        monotonic_at_start: ostd::timer::monotonic_nanos(),
    }
}

/// Records `info` and runs `boot` in a task of its own, as OSTD runs a kernel's main.
pub fn start(info: BootInfo, boot: impl FnOnce()) {
    ostd::boot::init(info);
    let task = TaskOptions::new(|| {})
        .data(())
        .build()
        .expect("a task needs nothing it can lack");
    Task::enter(Arc::new(task));
    boot();
    Task::leave();
}

/// The console the layer's `ttyS0` uses: the core's console. It takes no input yet, so only
/// [`Console::receive`] calls the callbacks.
pub struct Console {
    write: fn(&[u8]),
    callbacks: SpinLock<Vec<&'static ConsoleCallback>>,
}

impl Console {
    /// A console that sends with `write`.
    pub fn new(write: fn(&[u8])) -> Self {
        Self {
            write,
            callbacks: SpinLock::new(Vec::new()),
        }
    }

    /// Hands `input` to every registered callback, as a console device's interrupt does.
    pub fn receive(&self, input: &[u8]) {
        let callbacks = self.callbacks.lock().clone();
        for callback in callbacks {
            callback(VmReader::from(input));
        }
    }
}

impl fmt::Debug for Console {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Console")
            .field("callbacks", &self.callbacks.lock().len())
            .finish()
    }
}

impl AnyConsoleDevice for Console {
    fn send(&self, buf: &[u8]) {
        (self.write)(buf);
    }

    fn register_callback(&self, callback: &'static ConsoleCallback) {
        self.callbacks.lock().push(callback);
    }
}

/// Registers the core's console as the layer's serial console.
#[init_component]
fn register_console() -> Result<(), ComponentInitError> {
    aster_console::register_device(
        aster_console::UART_CONSOLE_NAME.into(),
        Arc::new(Console::new(ostd::console::send)),
    );
    Ok(())
}

#[cfg(test)]
mod test;
