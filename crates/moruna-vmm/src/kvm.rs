//! The KVM half of the monitor (Linux only): the VM, its memory slots, the interrupt
//! controller, the vCPUs and their run loops, around a [`Machine`] that holds everything else.
//!
//! This is the one module that needs `/dev/kvm`; its behaviour is exercised on the reference
//! host by the tests in `tests/kvm_boot.rs`, which are ignored elsewhere.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use kvm_bindings::*;
#[cfg(target_arch = "aarch64")]
use kvm_ioctls::VmFd;
use kvm_ioctls::{Cap, Kvm, VcpuExit, VcpuFd};
use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};
use vmm_sys_util::signal::{Killable, SIGRTMIN, register_signal_handler};

use crate::config::VmConfig;
use crate::control::ControlServer;
use crate::devices::{Bus, Irq, StopReason, StopSignal};
use crate::error::{Result, VmmError};
use crate::fdt::GicVersion;
use crate::layout::Arch;
use crate::machine::Machine;

/// The KVM API version this monitor speaks (stable since Linux 2.6.22).
pub const KVM_API_VERSION: i32 = 12;
/// How often a stopping monitor re-signals vCPU threads still inside `KVM_RUN`.
pub const KICK_INTERVAL: Duration = Duration::from_millis(10);

fn hv(op: &'static str) -> impl Fn(kvm_ioctls::Error) -> VmmError {
    move |e| VmmError::Hypervisor {
        op,
        msg: e.to_string(),
    }
}

/// An interrupt line: an eventfd registered with `KVM_IRQFD`.
struct EventIrq(EventFd);

impl Irq for EventIrq {
    fn trigger(&self) -> std::io::Result<()> {
        self.0.write(1)
    }
}

extern "C" fn kick_handler(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {}

/// Open `/dev/kvm` and check the capabilities the monitor uses.
pub fn open() -> Result<Kvm> {
    let kvm = Kvm::new().map_err(|e| VmmError::kvm_open(e.errno()))?;
    if kvm.get_api_version() != KVM_API_VERSION {
        return Err(VmmError::NoKvm(format!(
            "API version {} is not {KVM_API_VERSION}",
            kvm.get_api_version()
        )));
    }
    for (cap, name) in [
        (Cap::Irqfd, "KVM_CAP_IRQFD"),
        (Cap::UserMemory, "KVM_CAP_USER_MEMORY"),
    ] {
        if !kvm.check_extension(cap) {
            return Err(VmmError::NoKvm(format!("the host lacks {name}")));
        }
    }
    Ok(kvm)
}

/// Boot the guest `config` describes and run it to the end; returns the exit code.
pub fn run(config: &VmConfig) -> Result<i32> {
    config.validate()?;
    let image = crate::image::resolve(&config.image)?;
    let kvm = open()?;
    if config.cpus_max as usize > kvm.get_max_vcpus() {
        return Err(VmmError::config(
            "--cpus-max",
            format!("this host's KVM allows {} vCPUs", kvm.get_max_vcpus()),
        ));
    }
    // Bind the control socket first: a path in use is a configuration error, reported before
    // anything boots.
    let control = ControlServer::bind(&config.control_socket)?;
    let vm = kvm.create_vm().map_err(hv("KVM_CREATE_VM"))?;
    let arch = Arch::HOST;
    let vcpu_count = match arch {
        Arch::X86_64 => config.cpus_max,
        Arch::Aarch64 => config.cpus,
    };

    #[cfg(target_arch = "x86_64")]
    let (vcpus, gic) = {
        vm.set_tss_address(0xfffb_d000)
            .map_err(hv("KVM_SET_TSS_ADDR"))?;
        vm.create_irq_chip().map_err(hv("KVM_CREATE_IRQCHIP"))?;
        let pit = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        vm.create_pit2(pit).map_err(hv("KVM_CREATE_PIT2"))?;
        let vcpus = (0..vcpu_count)
            .map(|id| vm.create_vcpu(id as u64).map_err(hv("KVM_CREATE_VCPU")))
            .collect::<Result<Vec<_>>>()?;
        (vcpus, None::<(kvm_ioctls::DeviceFd, GicVersion)>)
    };
    #[cfg(target_arch = "aarch64")]
    let (vcpus, gic) = {
        let vcpus = arm::create_vcpus(&vm, vcpu_count)?;
        let gic = arm::create_gic(&vm, vcpu_count)?;
        (vcpus, Some(gic))
    };

    let mut irq = |gsi: u32| -> Result<Arc<dyn Irq>> {
        let fd = EventFd::new(EFD_NONBLOCK).map_err(|e| VmmError::io("eventfd", "irq", &e))?;
        vm.register_irqfd(&fd, gsi).map_err(hv("KVM_IRQFD"))?;
        Ok(Arc::new(EventIrq(fd)))
    };
    let machine = Machine::build(arch, config, image, &mut irq, Box::new(std::io::stdout()))?;

    for (slot, region) in machine.mem.iter().enumerate() {
        let host = machine
            .mem
            .get_host_address(region.start_addr())
            .map_err(|e| VmmError::device("memory", e.to_string()))?;
        let r = kvm_userspace_memory_region {
            slot: slot as u32,
            flags: 0,
            guest_phys_addr: region.start_addr().0,
            memory_size: region.len(),
            userspace_addr: host as u64,
        };
        // SAFETY: the region is a live mapping owned by `machine.mem`, which outlives the VM
        // (both are dropped at the end of this function, the VM first), and the slots do not
        // overlap because the layout's ranges do not.
        unsafe { vm.set_user_memory_region(r) }.map_err(hv("KVM_SET_USER_MEMORY_REGION"))?;
    }

    let loaded = crate::loader::load_kernel(&machine)?;
    let gic_version = gic.as_ref().map_or(GicVersion::V3, |(_, v)| *v);
    let initrd = machine.write_boot_data(loaded.kernel_end, gic_version)?;
    #[cfg(target_arch = "x86_64")]
    {
        crate::loader::write_zero_page(&machine, &loaded, initrd)?;
        let cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(hv("KVM_GET_SUPPORTED_CPUID"))?;
        for (id, v) in vcpus.iter().enumerate() {
            x86::configure_vcpu(v, id as u32, config.cpus_max, &cpuid, loaded.entry)?;
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        let _ = initrd;
        if let Some((fd, _)) = &gic {
            arm::finalize_gic(fd)?;
        }
        arm::set_boot_regs(&vcpus[0], loaded.entry, machine.layout.fdt_addr())?;
    }

    let exit = run_machine(Arc::new(machine), vcpus, control);
    drop(gic);
    drop(vm);
    Ok(exit)
}

/// Run the vCPUs, the vsock backend and the control socket until the guest stops, then stop
/// every thread and return the exit code.
fn run_machine(machine: Arc<Machine>, vcpus: Vec<VcpuFd>, control: ControlServer) -> i32 {
    // Registering the same no-op handler twice is harmless; failing to register it only means
    // stop falls back on the guest's own exit.
    let _ = register_signal_handler(SIGRTMIN(), kick_handler);
    let stop = machine.stop.clone();

    let mut threads: Vec<JoinHandle<()>> = Vec::new();
    for (id, vcpu) in vcpus.into_iter().enumerate() {
        let (mmio, pio, st) = (machine.mmio.clone(), machine.pio.clone(), stop.clone());
        match std::thread::Builder::new()
            .name(format!("vcpu{id}"))
            .spawn(move || vcpu_loop(vcpu, &mmio, &pio, &st))
        {
            Ok(h) => threads.push(h),
            Err(e) => {
                stop.request(StopReason::MonitorError(format!("vcpu thread: {e}")));
            }
        }
    }
    let m = machine.clone();
    let backend = std::thread::Builder::new()
        .name("vsock".into())
        .spawn(move || m.vsock_backend());
    let controller = machine.controller();
    let st = stop.clone();
    let ctl = std::thread::Builder::new()
        .name("control".into())
        .spawn(move || control.serve(&controller, &st));

    while !stop.is_stopped() {
        std::thread::sleep(KICK_INTERVAL);
    }
    while threads.iter().any(|t| !t.is_finished()) {
        for t in threads.iter().filter(|t| !t.is_finished()) {
            let _ = t.kill(SIGRTMIN());
        }
        std::thread::sleep(KICK_INTERVAL);
    }
    for t in threads {
        let _ = t.join();
    }
    if let Ok(t) = backend {
        let _ = t.join();
    }
    if let Ok(t) = ctl {
        let _ = t.join();
    }
    machine.finish(&mut std::io::stderr())
}

fn vcpu_loop(mut vcpu: VcpuFd, mmio: &Bus, pio: &Bus, stop: &StopSignal) {
    while !stop.is_stopped() {
        let reason = match vcpu.run() {
            Ok(VcpuExit::MmioRead(addr, data)) => {
                mmio.read(addr, data);
                None
            }
            Ok(VcpuExit::MmioWrite(addr, data)) => {
                mmio.write(addr, data);
                None
            }
            Ok(VcpuExit::IoIn(port, data)) => {
                pio.read(port as u64, data);
                None
            }
            Ok(VcpuExit::IoOut(port, data)) => {
                pio.write(port as u64, data);
                None
            }
            Ok(VcpuExit::Hlt) | Ok(VcpuExit::Intr) => None,
            Ok(VcpuExit::Shutdown) => Some(StopReason::Reset),
            Ok(VcpuExit::SystemEvent(kind, _)) => Some(match kind {
                KVM_SYSTEM_EVENT_SHUTDOWN => StopReason::PowerOff,
                KVM_SYSTEM_EVENT_RESET => StopReason::Reset,
                KVM_SYSTEM_EVENT_CRASH => StopReason::Crash,
                other => StopReason::MonitorError(format!("system event {other}")),
            }),
            Ok(VcpuExit::FailEntry(reason, cpu)) => Some(StopReason::MonitorError(format!(
                "vcpu {cpu} failed entry, hardware reason {reason:#x}"
            ))),
            Ok(VcpuExit::InternalError) => {
                Some(StopReason::MonitorError("KVM internal error".into()))
            }
            Ok(other) => Some(StopReason::MonitorError(format!(
                "unexpected vcpu exit {other:?}"
            ))),
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => None,
            Err(e) => Some(StopReason::MonitorError(format!("KVM_RUN: {e}"))),
        };
        if let Some(r) = reason {
            stop.request(r);
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::*;
    use crate::boot::{self, GDT, Segment};
    use crate::layout::x86 as lx;

    fn seg(s: Segment) -> kvm_segment {
        kvm_segment {
            base: s.base,
            limit: s.limit,
            selector: s.selector,
            type_: s.type_,
            present: s.present,
            dpl: s.dpl,
            db: s.db,
            s: s.s,
            l: s.l,
            g: s.g,
            avl: s.avl,
            unusable: 0,
            padding: 0,
        }
    }

    /// CPUID, MSRs and the LAPIC for every vCPU; registers, segments and paging for the first.
    pub fn configure_vcpu(
        v: &VcpuFd,
        id: u32,
        cpus_max: u32,
        supported: &CpuId,
        entry: u64,
    ) -> Result<()> {
        let mut cpuid = supported.clone();
        for e in cpuid.as_mut_slice() {
            let mut r = [e.eax, e.ebx, e.ecx, e.edx];
            boot::x86_patch_cpuid(e.function, e.index, &mut r, id, cpus_max);
            [e.eax, e.ebx, e.ecx, e.edx] = r;
        }
        v.set_cpuid2(&cpuid).map_err(hv("KVM_SET_CPUID2"))?;

        let entries: Vec<kvm_msr_entry> = boot::x86_boot_msrs()
            .into_iter()
            .map(|(index, data)| kvm_msr_entry {
                index,
                data,
                ..Default::default()
            })
            .collect();
        let msrs =
            Msrs::from_entries(&entries).map_err(|e| VmmError::device("msrs", format!("{e:?}")))?;
        let n = v.set_msrs(&msrs).map_err(hv("KVM_SET_MSRS"))?;
        if n != entries.len() {
            return Err(VmmError::Hypervisor {
                op: "KVM_SET_MSRS",
                msg: format!("set {n} of {} MSRs", entries.len()),
            });
        }

        let mut lapic = v.get_lapic().map_err(hv("KVM_GET_LAPIC"))?;
        let mut bytes: Vec<u8> = lapic.regs.iter().map(|b| *b as u8).collect();
        boot::x86_set_lint(&mut bytes);
        for (d, s) in lapic.regs.iter_mut().zip(bytes) {
            *d = s as _;
        }
        v.set_lapic(&lapic).map_err(hv("KVM_SET_LAPIC"))?;

        if id != 0 {
            // Application processors wait for INIT/SIPI from the kernel.
            return Ok(());
        }
        let fpu = kvm_fpu {
            fcw: 0x37f,
            mxcsr: 0x1f80,
            ..Default::default()
        };
        v.set_fpu(&fpu).map_err(hv("KVM_SET_FPU"))?;

        let r = boot::X86BootRegs::new(entry);
        let regs = kvm_regs {
            rip: r.rip,
            rsp: r.rsp,
            rbp: r.rsp,
            rsi: r.rsi,
            rflags: r.rflags,
            ..Default::default()
        };
        v.set_regs(&regs).map_err(hv("KVM_SET_REGS"))?;

        let mut s = v.get_sregs().map_err(hv("KVM_GET_SREGS"))?;
        s.gdt.base = lx::GDT_ADDR;
        s.gdt.limit = (GDT.len() * 8 - 1) as u16;
        s.idt.base = lx::IDT_ADDR;
        s.idt.limit = 7;
        s.cs = seg(boot::segment(GDT[1], 1));
        let data = seg(boot::segment(GDT[2], 2));
        s.ds = data;
        s.es = data;
        s.fs = data;
        s.gs = data;
        s.ss = data;
        s.tr = seg(boot::segment(GDT[3], 3));
        s.cr0 |= boot::CR0_PE | boot::CR0_PG;
        s.cr3 = lx::PML4_ADDR;
        s.cr4 |= boot::CR4_PAE;
        s.efer |= boot::EFER_LME | boot::EFER_LMA;
        v.set_sregs(&s).map_err(hv("KVM_SET_SREGS"))?;
        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
mod arm {
    use super::*;
    use crate::boot;
    use crate::layout::arm as la;
    use kvm_ioctls::DeviceFd;

    /// Interrupt lines the vGIC is sized for: 32 private plus 96 shared, more than the
    /// devices use (the last virtio GSI is below 70).
    pub const GIC_NR_IRQS: u32 = 128;

    pub fn create_vcpus(vm: &VmFd, n: u32) -> Result<Vec<VcpuFd>> {
        let mut kvi = kvm_vcpu_init::default();
        vm.get_preferred_target(&mut kvi)
            .map_err(hv("KVM_ARM_PREFERRED_TARGET"))?;
        kvi.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;
        (0..n)
            .map(|id| {
                let v = vm.create_vcpu(id as u64).map_err(hv("KVM_CREATE_VCPU"))?;
                let mut k = kvi;
                if id > 0 {
                    // Secondaries start powered off and are brought up through PSCI.
                    k.features[0] |= 1 << KVM_ARM_VCPU_POWER_OFF;
                }
                v.vcpu_init(&k).map_err(hv("KVM_ARM_VCPU_INIT"))?;
                Ok(v)
            })
            .collect()
    }

    fn attr(fd: &DeviceFd, group: u32, attr: u64, addr: u64) -> Result<()> {
        fd.set_device_attr(&kvm_device_attr {
            group,
            attr,
            addr,
            flags: 0,
        })
        .map_err(hv("KVM_SET_DEVICE_ATTR"))
    }

    /// GICv3 when the host has it, else GICv2 (the Raspberry Pi 5's GIC-400).
    pub fn create_gic(vm: &VmFd, cpus: u32) -> Result<(DeviceFd, GicVersion)> {
        let mut dev = kvm_create_device {
            type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };
        let (fd, version) = match vm.create_device(&mut dev) {
            Ok(fd) => (fd, GicVersion::V3),
            Err(_) => {
                dev.type_ = kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V2;
                let fd = vm.create_device(&mut dev).map_err(|e| {
                    VmmError::NoKvm(format!("neither a GICv3 nor a GICv2 can be created: {e}"))
                })?;
                (fd, GicVersion::V2)
            }
        };
        let dist: u64 = la::GIC_DIST;
        match version {
            GicVersion::V3 => {
                let redist: u64 = la::GIC_REDIST;
                attr(
                    &fd,
                    KVM_DEV_ARM_VGIC_GRP_ADDR,
                    KVM_VGIC_V3_ADDR_TYPE_DIST as u64,
                    &dist as *const u64 as u64,
                )?;
                attr(
                    &fd,
                    KVM_DEV_ARM_VGIC_GRP_ADDR,
                    KVM_VGIC_V3_ADDR_TYPE_REDIST as u64,
                    &redist as *const u64 as u64,
                )?;
                let _ = cpus;
            }
            GicVersion::V2 => {
                let cpu: u64 = la::GIC_CPU;
                attr(
                    &fd,
                    KVM_DEV_ARM_VGIC_GRP_ADDR,
                    KVM_VGIC_V2_ADDR_TYPE_DIST as u64,
                    &dist as *const u64 as u64,
                )?;
                attr(
                    &fd,
                    KVM_DEV_ARM_VGIC_GRP_ADDR,
                    KVM_VGIC_V2_ADDR_TYPE_CPU as u64,
                    &cpu as *const u64 as u64,
                )?;
            }
        }
        let nr: u32 = GIC_NR_IRQS;
        attr(
            &fd,
            KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            0,
            &nr as *const u32 as u64,
        )?;
        Ok((fd, version))
    }

    pub fn finalize_gic(fd: &DeviceFd) -> Result<()> {
        attr(
            fd,
            KVM_DEV_ARM_VGIC_GRP_CTRL,
            KVM_DEV_ARM_VGIC_CTRL_INIT as u64,
            0,
        )
    }

    pub fn set_boot_regs(v: &VcpuFd, entry: u64, fdt: u64) -> Result<()> {
        for (id, val) in [
            (boot::arm64_reg_pstate(), boot::ARM64_BOOT_PSTATE),
            (boot::arm64_reg_pc(), entry),
            (boot::arm64_reg_x(0), fdt),
        ] {
            v.set_one_reg(id, &val.to_le_bytes())
                .map_err(hv("KVM_SET_ONE_REG"))?;
        }
        Ok(())
    }
}
