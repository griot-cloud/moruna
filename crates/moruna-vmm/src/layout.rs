//! The guest-physical memory map and interrupt assignment, per architecture, and the kernel
//! command line.
//!
//! Everything here is arithmetic over [`Arch`], so both architectures' layouts are computed and
//! tested on any host; only the code that hands them to KVM is Linux-only.

use crate::config::VmConfig;
use crate::error::{Result, VmmError};

/// A guest architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    /// x86_64: bzImage or ELF kernel, ACPI, port-IO serial.
    X86_64,
    /// aarch64: `Image` kernel, device tree, GIC, MMIO serial.
    Aarch64,
}

impl Arch {
    /// The architecture this monitor was built for.
    #[cfg(target_arch = "x86_64")]
    pub const HOST: Arch = Arch::X86_64;
    /// The architecture this monitor was built for.
    #[cfg(not(target_arch = "x86_64"))]
    pub const HOST: Arch = Arch::Aarch64;
}

/// One MiB.
const MIB: u64 = 1 << 20;

/// Hot-plug regions start on, and are sized in, Linux's memory block size on both
/// architectures with 4 KiB pages (128 MiB), so every block the guest onlines is whole.
pub const MEMORY_BLOCK_BYTES: u64 = 128 * MIB;
/// Size of one virtio-mmio device window.
pub const MMIO_WINDOW: u64 = crate::devices::mmio::WINDOW_BYTES;

/// x86_64 constants.
pub mod x86 {
    /// Start of the 32-bit MMIO hole; low RAM ends here.
    pub const MMIO_GAP_START: u64 = 0xc000_0000;
    /// End of the 32-bit MMIO hole; high RAM starts here.
    pub const MMIO_GAP_END: u64 = 1 << 32;
    /// First virtio-mmio window.
    pub const VIRTIO_BASE: u64 = 0xd000_0000;
    /// The CPU hot-plug controller's registers.
    pub const CPU_HOTPLUG_ADDR: u64 = 0xfe00_0000;
    /// The IOAPIC.
    pub const IOAPIC_ADDR: u64 = 0xfec0_0000;
    /// The local APIC.
    pub const LAPIC_ADDR: u64 = 0xfee0_0000;
    /// GSI of the first virtio device; 0 to 4 are the legacy PIC lines (4 is COM1).
    pub const VIRTIO_GSI_BASE: u32 = 5;
    /// GSI of the ACPI Generic Event Device (CPU hot-add), the IOAPIC's last pin.
    pub const GED_GSI: u32 = 23;
    /// The GDT.
    pub const GDT_ADDR: u64 = 0x500;
    /// The (empty) IDT.
    pub const IDT_ADDR: u64 = 0x520;
    /// The Linux zero page (`boot_params`).
    pub const ZERO_PAGE_ADDR: u64 = 0x7000;
    /// The boot stack pointer.
    pub const BOOT_STACK: u64 = 0x8ff0;
    /// Level-4 page table.
    pub const PML4_ADDR: u64 = 0x9000;
    /// Level-3 page table.
    pub const PDPTE_ADDR: u64 = 0xa000;
    /// Level-2 page table (identity-maps the first GiB with 2 MiB pages).
    pub const PDE_ADDR: u64 = 0xb000;
    /// The kernel command line.
    pub const CMDLINE_ADDR: u64 = 0x2_0000;
    /// Start of the extended BIOS data area; RAM below it is usable.
    pub const EBDA_START: u64 = 0x9_fc00;
    /// Where the RSDP and the ACPI tables go (the kernel scans 0xe0000 to 0xfffff).
    pub const ACPI_ADDR: u64 = 0xe_0000;
    /// Bytes available for the ACPI tables.
    pub const ACPI_MAX: u64 = 0x2_0000;
    /// Where the kernel is loaded ("high memory" to the boot protocol).
    pub const KERNEL_ADDR: u64 = 0x10_0000;
}

/// aarch64 constants.
pub mod arm {
    /// GIC distributor.
    pub const GIC_DIST: u64 = 0x0800_0000;
    /// GIC distributor size.
    pub const GIC_DIST_SIZE: u64 = 0x1_0000;
    /// GICv2 CPU interface.
    pub const GIC_CPU: u64 = 0x0801_0000;
    /// GICv2 CPU interface size.
    pub const GIC_CPU_SIZE: u64 = 0x2000;
    /// GICv3 redistributors, one 128 KiB frame per vCPU.
    pub const GIC_REDIST: u64 = 0x080a_0000;
    /// One redistributor frame.
    pub const GIC_REDIST_STRIDE: u64 = 0x2_0000;
    /// The MMIO 8250 console.
    pub const SERIAL_ADDR: u64 = 0x0900_0000;
    /// First virtio-mmio window.
    pub const VIRTIO_BASE: u64 = 0x0a00_0000;
    /// RAM starts at 2 GiB.
    pub const RAM_START: u64 = 0x8000_0000;
    /// The first interrupt number the monitor assigns. On aarch64 an irqfd GSI `n` raises
    /// SPI `n` (INTID `32 + n`), and the device tree names the SPI, so GSI and SPI agree.
    pub const SPI_BASE: u32 = 32;
    /// GSI of the console.
    pub const SERIAL_GSI: u32 = SPI_BASE + 1;
    /// GSI of the first virtio device.
    pub const VIRTIO_GSI_BASE: u32 = SPI_BASE + 16;
    /// Room reserved at the top of boot RAM for the device tree.
    pub const FDT_MAX: u64 = 2 << 20;
}

/// Most virtio-mmio devices: vsock, mem and the disks, with the root filesystem.
pub const MAX_VIRTIO_DEVICES: usize = 2 + crate::config::MAX_DISKS;

/// One virtio device's place: its MMIO window and its GSI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// Window base.
    pub addr: u64,
    /// GSI, as KVM numbers it for this architecture.
    pub gsi: u32,
}

/// Where everything is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The architecture.
    pub arch: Arch,
    /// Boot RAM, as `(start, len)` ranges.
    pub ram: Vec<(u64, u64)>,
    /// The virtio-mem region, `(start, len)`, when `--memory-max` exceeds `--memory`.
    pub hotplug: Option<(u64, u64)>,
}

fn align_up(v: u64, a: u64) -> Option<u64> {
    v.checked_add(a - 1).map(|x| x / a * a)
}

impl Layout {
    /// The layout for `memory` bytes of boot RAM and `hotplug` bytes of virtio-mem region.
    pub fn new(arch: Arch, memory: u64, hotplug: u64) -> Result<Self> {
        let ram = match arch {
            Arch::X86_64 if memory <= x86::MMIO_GAP_START => vec![(0, memory)],
            Arch::X86_64 => vec![
                (0, x86::MMIO_GAP_START),
                (x86::MMIO_GAP_END, memory - x86::MMIO_GAP_START),
            ],
            Arch::Aarch64 => vec![(arm::RAM_START, memory)],
        };
        let end = ram.last().map_or(0, |(s, l)| s + l);
        let hotplug = if hotplug == 0 {
            None
        } else {
            let floor = match arch {
                Arch::X86_64 => end.max(x86::MMIO_GAP_END),
                Arch::Aarch64 => end,
            };
            let start = align_up(floor, MEMORY_BLOCK_BYTES);
            let len = align_up(hotplug, MEMORY_BLOCK_BYTES);
            match (start, len) {
                (Some(s), Some(l)) if s.checked_add(l).is_some() => Some((s, l)),
                _ => {
                    return Err(VmmError::config(
                        "--memory-max",
                        "overflows the address space",
                    ));
                }
            }
        };
        Ok(Layout { arch, ram, hotplug })
    }

    /// The layout for a configuration.
    pub fn for_config(arch: Arch, c: &VmConfig) -> Result<Self> {
        Layout::new(arch, c.memory_bytes, c.hotplug_bytes())
    }

    /// End of boot RAM (exclusive).
    pub fn ram_end(&self) -> u64 {
        self.ram.last().map_or(0, |(s, l)| s + l)
    }

    /// End of the RAM the kernel, initramfs and boot structures may use: the first RAM
    /// range (below the x86 hole; all of it on aarch64).
    pub fn low_ram_end(&self) -> u64 {
        self.ram.first().map_or(0, |(s, l)| s + l)
    }

    /// Every range of guest memory the monitor maps: boot RAM, then the hot-plug region.
    pub fn memory_ranges(&self) -> Vec<(u64, u64)> {
        let mut v = self.ram.clone();
        v.extend(self.hotplug);
        v
    }

    /// The `i`th virtio device's window and GSI.
    pub fn virtio_slot(&self, i: usize) -> Result<Slot> {
        if i >= MAX_VIRTIO_DEVICES {
            return Err(VmmError::config(
                "--disk",
                format!("more than {MAX_VIRTIO_DEVICES} virtio devices"),
            ));
        }
        let (base, gsi) = match self.arch {
            Arch::X86_64 => (x86::VIRTIO_BASE, x86::VIRTIO_GSI_BASE),
            Arch::Aarch64 => (arm::VIRTIO_BASE, arm::VIRTIO_GSI_BASE),
        };
        Ok(Slot {
            addr: base + i as u64 * MMIO_WINDOW,
            gsi: gsi + i as u32,
        })
    }

    /// Where the kernel is loaded.
    pub fn kernel_addr(&self) -> u64 {
        match self.arch {
            Arch::X86_64 => x86::KERNEL_ADDR,
            Arch::Aarch64 => arm::RAM_START,
        }
    }

    /// Where the device tree goes (aarch64): the top [`arm::FDT_MAX`] of boot RAM.
    pub fn fdt_addr(&self) -> u64 {
        self.low_ram_end() - arm::FDT_MAX
    }

    /// Where an initramfs of `size` bytes goes: page-aligned, as high in low RAM as fits,
    /// below the device tree on aarch64. `kernel_end` is the first byte after the kernel.
    pub fn initrd_addr(&self, size: u64, kernel_end: u64) -> Result<u64> {
        let top = match self.arch {
            Arch::X86_64 => self.low_ram_end(),
            Arch::Aarch64 => self.fdt_addr(),
        };
        let addr = top
            .checked_sub(size)
            .map(|a| a / 4096 * 4096)
            .filter(|a| *a >= kernel_end)
            .ok_or_else(|| {
                VmmError::config(
                    "--memory",
                    format!("{size} bytes of initramfs do not fit below {top:#x} after the kernel"),
                )
            })?;
        Ok(addr)
    }
}

/// The kernel command line (MH 4.8.3): the console, reboot and panic behaviour the monitor
/// relies on, memory hot-plug onlining, and the root filesystem.
pub fn cmdline(arch: Arch, has_rootfs: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    match arch {
        Arch::X86_64 => {
            parts.push("console=ttyS0".into());
            // reboot=k: reset through the i8042, which the monitor sees; the guest has no PCI.
            parts.push("reboot=k".into());
            parts.push("pci=off".into());
            parts.push("i8042.noaux".into());
            parts.push("i8042.nomux".into());
            parts.push("i8042.dumbkbd".into());
        }
        Arch::Aarch64 => {
            parts.push("console=ttyS0".into());
            parts.push(format!("earlycon=uart,mmio,{:#x}", arm::SERIAL_ADDR));
        }
    }
    // A panic reboots after one second, which ends the run; the monitor reports it as a panic.
    parts.push("panic=1".into());
    parts.push("memhp_default_state=online_movable".into());
    if has_rootfs {
        parts.push("moruna.root=moruna-root".into());
    }
    parts.push("quiet".into());
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    const GIB: u64 = 1 << 30;

    #[test]
    fn vm_t5_x86_layout_below_and_across_the_hole() {
        let l = Layout::new(Arch::X86_64, GIB, 0).unwrap();
        assert_eq!(l.ram, vec![(0, GIB)]);
        assert_eq!(l.hotplug, None);
        assert_eq!(l.ram_end(), GIB);
        assert_eq!(l.memory_ranges(), vec![(0, GIB)]);

        // Hot-plug memory starts above the hole even when boot RAM is small.
        let l = Layout::new(Arch::X86_64, GIB, 3 * GIB + 2 * MIB).unwrap();
        assert_eq!(l.hotplug, Some((1 << 32, 3 * GIB + MEMORY_BLOCK_BYTES)));

        // Boot RAM larger than the hole's start is split around it.
        let l = Layout::new(Arch::X86_64, 4 * GIB, GIB).unwrap();
        assert_eq!(l.ram, vec![(0, 3 * GIB), (1 << 32, GIB)]);
        assert_eq!(l.low_ram_end(), 3 * GIB);
        assert_eq!(l.ram_end(), 5 * GIB);
        assert_eq!(l.hotplug, Some((5 * GIB, GIB)));
        assert_eq!(l.memory_ranges().len(), 3);
        assert_eq!(l.kernel_addr(), x86::KERNEL_ADDR);

        // No range overlaps the MMIO hole or another range.
        let r = l.memory_ranges();
        for w in r.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0);
        }
        for (s, len) in r {
            assert!(s + len <= x86::MMIO_GAP_START || s >= x86::MMIO_GAP_END);
        }
    }

    #[test]
    fn vm_t5_arm_layout() {
        let l = Layout::new(Arch::Aarch64, GIB + 2 * MIB, GIB).unwrap();
        assert_eq!(l.ram, vec![(arm::RAM_START, GIB + 2 * MIB)]);
        let (hs, hl) = l.hotplug.unwrap();
        assert_eq!(hs % MEMORY_BLOCK_BYTES, 0);
        assert!(hs >= l.ram_end());
        assert_eq!(hl, GIB);
        assert_eq!(l.kernel_addr(), arm::RAM_START);
        assert_eq!(l.fdt_addr(), l.ram_end() - arm::FDT_MAX);
        // Devices sit below RAM, above the GIC and the console.
        let last = l.virtio_slot(MAX_VIRTIO_DEVICES - 1).unwrap();
        assert!(last.addr + MMIO_WINDOW <= arm::RAM_START);
        const { assert!(arm::GIC_REDIST + 64 * arm::GIC_REDIST_STRIDE <= arm::SERIAL_ADDR) };
        const { assert!(arm::SERIAL_ADDR + 0x1000 <= arm::VIRTIO_BASE) };
    }

    #[test]
    fn vm_t5_slots_are_distinct_and_bounded() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let l = Layout::new(arch, GIB, 0).unwrap();
            let slots: Vec<Slot> = (0..MAX_VIRTIO_DEVICES)
                .map(|i| l.virtio_slot(i).unwrap())
                .collect();
            for (i, a) in slots.iter().enumerate() {
                for b in &slots[i + 1..] {
                    assert!(a.addr + MMIO_WINDOW <= b.addr);
                    assert_ne!(a.gsi, b.gsi);
                }
            }
            assert!(l.virtio_slot(MAX_VIRTIO_DEVICES).is_err());
            if arch == Arch::X86_64 {
                // Every virtio GSI is an IOAPIC pin below the GED's, above COM1's.
                assert!(slots.iter().all(|s| s.gsi > 4 && s.gsi < x86::GED_GSI));
                let last = slots.last().unwrap();
                assert!(last.addr + MMIO_WINDOW <= x86::CPU_HOTPLUG_ADDR);
            }
        }
    }

    #[test]
    fn vm_t5_initrd_placement() {
        let l = Layout::new(Arch::X86_64, 512 * MIB, 0).unwrap();
        let a = l.initrd_addr(10 * MIB + 5, 20 * MIB).unwrap();
        assert_eq!(a % 4096, 0);
        assert!(a + 10 * MIB + 5 <= 512 * MIB);
        assert!(l.initrd_addr(600 * MIB, 0).is_err());
        assert!(l.initrd_addr(500 * MIB, 20 * MIB).is_err());
        let l = Layout::new(Arch::Aarch64, 512 * MIB, 0).unwrap();
        let a = l.initrd_addr(MIB, arm::RAM_START + 20 * MIB).unwrap();
        assert!(a + MIB <= l.fdt_addr());
    }

    #[test]
    fn vm_t5_overflowing_hotplug_is_refused() {
        assert!(Layout::new(Arch::Aarch64, GIB, u64::MAX - GIB).is_err());
    }

    #[test]
    fn vm_t6_cmdline_carries_what_the_monitor_relies_on() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let c = cmdline(arch, true);
            for want in [
                "console=ttyS0",
                "panic=1",
                "memhp_default_state=online_movable",
                "moruna.root=moruna-root",
            ] {
                assert!(c.split(' ').any(|p| p == want), "{arch:?}: {want} in {c}");
            }
            assert!(!cmdline(arch, false).contains("moruna.root"));
            // Nothing that would bring up a network.
            assert!(!c.contains("ip=") && !c.contains("net"), "{c}");
        }
        assert!(cmdline(Arch::X86_64, false).contains("reboot=k"));
        assert!(cmdline(Arch::Aarch64, false).contains("earlycon=uart,mmio,0x9000000"));
    }
}
