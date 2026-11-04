//! A real QEMU boot through the launcher, inside the jail, with the explicit TCG proof
//! acceleration (no KVM on the proof machine). The guest is a minimal Multiboot kernel built
//! here: it either halts (so the jailed processes can be inspected and QEMU quit over QMP) or
//! writes 0x10 to the debug-exit port (QEMU exits with 33). Run explicitly with MORUNA_QEMU (a
//! QEMU built with seccomp, vhost-user and slirp) and MORUNA_JAIL_CGROUP (a delegated cgroup v2
//! directory, or `none`).
#![cfg(all(target_os = "linux", feature = "native-tcg-proof"))]

use moruna_vmm::{Budget, jail::Cgroup, microkernel};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// An x86_64 ELF with Multiboot 1 (QEMU's loader) and Multiboot 2 (the launcher's check)
/// headers, loaded at 1 MiB, whose 32-bit entry runs `code`.
fn kernel(code: &[u8]) -> Vec<u8> {
    let mut elf = vec![0u8; 0x2000];
    let put = |elf: &mut Vec<u8>, at: usize, bytes: &[u8]| {
        elf[at..at + bytes.len()].copy_from_slice(bytes)
    };
    put(&mut elf, 0, b"\x7fELF\x02\x01\x01");
    put(&mut elf, 16, &2u16.to_le_bytes());
    put(&mut elf, 18, &62u16.to_le_bytes());
    put(&mut elf, 20, &1u32.to_le_bytes());
    put(&mut elf, 24, &0x10_0030u64.to_le_bytes());
    put(&mut elf, 32, &64u64.to_le_bytes());
    put(&mut elf, 52, &64u16.to_le_bytes());
    put(&mut elf, 54, &56u16.to_le_bytes());
    put(&mut elf, 56, &1u16.to_le_bytes());
    put(&mut elf, 58, &64u16.to_le_bytes());
    let ph = 64;
    put(&mut elf, ph, &1u32.to_le_bytes());
    put(&mut elf, ph + 4, &7u32.to_le_bytes());
    for (offset, value) in [
        (8, 0x1000u64),
        (16, 0x10_0000),
        (24, 0x10_0000),
        (32, 0x1000),
        (40, 0x1000),
        (48, 0x1000),
    ] {
        put(&mut elf, ph + offset, &value.to_le_bytes());
    }
    let mb1 = 0x1BAD_B002u32;
    put(&mut elf, 0x1000, &mb1.to_le_bytes());
    put(&mut elf, 0x1008, &0u32.wrapping_sub(mb1).to_le_bytes());
    let mb2 = 0xE852_50D6u32;
    put(&mut elf, 0x1010, &mb2.to_le_bytes());
    put(&mut elf, 0x1018, &24u32.to_le_bytes());
    put(
        &mut elf,
        0x101c,
        &0u32.wrapping_sub(mb2).wrapping_sub(24).to_le_bytes(),
    );
    put(&mut elf, 0x1024, &8u32.to_le_bytes());
    put(&mut elf, 0x1030, code);
    elf
}
/// hlt; jmp hlt (interrupts are off: the machine idles until QEMU is told to quit).
const HALT: &[u8] = &[0xF4, 0xEB, 0xFD];
/// mov dx, 0xf4; mov eax, 0x10; out dx, eax; hlt; jmp hlt.
const EXIT_0X10: &[u8] = &[
    0x66, 0xBA, 0xF4, 0x00, 0xB8, 0x10, 0x00, 0x00, 0x00, 0xEF, 0xF4, 0xEB, 0xFD,
];

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn artifact(dir: &Path, name: &str, bytes: &[u8]) -> serde_json::Value {
    fs::write(dir.join(name), bytes).unwrap();
    let digest: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    serde_json::json!({"path": name, "bytes": bytes.len(), "sha256": digest})
}

/// A descriptor directory holding the kernel, an initramfs, one layer and one writable disk.
fn descriptor(code: &[u8], user_network: bool) -> (Scratch, PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mj-{}-{}",
        std::process::id(),
        nanos % 1_000_000_000
    ));
    fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    fs::write(dir.join("home.img"), vec![0u8; 1 << 20]).unwrap();
    let document = serde_json::json!({
        "version": 1, "architecture": "x86_64", "boot_mode": "multiboot2",
        "kernel": artifact(&dir, "kernel.elf", &kernel(code)),
        "initramfs": artifact(&dir, "initramfs.cpio", b"070701 not a real archive; a multiboot module"),
        "layers": [{"artifact": artifact(&dir, "layer.raw", &[7u8; 4096])}],
        "writable_disks": [{"path": "home.img", "bytes": 1 << 20, "serial": "jail-home"}],
        "user_network": user_network, "cid": 3, "agent_port": 5000,
        "resources": {"node_memory_bytes": 256u64 << 20, "job_memory_bytes": 64u64 << 20,
                      "job_account_bytes": 96u64 << 20, "scratch_bytes": 16u64 << 20, "cpus": 1}
    });
    let path = dir.join("node.json");
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    (Scratch(dir), path)
}

fn budget() -> Budget {
    Budget {
        memory_bytes: 64 << 20,
        memory_max_bytes: 64 << 20,
        cpus: 1,
        cpus_max: 1,
    }
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("required {name}"))
}

fn launch(code: &[u8], user_network: bool) -> (Scratch, microkernel::RunningNode) {
    let (scratch, descriptor) = descriptor(code, user_network);
    let node = microkernel::launch_tcg_proof(
        &descriptor,
        budget(),
        &scratch.0.join("run"),
        Path::new(&required("MORUNA_QEMU")),
        Path::new(env!("CARGO_BIN_EXE_moruna-vhost-vsock")),
        &Cgroup::parse(&required("MORUNA_JAIL_CGROUP")).unwrap(),
    )
    .unwrap();
    (scratch, node)
}

fn status(pid: i32) -> std::collections::BTreeMap<String, String> {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect()
}

fn tree(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for item in fs::read_dir(dir).unwrap() {
        let item = item.unwrap();
        let path = item.path();
        let inside = Path::new("/").join(path.strip_prefix(root).unwrap());
        let kind = item.file_type().unwrap();
        out.push(format!(
            "{}{}",
            inside.display(),
            if kind.is_dir() { "/" } else { "" }
        ));
        if kind.is_dir()
            && !inside.starts_with("/usr/share/qemu")
            && !inside.ends_with("share/qemu")
        {
            tree(root, &path, out);
        }
    }
}

#[test]
#[ignore = "requires MORUNA_QEMU (seccomp, vhost-user, slirp) and MORUNA_JAIL_CGROUP"]
fn qemu_and_its_vsock_back_end_boot_jailed_and_leave_nothing() {
    let (scratch, mut node) = launch(HALT, true);
    let runtime = node.runtime_dir().to_path_buf();
    let leaf = node.cgroup().map(Path::to_path_buf);
    // Let the guest reach its halt (SeaBIOS, the Multiboot loader) under TCG.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        node.try_wait().unwrap().is_none(),
        "QEMU ended: {}",
        node.logs()
    );
    // SAFETY: plain getters.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let me = |kind: &str| fs::read_link(format!("/proc/self/ns/{kind}")).unwrap();
    for (name, pid) in ["qemu", "vsock back end"].into_iter().zip(node.processes()) {
        let s = status(pid);
        println!(
            "{name}: pid {pid}; Uid {}; Gid {}; Groups {}; NSpid {}; Seccomp {}; Seccomp_filters {}; NoNewPrivs {}; CapEff {}; CapBnd {}",
            s["Uid"],
            s["Gid"],
            s["Groups"],
            s["NSpid"],
            s["Seccomp"],
            s.get("Seccomp_filters").map_or("?", String::as_str),
            s["NoNewPrivs"],
            s["CapEff"],
            s["CapBnd"]
        );
        let (u, g) = (uid.to_string(), gid.to_string());
        assert_eq!(
            s["Uid"].split_whitespace().collect::<Vec<_>>(),
            [u.as_str(); 4],
            "{name}"
        );
        assert_eq!(
            s["Gid"].split_whitespace().collect::<Vec<_>>(),
            [g.as_str(); 4],
            "{name}"
        );
        assert_eq!(
            s["NSpid"].split_whitespace().nth(1),
            Some("1"),
            "{name} is PID 1 of its own namespace"
        );
        assert_eq!(s["Seccomp"], "2", "{name} filtered");
        assert_eq!(s["NoNewPrivs"], "1", "{name}");
        for cap in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
            assert_eq!(s[cap], "0000000000000000", "{name} {cap}");
        }
        for kind in ["user", "mnt", "pid", "net", "ipc", "uts", "cgroup"] {
            assert_ne!(
                me(kind),
                fs::read_link(format!("/proc/{pid}/ns/{kind}")).unwrap(),
                "{name} shares {kind}"
            );
        }
        let net = fs::read_to_string(format!("/proc/{pid}/net/dev")).unwrap();
        let interfaces: Vec<&str> = net
            .lines()
            .skip(2)
            .filter_map(|l| l.split(':').next())
            .map(str::trim)
            .collect();
        assert_eq!(interfaces, ["lo"], "{name} network");
        let root = PathBuf::from(format!("/proc/{pid}/root"));
        let mut seen = Vec::new();
        tree(&root, &root, &mut seen);
        println!("{name} root:\n  {}", seen.join("\n  "));
        // Every mount the jail holds: where, with which options, of which file system. Each
        // one is nosuid; only the program and its libraries are not noexec; only a device bind
        // is not nodev; only the disk, the serial log and the socket directories are rw.
        let mounts = fs::read_to_string(format!("/proc/{pid}/mountinfo")).unwrap();
        let mut table = Vec::new();
        for line in mounts.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let separator = fields.iter().position(|f| *f == "-").unwrap();
            let (point, options, kind) = (fields[4], fields[5], fields[separator + 1]);
            assert!(
                options.split(',').any(|o| o == "nosuid"),
                "{name} {point} {options}"
            );
            let rw = options.split(',').any(|o| o == "rw");
            assert!(
                !rw || point.ends_with("/home.img")
                    || point.ends_with("/serial.log")
                    || point.ends_with("/qmp")
                    || point.ends_with("/guest")
                    || (name == "vsock back end" && point.ends_with("/vsock"))
                    || point == "/dev/null",
                "{name} writes {point}"
            );
            table.push(format!("{point} {options} {kind}"));
        }
        println!("{name} mounts:\n  {}", table.join("\n  "));
        let neighbour = scratch.0.join("node.json");
        assert!(
            !seen
                .iter()
                .any(|p| p.starts_with(neighbour.to_str().unwrap())),
            "{name} sees the descriptor"
        );
        assert!(
            !seen
                .iter()
                .any(|p| p == "/etc/passwd" || p.starts_with("/proc") || p.starts_with("/sys"))
        );
        let devices: Vec<&str> = seen
            .iter()
            .filter(|p| p.starts_with("/dev/") && !p.ends_with('/'))
            .map(String::as_str)
            .collect();
        let expected: &[&str] = if name == "qemu" { &["/dev/null"] } else { &[] };
        assert_eq!(devices, expected, "{name} devices");
        if let Some(leaf) = &leaf {
            let membership = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
            assert!(
                membership
                    .trim()
                    .ends_with(leaf.file_name().unwrap().to_str().unwrap()),
                "{name}: {membership}"
            );
        }
    }
    let [qemu, backend] = node.processes();
    let root = |pid: i32, path: &Path| PathBuf::from(format!("/proc/{pid}/root{}", path.display()));
    // The back end is not shown the disks or the kernel; QEMU is shown them read-only except
    // its writable disk.
    assert!(!root(backend, &runtime.join("kernel.elf")).exists());
    assert!(!root(backend, &scratch.0.join("home.img")).exists());
    assert!(root(qemu, &scratch.0.join("home.img")).exists());
    assert!(
        !root(qemu, &runtime.join("guest")).exists(),
        "QEMU is not shown the host side of the vsock"
    );
    if let Some(leaf) = &leaf {
        let read = |f: &str| fs::read_to_string(leaf.join(f)).unwrap().trim().to_string();
        println!(
            "leaf {}: memory.max {} cpu.max {} pids.max {} pids.current {}",
            leaf.display(),
            read("memory.max"),
            read("cpu.max"),
            read("pids.max"),
            read("pids.current")
        );
        assert_eq!(
            read("memory.max"),
            ((256u64 << 20) + (512 << 20)).to_string()
        );
        assert_eq!(read("cpu.max"), "100000 100000");
    }
    print!("{}", node.logs());
    node.shutdown().unwrap();
    assert!(!runtime.exists(), "the run's directory goes with the run");
    if let Some(leaf) = &leaf {
        assert!(!leaf.exists(), "the run's cgroup leaf goes with the run");
    }
    for pid in [qemu, backend] {
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "{pid} reaped");
    }
}

#[test]
#[ignore = "requires MORUNA_QEMU (seccomp, vhost-user, slirp) and MORUNA_JAIL_CGROUP"]
fn a_jailed_guest_exit_code_reaches_the_launcher_and_the_run_is_removed() {
    let (_scratch, mut node) = launch(EXIT_0X10, false);
    let runtime = node.runtime_dir().to_path_buf();
    let deadline = Instant::now() + Duration::from_secs(60);
    let code = loop {
        if let Some(status) = node.try_wait().unwrap() {
            break status.code();
        }
        assert!(
            Instant::now() < deadline,
            "guest never exited: {}",
            node.logs()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(code, Some(33), "{}", node.logs());
    node.shutdown().unwrap();
    assert!(!runtime.exists());
}

#[test]
#[ignore = "requires MORUNA_QEMU (seccomp, vhost-user, slirp) and MORUNA_JAIL_CGROUP"]
fn a_launch_that_fails_after_its_directory_exists_leaves_nothing() {
    let (scratch, descriptor) = descriptor(HALT, false);
    // The layer no longer matches its digest: refused after the run's directory is made.
    fs::write(scratch.0.join("layer.raw"), [8u8; 4096]).unwrap();
    let cgroup = Cgroup::parse(&required("MORUNA_JAIL_CGROUP")).unwrap();
    let leaves = |cgroup: &Cgroup| -> Vec<String> {
        match cgroup {
            Cgroup::Delegated(parent) => fs::read_dir(parent)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("moruna-run-"))
                .collect(),
            Cgroup::Undelegated => Vec::new(),
        }
    };
    let before = leaves(&cgroup);
    let runtime = scratch.0.join("run");
    let error = microkernel::launch_tcg_proof(
        &descriptor,
        budget(),
        &runtime,
        Path::new(&required("MORUNA_QEMU")),
        Path::new(env!("CARGO_BIN_EXE_moruna-vhost-vsock")),
        &cgroup,
    )
    .err()
    .expect("a tampered layer is refused");
    assert!(
        error.to_string().contains("digest or size mismatch"),
        "{error}"
    );
    assert!(!runtime.exists(), "a failed launch removes its directory");
    assert_eq!(
        leaves(&cgroup),
        before,
        "a failed launch leaves no cgroup leaf"
    );
}

#[test]
#[ignore = "requires MORUNA_QEMU (seccomp, vhost-user, slirp) and MORUNA_JAIL_CGROUP"]
fn a_back_end_that_dies_is_reported_with_its_cause_and_the_run_is_removed() {
    let (_scratch, node) = launch(HALT, false);
    let runtime = node.runtime_dir().to_path_buf();
    let leaf = node.cgroup().map(Path::to_path_buf);
    let [qemu, backend] = node.processes();
    // SAFETY: our own unreaped child.
    assert_eq!(unsafe { libc::kill(backend, libc::SIGKILL) }, 0);
    // QEMU may notice its vhost-user peer gone and exit first; either way the back end's
    // death is what is reported.
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = loop {
        match node.try_wait() {
            Err(error) => break error.to_string(),
            Ok(None) => {
                assert!(Instant::now() < deadline, "back end death not observed");
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(Some(status)) => panic!("back end death hidden behind QEMU's {status}"),
        }
    };
    assert!(
        error.contains("owned vsock backend") && error.contains("signal: 9"),
        "{error}"
    );
    drop(node);
    assert!(!runtime.exists());
    assert!(!Path::new(&format!("/proc/{qemu}")).exists(), "QEMU reaped");
    if let Some(leaf) = leaf {
        assert!(!leaf.exists());
    }
}
