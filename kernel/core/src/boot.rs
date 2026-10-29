//! Boot (SDD 20, f.1): from OSTD's entry to the manifest's processes running.
//!
//! The kernel reads the initramfs OSTD was handed, finds the manifest in it, creates
//! the channels it declares, and starts each process it lists from its ELF with the
//! handles it lists. Only the first process, the supervisor, may receive the root
//! resources. Any refusal powers the machine off with the reason on the console:
//! a kernel that cannot start what it was told to start does not run half of it.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use moruna_abi::{
    ABI_MAJOR, ABI_MINOR, ResourceKind,
    manifest::{Manifest, Source, source_rights},
};
use moruna_loader::cpio;
use ostd::{
    early_println,
    power::{ExitCode, poweroff},
};

use crate::{
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
            poweroff(ExitCode::Failure);
        }
    }
}

fn start() -> Result<usize, String> {
    crate::sched::init()?;
    crate::clock::init()?;
    ostd::task::inject_post_schedule_handler(process::activate_current);
    early_println!("kk: {} CPUs online", ostd::cpu::num_cpus());
    process::boot_begins();
    start_manifest()
}

/// The PCI configuration space the firmware describes (ACPI MCFG, its first segment),
/// from its first bus to the end of its last: the range `root.ecam` covers.
fn ecam() -> Result<core::ops::Range<u64>, String> {
    let region = ostd::arch::kernel::ACPI_INFO
        .get()
        .and_then(|info| info.pci_ecam_region.as_ref())
        .ok_or("root.ecam: the firmware describes no PCI configuration space (ACPI MCFG)")?;
    let start = region.base_address + ((region.bus_start as u64) << 20);
    let end = region.base_address + ((region.bus_end as u64 + 1) << 20);
    if end <= start {
        return Err(String::from("root.ecam: the MCFG's bus range is empty"));
    }
    Ok(start..end)
}

fn start_manifest() -> Result<usize, String> {
    let initramfs = ostd::boot::boot_info()
        .initramfs
        .ok_or("the bootloader passed no initramfs")?;
    let files = cpio::files(initramfs).map_err(|e| format!("initramfs: {}", e.describe()))?;
    let text = cpio::find(&files, moruna_abi::manifest::FILE_NAME)
        .ok_or("the initramfs holds no `manifest`")?;
    let manifest = Manifest::parse(text).map_err(|e| e.to_string())?;

    let mut ends: Vec<[Option<Arc<Endpoint>>; 2]> = manifest
        .channels
        .iter()
        .map(|_| {
            let (a, b) = channel::create();
            [Some(a), Some(b)]
        })
        .collect();
    let console = Arc::new(Console);
    let root = |kind| Object::Resource(Arc::new(Resource::root(kind)));

    for (index, spec) in manifest.processes.iter().enumerate() {
        let elf = cpio::find(&files, &spec.path).ok_or_else(|| {
            format!(
                "process {}: the initramfs holds no {}",
                spec.name, spec.path
            )
        })?;
        let process = Process::new(spec.name.as_bytes(), index == 0)
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
                Source::RootProcess => root(ResourceKind::Process),
                Source::RootDevice => root(ResourceKind::DeviceMemory),
                Source::RootInterrupt => root(ResourceKind::Interrupt),
                Source::RootMsi => root(ResourceKind::Msi),
                Source::RootDma => root(ResourceKind::Dma),
                Source::RootEcam => Object::Resource(Arc::new(Resource::new(
                    ResourceKind::DeviceMemory,
                    ecam().map_err(|e| format!("process {}: {e}", spec.name))?,
                ))),
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
                    let object = MemoryObject::from_bytes(bytes).map_err(|e| {
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
