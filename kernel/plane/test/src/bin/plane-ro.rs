//! DP-T13: a task that writes into a batch it received. The batch is mapped read only, so
//! the write faults, and the kernel ends this task and nothing else (KK-I10), as KB1's
//! `fault` ends with its read of address 0.

#![no_std]
#![no_main]

use moruna_sys::plane::Plane;

moruna_sys::main!(run);

fn run() -> i64 {
    let Some(plane) = Plane::take() else {
        return 2;
    };
    let Some(mut input) = plane.input(0) else {
        return 3;
    };
    let Ok(Some(batch)) = input.get() else {
        return 4;
    };
    let address = core::hint::black_box(batch.addr());
    // SAFETY: none, on purpose. This is the test: the batch is mapped read only, the
    // write must fault, and the kernel must end this process.
    unsafe { core::ptr::write_volatile(address as *mut u8, 0x5a) };
    5
}
