//! The machine a boot assembles, independent of the hypervisor: guest memory, the devices on
//! their buses with their interrupt lines, the kernel and initramfs in memory, the boot
//! tables, the status port and the resize controller. The Linux-only KVM code
//! ([`crate::kvm`]) creates the VM and vCPUs around a [`Machine`] and runs it.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vm_memory::{Bytes, GuestAddress};

use crate::config::VmConfig;
use crate::control::{Controller, Status};
use crate::devices::block::Block;
use crate::devices::cpu_hotplug::{self, CpuHotplug};
use crate::devices::legacy::{
    ACPI_PM_BYTES, ACPI_PM_PORT, AcpiPm, I8042, I8042_COMMAND_PORT, I8042_DATA_PORT,
};
use crate::devices::mem::{MadviseBacking, VirtioMem};
use crate::devices::mmio::MmioTransport;
use crate::devices::serial::{self, ConsoleLog, ConsoleOut, SerialConsole};
use crate::devices::virtio::Interrupt;
use crate::devices::vsock::VirtioVsock;
use crate::devices::{Bus, Irq, Mem, StopReason, StopSignal};
use crate::error::{EXIT_GUEST_PANIC, EXIT_MONITOR, EXIT_NO_CODE, Result, VmmError};
use crate::image::GuestImage;
use crate::layout::{Arch, Layout, Slot, arm, x86};
use crate::sys::{PollFd, poll};

/// The guest port Moruna listens on (`moruna serve --listen vsock://-1:5000`, MH 4.8.3).
pub const GUEST_AGENT_PORT: u32 = 5000;
/// The host port the guest's init reports Moruna's exit code on (`exit <code>\n`). The
/// monitor serves it in-process; it is the monitor's own channel, not the host protocol.
pub const STATUS_PORT: u32 = 1024;
/// Longest status line read.
pub const STATUS_LINE_MAX: usize = 64;
/// The block device id of the image's root filesystem.
pub const ROOTFS_ID: &str = "moruna-root";
/// How long the vsock backend waits in one poll; bounds how late it notices a stop.
pub const BACKEND_POLL: Duration = Duration::from_millis(100);

/// Opens an interrupt line for a GSI: an irqfd under KVM, a counter in tests.
pub type IrqFactory<'a> = dyn FnMut(u32) -> Result<Arc<dyn Irq>> + 'a;

/// `exit <code>` with a code of 0 to 255, else nothing.
pub fn parse_status_line(line: &str) -> Option<i32> {
    let code: i32 = line.trim().strip_prefix("exit ")?.trim().parse().ok()?;
    (0..=255).contains(&code).then_some(code)
}

/// The monitor's end of the status port.
#[derive(Clone, Default)]
pub struct StatusPort {
    code: Arc<Mutex<Option<i32>>>,
}

impl StatusPort {
    /// The code the guest reported, if it did.
    pub fn reported(&self) -> Option<i32> {
        *self.code.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Accept one connection on `stream`: read one line and record the code (the first
    /// valid report wins; anything else is ignored).
    pub fn serve(&self, mut stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = Vec::new();
        let mut b = [0u8];
        while buf.len() < STATUS_LINE_MAX {
            match stream.read(&mut b) {
                Ok(1) if b[0] != b'\n' => buf.push(b[0]),
                _ => break,
            }
        }
        if let Some(code) = parse_status_line(&String::from_utf8_lossy(&buf)) {
            let mut slot = self.code.lock().unwrap_or_else(|p| p.into_inner());
            slot.get_or_insert(code);
        }
    }

    /// A vsock connector serving this port on a fresh thread per connection.
    pub fn connector(&self) -> crate::devices::vsock::muxer::Connector {
        let me = self.clone();
        Box::new(move || {
            let (ours, theirs) = UnixStream::pair()?;
            let me = me.clone();
            std::thread::Builder::new()
                .name("vmm-status".into())
                .spawn(move || me.serve(ours))?;
            Ok(theirs)
        })
    }
}

/// The exit code `moruna-vmm boot` ends with: a panic is 6 whatever else happened; a monitor
/// failure is 8; otherwise the code the guest reported, or 7 when it reported none.
pub fn exit_code(reason: &StopReason, reported: Option<i32>, panicked: bool) -> i32 {
    if panicked || *reason == StopReason::Crash {
        return EXIT_GUEST_PANIC;
    }
    match reason {
        StopReason::MonitorError(_) => EXIT_MONITOR,
        _ => reported.unwrap_or(EXIT_NO_CODE),
    }
}

/// Guest memory for `layout`: boot RAM and the hot-plug region, anonymous and
/// `MAP_NORESERVE`, so unplugged memory costs the host nothing.
pub fn guest_memory(layout: &Layout) -> Result<Mem> {
    let ranges: Vec<(GuestAddress, usize)> = layout
        .memory_ranges()
        .into_iter()
        .map(|(s, l)| (GuestAddress(s), l as usize))
        .collect();
    Mem::from_ranges(&ranges)
        .map_err(|e| VmmError::config("--memory", format!("cannot map guest memory: {e}")))
}

/// Read `path` into guest memory at `addr`.
pub fn load_file(mem: &Mem, path: &std::path::Path, addr: u64) -> Result<u64> {
    let mut f = File::open(path).map_err(|e| VmmError::Image {
        path: path.display().to_string(),
        msg: e.to_string(),
    })?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).map_err(|e| VmmError::Image {
        path: path.display().to_string(),
        msg: e.to_string(),
    })?;
    mem.write_slice(&data, GuestAddress(addr))
        .map_err(|e| VmmError::Image {
            path: path.display().to_string(),
            msg: format!("does not fit in guest memory at {addr:#x}: {e}"),
        })?;
    Ok(data.len() as u64)
}

/// The `virtio_mmio.device=` parameter the x86 kernel discovers a device by.
pub fn virtio_mmio_param(slot: &Slot) -> String {
    format!("virtio_mmio.device=4K@{:#x}:{}", slot.addr, slot.gsi)
}

/// Everything a boot assembled.
pub struct Machine {
    /// The architecture.
    pub arch: Arch,
    /// The memory map.
    pub layout: Layout,
    /// Guest memory.
    pub mem: Mem,
    /// The MMIO bus.
    pub mmio: Bus,
    /// The port-IO bus (x86_64).
    pub pio: Bus,
    /// The virtio devices' places, in slot order.
    pub virtio: Vec<Slot>,
    /// The vsock device.
    pub vsock: Arc<Mutex<MmioTransport<VirtioVsock>>>,
    /// The virtio-mem device, when the guest can grow.
    pub vmem: Option<Arc<Mutex<MmioTransport<VirtioMem>>>>,
    /// The CPU hot-plug controller (x86_64, when `--cpus-max` exceeds `--cpus`).
    pub hotplug: Option<Arc<Mutex<CpuHotplug>>>,
    /// The Generic Event Device's line (x86_64 CPU hot-add).
    pub ged: Option<Arc<dyn Irq>>,
    /// The console's log.
    pub console: Arc<ConsoleLog>,
    /// Why the guest stopped, once it has.
    pub stop: Arc<StopSignal>,
    /// The status port.
    pub status: StatusPort,
    /// The kernel command line.
    pub cmdline: String,
    /// The resolved image.
    pub image: GuestImage,
    /// Boot and maximum sizes.
    pub config: VmConfig,
}

impl Machine {
    /// Assemble the machine for `config` on `arch`; `irq` opens each interrupt line and
    /// `console` receives the guest's console.
    pub fn build(
        arch: Arch,
        config: &VmConfig,
        image: GuestImage,
        irq: &mut IrqFactory<'_>,
        console: Box<dyn Write + Send>,
    ) -> Result<Machine> {
        config.validate()?;
        if arch == Arch::Aarch64 && config.cpus_max > config.cpus {
            return Err(VmmError::config(
                "--cpus-max",
                "vCPU hot-add on aarch64 needs ACPI, which a device-tree boot does not have; \
                 boot aarch64 guests with --cpus-max equal to --cpus",
            ));
        }
        let layout = Layout::for_config(arch, config)?;
        let mem = guest_memory(&layout)?;
        let stop = Arc::new(StopSignal::default());
        let log = ConsoleLog::new();
        let mut mmio = Bus::default();
        let mut pio = Bus::default();

        // The console.
        let serial_gsi = match arch {
            Arch::X86_64 => serial::COM1_IRQ,
            Arch::Aarch64 => arm::SERIAL_GSI,
        };
        let uart = Arc::new(Mutex::new(SerialConsole::new(
            irq(serial_gsi)?,
            ConsoleOut::new(console, log.clone()),
        )));
        match arch {
            Arch::X86_64 => pio.insert(serial::COM1_PORT, serial::WINDOW_BYTES, uart)?,
            Arch::Aarch64 => mmio.insert(arm::SERIAL_ADDR, 0x1000, uart)?,
        }

        // virtio devices, in slot order: vsock, mem, the root filesystem, the disks.
        let mut virtio = Vec::new();
        let next = |virtio: &mut Vec<Slot>| -> Result<Slot> {
            let s = layout.virtio_slot(virtio.len())?;
            virtio.push(s);
            Ok(s)
        };

        let status = StatusPort::default();
        let slot = next(&mut virtio)?;
        let mut vs = VirtioVsock::new(config.vsock.cid, &config.vsock.uds_path)?;
        vs.add_connector(STATUS_PORT, status.connector());
        let vsock = Arc::new(Mutex::new(MmioTransport::new(
            vs,
            mem.clone(),
            Interrupt::new(irq(slot.gsi)?),
        )?));
        mmio.insert(slot.addr, crate::layout::MMIO_WINDOW, vsock.clone())?;

        let vmem = match layout.hotplug {
            Some((addr, len)) => {
                let slot = next(&mut virtio)?;
                let dev = VirtioMem::new(addr, len, Box::new(MadviseBacking))?;
                let t = Arc::new(Mutex::new(MmioTransport::new(
                    dev,
                    mem.clone(),
                    Interrupt::new(irq(slot.gsi)?),
                )?));
                mmio.insert(slot.addr, crate::layout::MMIO_WINDOW, t.clone())?;
                Some(t)
            }
            None => None,
        };

        let mut disks: Vec<(crate::config::DiskConfig, String)> = Vec::new();
        if let Some(root) = &image.rootfs {
            disks.push((
                crate::config::DiskConfig {
                    path: root.clone(),
                    read_only: true,
                },
                ROOTFS_ID.to_string(),
            ));
        }
        for (i, d) in config.disks.iter().enumerate() {
            disks.push((d.clone(), format!("moruna-disk-{i}")));
        }
        for (d, id) in &disks {
            let slot = next(&mut virtio)?;
            let blk = Block::open(d, id)?;
            let t = Arc::new(Mutex::new(MmioTransport::new(
                blk,
                mem.clone(),
                Interrupt::new(irq(slot.gsi)?),
            )?));
            mmio.insert(slot.addr, crate::layout::MMIO_WINDOW, t)?;
        }

        // x86_64: the stop ports and CPU hot-add.
        let (hotplug, ged) = match arch {
            Arch::X86_64 => {
                pio.insert(
                    I8042_DATA_PORT,
                    I8042_COMMAND_PORT - I8042_DATA_PORT + 1,
                    Arc::new(Mutex::new(I8042::new(stop.clone()))),
                )?;
                pio.insert(
                    ACPI_PM_PORT,
                    ACPI_PM_BYTES,
                    Arc::new(Mutex::new(AcpiPm::new(stop.clone()))),
                )?;
                let hp = Arc::new(Mutex::new(CpuHotplug::new(config.cpus, config.cpus_max)));
                mmio.insert(x86::CPU_HOTPLUG_ADDR, cpu_hotplug::WINDOW_BYTES, hp.clone())?;
                (Some(hp), Some(irq(x86::GED_GSI)?))
            }
            Arch::Aarch64 => (None, None),
        };

        let mut cmdline = crate::layout::cmdline(arch, image.rootfs.is_some());
        if arch == Arch::X86_64 {
            for s in &virtio {
                cmdline.push(' ');
                cmdline.push_str(&virtio_mmio_param(s));
            }
        }

        Ok(Machine {
            arch,
            layout,
            mem,
            mmio,
            pio,
            virtio,
            vsock,
            vmem,
            hotplug,
            ged,
            console: log,
            stop,
            status,
            cmdline,
            image,
            config: config.clone(),
        })
    }

    /// The control-socket view of this machine.
    pub fn controller(&self) -> VmController {
        VmController {
            vmem: self.vmem.clone(),
            hotplug: self.hotplug.clone(),
            ged: self.ged.clone(),
            boot_memory: self.config.memory_bytes,
            memory_max: self.config.memory_max_bytes,
            cpus_boot: self.config.cpus,
            cpus_max: self.config.cpus_max,
            stop: self.stop.clone(),
        }
    }

    /// Write the initramfs (if any) and the boot tables: the ACPI tables, GDT and page tables
    /// on x86_64 (the zero page is written by the loader, which knows the kernel's header);
    /// the device tree on aarch64. `kernel_end` is the first byte after the loaded kernel.
    /// Returns the initramfs range.
    pub fn write_boot_data(
        &self,
        kernel_end: u64,
        gic: crate::fdt::GicVersion,
    ) -> Result<Option<(u64, u64)>> {
        let initrd = match &self.image.initramfs {
            Some(p) => {
                let size = std::fs::metadata(p)
                    .map_err(|e| VmmError::Image {
                        path: p.display().to_string(),
                        msg: e.to_string(),
                    })?
                    .len();
                let addr = self.layout.initrd_addr(size, kernel_end)?;
                load_file(&self.mem, p, addr)?;
                Some((addr, size))
            }
            None => None,
        };
        match self.arch {
            Arch::X86_64 => {
                crate::boot::write_x86_boot_tables(&self.mem)?;
                for (addr, bytes) in crate::acpi::tables(self.config.cpus, self.config.cpus_max)? {
                    self.mem
                        .write_slice(&bytes, GuestAddress(addr))
                        .map_err(|e| VmmError::device("acpi", e.to_string()))?;
                }
                let mut c = self.cmdline.as_bytes().to_vec();
                c.push(0);
                if c.len() > 4096 {
                    return Err(VmmError::config(
                        "--disk",
                        "kernel command line exceeds 4 KiB",
                    ));
                }
                self.mem
                    .write_slice(&c, GuestAddress(x86::CMDLINE_ADDR))
                    .map_err(|e| VmmError::device("cmdline", e.to_string()))?;
            }
            Arch::Aarch64 => {
                let blob = crate::fdt::build(&crate::fdt::FdtSpec {
                    layout: &self.layout,
                    cpus: self.config.cpus,
                    gic,
                    cmdline: &self.cmdline,
                    initrd,
                    virtio: &self.virtio,
                })?;
                self.mem
                    .write_slice(&blob, GuestAddress(self.layout.fdt_addr()))
                    .map_err(|e| VmmError::device("fdt", e.to_string()))?;
            }
        }
        Ok(initrd)
    }

    /// One iteration of the vsock backend: poll the host side for up to `timeout` and serve
    /// it, interrupting the guest when buffers were used. While the guest has not activated
    /// the device the host side is left alone (connections wait in the listen backlog).
    pub fn vsock_backend_once(&self, timeout: Duration) -> Result<()> {
        let set = {
            let t = self.vsock.lock().unwrap_or_else(|p| p.into_inner());
            if !t.is_active() {
                None
            } else {
                Some(t.device().poll_set())
            }
        };
        let Some(set) = set else {
            std::thread::sleep(timeout);
            return Ok(());
        };
        let fds: Vec<PollFd> = set.iter().map(|(p, _)| *p).collect();
        let ready = poll(&fds, timeout).map_err(|e| VmmError::io("poll", "vsock", &e))?;
        let pairs: Vec<_> = set.iter().map(|(_, t)| *t).zip(ready).collect();
        if !pairs.iter().any(|(_, r)| r.read || r.write || r.error) {
            return Ok(());
        }
        let mut t = self.vsock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(true) = t.with_active(|d, q, m| d.backend_ready(&pairs, q, m)) {
            t.interrupt().signal_used()?;
        }
        Ok(())
    }

    /// Run the vsock backend until the guest stops.
    pub fn vsock_backend(&self) {
        while !self.stop.is_stopped() {
            if let Err(e) = self.vsock_backend_once(BACKEND_POLL) {
                self.stop.request(StopReason::MonitorError(e.to_string()));
            }
        }
    }

    /// The exit code for how the guest stopped; writes the console's tail to `err` when the
    /// guest panicked.
    pub fn finish(&self, err: &mut dyn Write) -> i32 {
        let reason = self
            .stop
            .reason()
            .unwrap_or(StopReason::MonitorError("no stop reason".into()));
        let panicked = self.console.panicked();
        let code = exit_code(&reason, self.status.reported(), panicked);
        if code == EXIT_GUEST_PANIC {
            let _ = writeln!(err, "moruna-vmm: guest kernel panic; console follows");
            let _ = err.write_all(&self.console.tail());
            let _ = writeln!(err);
        } else if let StopReason::MonitorError(m) = &reason {
            let _ = writeln!(err, "moruna-vmm: {m}");
        } else if code == EXIT_NO_CODE {
            let _ = writeln!(
                err,
                "moruna-vmm: guest stopped ({reason:?}) without reporting an exit code"
            );
        }
        code
    }
}

/// The control socket's view of a running machine.
pub struct VmController {
    vmem: Option<Arc<Mutex<MmioTransport<VirtioMem>>>>,
    hotplug: Option<Arc<Mutex<CpuHotplug>>>,
    ged: Option<Arc<dyn Irq>>,
    boot_memory: u64,
    memory_max: u64,
    cpus_boot: u32,
    cpus_max: u32,
    stop: Arc<StopSignal>,
}

impl Controller for VmController {
    fn resize_memory(&self, bytes: u64) -> Result<Status> {
        if bytes < self.boot_memory {
            return Err(VmmError::Control(format!(
                "{bytes} is below the {} bytes of boot memory; only hot-plugged memory can be removed",
                self.boot_memory
            )));
        }
        if bytes > self.memory_max {
            return Err(VmmError::Control(format!(
                "{bytes} exceeds --memory-max {}",
                self.memory_max
            )));
        }
        let Some(vmem) = &self.vmem else {
            return Err(VmmError::Control(
                "the guest was booted with --memory-max equal to --memory".into(),
            ));
        };
        {
            let mut t = vmem.lock().unwrap_or_else(|p| p.into_inner());
            t.device_mut().set_requested(bytes - self.boot_memory)?;
            t.config_changed()?;
        }
        Ok(self.status())
    }

    fn resize_cpus(&self, cpus: u32) -> Result<Status> {
        let Some(hp) = &self.hotplug else {
            if cpus == self.cpus_boot {
                return Ok(self.status());
            }
            return Err(VmmError::Control(
                "vCPU hot-add is not available for this guest".into(),
            ));
        };
        let added = hp.lock().unwrap_or_else(|p| p.into_inner()).grow_to(cpus)?;
        if let (false, Some(ged)) = (added.is_empty(), &self.ged) {
            ged.trigger().map_err(|e| VmmError::io("irq", "ged", &e))?;
        }
        Ok(self.status())
    }

    fn status(&self) -> Status {
        let (requested, plugged) = self.vmem.as_ref().map_or((0, 0), |v| {
            let t = v.lock().unwrap_or_else(|p| p.into_inner());
            (t.device().requested_size(), t.device().plugged_size())
        });
        Status {
            memory_bytes: self.boot_memory + requested,
            memory_plugged_bytes: self.boot_memory + plugged,
            memory_max_bytes: self.memory_max,
            cpus: self.hotplug.as_ref().map_or(self.cpus_boot, |h| {
                h.lock().unwrap_or_else(|p| p.into_inner()).enabled()
            }),
            cpus_max: self.cpus_max,
            stopped: self.stop.is_stopped(),
        }
    }

    fn stop(&self) {
        self.stop.request(StopReason::Killed);
    }
}

#[cfg(test)]
mod tests;
