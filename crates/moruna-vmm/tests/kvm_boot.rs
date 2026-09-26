//! Boots under real KVM. Every test here needs `/dev/kvm` and a built guest, so each is
//! tagged "(reference host, E1)" and ignored elsewhere; on the Nairobi reference host run
//!
//! ```text
//! MORUNA_VMM_IMAGE=<guest image dir or OCI layout> \
//! MORUNA_VMM_KERNEL=<the pinned kernel alone> \
//! MORUNA_VMM_SPEC=<a spec that completes> \
//! MORUNA_VMM_H10_SPEC=<a spec whose kernel fails unless /sys/class/net holds only lo> \
//! MORUNA_VMM_H12_SPEC=<a spec whose kernel waits for 2 GiB and 4 CPUs to appear> \
//!   cargo test -p moruna-vmm --test kvm_boot -- --ignored --test-threads 1
//! ```
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use moruna_vmm::config::{GIB, MIB, VmConfig};
use moruna_vmm::control::{Request, request};
use moruna_vmm::error::{EXIT_GUEST_PANIC, EXIT_NO_CODE};
use moruna_vmm::session::{self, Budget};

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("set {name}")))
}

fn scratch(tag: &str) -> PathBuf {
    let d = PathBuf::from("/tmp").join(format!("mvmm-kvm-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn budget() -> Budget {
    Budget {
        memory_bytes: GIB,
        memory_max_bytes: 4 * GIB,
        cpus: 2,
        cpus_max: 4,
    }
}

/// H11: a run in a VM costs one boot; the first message (`hello`) arrives within 300 ms on
/// x86_64 and 1 s on aarch64, and the run completes with Moruna's own exit code.
#[test]
#[ignore = "(reference host, E1): needs /dev/kvm and a built guest image"]
fn vm_t40_h11_boot_to_hello_and_completion() {
    let image = env_path("MORUNA_VMM_IMAGE");
    let spec = session::read_spec(&env_path("MORUNA_VMM_SPEC")).unwrap();
    let dir = scratch("h11");
    let mut c = VmConfig::with_defaults(image, vec![], GIB, 2, 1000 + std::process::id());
    c.vsock.uds_path = dir.join("v.sock");
    c.control_socket = dir.join("c.sock");
    let uds = c.vsock.uds_path.clone();
    let start = Instant::now();
    let vm = std::thread::spawn(move || moruna_vmm::boot(&c));
    let mut conn = session::connect_guest(
        &uds,
        moruna_vmm::machine::GUEST_AGENT_PORT,
        Instant::now() + Duration::from_secs(10),
        &|| true,
    )
    .unwrap();
    let mut first = String::new();
    std::io::BufRead::read_line(&mut conn, &mut first).unwrap();
    let to_hello = start.elapsed();
    assert!(first.contains("hello"), "{first}");
    let limit = if cfg!(target_arch = "x86_64") {
        Duration::from_millis(300)
    } else {
        Duration::from_secs(1)
    };
    eprintln!("time to hello: {to_hello:?} (host: reference, E1)");
    assert!(to_hello < limit, "{to_hello:?}");
    let mut out = Vec::new();
    session::drive(conn, &spec, &mut out).unwrap();
    assert_eq!(vm.join().unwrap().unwrap(), 0);
}

/// H10: the guest has no network interface but `lo`.
#[test]
#[ignore = "(reference host, E1): needs /dev/kvm and a built guest image"]
fn vm_t41_h10_guest_has_loopback_only() {
    let st = session::boot_and_run_with(
        &env_path("MORUNA_VMM_H10_SPEC"),
        &env_path("MORUNA_VMM_IMAGE"),
        &[],
        budget(),
        2000 + std::process::id(),
        &mut std::io::stderr(),
        moruna_vmm::boot,
    )
    .unwrap();
    assert_eq!(
        st.code, 0,
        "the spec's kernel found an interface other than lo"
    );
}

/// H12: memory and vCPUs added by `resize` appear in the guest within one second.
#[test]
#[ignore = "(reference host, E1): needs /dev/kvm and a built guest image"]
fn vm_t42_h12_resize_reaches_the_guest() {
    let dir = scratch("h12");
    let cid = 3000 + std::process::id();
    let ctl = moruna_vmm::config::default_control_path(cid);
    let resizer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while request(&ctl, &Request::Status).is_err() {
            assert!(Instant::now() < deadline, "the monitor never came up");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(2));
        let m = request(&ctl, &Request::ResizeMemory { bytes: 2 * GIB }).unwrap();
        assert!(m.ok, "{m:?}");
        if cfg!(target_arch = "x86_64") {
            let c = request(&ctl, &Request::ResizeCpus { cpus: 4 }).unwrap();
            assert!(c.ok, "{c:?}");
        }
    });
    let mut b = budget();
    if cfg!(target_arch = "aarch64") {
        b.cpus_max = b.cpus;
    }
    let st = session::boot_and_run_with(
        &env_path("MORUNA_VMM_H12_SPEC"),
        &env_path("MORUNA_VMM_IMAGE"),
        &[],
        b,
        cid,
        &mut std::io::stderr(),
        moruna_vmm::boot,
    )
    .unwrap();
    resizer.join().unwrap();
    assert_eq!(st.code, 0);
    let _ = dir;
}

/// A kernel with no init panics; the monitor exits 6 with the console on stderr.
#[test]
#[ignore = "(reference host, E1): needs /dev/kvm and the pinned guest kernel"]
fn vm_t43_guest_panic_exits_6() {
    let dir = scratch("panic");
    let img = dir.join("img");
    std::fs::create_dir_all(&img).unwrap();
    let name = moruna_vmm::image::KERNEL_NAMES[0];
    std::fs::copy(env_path("MORUNA_VMM_KERNEL"), img.join(name)).unwrap();
    let mut c = VmConfig::with_defaults(img, vec![], 512 * MIB, 1, 4000 + std::process::id());
    c.vsock.uds_path = dir.join("v.sock");
    c.control_socket = dir.join("c.sock");
    assert_eq!(moruna_vmm::boot(&c).unwrap(), EXIT_GUEST_PANIC);
}

/// A guest stopped from the control socket before reporting ends with 7.
#[test]
#[ignore = "(reference host, E1): needs /dev/kvm and a built guest image"]
fn vm_t44_stop_before_report_exits_7() {
    let dir = scratch("stop");
    let mut c = VmConfig::with_defaults(
        env_path("MORUNA_VMM_IMAGE"),
        vec![],
        GIB,
        1,
        5000 + std::process::id(),
    );
    c.vsock.uds_path = dir.join("v.sock");
    c.control_socket = dir.join("c.sock");
    let ctl = c.control_socket.clone();
    let vm = std::thread::spawn(move || moruna_vmm::boot(&c));
    while request(&ctl, &Request::Status).is_err() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(request(&ctl, &Request::Stop).unwrap().ok);
    assert_eq!(vm.join().unwrap().unwrap(), EXIT_NO_CODE);
}
