//! A real QEMU boot through the launcher with a run's data disks and a vCPU placement (stream
//! D): QEMU itself is asked, over its QMP socket, what it attached, and `/proc` what the jail
//! shows and where each thread may run. The guest is a minimal Multiboot kernel that halts.
//! Run explicitly with MORUNA_QEMU (a QEMU built with seccomp, vhost-user and slirp) and
//! MORUNA_JAIL_CGROUP (a delegated cgroup v2 directory, or `none`); TCG unless MORUNA_KVM=1.
#![cfg(all(target_os = "linux", feature = "native-tcg-proof"))]

use moruna_vmm::{
    Budget,
    data::{Input, RunDevices},
    jail::Cgroup,
    microkernel,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::DirBuilderExt, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

/// An x86_64 ELF with Multiboot 1 and 2 headers, loaded at 1 MiB, whose entry halts.
fn kernel() -> Vec<u8> {
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
    // hlt; jmp hlt
    put(&mut elf, 0x1030, &[0xF4, 0xEB, 0xFD]);
    elf
}

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

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("required {name}"))
}

/// One QMP command's answer, after the greeting and the capabilities.
fn qmp(socket: &Path, command: serde_json::Value) -> serde_json::Value {
    let stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let answer = |reader: &mut BufReader<UnixStream>| loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        if value.get("event").is_none() {
            break value;
        }
    };
    assert!(answer(&mut reader).get("QMP").is_some());
    for request in [serde_json::json!({"execute": "qmp_capabilities"}), command] {
        let mut line = serde_json::to_vec(&request).unwrap();
        line.push(b'\n');
        reader.get_mut().write_all(&line).unwrap();
        let value = answer(&mut reader);
        if request["execute"] != "qmp_capabilities" {
            return value["return"].clone();
        }
    }
    unreachable!()
}

#[test]
#[ignore = "requires MORUNA_QEMU (seccomp, vhost-user, slirp) and MORUNA_JAIL_CGROUP"]
fn data_disks_attach_in_order_and_vcpus_run_where_placed() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "md-{}-{}",
        std::process::id(),
        nanos % 1_000_000_000
    ));
    fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let scratch = Scratch(dir.canonicalize().unwrap());
    let dir = &scratch.0;
    let document = serde_json::json!({
        "version": 1, "architecture": "x86_64", "boot_mode": "multiboot2",
        "kernel": artifact(dir, "kernel.elf", &kernel()),
        "initramfs": artifact(dir, "initramfs.cpio", b"070701 not a real archive; a multiboot module"),
        "layers": [{"artifact": artifact(dir, "layer.raw", &[7u8; 4096])}],
        "writable_disks": [], "user_network": false, "cid": 3, "agent_port": 5000,
        "resources": {"node_memory_bytes": 256u64 << 20, "job_memory_bytes": 64u64 << 20,
                      "job_account_bytes": 96u64 << 20, "scratch_bytes": 16u64 << 20, "cpus": 2},
        "data_disks": {"inputs": ["a", "b"], "output_bytes": 1u64 << 20}
    });
    let descriptor = dir.join("node.json");
    fs::write(&descriptor, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let inputs = dir.join("inputs");
    fs::create_dir(&inputs).unwrap();
    fs::write(inputs.join("b.parquet"), vec![2u8; 5000]).unwrap();
    fs::write(inputs.join("a.parquet"), vec![1u8; 1000]).unwrap();
    let allowed = moruna_vmm::placement::allowed_cpus().unwrap();
    let vcpu_cpus = if allowed.len() >= 3 {
        vec![allowed[allowed.len() - 2], allowed[allowed.len() - 1]]
    } else {
        Vec::new()
    };
    let devices = RunDevices {
        // Bound in the other order: the image's order decides.
        inputs: vec![
            Input {
                name: "b".into(),
                path: inputs.join("b.parquet"),
            },
            Input {
                name: "a".into(),
                path: inputs.join("a.parquet"),
            },
        ],
        output: Some(dir.join("output.disk")),
        vcpu_cpus: vcpu_cpus.clone(),
    };
    let authority = microkernel::RunAuthority {
        job_digest: String::new(),
        run_id: "0".repeat(32),
        attempt: 0,
    };
    let budget = Budget {
        memory_bytes: 64 << 20,
        memory_max_bytes: 64 << 20,
        cpus: 1,
        cpus_max: 1,
    };
    let launch = if std::env::var("MORUNA_KVM").as_deref() == Ok("1") {
        microkernel::launch_with
    } else {
        microkernel::launch_tcg_proof_with
    };
    let mut node = launch(
        &descriptor,
        budget,
        &dir.join("run"),
        Path::new(&required("MORUNA_QEMU")),
        Path::new(env!("CARGO_BIN_EXE_moruna-vhost-vsock")),
        &Cgroup::parse(&required("MORUNA_JAIL_CGROUP")).unwrap(),
        &authority,
        &devices,
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        node.try_wait().unwrap().is_none(),
        "QEMU ended: {}",
        node.logs()
    );
    let socket = node.runtime_dir().join("qmp/qmp.sock");
    // The PCI functions: three transitional virtio-blk (1af4:1001) at 0x10, 0x11 and 0x12, the
    // image's layer a modern one (1af4:1042) elsewhere.
    let pci = qmp(&socket, serde_json::json!({"execute": "query-pci"}));
    let functions: Vec<(u64, u64, u64)> = pci[0]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["slot"].as_u64().unwrap(),
                d["id"]["vendor"].as_u64().unwrap(),
                d["id"]["device"].as_u64().unwrap(),
            )
        })
        .collect();
    println!("PCI: {functions:x?}");
    for slot in [0x10, 0x11, 0x12] {
        assert!(
            functions.contains(&(slot, 0x1af4, 0x1001)),
            "no data disk at slot {slot:#x}"
        );
    }
    assert!(
        functions
            .iter()
            .any(|f| f.1 == 0x1af4 && f.2 == 0x1042 && !(0x10..=0x12).contains(&f.0))
    );
    // The drives: the inputs read-only in the image's order, the output writable.
    let block = qmp(&socket, serde_json::json!({"execute": "query-block"}));
    let drive = |id: &str| {
        block
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device"] == id)
            .unwrap_or_else(|| panic!("no drive {id}: {block}"))
            .clone()
    };
    for (id, file, ro) in [
        ("data0", "a.parquet", true),
        ("data1", "b.parquet", true),
        ("data2", "output.disk", false),
    ] {
        let d = drive(id);
        assert!(
            d["inserted"]["file"].as_str().unwrap().ends_with(file),
            "{d}"
        );
        assert_eq!(d["inserted"]["ro"], ro, "{d}");
    }
    // Each input's serial is its file's length.
    for (id, serial) in [("data-disk0", "1000"), ("data-disk1", "5000")] {
        let got = qmp(
            &socket,
            serde_json::json!({"execute": "qom-get",
                "arguments": {"path": format!("/machine/peripheral/{id}"), "property": "serial"}}),
        );
        assert_eq!(got, serial, "{id}");
    }
    // The jail shows the inputs read-only and the output disk writable.
    let [qemu, backend] = node.processes();
    let mounts = fs::read_to_string(format!("/proc/{qemu}/mountinfo")).unwrap();
    for (file, rw) in [
        ("a.parquet", false),
        ("b.parquet", false),
        ("output.disk", true),
    ] {
        let line = mounts
            .lines()
            .find(|l| {
                l.split_whitespace()
                    .nth(4)
                    .is_some_and(|p| p.ends_with(file))
            })
            .unwrap_or_else(|| panic!("{file} is not in QEMU's jail"));
        let options = line.split_whitespace().nth(5).unwrap();
        assert_eq!(options.split(',').any(|o| o == "rw"), rw, "{line}");
    }
    // Each vCPU on its host CPU, every other thread off them.
    if !vcpu_cpus.is_empty() {
        let placed = node.placed().expect("a placement was asked for").clone();
        assert_eq!(placed.vcpus.len(), 2);
        let allowed_of = |pid: i32, tid: i32| {
            fs::read_to_string(format!("/proc/{pid}/task/{tid}/status"))
                .unwrap()
                .lines()
                .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
                .unwrap()
                .trim()
                .to_string()
        };
        for (vcpu, tid, cpu) in &placed.vcpus {
            assert_eq!(*cpu, vcpu_cpus[*vcpu as usize]);
            assert_eq!(allowed_of(qemu, *tid), cpu.to_string(), "vCPU {vcpu}");
        }
        for pid in [qemu, backend] {
            for task in fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
                let tid: i32 = task.unwrap().file_name().to_str().unwrap().parse().unwrap();
                if placed.vcpus.iter().any(|(_, t, _)| *t == tid) {
                    continue;
                }
                let list = allowed_of(pid, tid);
                let cpus = moruna_vmm::placement::parse_cpu_list(&list).unwrap();
                for cpu in &vcpu_cpus {
                    assert!(!cpus.contains(cpu), "thread {tid} may run on {cpu}: {list}");
                }
            }
        }
    }
    let output = dir.join("output.disk");
    node.shutdown().unwrap();
    assert_eq!(
        fs::metadata(&output).unwrap().len(),
        1 << 20,
        "outlives the VM"
    );
    assert!(!dir.join("run").exists());
}
