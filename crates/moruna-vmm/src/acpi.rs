//! The x86_64 ACPI tables: a hardware-reduced platform with the CPUs, the CPU hot-plug
//! controller, the Generic Event Device that announces a hot-added CPU, the console, the
//! virtio-mmio devices, and the sleep and reset registers a guest powers off and reboots
//! through.
//!
//! Tables (all at [`x86::ACPI_ADDR`], where the kernel scans for the RSDP): RSDP, XSDT, FADT,
//! DSDT, MADT. Built with `acpi_tables`, so they are generated and checked on any host.
//!
//! A hardware-reduced platform has no legacy PIC, so Linux maps no interrupt line until a
//! device's `_CRS` names its GSI: every device that interrupts (COM1, each virtio-mmio
//! window, the GED) is described here, and a GSI named anywhere else (a kernel command line
//! `virtio_mmio.device=`, the 8250's built-in ISA table) has no Linux IRQ behind it.

use acpi_tables::Aml;
use acpi_tables::aml::{
    self, Acquire, Add, Arg, BufferData, EISAName, Equal, Field, FieldAccessType, FieldEntry,
    FieldLockRule, FieldUpdateRule, IO, If, Interrupt, LessThan, Local, Memory32Fixed, Method,
    MethodCall, Name, Notify, OpRegion, OpRegionSpace, Package, Path, Release, ResourceTemplate,
    Return, Scope, Store, While,
};
use acpi_tables::fadt::{FADTBuilder, Flags};
use acpi_tables::gas::{AccessSize, AddressSpace, GAS};
use acpi_tables::madt::{
    EnabledStatus, IoApic, LocalInterruptController, MADT, ProcessorLocalApic,
};
use acpi_tables::rsdp::Rsdp;
use acpi_tables::sdt::Sdt;
use acpi_tables::xsdt::XSDT;

use crate::devices::cpu_hotplug;
use crate::devices::legacy::{
    ACPI_PM_PORT, RESET_REGISTER, RESET_VALUE, S5_TYPE, SLEEP_CONTROL, SLEEP_STATUS,
};
use crate::devices::serial;
use crate::error::{Result, VmmError};
use crate::layout::{MMIO_WINDOW, Slot, x86};

/// The `_HID` Linux's virtio-mmio driver binds an ACPI device by.
pub const VIRTIO_MMIO_HID: &str = "LNRO0005";
/// The `_HID` of a 16550-compatible UART, which Linux's 8250 PNP driver binds.
pub const UART_HID: &str = "PNP0501";

const OEM_ID: [u8; 6] = *b"MORUNA";
const OEM_TABLE: [u8; 8] = *b"MRNAVMM ";
const OEM_REV: u32 = 1;

/// One table at its guest address.
pub type Placed = (u64, Vec<u8>);

fn bytes(t: &dyn Aml) -> Vec<u8> {
    let mut v = Vec::new();
    t.to_aml_bytes(&mut v);
    v
}

fn name4(prefix: char, i: u32) -> String {
    format!("{prefix}{i:03X}")
}

/// The MADT entry of vCPU `i` as `_MAT` returns it: enabled.
fn lapic_entry(i: u32) -> Vec<u8> {
    vec![0, 8, i as u8, i as u8, 1, 0, 0, 0]
}

/// An edge-triggered, active-high, exclusive interrupt: what an irqfd without a resample fd
/// raises.
fn edge_irq(gsi: u32) -> Interrupt {
    Interrupt::new(true, true, false, false, gsi)
}

/// The DSDT body: `\_SB.COM1`, one `\_SB.Vnnn` per virtio-mmio slot, `\_SB.CPUS` with one
/// device per possible vCPU and the scan method, the GED, and `\_S5`.
pub fn dsdt(cpus_max: u32, virtio: &[Slot]) -> Result<Vec<u8>> {
    let hp_addr = x86::CPU_HOTPLUG_ADDR as u32;
    let hp_len = cpu_hotplug::WINDOW_BYTES as u32;
    let region = OpRegion::new(
        "PRST".into(),
        OpRegionSpace::SystemMemory,
        &hp_addr,
        &hp_len,
    );
    let csel_field = Field::new(
        "PRST".into(),
        FieldAccessType::DWord,
        FieldLockRule::NoLock,
        FieldUpdateRule::Preserve,
        vec![FieldEntry::Named(*b"CSEL", 32)],
    );
    let stat_field = Field::new(
        "PRST".into(),
        FieldAccessType::Byte,
        FieldLockRule::NoLock,
        FieldUpdateRule::WriteAsZeroes,
        vec![
            FieldEntry::Reserved(32),
            FieldEntry::Named(*b"CPEN", 1),
            FieldEntry::Named(*b"CINS", 1),
            FieldEntry::Reserved(6),
        ],
    );
    let lock = aml::Mutex::new("CLCK".into(), 0);

    // CSTA(id): 0xF if enabled, else 0.
    let csel: Path = "CSEL".into();
    let cpen: Path = "CPEN".into();
    let cins: Path = "CINS".into();
    let one = aml::One {};
    let zero = aml::Zero {};
    let fifteen: u8 = 0xf;
    let acquire = Acquire::new("CLCK".into(), 0xffff);
    let release = Release::new("CLCK".into());
    let sel_arg = Store::new(&csel, &Arg(0));
    let l0_zero = Store::new(&Local(0), &zero);
    let l0_f = Store::new(&Local(0), &fifteen);
    let en = Equal::new(&cpen, &one);
    let if_en = If::new(&en, vec![&l0_f]);
    let ret_l0 = Return::new(&Local(0));
    let csta = Method::new(
        "CSTA".into(),
        1,
        true,
        vec![&acquire, &sel_arg, &l0_zero, &if_en, &release, &ret_l0],
    );

    // CTFY(id, event): Notify the CPU device `id`.
    let ids: Vec<u32> = (0..cpus_max).collect();
    let paths: Vec<Path> = ids.iter().map(|i| name4('C', *i).as_str().into()).collect();
    let notifies: Vec<Notify<'_>> = paths.iter().map(|p| Notify::new(p, &Arg(1))).collect();
    let eqs: Vec<Equal<'_>> = ids.iter().map(|i| Equal::new(&Arg(0), i)).collect();
    let ifs: Vec<If<'_>> = eqs
        .iter()
        .zip(&notifies)
        .map(|(e, n)| If::new(e, vec![n]))
        .collect();
    let ctfy = Method::new(
        "CTFY".into(),
        2,
        true,
        ifs.iter().map(|i| i as &dyn Aml).collect(),
    );

    // CSCN(): for each vCPU with CINS set, notify it (device check) and clear CINS.
    let max = cpus_max;
    let sel_l0 = Store::new(&csel, &Local(0));
    let ins = Equal::new(&cins, &one);
    let ctfy_call_args: [&dyn Aml; 2] = [&Local(0), &one];
    let ctfy_call = MethodCall::new("CTFY".into(), ctfy_call_args.to_vec());
    let clear = Store::new(&cins, &one);
    let if_ins = If::new(&ins, vec![&ctfy_call, &clear]);
    let inc = Add::new(&Local(0), &Local(0), &one);
    let lt = LessThan::new(&Local(0), &max);
    let loop_ = While::new(&lt, vec![&sel_l0, &if_ins, &inc]);
    let cscn = Method::new(
        "CSCN".into(),
        0,
        true,
        vec![&acquire, &l0_zero, &loop_, &release],
    );

    // One device per possible vCPU.
    let hid_cpu = Name::new("_HID".into(), &"ACPI0007");
    let uids: Vec<u32> = ids.clone();
    let uid_names: Vec<Name> = uids.iter().map(|u| Name::new("_UID".into(), u)).collect();
    let sta_calls: Vec<MethodCall<'_>> = uids
        .iter()
        .map(|u| MethodCall::new("CSTA".into(), vec![u as &dyn Aml]))
        .collect();
    let sta_rets: Vec<Return<'_>> = sta_calls.iter().map(|c| Return::new(c)).collect();
    let sta_methods: Vec<Method<'_>> = sta_rets
        .iter()
        .map(|r| Method::new("_STA".into(), 0, false, vec![r]))
        .collect();
    let mats: Vec<BufferData> = uids
        .iter()
        .map(|u| BufferData::new(lapic_entry(*u)))
        .collect();
    let mat_rets: Vec<Return<'_>> = mats.iter().map(|m| Return::new(m)).collect();
    let mat_methods: Vec<Method<'_>> = mat_rets
        .iter()
        .map(|r| Method::new("_MAT".into(), 0, false, vec![r]))
        .collect();
    let cpu_devs: Vec<aml::Device<'_>> = (0..cpus_max as usize)
        .map(|i| {
            aml::Device::new(
                name4('C', i as u32).as_str().into(),
                vec![&hid_cpu, &uid_names[i], &sta_methods[i], &mat_methods[i]],
            )
        })
        .collect();

    let hid_cont = Name::new("_HID".into(), &"ACPI0010");
    let cid_cont_v = EISAName::new("PNP0A05");
    let cid_cont = Name::new("_CID".into(), &cid_cont_v);
    let mut cpus_children: Vec<&dyn Aml> = vec![
        &hid_cont,
        &cid_cont,
        &region,
        &csel_field,
        &stat_field,
        &lock,
        &csta,
        &ctfy,
        &cscn,
    ];
    cpus_children.extend(cpu_devs.iter().map(|d| d as &dyn Aml));
    let cpus_dev = aml::Device::new("CPUS".into(), cpus_children);

    // The Generic Event Device: its interrupt runs CSCN.
    let hid_ged = Name::new("_HID".into(), &"ACPI0013");
    let uid_ged = Name::new("_UID".into(), &zero);
    let irq = Interrupt::new(true, true, false, false, x86::GED_GSI);
    let crs_v = ResourceTemplate::new(vec![&irq]);
    let crs = Name::new("_CRS".into(), &crs_v);
    let cscn_call = MethodCall::new("\\_SB_.CPUS.CSCN".into(), vec![]);
    let evt = Method::new("_EVT".into(), 1, true, vec![&cscn_call]);
    let ged = aml::Device::new("GED0".into(), vec![&hid_ged, &uid_ged, &crs, &evt]);

    // The console, at COM1's port and line.
    let hid_com_v = EISAName::new(UART_HID);
    let hid_com = Name::new("_HID".into(), &hid_com_v);
    let uid_com = Name::new("_UID".into(), &zero);
    let com_port = serial::COM1_PORT as u16;
    let com_io = IO::new(com_port, com_port, 1, serial::WINDOW_BYTES as u8);
    let com_irq = edge_irq(serial::COM1_IRQ);
    let com_crs_v = ResourceTemplate::new(vec![&com_io, &com_irq]);
    let com_crs = Name::new("_CRS".into(), &com_crs_v);
    let com1 = aml::Device::new("COM1".into(), vec![&hid_com, &uid_com, &com_crs]);

    // One device per virtio-mmio slot: its window and its line.
    let hid_virtio = Name::new("_HID".into(), &VIRTIO_MMIO_HID);
    let mut v_uids = Vec::with_capacity(virtio.len());
    let mut v_crs = Vec::with_capacity(virtio.len());
    for (i, s) in virtio.iter().enumerate() {
        let base = u32::try_from(s.addr)
            .ok()
            .filter(|b| b.checked_add(MMIO_WINDOW as u32).is_some())
            .ok_or_else(|| {
                VmmError::config(
                    "--disk",
                    format!("virtio-mmio window {:#x} is above 4 GiB", s.addr),
                )
            })?;
        let window = Memory32Fixed::new(true, base, MMIO_WINDOW as u32);
        let line = edge_irq(s.gsi);
        let crs = ResourceTemplate::new(vec![&window, &line]);
        v_uids.push(Name::new("_UID".into(), &(i as u32)));
        v_crs.push(Name::new("_CRS".into(), &crs));
    }
    let v_devs: Vec<aml::Device<'_>> = (0..virtio.len())
        .map(|i| {
            aml::Device::new(
                name4('V', i as u32).as_str().into(),
                vec![&hid_virtio, &v_uids[i], &v_crs[i]],
            )
        })
        .collect();

    let mut sb_children: Vec<&dyn Aml> = vec![&com1];
    sb_children.extend(v_devs.iter().map(|d| d as &dyn Aml));
    sb_children.push(&cpus_dev);
    sb_children.push(&ged);
    let sb = Scope::new("\\_SB_".into(), sb_children);
    let s5_typ = S5_TYPE;
    let s5_pkg = Package::new(vec![&s5_typ, &zero]);
    let s5 = Name::new("_S5_".into(), &s5_pkg);

    let mut out = Vec::new();
    sb.to_aml_bytes(&mut out);
    s5.to_aml_bytes(&mut out);
    Ok(out)
}

/// The MADT: one local APIC per possible vCPU (enabled for the boot vCPUs, online-capable for
/// the rest) and the IOAPIC.
pub fn madt(cpus: u32, cpus_max: u32) -> Vec<u8> {
    let mut m = MADT::new(
        OEM_ID,
        OEM_TABLE,
        OEM_REV,
        LocalInterruptController::Address(x86::LAPIC_ADDR as u32),
    );
    for i in 0..cpus_max {
        let status = if i < cpus {
            EnabledStatus::Enabled
        } else {
            EnabledStatus::DisabledOnlineCapable
        };
        m.add_structure(ProcessorLocalApic::new(i as u8, i as u8, status));
    }
    // The IOAPIC id follows the last possible local APIC id.
    m.add_structure(IoApic::new(cpus_max as u8, x86::IOAPIC_ADDR as u32, 0));
    bytes(&m)
}

fn io_gas(port: u64) -> GAS {
    GAS::new(AddressSpace::SystemIo, 8, 0, AccessSize::ByteAccess, port)
}

/// The FADT: hardware-reduced, with the reset and sleep registers of
/// [`crate::devices::legacy::AcpiPm`].
pub fn fadt(dsdt_addr: u64) -> Vec<u8> {
    let mut b = FADTBuilder::new(OEM_ID, OEM_TABLE, OEM_REV)
        .dsdt_64(dsdt_addr)
        .flag(Flags::HwReducedAcpi)
        .flag(Flags::ResetRegSup)
        .flag(Flags::PwrButton)
        .flag(Flags::SlpButton);
    b.reset_reg = io_gas(ACPI_PM_PORT + RESET_REGISTER);
    b.reset_value = RESET_VALUE;
    b.sleep_control_reg = io_gas(ACPI_PM_PORT + SLEEP_CONTROL);
    b.sleep_status_reg = io_gas(ACPI_PM_PORT + SLEEP_STATUS);
    b.hypervisor_vendor_identity = u64::from_le_bytes(*b"MORUNAVM").into();
    bytes(&b.finalize())
}

/// Every table, placed from [`x86::ACPI_ADDR`] up: RSDP first (where the kernel finds it),
/// then XSDT, FADT, DSDT and MADT, each 8-byte aligned. `virtio` is every virtio-mmio slot.
pub fn tables(cpus: u32, cpus_max: u32, virtio: &[Slot]) -> Result<Vec<Placed>> {
    let align = |a: u64| a.div_ceil(8) * 8;
    let rsdp_addr = x86::ACPI_ADDR;
    let xsdt_addr = align(rsdp_addr + Rsdp::len() as u64);
    // XSDT with three entries.
    let xsdt_len = 36 + 3 * 8;
    let fadt_addr = align(xsdt_addr + xsdt_len);
    let fadt_bytes = fadt(0); // length only, rebuilt below with the DSDT address
    let dsdt_addr = align(fadt_addr + fadt_bytes.len() as u64);

    let mut dsdt_sdt = Sdt::new(*b"DSDT", 36, 6, OEM_ID, OEM_TABLE, OEM_REV);
    dsdt_sdt.append_slice(&dsdt(cpus_max, virtio)?);
    let dsdt_bytes = dsdt_sdt.as_slice().to_vec();
    let madt_addr = align(dsdt_addr + dsdt_bytes.len() as u64);
    let madt_bytes = madt(cpus, cpus_max);

    let mut xsdt = XSDT::new(OEM_ID, OEM_TABLE, OEM_REV);
    xsdt.add_entry(fadt_addr);
    xsdt.add_entry(madt_addr);
    xsdt.add_entry(dsdt_addr);
    let xsdt_bytes = bytes(&xsdt);
    debug_assert_eq!(xsdt_bytes.len() as u64, xsdt_len);

    let placed = vec![
        (rsdp_addr, bytes(&Rsdp::new(OEM_ID, xsdt_addr))),
        (xsdt_addr, xsdt_bytes),
        (fadt_addr, fadt(dsdt_addr)),
        (dsdt_addr, dsdt_bytes),
        (madt_addr, madt_bytes),
    ];
    let end = madt_addr + placed[4].1.len() as u64;
    if end > x86::ACPI_ADDR + x86::ACPI_MAX {
        return Err(VmmError::config(
            "--cpus-max",
            "the ACPI tables do not fit below 1 MiB",
        ));
    }
    Ok(placed)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::layout::{Arch, Layout, MAX_VIRTIO_DEVICES};

    fn sum(b: &[u8]) -> u8 {
        b.iter().fold(0u8, |a, x| a.wrapping_add(*x))
    }

    /// The first `n` virtio slots of an x86_64 machine.
    fn slots(n: usize) -> Vec<Slot> {
        let l = Layout::new(Arch::X86_64, 1 << 30, 1 << 30).unwrap();
        (0..n).map(|i| l.virtio_slot(i).unwrap()).collect()
    }

    /// `ExtendedInterrupt (ResourceConsumer, Edge, ActiveHigh, Exclusive) { gsi }`.
    fn edge_irq_bytes(gsi: u32) -> Vec<u8> {
        let mut v = vec![0x89, 6, 0, 0b0011, 1];
        v.extend(gsi.to_le_bytes());
        v
    }

    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    #[test]
    fn vm_t20_tables_checksum_and_link() {
        let t = tables(2, 8, &slots(MAX_VIRTIO_DEVICES)).unwrap();
        let sig = |b: &[u8]| String::from_utf8_lossy(&b[0..4]).to_string();
        let (rsdp_at, rsdp) = &t[0];
        assert_eq!(*rsdp_at, x86::ACPI_ADDR);
        assert_eq!(&rsdp[0..8], b"RSD PTR ");
        assert_eq!(sum(&rsdp[0..20]), 0, "RSDP v1 checksum");
        assert_eq!(sum(rsdp), 0, "RSDP extended checksum");
        let xsdt_addr = u64::from_le_bytes(rsdp[24..32].try_into().unwrap());
        assert_eq!(xsdt_addr, t[1].0);
        for (_, b) in &t[1..] {
            assert_eq!(sum(b), 0, "{} checksum", sig(b));
            let len = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
            assert_eq!(len, b.len(), "{} length", sig(b));
        }
        assert_eq!(
            t[1..].iter().map(|(_, b)| sig(b)).collect::<Vec<_>>(),
            vec!["XSDT", "FACP", "DSDT", "APIC"]
        );
        // The XSDT points at FADT, MADT, DSDT; the FADT at the DSDT.
        let xsdt = &t[1].1;
        let entries: Vec<u64> = xsdt[36..]
            .chunks(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(entries, vec![t[2].0, t[4].0, t[3].0]);
        let fadt = &t[2].1;
        assert_eq!(
            u64::from_le_bytes(fadt[140..148].try_into().unwrap()),
            t[3].0
        );
        // Tables do not overlap and fit the scanned area.
        for w in t.windows(2) {
            assert!(w[0].0 + w[0].1.len() as u64 <= w[1].0);
        }
    }

    #[test]
    fn vm_t20_fadt_registers() {
        let f = fadt(0x1234);
        let flags = u32::from_le_bytes(f[112..116].try_into().unwrap());
        assert_ne!(flags & (1 << 20), 0, "hardware reduced");
        assert_ne!(flags & (1 << 10), 0, "reset register supported");
        // RESET_REG: system IO at the PM block's reset register, value 1.
        assert_eq!(f[116], 1);
        assert_eq!(
            u64::from_le_bytes(f[120..128].try_into().unwrap()),
            ACPI_PM_PORT + RESET_REGISTER
        );
        assert_eq!(f[128], RESET_VALUE);
        // SLEEP_CONTROL_REG and SLEEP_STATUS_REG.
        assert_eq!(
            u64::from_le_bytes(f[248..256].try_into().unwrap()),
            ACPI_PM_PORT + SLEEP_CONTROL
        );
        assert_eq!(
            u64::from_le_bytes(f[260..268].try_into().unwrap()),
            ACPI_PM_PORT + SLEEP_STATUS
        );
    }

    #[test]
    fn vm_t21_madt_marks_hot_add_cpus_online_capable() {
        let m = madt(2, 4);
        // Header 44 bytes, then 4 local APICs of 8 bytes, then the IOAPIC of 12.
        assert_eq!(m.len(), 44 + 4 * 8 + 12);
        let flags: Vec<u32> = (0..4)
            .map(|i| {
                let o = 44 + i * 8;
                assert_eq!((m[o], m[o + 1], m[o + 3]), (0, 8, i as u8));
                u32::from_le_bytes(m[o + 4..o + 8].try_into().unwrap())
            })
            .collect();
        assert_eq!(flags, vec![1, 1, 2, 2]);
        assert_eq!(m[44 + 32], 1, "IOAPIC entry");
        assert_eq!(
            u32::from_le_bytes(m[36..40].try_into().unwrap()),
            x86::LAPIC_ADDR as u32
        );
    }

    #[test]
    fn vm_t21_dsdt_names_every_possible_cpu_and_the_ged() {
        let d = dsdt(3, &[]).unwrap();
        let has = |s: &[u8]| d.windows(s.len()).any(|w| w == s);
        for n in [
            &b"C000"[..],
            b"C001",
            b"C002",
            b"CSCN",
            b"CSTA",
            b"CTFY",
            b"GED0",
            b"ACPI0013",
            b"ACPI0007",
            b"ACPI0010",
            b"_S5_",
            b"PRST",
        ] {
            assert!(has(n), "{}", String::from_utf8_lossy(n));
        }
        assert!(!has(b"C003"));
        // The hot-plug controller's address appears as the OpRegion offset.
        assert!(has(&(x86::CPU_HOTPLUG_ADDR as u32).to_le_bytes()));
        // Too many vCPUs for the space below 1 MiB would be refused, not truncated.
        assert!(tables(1, 64, &slots(MAX_VIRTIO_DEVICES)).is_ok());
    }

    /// A hardware-reduced x86 guest has no legacy PIC, so it maps a GSI to an IRQ only when a
    /// `_CRS` names it: every virtio-mmio window and COM1 must be in the DSDT with its line.
    #[test]
    fn vm_t20_dsdt_describes_every_device_that_interrupts() {
        let s = slots(MAX_VIRTIO_DEVICES);
        let d = dsdt(2, &s).unwrap();
        let mut hid = vec![0x08, b'_', b'H', b'I', b'D', 0x0d];
        hid.extend(VIRTIO_MMIO_HID.as_bytes());
        hid.push(0);
        assert_eq!(count(&d, &hid), s.len(), "one LNRO0005 per slot");
        for (i, slot) in s.iter().enumerate() {
            assert!(count(&d, name4('V', i as u32).as_bytes()) == 1, "V{i:03X}");
            // Memory32Fixed (ReadWrite, base, 4 KiB), then the slot's edge-triggered line.
            let mut crs = vec![0x86, 9, 0, 1];
            crs.extend((slot.addr as u32).to_le_bytes());
            crs.extend((MMIO_WINDOW as u32).to_le_bytes());
            crs.extend(edge_irq_bytes(slot.gsi));
            assert_eq!(count(&d, &crs), 1, "slot {i} at {:#x}", slot.addr);
        }
        assert_eq!(count(&d, name4('V', s.len() as u32).as_bytes()), 0);
        // COM1: EISAID ("PNP0501"), IO (Decode16, 0x3F8, 0x3F8, 1, 8), IRQ 4.
        let mut com = vec![0x08, b'_', b'H', b'I', b'D', 0x0c, 0x41, 0xd0, 0x05, 0x01];
        assert_eq!(count(&d, &com), 1);
        com = vec![0x47, 1, 0xf8, 0x03, 0xf8, 0x03, 1, 8];
        com.extend(edge_irq_bytes(serial::COM1_IRQ));
        assert_eq!(count(&d, &com), 1);
        // The GED keeps its own line; no two devices share one.
        assert_eq!(count(&d, &edge_irq_bytes(x86::GED_GSI)), 1);
        let mut gsis: Vec<u32> = s.iter().map(|s| s.gsi).collect();
        gsis.extend([serial::COM1_IRQ, x86::GED_GSI]);
        gsis.sort_unstable();
        gsis.dedup();
        assert_eq!(gsis.len(), s.len() + 2);
        // A window the 32-bit descriptor cannot express is refused.
        let high = [Slot {
            addr: 1 << 32,
            gsi: 5,
        }];
        assert!(dsdt(1, &high).is_err());
    }

    /// The guest kernel's merged configuration (allnoconfig, `moruna.config`, then the x86_64
    /// fragment): `CONFIG_NAME` to `y` or `n`.
    fn x86_guest_config() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for f in [
            include_str!("../../../guest/kernel/moruna.config"),
            include_str!("../../../guest/kernel/x86_64.config"),
        ] {
            for line in f.lines() {
                if let Some((k, v)) = line.strip_prefix("CONFIG_").and_then(|l| l.split_once('=')) {
                    m.insert(k.to_string(), v.to_string());
                }
            }
        }
        m
    }

    /// The guest kernel binds what these tables describe. ACPICA installs a default handler
    /// for every default address space, PCI_Config among them, and without `CONFIG_PCI` that
    /// install fails with AE_BAD_PARAMETER, so no table loads ("During Region
    /// initialization"). PNPACPI and the 8250 PNP driver take COM1 with its mapped line; the
    /// virtio-mmio driver binds LNRO0005.
    #[test]
    fn vm_t20_guest_kernel_binds_what_the_tables_describe() {
        let c = x86_guest_config();
        let on = |k: &str| c.get(k).map(String::as_str) == Some("y");
        for k in [
            "ACPI",
            "PCI",
            "PNP",
            "PNPACPI",
            "SERIAL_8250",
            "SERIAL_8250_PNP",
            "VIRTIO_MMIO",
            "VIRTIO_BLK",
            "VIRTIO_MEM",
            "VIRTIO_VSOCKETS",
            "ACPI_HOTPLUG_CPU",
        ] {
            assert!(on(k), "the x86_64 guest kernel needs CONFIG_{k}=y");
        }
        // Nothing names a device on the command line any more.
        assert!(!on("VIRTIO_MMIO_CMDLINE_DEVICES"));
    }
}
