//! The aarch64 device tree: CPUs, memory, the GIC, the timer, PSCI, the console and the
//! virtio-mmio devices. There is no network node because there is no network device.
//!
//! Built with `vm-fdt`, so it is generated and checked on any host.

use vm_fdt::FdtWriter;

use crate::error::{Result, VmmError};
use crate::layout::{Layout, Slot, arm};

/// Which GIC the host's KVM gave the guest. The Raspberry Pi 5's GIC-400 is v2 only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GicVersion {
    /// GICv2: distributor and CPU interface.
    V2,
    /// GICv3: distributor and one redistributor per vCPU.
    V3,
}

/// Everything the device tree describes.
#[derive(Clone, Debug)]
pub struct FdtSpec<'a> {
    /// The memory map.
    pub layout: &'a Layout,
    /// vCPUs (each gets an MPIDR equal to its index).
    pub cpus: u32,
    /// The GIC.
    pub gic: GicVersion,
    /// The kernel command line.
    pub cmdline: &'a str,
    /// The initramfs, `(start, len)`.
    pub initrd: Option<(u64, u64)>,
    /// The virtio-mmio devices.
    pub virtio: &'a [Slot],
}

const PHANDLE_GIC: u32 = 1;
const PHANDLE_CLOCK: u32 = 2;
/// GIC interrupt specifier: SPI.
const GIC_SPI: u32 = 0;
/// GIC interrupt specifier: PPI.
const GIC_PPI: u32 = 1;
/// Level, active high.
const IRQ_LEVEL_HI: u32 = 4;
/// Edge, rising.
const IRQ_EDGE_RISING: u32 = 1;

fn e(err: vm_fdt::Error) -> VmmError {
    VmmError::device("fdt", err.to_string())
}

/// Build the device tree blob.
pub fn build(s: &FdtSpec<'_>) -> Result<Vec<u8>> {
    let mut f = FdtWriter::new().map_err(e)?;
    let root = f.begin_node("").map_err(e)?;
    f.property_string("compatible", "linux,dummy-virt")
        .map_err(e)?;
    f.property_u32("#address-cells", 2).map_err(e)?;
    f.property_u32("#size-cells", 2).map_err(e)?;
    f.property_u32("interrupt-parent", PHANDLE_GIC).map_err(e)?;

    let cpus = f.begin_node("cpus").map_err(e)?;
    f.property_u32("#address-cells", 1).map_err(e)?;
    f.property_u32("#size-cells", 0).map_err(e)?;
    for i in 0..s.cpus {
        let mpidr = crate::boot::arm64_mpidr(i);
        let c = f.begin_node(&format!("cpu@{mpidr:x}")).map_err(e)?;
        f.property_string("device_type", "cpu").map_err(e)?;
        f.property_string("compatible", "arm,arm-v8").map_err(e)?;
        f.property_string("enable-method", "psci").map_err(e)?;
        f.property_u32("reg", mpidr).map_err(e)?;
        f.end_node(c).map_err(e)?;
    }
    f.end_node(cpus).map_err(e)?;

    for (start, len) in &s.layout.ram {
        let m = f.begin_node(&format!("memory@{start:x}")).map_err(e)?;
        f.property_string("device_type", "memory").map_err(e)?;
        f.property_array_u64("reg", &[*start, *len]).map_err(e)?;
        f.end_node(m).map_err(e)?;
    }

    let chosen = f.begin_node("chosen").map_err(e)?;
    f.property_string("bootargs", s.cmdline).map_err(e)?;
    f.property_string("stdout-path", &format!("/uart@{:x}", arm::SERIAL_ADDR))
        .map_err(e)?;
    if let Some((start, len)) = s.initrd {
        f.property_u64("linux,initrd-start", start).map_err(e)?;
        f.property_u64("linux,initrd-end", start + len).map_err(e)?;
    }
    f.end_node(chosen).map_err(e)?;

    let gic = f
        .begin_node(&format!("intc@{:x}", arm::GIC_DIST))
        .map_err(e)?;
    match s.gic {
        GicVersion::V2 => {
            f.property_string("compatible", "arm,cortex-a15-gic")
                .map_err(e)?;
            f.property_array_u64(
                "reg",
                &[
                    arm::GIC_DIST,
                    arm::GIC_DIST_SIZE,
                    arm::GIC_CPU,
                    arm::GIC_CPU_SIZE,
                ],
            )
            .map_err(e)?;
        }
        GicVersion::V3 => {
            f.property_string("compatible", "arm,gic-v3").map_err(e)?;
            f.property_array_u64(
                "reg",
                &[
                    arm::GIC_DIST,
                    arm::GIC_DIST_SIZE,
                    arm::GIC_REDIST,
                    arm::GIC_REDIST_STRIDE * s.cpus as u64,
                ],
            )
            .map_err(e)?;
        }
    }
    f.property_u32("#interrupt-cells", 3).map_err(e)?;
    f.property_null("interrupt-controller").map_err(e)?;
    f.property_u32("#address-cells", 2).map_err(e)?;
    f.property_u32("#size-cells", 2).map_err(e)?;
    f.property_phandle(PHANDLE_GIC).map_err(e)?;
    f.end_node(gic).map_err(e)?;

    let timer = f.begin_node("timer").map_err(e)?;
    f.property_string("compatible", "arm,armv8-timer")
        .map_err(e)?;
    f.property_null("always-on").map_err(e)?;
    let mut irqs = Vec::new();
    // Secure, non-secure, virtual and hypervisor timers: PPIs 13, 14, 11, 10.
    for ppi in [13u32, 14, 11, 10] {
        irqs.extend([GIC_PPI, ppi, IRQ_LEVEL_HI]);
    }
    f.property_array_u32("interrupts", &irqs).map_err(e)?;
    f.end_node(timer).map_err(e)?;

    let psci = f.begin_node("psci").map_err(e)?;
    f.property_string_list(
        "compatible",
        vec!["arm,psci-1.0".into(), "arm,psci-0.2".into()],
    )
    .map_err(e)?;
    f.property_string("method", "hvc").map_err(e)?;
    f.end_node(psci).map_err(e)?;

    let clk = f.begin_node("apb-pclk").map_err(e)?;
    f.property_string("compatible", "fixed-clock").map_err(e)?;
    f.property_u32("#clock-cells", 0).map_err(e)?;
    f.property_u32("clock-frequency", 24_000_000).map_err(e)?;
    f.property_phandle(PHANDLE_CLOCK).map_err(e)?;
    f.end_node(clk).map_err(e)?;

    let uart = f
        .begin_node(&format!("uart@{:x}", arm::SERIAL_ADDR))
        .map_err(e)?;
    f.property_string("compatible", "ns16550a").map_err(e)?;
    f.property_array_u64("reg", &[arm::SERIAL_ADDR, 0x1000])
        .map_err(e)?;
    f.property_u32("clock-frequency", 1_843_200).map_err(e)?;
    f.property_array_u32("interrupts", &[GIC_SPI, arm::SERIAL_GSI, IRQ_EDGE_RISING])
        .map_err(e)?;
    f.end_node(uart).map_err(e)?;

    for slot in s.virtio {
        let v = f
            .begin_node(&format!("virtio_mmio@{:x}", slot.addr))
            .map_err(e)?;
        f.property_string("compatible", "virtio,mmio").map_err(e)?;
        f.property_array_u64("reg", &[slot.addr, crate::layout::MMIO_WINDOW])
            .map_err(e)?;
        f.property_array_u32("interrupts", &[GIC_SPI, slot.gsi, IRQ_EDGE_RISING])
            .map_err(e)?;
        f.property_null("dma-coherent").map_err(e)?;
        f.end_node(v).map_err(e)?;
    }

    f.end_node(root).map_err(e)?;
    let blob = f.finish().map_err(e)?;
    if blob.len() as u64 > arm::FDT_MAX {
        return Err(VmmError::device(
            "fdt",
            "device tree exceeds its reservation",
        ));
    }
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Arch;

    /// Every node name in the blob's structure block (FDT_BEGIN_NODE tokens).
    fn node_names(blob: &[u8]) -> Vec<String> {
        let be = |o: usize| u32::from_be_bytes([blob[o], blob[o + 1], blob[o + 2], blob[o + 3]]);
        assert_eq!(be(0), 0xd00d_feed);
        let off = be(8) as usize;
        let size = be(36) as usize;
        let mut names = Vec::new();
        let mut i = off;
        while i < off + size {
            match be(i) {
                1 => {
                    let start = i + 4;
                    let end = start + blob[start..].iter().position(|b| *b == 0).unwrap();
                    names.push(String::from_utf8(blob[start..end].to_vec()).unwrap());
                    i = (end + 1).div_ceil(4) * 4;
                }
                3 => {
                    let len = be(i + 4) as usize;
                    i += 12 + len.div_ceil(4) * 4;
                }
                _ => i += 4,
            }
        }
        names
    }

    fn spec_blob(gic: GicVersion) -> Vec<u8> {
        let layout = Layout::new(Arch::Aarch64, 1 << 30, 1 << 30).unwrap();
        let slots: Vec<Slot> = (0..3).map(|i| layout.virtio_slot(i).unwrap()).collect();
        build(&FdtSpec {
            layout: &layout,
            cpus: 2,
            gic,
            cmdline: "console=ttyS0",
            initrd: Some((0x9000_0000, 4096)),
            virtio: &slots,
        })
        .unwrap()
    }

    #[test]
    fn vm_t19_device_tree_describes_the_machine() {
        for gic in [GicVersion::V2, GicVersion::V3] {
            let blob = spec_blob(gic);
            let names = node_names(&blob);
            for want in [
                "cpus",
                "cpu@0",
                "cpu@1",
                "memory@80000000",
                "chosen",
                "intc@8000000",
                "timer",
                "psci",
                "uart@9000000",
                "virtio_mmio@a000000",
                "virtio_mmio@a002000",
            ] {
                assert!(
                    names.iter().any(|n| n == want),
                    "{gic:?}: {want} in {names:?}"
                );
            }
            assert!(!names.iter().any(|n| n.contains("cpu@2")));
            // H10: nothing that could be a network device.
            assert!(!names.iter().any(|n| n.contains("eth") || n.contains("net")));
            let text = String::from_utf8_lossy(&blob);
            assert!(text.contains("console=ttyS0"));
            assert!(text.contains(match gic {
                GicVersion::V2 => "arm,cortex-a15-gic",
                GicVersion::V3 => "arm,gic-v3",
            }));
        }
    }
}
