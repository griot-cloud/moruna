//! The machine assembled for both architectures on any host, the resize controller, the
//! status port and the exit-code rule.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::*;
use crate::config::{DiskConfig, GIB, MIB, VsockConfig};
use crate::devices::Device;
use crate::devices::mmio::regs;
use crate::devices::vsock::packet::{HDR_BYTES, Header, op};
use crate::fdt::GicVersion;
use crate::testing::{CountingIrq, DriverQueue, bring_up, scratch_dir};

struct Irqs(BTreeMap<u32, Arc<CountingIrq>>);

impl Irqs {
    fn factory(&mut self) -> impl FnMut(u32) -> Result<Arc<dyn Irq>> + '_ {
        |gsi| {
            let c = Arc::new(CountingIrq::default());
            self.0.insert(gsi, c.clone());
            Ok(c as Arc<dyn Irq>)
        }
    }
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn config(dir: &Path, cpus: u32, cpus_max: u32) -> VmConfig {
    let disk = dir.join("d.img");
    std::fs::write(&disk, vec![0u8; 8192]).unwrap();
    VmConfig {
        image: dir.join("img"),
        disks: vec![DiskConfig {
            path: disk,
            read_only: false,
        }],
        memory_bytes: 256 * MIB,
        memory_max_bytes: GIB,
        cpus,
        cpus_max,
        vsock: VsockConfig {
            cid: 3,
            uds_path: dir.join("v.sock"),
        },
        control_socket: dir.join("c.sock"),
    }
}

fn image(dir: &Path) -> GuestImage {
    let img = dir.join("img");
    std::fs::create_dir_all(&img).unwrap();
    std::fs::write(img.join("kernel"), b"k").unwrap();
    std::fs::write(img.join("initramfs.cpio"), vec![7u8; 10000]).unwrap();
    std::fs::write(img.join("rootfs.erofs"), vec![0u8; 4096]).unwrap();
    GuestImage {
        kernel: img.join("kernel"),
        initramfs: Some(img.join("initramfs.cpio")),
        rootfs: Some(img.join("rootfs.erofs")),
        digest: None,
    }
}

fn machine(arch: Arch, cpus: u32, cpus_max: u32) -> (Machine, Irqs, Sink, std::path::PathBuf) {
    let dir = scratch_dir("mach");
    let c = config(&dir, cpus, cpus_max);
    let mut irqs = Irqs(BTreeMap::new());
    let sink = Sink::default();
    let m = Machine::build(
        arch,
        &c,
        image(&dir),
        &mut irqs.factory(),
        Box::new(sink.clone()),
    )
    .unwrap();
    (m, irqs, sink, dir)
}

fn rd32(bus: &Bus, addr: u64) -> u32 {
    let mut b = [0u8; 4];
    assert!(bus.read(addr, &mut b), "{addr:#x}");
    u32::from_le_bytes(b)
}

#[test]
fn vm_t23_x86_machine_has_exactly_its_devices() {
    let (m, irqs, sink, _dir) = machine(Arch::X86_64, 1, 4);
    // vsock, mem, root filesystem, one disk.
    assert_eq!(m.virtio.len(), 4);
    let ids: Vec<u32> = m
        .virtio
        .iter()
        .map(|s| rd32(&m.mmio, s.addr + regs::DEVICE_ID))
        .collect();
    assert_eq!(ids, vec![19, 24, 2, 2]);
    for s in &m.virtio {
        assert_eq!(rd32(&m.mmio, s.addr), crate::devices::mmio::MAGIC_VALUE);
        assert!(m.cmdline.contains(&virtio_mmio_param(s)));
    }
    // Nothing else answers in the device window.
    let past = m.virtio.last().unwrap().addr + crate::layout::MMIO_WINDOW;
    let mut b = [0u8; 4];
    assert!(!m.mmio.read(past, &mut b));
    // The console, the stop ports and the hot-plug controller.
    assert!(m.pio.write(serial::COM1_PORT, b"x"));
    assert_eq!(&*sink.0.lock().unwrap(), b"x");
    assert!(m.pio.find(I8042_COMMAND_PORT).is_some());
    assert!(m.pio.find(ACPI_PM_PORT + 2).is_some());
    assert!(m.mmio.find(x86::CPU_HOTPLUG_ADDR).is_some());
    assert!(m.hotplug.is_some() && m.ged.is_some() && m.vmem.is_some());
    // Interrupt lines: COM1, one per virtio device, the GED.
    let gsis: Vec<u32> = irqs.0.keys().copied().collect();
    assert_eq!(gsis, vec![4, 5, 6, 7, 8, x86::GED_GSI]);
    assert!(m.cmdline.contains("memhp_default_state=online_movable"));
    // An i8042 reset stops the machine.
    m.pio.write(I8042_COMMAND_PORT, &[0xfe]);
    assert_eq!(m.stop.reason(), Some(StopReason::Reset));
}

#[test]
fn vm_t23_x86_boot_data() {
    let (m, _, _, _) = machine(Arch::X86_64, 2, 2);
    let initrd = m
        .write_boot_data(32 * MIB, GicVersion::V3)
        .unwrap()
        .unwrap();
    assert_eq!(initrd.1, 10000);
    assert!(initrd.0 + initrd.1 <= m.layout.low_ram_end());
    let mut sig = [0u8; 8];
    m.mem
        .read_slice(&mut sig, GuestAddress(x86::ACPI_ADDR))
        .unwrap();
    assert_eq!(&sig, b"RSD PTR ");
    let mut c = vec![0u8; m.cmdline.len() + 1];
    m.mem
        .read_slice(&mut c, GuestAddress(x86::CMDLINE_ADDR))
        .unwrap();
    assert_eq!(&c[..m.cmdline.len()], m.cmdline.as_bytes());
    assert_eq!(c[m.cmdline.len()], 0);
    let b: u8 = m.mem.read_obj(GuestAddress(initrd.0)).unwrap();
    assert_eq!(b, 7);
    // A kernel that leaves no room for the initramfs is refused.
    assert!(m.write_boot_data(256 * MIB - 100, GicVersion::V3).is_err());
}

#[test]
fn vm_t23_arm_machine_and_device_tree() {
    let (m, irqs, sink, _) = machine(Arch::Aarch64, 2, 2);
    assert!(m.hotplug.is_none() && m.ged.is_none());
    assert!(m.pio.is_empty());
    assert!(m.mmio.write(arm::SERIAL_ADDR, b"y"));
    assert_eq!(&*sink.0.lock().unwrap(), b"y");
    assert!(!m.cmdline.contains("virtio_mmio.device"));
    assert!(irqs.0.contains_key(&arm::SERIAL_GSI));
    m.write_boot_data(arm::RAM_START + 32 * MIB, GicVersion::V2)
        .unwrap();
    let magic: u32 = m.mem.read_obj(GuestAddress(m.layout.fdt_addr())).unwrap();
    assert_eq!(u32::from_be(magic), 0xd00d_feed);

    // aarch64 cannot hot-add vCPUs, so a boot that asks for it is refused up front.
    let dir = scratch_dir("mach-arm");
    let c = config(&dir, 1, 2);
    let mut irqs = Irqs(BTreeMap::new());
    let r = Machine::build(
        Arch::Aarch64,
        &c,
        image(&dir),
        &mut irqs.factory(),
        Box::new(Sink::default()),
    );
    assert!(matches!(
        r,
        Err(VmmError::Config {
            field: "--cpus-max",
            ..
        })
    ));
}

#[test]
fn vm_t23_invalid_config_is_refused_before_anything_is_built() {
    let dir = scratch_dir("mach-bad");
    let mut c = config(&dir, 1, 1);
    c.memory_bytes = 1;
    let mut irqs = Irqs(BTreeMap::new());
    let r = Machine::build(
        Arch::X86_64,
        &c,
        image(&dir),
        &mut irqs.factory(),
        Box::new(Sink::default()),
    );
    assert!(r.is_err());
    assert!(irqs.0.is_empty());
    assert!(!dir.join("v.sock").exists());
}

#[test]
fn vm_t24_resize_through_the_controller() {
    let (m, irqs, _, _) = machine(Arch::X86_64, 1, 4);
    let c = m.controller();
    let s = c.status();
    assert_eq!(
        (s.memory_bytes, s.memory_plugged_bytes, s.memory_max_bytes),
        (256 * MIB, 256 * MIB, GIB)
    );
    assert_eq!((s.cpus, s.cpus_max, s.stopped), (1, 4, false));

    let s = c.resize_memory(512 * MIB).unwrap();
    assert_eq!(s.memory_bytes, 512 * MIB);
    {
        let t = m.vmem.as_ref().unwrap().lock().unwrap();
        assert_eq!(t.device().requested_size(), 256 * MIB);
    }
    // The guest was told: the virtio-mem line was raised (slot 1 is GSI 6).
    assert_eq!(irqs.0[&6].get(), 1);
    assert!(
        matches!(c.resize_memory(128 * MIB), Err(VmmError::Control(m)) if m.contains("boot memory"))
    );
    assert!(
        matches!(c.resize_memory(2 * GIB), Err(VmmError::Control(m)) if m.contains("--memory-max"))
    );
    assert!(c.resize_memory(512 * MIB + 4096).is_err());

    let s = c.resize_cpus(3).unwrap();
    assert_eq!(s.cpus, 3);
    assert_eq!(
        irqs.0[&x86::GED_GSI].get(),
        1,
        "the GED announced the new vCPUs"
    );
    c.resize_cpus(3).unwrap();
    assert_eq!(irqs.0[&x86::GED_GSI].get(), 1, "nothing new, no event");
    assert!(c.resize_cpus(5).is_err());

    c.stop();
    assert!(c.status().stopped);
    assert_eq!(m.stop.reason(), Some(StopReason::Killed));
}

#[test]
fn vm_t24_a_fixed_guest_refuses_resize() {
    let (m, _, _, _) = machine(Arch::Aarch64, 1, 1);
    let dir = scratch_dir("fixed");
    let mut cfg = config(&dir, 1, 1);
    cfg.memory_max_bytes = cfg.memory_bytes;
    let mut irqs = Irqs(BTreeMap::new());
    let fixed = Machine::build(
        Arch::Aarch64,
        &cfg,
        image(&dir),
        &mut irqs.factory(),
        Box::new(Sink::default()),
    )
    .unwrap();
    let c = fixed.controller();
    assert!(fixed.vmem.is_none());
    assert!(
        matches!(c.resize_memory(256 * MIB), Err(VmmError::Control(m)) if m.contains("--memory-max equal"))
    );
    assert!(c.resize_cpus(1).is_ok());
    assert!(matches!(c.resize_cpus(2), Err(VmmError::Control(m)) if m.contains("hot-add")));
    drop(m);
}

#[test]
fn vm_t25_status_port_records_the_first_valid_code() {
    for (line, want) in [
        ("exit 0", Some(0)),
        ("exit 130\n", Some(130)),
        (" exit 4 ", Some(4)),
        ("exit 256", None),
        ("exit -1", None),
        ("quit 1", None),
        ("exit x", None),
    ] {
        assert_eq!(parse_status_line(line), want, "{line}");
    }
    let p = StatusPort::default();
    assert_eq!(p.reported(), None);
    let connect = p.connector();
    let mut s = connect().unwrap();
    s.write_all(b"exit 3\n").unwrap();
    drop(s);
    for _ in 0..200 {
        if p.reported().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(p.reported(), Some(3));
    // A second report does not overwrite the first; garbage is ignored.
    let (a, mut b) = UnixStream::pair().unwrap();
    b.write_all(b"exit 9\n").unwrap();
    p.serve(a);
    let (a, mut b) = UnixStream::pair().unwrap();
    b.write_all(&[b'e'; 100]).unwrap();
    p.serve(a);
    assert_eq!(p.reported(), Some(3));
}

#[test]
fn vm_t26_exit_code_rule() {
    use StopReason::*;
    assert_eq!(exit_code(&PowerOff, Some(0), false), 0);
    assert_eq!(exit_code(&PowerOff, Some(4), false), 4);
    assert_eq!(exit_code(&Reset, Some(130), false), 130);
    assert_eq!(exit_code(&PowerOff, None, false), EXIT_NO_CODE);
    assert_eq!(exit_code(&Killed, None, false), EXIT_NO_CODE);
    assert_eq!(exit_code(&Reset, Some(0), true), EXIT_GUEST_PANIC);
    assert_eq!(exit_code(&Crash, None, false), EXIT_GUEST_PANIC);
    assert_eq!(
        exit_code(&MonitorError("x".into()), Some(0), false),
        EXIT_MONITOR
    );
}

#[test]
fn vm_t26_finish_reports_panics_with_the_console() {
    let (m, _, _, _) = machine(Arch::X86_64, 1, 1);
    for b in b"booting\nKernel panic - not syncing: VFS\n" {
        m.pio.write(serial::COM1_PORT, &[*b]);
    }
    m.stop.request(StopReason::Reset);
    let mut err = Vec::new();
    assert_eq!(m.finish(&mut err), EXIT_GUEST_PANIC);
    let text = String::from_utf8(err).unwrap();
    assert!(text.contains("guest kernel panic") && text.contains("not syncing: VFS"));

    let (m, _, _, _) = machine(Arch::X86_64, 1, 1);
    m.stop.request(StopReason::PowerOff);
    let mut err = Vec::new();
    assert_eq!(m.finish(&mut err), EXIT_NO_CODE);
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("without reporting")
    );

    let (m, _, _, _) = machine(Arch::X86_64, 1, 1);
    m.stop
        .request(StopReason::MonitorError("kvm run: EFAULT".into()));
    let mut err = Vec::new();
    assert_eq!(m.finish(&mut err), EXIT_MONITOR);
    assert!(String::from_utf8(err).unwrap().contains("EFAULT"));

    let (m, _, _, _) = machine(Arch::X86_64, 1, 1);
    let mut err = Vec::new();
    assert_eq!(
        m.finish(&mut err),
        EXIT_MONITOR,
        "no reason recorded is a monitor fault"
    );
}

#[test]
fn vm_t27_vsock_backend_serves_host_connections() {
    let (m, _, _, dir) = machine(Arch::X86_64, 1, 1);
    // Inactive device: the backend waits without touching the host side.
    let mut host = UnixStream::connect(dir.join("v.sock")).unwrap();
    host.write_all(b"CONNECT 5000\n").unwrap();
    m.vsock_backend_once(Duration::from_millis(1)).unwrap();

    // Activate it the way the guest driver would, with receive buffers posted.
    let slot = m.virtio[0];
    let mut rxq = DriverQueue::new(0x10_0000, 16);
    let txq = DriverQueue::new(0x11_0000, 16);
    let evq = DriverQueue::new(0x12_0000, 16);
    {
        let mut t = m.vsock.lock().unwrap();
        let dev: &mut dyn Device = &mut *t;
        bring_up(dev, &[&rxq, &txq, &evq]);
    }
    rxq.add(&m.mem, &[(0x20_0000, 4096, true)]);
    m.mmio
        .write(slot.addr + regs::QUEUE_NOTIFY, &0u32.to_le_bytes());
    for _ in 0..20 {
        m.vsock_backend_once(Duration::from_millis(20)).unwrap();
    }
    let used = rxq.used(&m.mem);
    assert_eq!(used.len(), 1);
    let mut raw = [0u8; HDR_BYTES];
    m.mem.read_slice(&mut raw, GuestAddress(0x20_0000)).unwrap();
    let h = Header::decode(&raw);
    assert_eq!((h.op, h.dst_port), (op::REQUEST, GUEST_AGENT_PORT));

    // The backend loop ends when the machine stops.
    m.stop.request(StopReason::PowerOff);
    m.vsock_backend();
}

#[test]
fn vm_t23_load_file_errors_name_the_file() {
    let mem = crate::testing::guest_memory(4096);
    let dir = scratch_dir("load");
    assert!(matches!(
        load_file(&mem, &dir.join("absent"), 0),
        Err(VmmError::Image { .. })
    ));
    std::fs::write(dir.join("big"), vec![1u8; 8192]).unwrap();
    assert!(matches!(
        load_file(&mem, &dir.join("big"), 0),
        Err(VmmError::Image { msg, .. }) if msg.contains("does not fit")
    ));
    std::fs::write(dir.join("ok"), b"abc").unwrap();
    assert_eq!(load_file(&mem, &dir.join("ok"), 16).unwrap(), 3);
    let tiny = Layout {
        arch: Arch::X86_64,
        ram: vec![(0, u64::MAX / 2)],
        hotplug: None,
    };
    assert!(guest_memory(&tiny).is_err());
}
