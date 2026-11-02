//! KK-T10: a process that dereferences null is ended by the kernel, which says so on
//! the console; every other process runs on.

#![no_std]
#![no_main]

use moruna_kb1_programs::console;
use moruna_sys::println;

moruna_sys::main!(run);

fn run() -> i64 {
    let out = console();
    println!(out, "KB1 fault: about to read address 0");
    let address = core::hint::black_box(0usize);
    // SAFETY: none, on purpose. This is the test: the read of the null page must fault,
    // and the kernel must end this process and nothing else.
    let value = unsafe { core::ptr::read_volatile(address as *const u8) };
    println!(
        out,
        "KB1 fault: FAIL: read {} from address 0 and was not stopped", value
    );
    1
}
