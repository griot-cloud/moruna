//! KK-T11: the supervisor holds the root resources and uses each: it starts a child
//! process from an image file, claims a device memory region and maps it, refuses RAM,
//! creates an interrupt line and waits on it with a deadline.
//!
//! SV-T13 (SDD 23): it then narrows its device and interrupt resources to one page and
//! one line and starts a driver holding only those, which checks it can reach nothing
//! else; and it is refused a narrowing of the process resource and an unaligned one.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Channel, DeviceMemory, Error, Handle, Interrupt, Process, Rights, Signals, abi_version, map,
    now, println, wait_one,
};

moruna_sys::main!(run);

/// Pages of high MMIO, above any RAM the test machine has: one below the range QEMU
/// reserves on AMD hosts (1012 GiB to 1 TiB) and one above it, since OSTD's window
/// starts above the highest reserved region. The region is mapped, never read.
const DEVICE_PAGES: [u64; 2] = [0x10_0000_0000, 0x110_0000_0000];
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
    let driver_image = slot("driver-image");
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
    // A handle without TRANSFER cannot be given: the start is refused and the handle
    // comes back still owned by the caller.
    let kept = match out.0.duplicate(Rights::WRITE) {
        Ok(h) => h,
        Err(e) => return fail(&out, "handle_duplicate", e),
    };
    match child.start(entry, [(kept, "console")]) {
        Err((Error::AccessDenied, [(back, _)])) if back.info().is_ok() => {}
        Err((e, _)) => return fail(&out, "process_start without TRANSFER", e),
        Ok(()) => {
            return fail(
                &out,
                "process_start moved a handle without TRANSFER",
                Error::BadState,
            );
        }
    }
    let child_console = match out.0.duplicate(Rights::WRITE | Rights::TRANSFER) {
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

    let mut claimed = None;
    for page in DEVICE_PAGES {
        match DeviceMemory::create(&root_device, page, 4096) {
            Ok(d) => {
                claimed = Some((page, d));
                break;
            }
            Err(Error::AccessDenied) => {}
            Err(e) => return fail(&out, "device_memory_create", e),
        }
    }
    let Some((device_page, device)) = claimed else {
        return fail(
            &out,
            "no high MMIO page could be claimed",
            Error::AccessDenied,
        );
    };
    match device.map(0, 4096, map::WRITE) {
        Ok(mapping) => drop(mapping),
        Err(e) => return fail(&out, "memory_map of device memory", e),
    }
    match DeviceMemory::create(&root_device, device_page, 4096) {
        Err(Error::AccessDenied) => {}
        Ok(_) => {
            return fail(
                &out,
                "a claimed device page was granted twice",
                Error::BadState,
            );
        }
        Err(e) => return fail(&out, "device memory claimed twice", e),
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
    drop(line);
    if let Err(code) = narrowing(
        &out,
        &root_process,
        &root_device,
        &root_interrupt,
        &driver_image,
        device_page,
    ) {
        return code;
    }

    println!(
        out,
        "KB1 supervisor: ok: started a child from its image (and was refused a handle without transfer) and saw it end; mapped device memory at {:#x}, and was refused it a second time and refused RAM; waited on gsi {} until its deadline",
        device_page,
        QUIET_GSI
    );
    0
}

fn fail(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "KB1 supervisor: FAIL: {}: {}", what, e.name());
    1
}

/// SV-T13: narrows the device resource to the page after `device_page` and the
/// interrupt resource to the quiet line, starts the driver with only those, and waits
/// for it to end with code 0.
fn narrowing(
    out: &moruna_sys::Console,
    root_process: &Handle,
    root_device: &Handle,
    root_interrupt: &Handle,
    driver_image: &Handle,
    device_page: u64,
) -> Result<(), i64> {
    let window = device_page + 4096;
    match root_process.narrow(0, 1) {
        Err(Error::InvalidArgs) => {}
        Ok(_) => {
            return Err(fail(
                out,
                "the process resource was narrowed",
                Error::BadState,
            ));
        }
        Err(e) => return Err(fail(out, "narrowing the process resource", e)),
    }
    match root_device.narrow(window + 1, 4096) {
        Err(Error::InvalidArgs) => {}
        Ok(_) => {
            return Err(fail(
                out,
                "an unaligned device window was granted",
                Error::BadState,
            ));
        }
        Err(e) => return Err(fail(out, "narrowing to an unaligned window", e)),
    }
    let device = root_device
        .narrow(window, 4096)
        .map_err(|e| fail(out, "narrowing the device resource", e))?;
    let interrupt = root_interrupt
        .narrow(QUIET_GSI as u64, 1)
        .map_err(|e| fail(out, "narrowing the interrupt resource", e))?;
    let (control, theirs) = Channel::create().map_err(|e| fail(out, "channel_create", e))?;
    let mut message = [0u8; 16];
    message[..8].copy_from_slice(&window.to_le_bytes());
    message[8..].copy_from_slice(&(QUIET_GSI as u64).to_le_bytes());
    if let Err((e, _)) = control.write(&message, []) {
        return Err(fail(out, "writing the driver its window", e));
    }
    let driver = Process::create(root_process, "driver")
        .map_err(|e| fail(out, "process_create for the driver", e))?;
    let entry = driver
        .map_image(driver_image)
        .map_err(|e| fail(out, "process_map_image for the driver", e))?;
    let console = out
        .0
        .duplicate(Rights::WRITE | Rights::TRANSFER)
        .map_err(|e| fail(out, "handle_duplicate", e))?;
    if let Err((e, _)) = driver.start(
        entry,
        [
            (console, "console"),
            (device, "device"),
            (interrupt, "interrupt"),
            (theirs.0, "control"),
        ],
    ) {
        return Err(fail(out, "process_start for the driver", e));
    }
    match wait_one(&driver.0, Signals::TERMINATED, now() + ms(10_000)) {
        Ok(s) if s.contains(Signals::TERMINATED) => Ok(()),
        Ok(_) => Err(fail(out, "wait on the driver", Error::BadState)),
        Err(e) => Err(fail(out, "wait on the driver", e)),
    }
}
