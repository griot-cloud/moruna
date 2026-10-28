//! KK-T11: the supervisor holds the root resources and uses each: it starts a child
//! process from an image file, claims a device memory region and maps it, refuses RAM,
//! creates an interrupt line and waits on it with a deadline.
//!
//! SV-T13 (SDD 23): it then narrows its device and interrupt resources to one page and
//! one line and starts a driver holding only those, which checks it can reach nothing
//! else; and it is refused a narrowing of the process resource and an unaligned one.
//!
//! DR-T7 (SDD 25): it makes each call ABI 1.2 added (`resource_info`,
//! `interrupt_create_msi`, `memory_pin`, `dma_release`, `process_kill`, and
//! `memory_create`'s flags) and is refused what each must refuse; then it hands the
//! device manager the roots of the devices: the process, device memory, ECAM, MSI and
//! DMA resources.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Channel, DeviceMemory, Error, Handle, Interrupt, Memory, Pin, Process, ResourceKind, Rights,
    Signals, abi_version, dma_release, map, now, println, raw, wait_one,
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
    let root_ecam = slot("root.ecam");
    let root_msi = slot("root.msi");
    let root_dma = slot("root.dma");
    let devices = Channel(slot("devices"));
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

    // A region whose last handle has closed leaves its range claimable again.
    drop(device);
    match DeviceMemory::create(&root_device, device_page, 4096) {
        Ok(again) => drop(again),
        Err(e) => return fail(&out, "claiming a released device page again", e),
    }

    println!(
        out,
        "KB1 supervisor: ok: started a child from its image (and was refused a handle without transfer) and saw it end; mapped device memory at {:#x}, and was refused it a second time and refused RAM; waited on gsi {} until its deadline",
        device_page,
        QUIET_GSI
    );
    if let Err(code) = driver_calls(
        &out,
        &root_process,
        &root_device,
        &root_ecam,
        &root_msi,
        &root_dma,
        &driver_image,
    ) {
        return code;
    }
    // SDD 25: the device manager does the device work with the device roots.
    if let Err((e, _)) = devices.write(
        b"roots",
        [root_process, root_device, root_ecam, root_msi, root_dma],
    ) {
        return fail(&out, "handing the device manager its roots", e);
    }
    0
}

fn kb3(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "KB3 kernel calls: FAIL: {}: {}", what, e.name());
    1
}

/// `result` must be the error `want`.
fn refused_with<T>(
    out: &moruna_sys::Console,
    what: &str,
    want: Error,
    result: Result<T, Error>,
) -> Result<(), i64> {
    match result {
        Err(e) if e == want => Ok(()),
        Err(e) => Err(kb3(out, what, e)),
        Ok(_) => Err(kb3(out, what, Error::BadState)),
    }
}

/// DR-T7: the calls ABI 1.2 added, each made and each refused where it must be.
fn driver_calls(
    out: &moruna_sys::Console,
    root_process: &Handle,
    root_device: &Handle,
    root_ecam: &Handle,
    root_msi: &Handle,
    root_dma: &Handle,
    driver_image: &Handle,
) -> Result<(), i64> {
    // resource_info: the ECAM window is whole buses; each root covers its kind's range.
    let ecam = root_ecam
        .resource_info()
        .map_err(|e| kb3(out, "resource_info of root.ecam", e))?;
    if ecam.kind != ResourceKind::DeviceMemory
        || ecam.len == 0
        || ecam.len % (1 << 20) != 0
        || ecam.base % (1 << 20) != 0
    {
        return Err(kb3(out, "root.ecam is not whole buses", Error::BadState));
    }
    for (h, kind, len) in [
        (root_msi, ResourceKind::Msi, 224),
        (root_dma, ResourceKind::Dma, 1 << 16),
        (root_process, ResourceKind::Process, 0),
    ] {
        match h.resource_info() {
            Ok(i) if i.kind == kind && i.base == 0 && i.len == len => {}
            Ok(_) => return Err(kb3(out, "a root's range", Error::BadState)),
            Err(e) => return Err(kb3(out, "resource_info of a root", e)),
        }
    }
    let mut buf = [0u8; 24];
    refused_with(
        out,
        "resource_info of a memory object",
        Error::WrongType,
        moruna_sys::Error::from_raw(raw::resource_info(driver_image.raw(), &mut buf)),
    )?;

    // interrupt_create_msi: a slot inside the resource, once at a time.
    let slots = root_msi
        .narrow(200, 1)
        .map_err(|e| kb3(out, "narrowing MSI slots", e))?;
    let (line, message) =
        Interrupt::create_msi(&slots, 200).map_err(|e| kb3(out, "interrupt_create_msi", e))?;
    if message.address & !0xf_fff0 != 0xfee0_0000 || !(32..=255).contains(&message.data) {
        return Err(kb3(out, "the MSI message", Error::BadState));
    }
    refused_with(
        out,
        "a slot held twice",
        Error::AccessDenied,
        Interrupt::create_msi(&slots, 200),
    )?;
    refused_with(
        out,
        "a slot outside the resource",
        Error::AccessDenied,
        Interrupt::create_msi(&slots, 201),
    )?;
    refused_with(
        out,
        "a slot past the last",
        Error::InvalidArgs,
        Interrupt::create_msi(root_msi, 224),
    )?;
    refused_with(
        out,
        "MSI from the device resource",
        Error::AccessDenied,
        Interrupt::create_msi(root_device, 0),
    )?;
    drop(line);
    drop(Interrupt::create_msi(&slots, 200).map_err(|e| kb3(out, "a released slot again", e))?);

    // memory_create's flags; memory_pin and the quarantine.
    refused_with(
        out,
        "memory_create with an unknown flag",
        Error::InvalidArgs,
        moruna_sys::Error::from_raw(raw::memory_create(4096, 2)),
    )?;
    let domain = root_dma
        .narrow(0xffff, 1)
        .map_err(|e| kb3(out, "narrowing DMA", e))?;
    let memory = Memory::create_dma(8192).map_err(|e| kb3(out, "a DMA memory object", e))?;
    let mut one = [0u64; 1];
    refused_with(
        out,
        "pinning into too few addresses",
        Error::BufferTooSmall,
        Pin::create(&domain, &memory.0, true, &mut one),
    )?;
    match Pin::pages(&domain, &memory.0, true) {
        Ok(2) => {}
        Ok(_) => return Err(kb3(out, "the pages a pin needs", Error::BadState)),
        Err(e) => return Err(kb3(out, "the pages a pin needs", e)),
    }
    let mut pages = [0u64; 2];
    let (pin, n) =
        Pin::create(&domain, &memory.0, true, &mut pages).map_err(|e| kb3(out, "memory_pin", e))?;
    if n != 2
        || pages[0] % 4096 != 0
        || pages[1] % 4096 != 0
        || pages[0] == pages[1]
        || pages[0] == 0
    {
        return Err(kb3(out, "the pinned pages' addresses", Error::BadState));
    }
    let plain = Memory::create(4096).map_err(|e| kb3(out, "memory_create", e))?;
    refused_with(
        out,
        "pinning memory not made for DMA",
        Error::InvalidArgs,
        Pin::create(&domain, &plain.0, false, &mut pages),
    )?;
    let read_only = memory
        .0
        .duplicate(Rights::READ | Rights::MAP)
        .map_err(|e| kb3(out, "handle_duplicate", e))?;
    refused_with(
        out,
        "a device writing through a read-only handle",
        Error::AccessDenied,
        Pin::create(&domain, &read_only, true, &mut pages),
    )?;
    drop(
        Pin::create(&domain, &read_only, false, &mut pages)
            .map_err(|e| kb3(out, "a read-only pin", e))?,
    );
    refused_with(
        out,
        "pinning without a DMA resource",
        Error::AccessDenied,
        Pin::create(&slots, &memory.0, false, &mut pages),
    )?;
    drop(pin);
    match dma_release(&domain) {
        Ok(2) => {}
        Ok(_) => return Err(kb3(out, "the quarantine's pins", Error::BadState)),
        Err(e) => return Err(kb3(out, "dma_release", e)),
    }
    match dma_release(&domain) {
        Ok(0) => {}
        Ok(_) => return Err(kb3(out, "a released quarantine", Error::BadState)),
        Err(e) => return Err(kb3(out, "dma_release again", e)),
    }

    // process_kill: refused before a start; ends a running process; refused after.
    let killee =
        Process::create(root_process, "killee").map_err(|e| kb3(out, "process_create", e))?;
    refused_with(
        out,
        "killing a process not started",
        Error::BadState,
        killee.kill(),
    )?;
    let entry = killee
        .map_image(driver_image)
        .map_err(|e| kb3(out, "process_map_image", e))?;
    let device = root_device
        .narrow(0x110_0000_0000, 4096)
        .map_err(|e| kb3(out, "narrowing", e))?;
    let interrupt = root_msi
        .narrow(0, 1)
        .map_err(|e| kb3(out, "narrowing", e))?;
    // The driver program waits on `control` for its window; nothing is ever sent.
    let (_quiet, control) = Channel::create().map_err(|e| kb3(out, "channel_create", e))?;
    let console = out
        .0
        .duplicate(Rights::WRITE | Rights::TRANSFER)
        .map_err(|e| kb3(out, "handle_duplicate", e))?;
    if let Err((e, _)) = killee.start(
        entry,
        [
            (console, "console"),
            (device, "device"),
            (interrupt, "interrupt"),
            (control.0, "control"),
        ],
    ) {
        return Err(kb3(out, "process_start", e));
    }
    killee.kill().map_err(|e| kb3(out, "process_kill", e))?;
    match wait_one(&killee.0, Signals::TERMINATED, now() + ms(5000)) {
        Ok(s) if s.contains(Signals::TERMINATED) => {}
        Ok(_) => return Err(kb3(out, "the killed process did not end", Error::BadState)),
        Err(e) => return Err(kb3(out, "waiting on the killed process", e)),
    }
    refused_with(
        out,
        "killing an ended process",
        Error::BadState,
        killee.kill(),
    )?;
    println!(
        out,
        "KB3 kernel calls: ok: root.ecam is {} buses at {:#x}; an MSI slot gave address {:#x} data {}, once at a time, only inside its resource; a pin gave two device pages and went to its quarantine, which dma_release emptied; a running process was killed, one not started and one ended were refused",
        ecam.len >> 20,
        ecam.base,
        message.address,
        message.data
    );
    Ok(())
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
