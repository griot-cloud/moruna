//! Boot (SDD 20, f.1): from OSTD's entry to the manifest's processes running.
//!
//! The kernel reads the initramfs OSTD was handed, finds the manifest in it, creates
//! the channels it declares, and starts each process it lists from its ELF with the
//! handles it lists. Only the first process, the supervisor, may receive the root
//! resources. Any refusal powers the machine off with the reason on the console:
//! a kernel that cannot start what it was told to start does not run half of it.
//!
//! The node's console (`architecture/moruna-os-system.md`, section 1B) has the core's line,
//! `moruna: core <version> cpus=<n> memory=<bytes>`, once every CPU is online, and
//! `moruna: fatal core: <reason>` for a refused boot or a panic, each before the power-off.
//!
//! Before anything is made the kernel sizes the machine's account (SDD 20 f.18): the
//! free memory its frame allocator holds, less the kernel's own reserve. A manifest
//! process with a `memory` line gets an account of its own carved out of it; every other
//! is charged to the machine's account, as is what the boot makes for no one process
//! (the channels, the root resources).

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use core::sync::atomic::{AtomicBool, Ordering};

use moruna_abi::{
    ABI_MAJOR, ABI_MINOR, ResourceKind,
    boot_line::{self, Core, Fatal, Panic},
    manifest::{Manifest, Source, source_rights},
    resource_range,
};
use moruna_loader::cpio;
use ostd::{
    boot::memory_region::MemoryRegionType,
    early_println,
    power::{ExitCode, poweroff},
};

use crate::{
    account::Account,
    channel::{self, Endpoint},
    memory::MemoryObject,
    object::{Console, Handle, Object, Resource},
    process::{self, Process},
};

pub fn boot() {
    match start() {
        Ok(count) => {
            early_println!(
                "kk: moruna microkernel, abi {}.{}: started {} processes from the manifest",
                ABI_MAJOR,
                ABI_MINOR,
                count
            );
            // Only now may the last process to end power the machine off.
            process::boot_done();
        }
        Err(reason) => {
            early_println!("kk: boot refused: {}", reason);
            early_println!(
                "{}",
                Fatal {
                    stage: boot_line::CORE,
                    reason: &reason
                }
            );
            poweroff(ExitCode::Failure);
        }
    }
}

/// A panic in kernel mode is a fatal error of the core: its line, before OSTD's handler
/// prints the panic whole and powers the machine off. It is said once, whichever CPU
/// panics first and whatever panics after it, the writing of this line included.
pub fn panicked(info: &core::panic::PanicInfo) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if SAID.swap(true, Ordering::AcqRel) {
        return;
    }
    early_println!(
        "{}",
        Fatal {
            stage: boot_line::CORE,
            reason: Panic {
                location: info.location().map(|at| (at.file(), at.line())),
                message: info.message(),
            }
        }
    );
}

/// The usable RAM in the boot memory map: the bytes of every region OSTD's map of the
/// bootloader's gives the type `Usable`.
fn usable_memory() -> u64 {
    ostd::boot::boot_info()
        .memory_regions
        .iter()
        .filter(|region| region.typ() == MemoryRegionType::Usable)
        .map(|region| region.len() as u64)
        .sum()
}

fn start() -> Result<usize, String> {
    use ostd::arch::cpu::extension::{IsaExtensions, has_extensions};
    // OSTD logs at the debug level by default, a line on the serial port for every FPU
    // save and restore; the kernel's own lines are its console's (SDD 20 j).
    ostd::log::set_max_level(ostd::log::LevelFilter::Info);
    // A thread's FS base is saved and restored with RDFSBASE and WRFSBASE.
    if !has_extensions(IsaExtensions::FSGSBASE) {
        return Err(String::from(
            "the CPU has no FSGSBASE, which keeping each thread's FS base needs",
        ));
    }
    crate::sched::init()?;
    crate::clock::init()?;
    ostd::task::inject_pre_schedule_handler(process::deactivate_current);
    ostd::task::inject_post_schedule_handler(process::activate_current);
    // stream B1 hook: a kernel copy that faults on a user page (a bad pointer, a page not
    // touched yet, or one shared copy-on-write since a clone) resumes at its exception
    // table's recovery, so `usercopy` answers `Fault`, or makes the page ready and copies
    // once more. With no handler OSTD panics.
    ostd::mm::fault::inject_user_page_fault_handler(|_| Err(()));
    early_println!("kk: {} CPUs online", ostd::cpu::num_cpus());
    early_println!(
        "{}",
        Core {
            version: env!("CARGO_PKG_VERSION"),
            cpus: ostd::cpu::num_cpus(),
            memory: usable_memory(),
        }
    );
    #[cfg(feature = "probe")]
    osdk_frame_allocator::probe_shared_pool();
    let machine = machine_account()?;
    process::boot_begins();
    start_manifest(&machine)
}

/// The machine's account: the free memory, less the reserve, in whole pages.
fn machine_account() -> Result<Arc<Account>, String> {
    let free = osdk_frame_allocator::load_total_free_size() as u64;
    let limit = moruna_quota::machine_limit(free);
    let kept = free - limit;
    if limit == 0 {
        return Err(format!(
            "{free} bytes of free memory leave nothing for processes beside the kernel's reserve of {kept}"
        ));
    }
    early_println!(
        "kk: memory: {} bytes for processes, {} kept by the kernel",
        limit,
        free - limit
    );
    Ok(Account::machine(limit))
}

/// The PCI configuration space the firmware describes (ACPI MCFG, its first segment),
/// from its first bus to the end of its last: the range `root.ecam` covers. A machine
/// whose firmware describes none, as a Hyper-V Generation 2 machine (the system design,
/// section 1F), has no PCI bus: `root.ecam` covers nothing, and the boot goes on.
fn ecam() -> Result<core::ops::Range<u64>, String> {
    let Some(region) = ostd::arch::kernel::ACPI_INFO
        .get()
        .and_then(|info| info.pci_ecam_region.as_ref())
    else {
        return Ok(resource_range::NO_ECAM);
    };
    resource_range::ecam(region.base_address, region.bus_start, region.bus_end)
        .ok_or_else(|| String::from("root.ecam: the MCFG's bus range is empty"))
}

fn start_manifest(machine: &Arc<Account>) -> Result<usize, String> {
    let initramfs = ostd::boot::boot_info()
        .initramfs
        .ok_or("the bootloader passed no initramfs")?;
    let files = cpio::files(initramfs).map_err(|e| format!("initramfs: {}", e.describe()))?;
    let text = cpio::find(&files, moruna_abi::manifest::FILE_NAME)
        .ok_or("the initramfs holds no `manifest`")?;
    let manifest = Manifest::parse(text).map_err(|e| e.to_string())?;

    let mut ends: Vec<[Option<Arc<Endpoint>>; 2]> = Vec::new();
    for name in &manifest.channels {
        let (a, b) =
            channel::create(machine).map_err(|e| format!("channel {name}: {}", e.name()))?;
        ends.push([Some(a), Some(b)]);
    }
    let console = Arc::new(Console);
    let root = |kind| -> Result<Object, String> {
        Resource::root(kind, machine)
            .map(|r| Object::Resource(Arc::new(r)))
            .map_err(|e| format!("a root resource: {}", e.name()))
    };

    for (index, spec) in manifest.processes.iter().enumerate() {
        let elf = cpio::find(&files, &spec.path).ok_or_else(|| {
            format!(
                "process {}: the initramfs holds no {}",
                spec.name, spec.path
            )
        })?;
        let account = match spec.memory {
            Some(limit) => machine
                .carve(spec.name.as_bytes(), limit, machine)
                .map_err(|e| {
                    format!(
                        "process {}: an account of {limit} bytes from the machine's: {}",
                        spec.name,
                        e.name()
                    )
                })?,
            None => machine.clone(),
        };
        let process = Process::new(spec.name.as_bytes(), index == 0, &account)
            .map_err(|e| format!("process {}: {}", spec.name, e.name()))?;
        let entry = process.load_image(elf).map_err(|e| {
            format!(
                "process {}: {} is not loadable: {}",
                spec.name,
                spec.path,
                e.name()
            )
        })?;
        let mut handles = Vec::new();
        for h in &spec.handles {
            let object = match &h.source {
                Source::Console => Object::Console(console.clone()),
                Source::RootProcess => root(ResourceKind::Process)?,
                Source::RootDevice => root(ResourceKind::DeviceMemory)?,
                Source::RootInterrupt => root(ResourceKind::Interrupt)?,
                Source::RootMsi => root(ResourceKind::Msi)?,
                Source::RootDma => root(ResourceKind::Dma)?,
                Source::RootEcam => Object::Resource(Arc::new(
                    Resource::new(
                        ResourceKind::DeviceMemory,
                        ecam().map_err(|e| format!("process {}: {e}", spec.name))?,
                        machine,
                    )
                    .map_err(|e| format!("process {}: root.ecam: {}", spec.name, e.name()))?,
                )),
                Source::ChannelEnd { channel, end } => {
                    let endpoint = ends
                        .get_mut(*channel)
                        .and_then(|pair| pair[*end as usize].take())
                        .ok_or_else(|| {
                            format!("process {}: channel endpoint given twice", spec.name)
                        })?;
                    Object::Channel(endpoint)
                }
                Source::File { path } => {
                    let bytes = cpio::find(&files, path).ok_or_else(|| {
                        format!("process {}: the initramfs holds no {}", spec.name, path)
                    })?;
                    // The process's: it is charged to the process's account.
                    let object = MemoryObject::from_bytes(bytes, &account).map_err(|e| {
                        format!("process {}: file {}: {}", spec.name, path, e.name())
                    })?;
                    Object::Memory(Arc::new(object))
                }
            };
            let rights = h.rights.unwrap_or_else(|| source_rights(&h.source));
            handles.push((Handle { object, rights }, h.slot.as_bytes().to_vec()));
        }
        process
            .start(entry, handles)
            .map_err(|e| format!("process {}: could not start: {}", spec.name, e.name()))?;
    }
    Ok(manifest.processes.len())
}
