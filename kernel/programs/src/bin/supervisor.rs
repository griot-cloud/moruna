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
//!
//! KK-T14 (ABI 2.0): it builds a process's memory from outside, as the Linux
//! personality does (`process_map` at a chosen address, of zero pages and of an object,
//! `process_write` and `process_read`, `process_protect`, a partial `process_unmap`),
//! and is refused each thing it must be; then it hands the Linux personality server a
//! process resource of its own.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Channel, DeviceMemory, Error, Handle, Interrupt, Memory, Pin, Process, ResourceKind, Rights,
    Signals, abi_version, dma_release, map, memory_flags, now, println, raw, wait_one,
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
    let linux = Channel(slot("linux"));
    let plane = Channel(slot("plane"));
    let quota = Channel(slot("quota"));
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
    if let Err(code) = remote_memory(&out, &root_process) {
        return code;
    }
    if let Err(code) = grant(&out, &root_process, &image) {
        return code;
    }
    // SDD 28: the Linux personality makes its processes with a process resource of its
    // own.
    let for_linux = match root_process.duplicate(Rights::WRITE | Rights::TRANSFER) {
        Ok(h) => h,
        Err(e) => return fail(&out, "duplicating the process resource", e),
    };
    if let Err((e, _)) = linux.write(moruna_node::Bootstrap::Linux.header(), [for_linux]) {
        return fail(
            &out,
            "handing the Linux personality its process resource",
            e,
        );
    }
    // SDD 29: the data plane's test engine starts its tasks with a process resource of
    // its own.
    let for_plane = match root_process.duplicate(Rights::WRITE | Rights::TRANSFER) {
        Ok(h) => h,
        Err(e) => return fail(&out, "duplicating the process resource", e),
    };
    if let Err((e, _)) = plane.write(b"process", [for_plane]) {
        return fail(
            &out,
            "handing the data plane's engine its process resource",
            e,
        );
    }
    // SDD 20 f.18 (KB5): the accounts test starts its children with a process resource
    // of its own.
    let for_quota = match root_process.duplicate(Rights::WRITE | Rights::TRANSFER) {
        Ok(h) => h,
        Err(e) => return fail(&out, "duplicating the process resource", e),
    };
    if let Err((e, _)) = quota.write(b"process", [for_quota]) {
        return fail(&out, "handing the accounts test its process resource", e);
    }
    // SDD 25: the device manager does the device work with the device roots.
    if let Err((e, _)) = devices.write(
        moruna_node::Bootstrap::Devices.header(),
        [root_process, root_device, root_ecam, root_msi, root_dma],
    ) {
        return fail(&out, "handing the device manager its roots", e);
    }
    supervise_descendants(&out, &[devices, linux])
}

/// The test boot is a real supervisor too: it drains the same escrow codec while
/// device and Linux services run, rather than dropping their supervisory peers.
fn supervise_descendants(out: &moruna_sys::Console, guardians: &[Channel]) -> i64 {
    extern crate alloc;
    use alloc::vec::Vec;
    let mut children: Vec<(usize, u64, Handle)> = Vec::new();
    let mut open = alloc::vec![true; guardians.len()];
    let deadline = now().saturating_add(300_000_000_000);
    loop {
        for (index, guardian) in guardians.iter().enumerate() {
            if !open[index] {
                continue;
            }
            loop {
                let mut bytes = [0; 16];
                let mut handles = [None];
                let (n, count) = match guardian.read(&mut bytes, &mut handles) {
                    Ok(message) => message,
                    Err(Error::ShouldWait) => break,
                    Err(Error::PeerClosed) => {
                        open[index] = false;
                        break;
                    }
                    Err(e) => return fail(out, "reading child escrow", e),
                };
                let Some(record) = moruna_node::ChildRecord::decode(&bytes[..n], count) else {
                    return fail(out, "child escrow codec", Error::InvalidArgs);
                };
                if record.retain {
                    if children
                        .iter()
                        .any(|(g, id, _)| *g == index && *id == record.id)
                        || children.iter().filter(|(g, _, _)| *g == index).count() >= 64
                    {
                        return fail(out, "child escrow limit", Error::Limit);
                    }
                    let Some(handle) = handles[0].take() else {
                        return fail(out, "missing child escrow", Error::InvalidArgs);
                    };
                    if !matches!(handle.info(), Ok((moruna_sys::ObjectType::Process, rights)) if rights.contains(Rights::WRITE | Rights::WAIT))
                    {
                        return fail(out, "child escrow rights", Error::AccessDenied);
                    }
                    children.push((index, record.id, handle));
                } else {
                    children.retain(|(g, id, _)| *g != index || *id != record.id);
                }
            }
        }
        children.retain(|(_, _, child)| !matches!(wait_one(child, Signals::TERMINATED, now()), Ok(seen) if seen.contains(Signals::TERMINATED)));
        if open.iter().all(|open| !open) {
            for (_, _, child) in children {
                let _ = Process(child).kill();
            }
            return 0;
        }
        let mut items: Vec<_> = guardians
            .iter()
            .enumerate()
            .filter(|(i, _)| open[*i])
            .map(|(_, g)| moruna_sys::WaitItem::new(&g.0, Signals::READABLE | Signals::PEER_CLOSED))
            .collect();
        items.extend(
            children
                .iter()
                .map(|(_, _, child)| moruna_sys::WaitItem::new(child, Signals::TERMINATED)),
        );
        if let Err(e) = moruna_sys::wait(&mut items, deadline) {
            for (_, _, child) in children {
                let _ = Process(child).kill();
            }
            return fail(out, "supervising descendants", e);
        }
    }
}

fn kd2(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "KD2 grant: FAIL: {}: {}", what, e.name());
    1
}

/// KD2-T8 (SDD 30 k): `process_grant` (ABI 2.2) moves handles into a created process and maps
/// a start page naming them, once; a handle without `TRANSFER` is refused and stays here;
/// `process_start` refuses a granted process, though its image is mapped.
fn grant(out: &moruna_sys::Console, root_process: &Handle, image: &Handle) -> Result<(), i64> {
    let p = Process::create(root_process, "granted").map_err(|e| kd2(out, "process_create", e))?;
    let entry = p
        .map_image(image)
        .map_err(|e| kd2(out, "process_map_image", e))?;
    let (mine, theirs) = Channel::create().map_err(|e| kd2(out, "channel_create", e))?;
    // A handle without TRANSFER: refused, and nothing moves.
    let fixed = mine
        .0
        .duplicate(Rights::READ | Rights::WRITE)
        .map_err(|e| kd2(out, "handle_duplicate", e))?;
    let mut refused = [(Some(fixed), "fixed")];
    match p.grant(&mut refused) {
        Err(Error::AccessDenied) if refused[0].0.is_some() => {}
        Err(e) => return Err(kd2(out, "a grant without TRANSFER", e)),
        Ok(_) => {
            return Err(kd2(
                out,
                "a grant without TRANSFER was taken",
                Error::BadState,
            ));
        }
    }
    let mut named = [(Some(theirs.0), "peer")];
    let page = p
        .grant(&mut named)
        .map_err(|e| kd2(out, "process_grant", e))?;
    if named[0].0.is_some() || page == 0 || page % 4096 != 0 {
        return Err(kd2(out, "the grant's page or its handle", Error::BadState));
    }
    let (_, again) = Channel::create().map_err(|e| kd2(out, "channel_create", e))?;
    let mut twice = [(Some(again.0), "again")];
    match p.grant(&mut twice) {
        Err(Error::BadState) if twice[0].0.is_some() => {}
        Err(e) => return Err(kd2(out, "a second grant", e)),
        Ok(_) => return Err(kd2(out, "a second grant was taken", Error::BadState)),
    }
    // The page is mapped read only in the process and names the one handle.
    use moruna_abi::start_page as sp;
    let mut head = [0u8; sp::HEADER_SIZE + sp::ENTRY_SIZE];
    p.read(page, &mut head)
        .map_err(|e| kd2(out, "process_read of the start page", e))?;
    let name_at = sp::HEADER_SIZE + sp::ENTRY_NAME;
    if moruna_abi::read_u32(&head, sp::MAGIC_OFFSET) != Some(sp::MAGIC)
        || moruna_abi::read_u32(&head, sp::COUNT_OFFSET) != Some(1)
        || moruna_abi::fixed_name(&head[name_at..name_at + 32]) != b"peer"
    {
        return Err(kd2(out, "the start page's contents", Error::BadState));
    }
    match p.start_with(entry, &mut []) {
        Err(Error::BadState) => {}
        Err(e) => return Err(kd2(out, "process_start of a granted process", e)),
        Ok(()) => return Err(kd2(out, "a granted process was started", Error::BadState)),
    }
    drop(p);
    // The other end of the granted channel closes with the process that held it.
    match wait_one(&mine.0, Signals::PEER_CLOSED, now() + ms(5_000)) {
        Ok(s) if s.contains(Signals::PEER_CLOSED) => {}
        Ok(_) => return Err(kd2(out, "the granted channel's peer", Error::BadState)),
        Err(e) => return Err(kd2(out, "the granted channel's peer closing", e)),
    }
    println!(
        out,
        "KD2 grant: ok: a channel granted to a created process under `peer`, its start page at {:#x} naming it; a handle without TRANSFER refused and kept, a second grant and process_start refused, the handle closed with the process",
        page
    );
    Ok(())
}

fn kc1(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "KC1 remote memory: FAIL: {}: {}", what, e.name());
    1
}

/// `result` must be `want`, for KK-T14.
fn kc1_refused<T>(
    out: &moruna_sys::Console,
    what: &str,
    want: Error,
    result: Result<T, Error>,
) -> Result<(), i64> {
    match result {
        Err(e) if e == want => Ok(()),
        Err(e) => Err(kc1(out, what, e)),
        Ok(_) => Err(kc1(out, what, Error::BadState)),
    }
}

/// KK-T14: another process's memory, built and read from outside.
fn remote_memory(out: &moruna_sys::Console, root_process: &Handle) -> Result<(), i64> {
    const AT: u64 = 0x5000_0000;
    let p = Process::create(root_process, "remote").map_err(|e| kc1(out, "process_create", e))?;
    // Zero pages at a chosen address, written and read back through the process.
    let va = p
        .map(None, 0, 4 * 4096, AT, map::READ | map::WRITE)
        .map_err(|e| kc1(out, "process_map of zero pages", e))?;
    if va != AT {
        return Err(kc1(out, "process_map at a chosen address", Error::BadState));
    }
    p.write(AT + 4090, b"across a page")
        .map_err(|e| kc1(out, "process_write", e))?;
    let mut back = [0u8; 13];
    p.read(AT + 4090, &mut back)
        .map_err(|e| kc1(out, "process_read", e))?;
    let mut zero = [0xffu8; 16];
    p.read(AT + 3 * 4096, &mut zero)
        .map_err(|e| kc1(out, "process_read of an untouched page", e))?;
    if &back != b"across a page" || zero != [0; 16] {
        return Err(kc1(out, "the bytes read back", Error::BadState));
    }
    // The range is taken: without FIXED a second mapping is refused, with it replaces.
    kc1_refused(
        out,
        "a second process_map without FIXED",
        Error::BadState,
        p.map(None, 0, 4096, AT, map::READ),
    )?;
    // An object's pages, mapped read only through a handle without WRITE.
    let object = Memory::create(2 * 4096).map_err(|e| kc1(out, "memory_create", e))?;
    {
        let m = object
            .map(0, 2 * 4096, map::WRITE)
            .map_err(|e| kc1(out, "memory_map", e))?;
        m.write(4096, b"object")
            .map_err(|e| kc1(out, "writing the object", e))?;
    }
    let read_only = object
        .0
        .duplicate(Rights::READ | Rights::MAP)
        .map_err(|e| kc1(out, "handle_duplicate", e))?;
    p.map(Some(&read_only), 0, 2 * 4096, AT, map::READ | map::FIXED)
        .map_err(|e| kc1(out, "process_map FIXED of an object", e))?;
    let mut word = [0u8; 6];
    p.read(AT + 4096, &mut word)
        .map_err(|e| kc1(out, "process_read of the object", e))?;
    if &word != b"object" {
        return Err(kc1(
            out,
            "the object's bytes through the process",
            Error::BadState,
        ));
    }
    kc1_refused(
        out,
        "process_write to a read-only page",
        Error::Fault,
        p.write(AT + 4096, b"x"),
    )?;
    kc1_refused(
        out,
        "process_protect writable over a handle without WRITE",
        Error::AccessDenied,
        p.protect(AT, 2 * 4096, map::READ | map::WRITE),
    )?;
    kc1_refused(
        out,
        "process_map writable and executable",
        Error::InvalidArgs,
        p.map(
            None,
            0,
            4096,
            AT + 64 * 4096,
            map::READ | map::WRITE | map::EXECUTE,
        ),
    )?;
    // The zero pages left above the object keep their bytes; protecting them to none
    // and back keeps them too.
    p.protect(AT + 2 * 4096, 2 * 4096, 0)
        .map_err(|e| kc1(out, "process_protect to no access", e))?;
    kc1_refused(
        out,
        "process_read of an inaccessible page",
        Error::Fault,
        p.read(AT + 2 * 4096, &mut word),
    )?;
    p.write(AT + 2 * 4096, b"")
        .map_err(|e| kc1(out, "an empty process_write", e))?;
    p.protect(AT + 2 * 4096, 2 * 4096, map::READ | map::WRITE)
        .map_err(|e| kc1(out, "process_protect back to writable", e))?;
    // A partial unmap: the first object page goes, the second stays.
    p.unmap(AT, 4096)
        .map_err(|e| kc1(out, "a partial process_unmap", e))?;
    kc1_refused(
        out,
        "process_read of an unmapped page",
        Error::Fault,
        p.read(AT, &mut word),
    )?;
    p.read(AT + 4096, &mut word)
        .map_err(|e| kc1(out, "process_read of the page kept", e))?;
    kc1_refused(
        out,
        "process_protect over a hole",
        Error::OutOfRange,
        p.protect(AT, 2 * 4096, map::READ),
    )?;
    // A handle without WRITE reaches nothing.
    let looking = Process(
        p.0.duplicate(Rights::READ)
            .map_err(|e| kc1(out, "handle_duplicate of the process", e))?,
    );
    kc1_refused(
        out,
        "process_write through a read-only handle",
        Error::AccessDenied,
        looking.write(AT + 4096, b"y"),
    )?;
    kc1_refused(
        out,
        "process_map through a read-only handle",
        Error::AccessDenied,
        looking.map(None, 0, 4096, 0, map::READ),
    )?;
    looking
        .read(AT + 4096, &mut word)
        .map_err(|e| kc1(out, "process_read through a read-only handle", e))?;
    println!(
        out,
        "KC1 remote memory: ok: process_map at a chosen address and FIXED, process_write and process_read across pages, process_protect and a partial process_unmap, and every refusal"
    );
    Ok(())
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
        || !ecam.len.is_multiple_of(1 << 20)
        || !ecam.base.is_multiple_of(1 << 20)
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
        moruna_sys::Error::from_raw(raw::memory_create(4096, 4, 0)),
    )?;
    refused_with(
        out,
        "a contiguous object not made for DMA",
        Error::InvalidArgs,
        moruna_sys::Error::from_raw(raw::memory_create(4096, memory_flags::CONTIGUOUS, 0)),
    )?;
    refused_with(
        out,
        "a contiguous object over the bound",
        Error::Limit,
        moruna_sys::Error::from_raw(raw::memory_create(
            memory_flags::CONTIGUOUS_MAX_BYTES + 4096,
            memory_flags::DMA | memory_flags::CONTIGUOUS,
            0,
        )),
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
        || !pages[0].is_multiple_of(4096)
        || !pages[1].is_multiple_of(4096)
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
    // ABI 2.1: a contiguous object's pin gives a run of consecutive device addresses.
    let run = Memory::create_dma_contiguous(3 * 4096)
        .map_err(|e| kb3(out, "a contiguous DMA memory object", e))?;
    let mut addresses = [0u64; 3];
    let (run_pin, n) = Pin::create(&domain, &run.0, true, &mut addresses)
        .map_err(|e| kb3(out, "pinning a contiguous object", e))?;
    if n != 3
        || addresses[0] == 0
        || !addresses[0].is_multiple_of(4096)
        || addresses[1] != addresses[0] + 4096
        || addresses[2] != addresses[0] + 2 * 4096
    {
        return Err(kb3(out, "a contiguous pin's addresses", Error::BadState));
    }
    drop(run_pin);
    match dma_release(&domain) {
        Ok(1) => {}
        Ok(_) => return Err(kb3(out, "a contiguous pin's quarantine", Error::BadState)),
        Err(e) => return Err(kb3(out, "dma_release of a contiguous pin", e)),
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
