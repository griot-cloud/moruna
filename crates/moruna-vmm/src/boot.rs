//! The boot state the kernel expects when the first vCPU starts, computed without KVM: the
//! x86_64 long-mode GDT, identity page tables, e820 map and register values, and the aarch64
//! register ids and values. The Linux-only KVM code writes these into the vCPUs.

use vm_memory::{Bytes, GuestAddress};

use crate::devices::Mem;
use crate::error::{Result, VmmError};
use crate::layout::{Layout, x86};

/// A GDT entry from its flags, base and limit (Intel SDM vol. 3, 3.4.5).
pub const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    let base = base as u64;
    let limit = limit as u64;
    let flags = flags as u64;
    ((base & 0xff00_0000) << (56 - 24))
        | ((flags & 0x0000_f0ff) << 40)
        | ((limit & 0x000f_0000) << (48 - 16))
        | ((base & 0x00ff_ffff) << 16)
        | (limit & 0x0000_ffff)
}

/// The boot GDT: null, 64-bit code, data, TSS.
pub const GDT: [u64; 4] = [
    0,
    // Code: present, DPL 0, execute/read, long mode, 4 KiB granularity.
    gdt_entry(0xa09b, 0, 0xfffff),
    // Data: present, DPL 0, read/write, 32-bit default, 4 KiB granularity.
    gdt_entry(0xc093, 0, 0xfffff),
    // TSS: present, busy 64-bit TSS.
    gdt_entry(0x808b, 0, 0xfffff),
];

/// A segment register's contents, as KVM's `kvm_segment` wants them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    /// Base address.
    pub base: u64,
    /// Limit, in bytes.
    pub limit: u32,
    /// Selector: the GDT index times 8.
    pub selector: u16,
    /// Segment type.
    pub type_: u8,
    /// Present.
    pub present: u8,
    /// Descriptor privilege level.
    pub dpl: u8,
    /// Default operation size.
    pub db: u8,
    /// Code or data (not system).
    pub s: u8,
    /// 64-bit code.
    pub l: u8,
    /// Granularity.
    pub g: u8,
    /// Available bit.
    pub avl: u8,
}

/// The segment register image of GDT entry `index`.
pub fn segment(entry: u64, index: u16) -> Segment {
    let base = ((entry >> 16) & 0x00ff_ffff) | (((entry >> 56) & 0xff) << 24);
    let raw_limit = ((entry & 0xffff) | (((entry >> 48) & 0xf) << 16)) as u32;
    let g = ((entry >> 55) & 1) as u8;
    let limit = if g == 1 {
        (raw_limit << 12) | 0xfff
    } else {
        raw_limit
    };
    Segment {
        base,
        limit,
        selector: index * 8,
        type_: ((entry >> 40) & 0xf) as u8,
        present: ((entry >> 47) & 1) as u8,
        dpl: ((entry >> 45) & 3) as u8,
        db: ((entry >> 54) & 1) as u8,
        s: ((entry >> 44) & 1) as u8,
        l: ((entry >> 53) & 1) as u8,
        g,
        avl: ((entry >> 52) & 1) as u8,
    }
}

/// `CR0.PE`.
pub const CR0_PE: u64 = 1;
/// `CR0.PG`.
pub const CR0_PG: u64 = 1 << 31;
/// `CR4.PAE`.
pub const CR4_PAE: u64 = 1 << 5;
/// `EFER.LME`.
pub const EFER_LME: u64 = 1 << 8;
/// `EFER.LMA`.
pub const EFER_LMA: u64 = 1 << 10;

/// Write the GDT, an empty IDT, and page tables identity-mapping the first GiB with 2 MiB
/// pages (what the 64-bit boot protocol needs before the kernel builds its own).
pub fn write_x86_boot_tables(mem: &Mem) -> Result<()> {
    let w = |v: u64, at: u64| {
        mem.write_obj(v, GuestAddress(at))
            .map_err(|e| VmmError::device("boot", format!("write {at:#x}: {e}")))
    };
    for (i, e) in GDT.iter().enumerate() {
        w(*e, x86::GDT_ADDR + 8 * i as u64)?;
    }
    w(0, x86::IDT_ADDR)?;
    w(x86::PDPTE_ADDR | 0x03, x86::PML4_ADDR)?;
    w(x86::PDE_ADDR | 0x03, x86::PDPTE_ADDR)?;
    for i in 0..512u64 {
        // Present, writable, 2 MiB page.
        w((i << 21) | 0x83, x86::PDE_ADDR + i * 8)?;
    }
    Ok(())
}

/// e820 type: usable RAM.
pub const E820_RAM: u32 = 1;

/// The e820 map: RAM below the EBDA, RAM from 1 MiB to the end of low RAM, high RAM. The
/// hot-plug region is not in it: virtio-mem describes it to the guest.
pub fn e820(layout: &Layout) -> Vec<(u64, u64, u32)> {
    let mut v = vec![(0, x86::EBDA_START, E820_RAM)];
    let low_end = layout.low_ram_end();
    if low_end > x86::KERNEL_ADDR {
        v.push((x86::KERNEL_ADDR, low_end - x86::KERNEL_ADDR, E820_RAM));
    }
    for (s, l) in layout.ram.iter().skip(1) {
        v.push((*s, *l, E820_RAM));
    }
    v
}

/// The first-vCPU register values of the 64-bit Linux boot protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct X86BootRegs {
    /// Kernel entry.
    pub rip: u64,
    /// Stack.
    pub rsp: u64,
    /// The zero page, which the kernel expects in RSI.
    pub rsi: u64,
    /// Reserved bit 1 set, interrupts off.
    pub rflags: u64,
}

impl X86BootRegs {
    /// Registers entering the kernel at `entry`.
    pub fn new(entry: u64) -> Self {
        X86BootRegs {
            rip: entry,
            rsp: x86::BOOT_STACK,
            rsi: x86::ZERO_PAGE_ADDR,
            rflags: 0x2,
        }
    }
}

/// The 64-bit entry point of a bzImage whose protected-mode kernel was loaded at `load`.
pub const BZIMAGE_64BIT_ENTRY_OFFSET: u64 = 0x200;

/// aarch64: `KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE`.
pub const ARM64_CORE_REG_BASE: u64 = 0x6030_0000_0010_0000;

/// The `KVM_{GET,SET}_ONE_REG` id of the aarch64 core register at byte `offset` in
/// `struct kvm_regs` (the offset counted in 32-bit words, as the kernel does).
pub fn arm64_core_reg(offset: u64) -> u64 {
    ARM64_CORE_REG_BASE | (offset / 4)
}

/// `x0`: the device tree address at entry.
pub fn arm64_reg_x(n: u64) -> u64 {
    arm64_core_reg(8 * n)
}

/// `pc`.
pub fn arm64_reg_pc() -> u64 {
    arm64_core_reg(8 * 32)
}

/// `pstate`.
pub fn arm64_reg_pstate() -> u64 {
    arm64_core_reg(8 * 33)
}

/// EL1h with D, A, I, F masked: the state the arm64 boot protocol requires.
pub const ARM64_BOOT_PSTATE: u64 = 0x3c5;

/// The MSRs the first instruction of a 64-bit kernel expects zeroed, and fast string
/// operations enabled (`IA32_MISC_ENABLE` bit 0): `(index, value)`.
pub fn x86_boot_msrs() -> Vec<(u32, u64)> {
    vec![
        (0x174, 0),       // IA32_SYSENTER_CS
        (0x175, 0),       // IA32_SYSENTER_ESP
        (0x176, 0),       // IA32_SYSENTER_EIP
        (0xc000_0081, 0), // STAR
        (0xc000_0082, 0), // LSTAR
        (0xc000_0083, 0), // CSTAR
        (0xc000_0084, 0), // SYSCALL_MASK
        (0xc000_0102, 0), // KERNEL_GS_BASE
        (0x10, 0),        // TSC
        (0x1a0, 1),       // IA32_MISC_ENABLE: fast strings
    ]
}

/// Adjust one CPUID leaf for vCPU `id` of `cpus_max`: the APIC id in leaf 1 and the x2APIC
/// topology in leaf 0xb, and the hypervisor bit. `regs` is `[eax, ebx, ecx, edx]`.
pub fn x86_patch_cpuid(function: u32, index: u32, regs: &mut [u32; 4], id: u32, cpus_max: u32) {
    match function {
        1 => {
            regs[1] = (regs[1] & 0x0000_ffff) | (id << 24) | ((cpus_max.min(255)) << 16);
            regs[2] |= 1 << 31;
            // HTT: the logical processor count in EBX is valid.
            regs[3] |= 1 << 28;
        }
        0xb => {
            let shift = 32 - cpus_max.max(1).saturating_sub(1).leading_zeros();
            match index {
                0 => *regs = [0, 1, (1 << 8), id],
                1 => *regs = [shift, cpus_max, (2 << 8) | 1, id],
                _ => *regs = [0, 0, index, id],
            }
        }
        _ => {}
    }
}

/// Program the local APIC's LINT0 as ExtINT and LINT1 as NMI (the virtual-wire setup the
/// kernel expects from firmware), in a `kvm_lapic_state` register image.
pub fn x86_set_lint(regs: &mut [u8]) {
    for (off, mode) in [(0x350usize, 7u32), (0x360, 4)] {
        let mut b = [0u8; 4];
        b.copy_from_slice(&regs[off..off + 4]);
        let v = (u32::from_le_bytes(b) & !0x700) | (mode << 8);
        regs[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// The aarch64 MPIDR affinity KVM gives vCPU `id` (Aff0 is the low four bits, Aff1 the rest),
/// which is the `reg` of its device-tree node.
pub fn arm64_mpidr(id: u32) -> u32 {
    ((id >> 4) << 8) | (id & 0xf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Arch;
    use crate::testing::guest_memory;

    #[test]
    fn vm_t22_gdt_and_segments() {
        assert_eq!(GDT[1], 0x00af_9b00_0000_ffff);
        assert_eq!(GDT[2], 0x00cf_9300_0000_ffff);
        assert_eq!(GDT[3], 0x008f_8b00_0000_ffff);
        assert_eq!(gdt_entry(0xa09b, 0, 0xfffff), GDT[1]);
        let cs = segment(GDT[1], 1);
        assert_eq!(
            (cs.selector, cs.type_, cs.present, cs.l, cs.db, cs.s, cs.g),
            (8, 0xb, 1, 1, 0, 1, 1)
        );
        assert_eq!(cs.limit, 0xffff_ffff);
        let ds = segment(GDT[2], 2);
        assert_eq!((ds.selector, ds.type_, ds.l, ds.db), (16, 3, 0, 1));
        let tss = segment(GDT[3], 3);
        assert_eq!((tss.selector, tss.type_, tss.s), (24, 0xb, 0));
        let odd = segment(gdt_entry(0x0093, 0x1234_5678, 0xabc), 4);
        assert_eq!((odd.base, odd.limit, odd.g), (0x1234_5678, 0xabc, 0));
    }

    #[test]
    fn vm_t22_page_tables_identity_map_the_first_gib() {
        let mem = guest_memory(1 << 20);
        write_x86_boot_tables(&mem).unwrap();
        let r = |a: u64| mem.read_obj::<u64>(GuestAddress(a)).unwrap();
        assert_eq!(r(x86::PML4_ADDR), x86::PDPTE_ADDR | 3);
        assert_eq!(r(x86::PDPTE_ADDR), x86::PDE_ADDR | 3);
        assert_eq!(r(x86::PDE_ADDR), 0x83);
        assert_eq!(r(x86::PDE_ADDR + 511 * 8), (511 << 21) | 0x83);
        assert_eq!(r(x86::GDT_ADDR + 8), GDT[1]);
        assert_eq!(r(x86::IDT_ADDR), 0);
        // Too little memory for the tables is an error, not a panic.
        let tiny = guest_memory(0x1000);
        assert!(write_x86_boot_tables(&tiny).is_err());
    }

    #[test]
    fn vm_t22_e820_and_registers() {
        let l = Layout::new(Arch::X86_64, 4 << 30, 1 << 30).unwrap();
        let m = e820(&l);
        assert_eq!(
            m,
            vec![
                (0, x86::EBDA_START, E820_RAM),
                (x86::KERNEL_ADDR, (3 << 30) - x86::KERNEL_ADDR, E820_RAM),
                (1 << 32, 1 << 30, E820_RAM),
            ]
        );
        // Nothing overlaps the boot structures' reserved area or the hot-plug region.
        let (hs, _) = l.hotplug.unwrap();
        assert!(m.iter().all(|(s, len, _)| s + len <= hs));
        let r = X86BootRegs::new(0x100_0200);
        assert_eq!(
            (r.rsp, r.rsi, r.rflags),
            (x86::BOOT_STACK, x86::ZERO_PAGE_ADDR, 2)
        );
        assert_eq!(r.rip, 0x100_0200);
        // A layout too small for anything above 1 MiB has just the low entry.
        let small = Layout {
            arch: Arch::X86_64,
            ram: vec![(0, 0x8_0000)],
            hotplug: None,
        };
        assert_eq!(e820(&small).len(), 1);
    }

    #[test]
    fn vm_t22_x86_cpu_state_helpers() {
        let msrs = x86_boot_msrs();
        assert!(msrs.contains(&(0x1a0, 1)));
        assert_eq!(msrs.len(), 10);
        let mut r = [0, 0x0000_0800, 0, 0];
        x86_patch_cpuid(1, 0, &mut r, 3, 8);
        assert_eq!(r[1] >> 24, 3);
        assert_eq!((r[1] >> 16) & 0xff, 8);
        assert_eq!(r[1] & 0xffff, 0x0800);
        assert_ne!(r[2] & (1 << 31), 0);
        let mut r = [9; 4];
        x86_patch_cpuid(0xb, 0, &mut r, 5, 8);
        assert_eq!(r, [0, 1, 0x100, 5]);
        x86_patch_cpuid(0xb, 1, &mut r, 5, 8);
        assert_eq!(r, [3, 8, 0x201, 5]);
        x86_patch_cpuid(0xb, 2, &mut r, 5, 8);
        assert_eq!(r, [0, 0, 2, 5]);
        let mut r = [1, 2, 3, 4];
        x86_patch_cpuid(7, 0, &mut r, 5, 8);
        assert_eq!(r, [1, 2, 3, 4]);
        let mut lapic = vec![0xffu8; 1024];
        x86_set_lint(&mut lapic);
        assert_eq!(
            u32::from_le_bytes(lapic[0x350..0x354].try_into().unwrap()) & 0x700,
            0x700
        );
        assert_eq!(
            u32::from_le_bytes(lapic[0x360..0x364].try_into().unwrap()) & 0x700,
            0x400
        );
    }

    #[test]
    fn vm_t22_arm64_register_ids() {
        assert_eq!(arm64_mpidr(3), 3);
        assert_eq!(arm64_mpidr(17), 0x101);
        assert_eq!(arm64_reg_x(0), 0x6030_0000_0010_0000);
        assert_eq!(arm64_reg_x(1), 0x6030_0000_0010_0002);
        assert_eq!(arm64_reg_pc(), 0x6030_0000_0010_0040);
        assert_eq!(arm64_reg_pstate(), 0x6030_0000_0010_0042);
        assert_eq!(ARM64_BOOT_PSTATE & 0xf, 0x5, "EL1h");
    }
}
