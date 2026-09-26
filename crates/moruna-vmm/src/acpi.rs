//! The x86_64 ACPI tables: a hardware-reduced platform with the CPUs, the CPU hot-plug
//! controller, the Generic Event Device that announces a hot-added CPU, and the sleep and
//! reset registers a guest powers off and reboots through.
//!
//! Tables (all at [`x86::ACPI_ADDR`], where the kernel scans for the RSDP): RSDP, XSDT, FADT,
//! DSDT, MADT. The virtio-mmio devices are not in ACPI: the kernel command line names them.
//! Built with `acpi_tables`, so they are generated and checked on any host.

use acpi_tables::Aml;
use acpi_tables::aml::{
    self, Acquire, Add, Arg, BufferData, EISAName, Equal, Field, FieldAccessType, FieldEntry,
    FieldLockRule, FieldUpdateRule, If, Interrupt, LessThan, Local, Method, MethodCall, Name,
    Notify, OpRegion, OpRegionSpace, Package, Path, Release, ResourceTemplate, Return, Scope,
    Store, While,
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
use crate::error::{Result, VmmError};
use crate::layout::x86;

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

/// The DSDT body: `\_SB.CPUS` with one device per possible vCPU and the scan method, the GED,
/// and `\_S5`.
pub fn dsdt(cpus_max: u32) -> Vec<u8> {
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

    let sb = Scope::new("\\_SB_".into(), vec![&cpus_dev, &ged]);
    let s5_typ = S5_TYPE;
    let s5_pkg = Package::new(vec![&s5_typ, &zero]);
    let s5 = Name::new("_S5_".into(), &s5_pkg);

    let mut out = Vec::new();
    sb.to_aml_bytes(&mut out);
    s5.to_aml_bytes(&mut out);
    out
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
/// then XSDT, FADT, DSDT and MADT, each 8-byte aligned.
pub fn tables(cpus: u32, cpus_max: u32) -> Result<Vec<Placed>> {
    let align = |a: u64| a.div_ceil(8) * 8;
    let rsdp_addr = x86::ACPI_ADDR;
    let xsdt_addr = align(rsdp_addr + Rsdp::len() as u64);
    // XSDT with three entries.
    let xsdt_len = 36 + 3 * 8;
    let fadt_addr = align(xsdt_addr + xsdt_len);
    let fadt_bytes = fadt(0); // length only, rebuilt below with the DSDT address
    let dsdt_addr = align(fadt_addr + fadt_bytes.len() as u64);

    let mut dsdt_sdt = Sdt::new(*b"DSDT", 36, 6, OEM_ID, OEM_TABLE, OEM_REV);
    dsdt_sdt.append_slice(&dsdt(cpus_max));
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
    use super::*;

    fn sum(b: &[u8]) -> u8 {
        b.iter().fold(0u8, |a, x| a.wrapping_add(*x))
    }

    #[test]
    fn vm_t20_tables_checksum_and_link() {
        let t = tables(2, 8).unwrap();
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
        let d = dsdt(3);
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
        assert!(tables(1, 64).is_ok());
    }
}
