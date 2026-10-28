//! KK-T11: the supervisor holds the root resources and uses each: it starts a child
//! process from an image file, claims a device memory region and maps it, refuses RAM,
//! creates an interrupt line and waits on it with a deadline.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    DeviceMemory, Error, Interrupt, Process, Rights, Signals, abi_version, now, println, wait_one,
};

moruna_sys::main!(run);

/// A page of high MMIO, above any RAM the test machine has.
const DEVICE_PAGE: u64 = 0x10_0000_0000;
/// A physical address inside RAM.
const RAM_PAGE: u64 = 0x10_0000;
/// An interrupt line no device on the test machine drives.
const QUIET_GSI: u32 = 10;

fn run() -> i64 {
    let out = console();
    let root_process = slot("root.process");
    let root_device = slot("root.device");
    let root_interrupt = slot("root.interrupt");
    let image = slot("child-image");
    if abi_version() != moruna_sys::ABI_VERSION {
        println!(
            out,
            "KB1 supervisor: FAIL: abi version {:#x}",
            abi_version()
        );
        return 1;
    }

    let child = match Process::create(&root_process, "child") {
        Ok(p) => p,
        Err(e) => return fail(&out, "process_create", e),
    };
    let entry = match child.map_image(&image) {
        Ok(e) => e,
        Err(e) => return fail(&out, "process_map_image", e),
    };
    let child_console = match out.0.duplicate(Rights::WRITE) {
        Ok(h) => h,
        Err(e) => return fail(&out, "handle_duplicate", e),
    };
    if let Err((e, _)) = child.start(entry, [(child_console, "console")]) {
        return fail(&out, "process_start", e);
    }
    match wait_one(&child.0, Signals::TERMINATED, now() + ms(5000)) {
        Ok(s) if s.contains(Signals::TERMINATED) => {}
        Ok(_) => return fail(&out, "wait on the child", Error::BadState),
        Err(e) => return fail(&out, "wait on the child", e),
    }

    let device = match DeviceMemory::create(&root_device, DEVICE_PAGE, 4096) {
        Ok(d) => d,
        Err(e) => return fail(&out, "device_memory_create", e),
    };
    match device.map(0, 4096, 0) {
        Ok(m) => {
            if let Err(e) = m.read_u32(0) {
                return fail(&out, "read device memory", e);
            }
        }
        Err(e) => return fail(&out, "memory_map of device memory", e),
    }
    match DeviceMemory::create(&root_device, RAM_PAGE, 4096) {
        Err(Error::AccessDenied) => {}
        Ok(_) => return fail(&out, "device memory over RAM was granted", Error::BadState),
        Err(e) => return fail(&out, "device memory over RAM", e),
    }
    match Interrupt::create(&root_process, QUIET_GSI) {
        Err(Error::AccessDenied) => {}
        Ok(_) => return fail(&out, "interrupt from the process resource", Error::BadState),
        Err(e) => return fail(&out, "interrupt from the process resource", e),
    }

    let line = match Interrupt::create(&root_interrupt, QUIET_GSI) {
        Ok(i) => i,
        Err(e) => return fail(&out, "interrupt_create", e),
    };
    match wait_one(&line.0, Signals::SIGNALED, now() + ms(20)) {
        Err(Error::TimedOut) => {}
        Ok(s) => println!(
            out,
            "KB1 supervisor: note: gsi {} raised {:?}", QUIET_GSI, s
        ),
        Err(e) => return fail(&out, "wait on the interrupt", e),
    }
    if let Err(e) = line.ack() {
        return fail(&out, "interrupt_ack", e);
    }

    println!(
        out,
        "KB1 supervisor: ok: started a child from its image and saw it end; mapped device memory at {:#x} and was refused RAM; waited on gsi {} until its deadline",
        DEVICE_PAGE,
        QUIET_GSI
    );
    0
}

fn fail(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "KB1 supervisor: FAIL: {}: {}", what, e.name());
    1
}
