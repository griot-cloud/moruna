//! Actual native artifact gate, explicitly invoked on Linux x86 KVM with supplied
//! MORUNA_NODE_DESCRIPTOR, MORUNA_QEMU, MORUNA_VSOCK_BACKEND, MORUNA_NODE_PROOF_DIR and
//! MORUNA_NODE_CGROUP (a delegated cgroup v2 directory, or `none`).
//! No Linux guest/OCI fixture or TCG fallback is accepted.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use moruna_vmm::{Budget, jail::Cgroup, microkernel};
use std::{
    io::BufRead,
    path::PathBuf,
    time::{Duration, Instant},
};
fn required(name: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("required {name}"))
}
#[test]
#[ignore = "requires real merged native artifact, Linux x86 KVM and owned QEMU/vsock tools"]
fn native_node_accepts_actual_engine_connection_and_reaps_owned_processes() {
    let descriptor = required("MORUNA_NODE_DESCRIPTOR");
    let artifacts = microkernel::inspect(&descriptor).unwrap();
    let r = artifacts.resources;
    let budget = Budget {
        memory_bytes: r.job_memory_bytes,
        memory_max_bytes: r.job_memory_bytes,
        cpus: r.cpus,
        cpus_max: r.cpus,
    };
    let proof = required("MORUNA_NODE_PROOF_DIR");
    // launch itself refuses an existing directory; no shared scratch is removed.
    let mut node = microkernel::launch(
        &descriptor,
        budget,
        &proof,
        &required("MORUNA_QEMU"),
        &required("MORUNA_VSOCK_BACKEND"),
        &Cgroup::parse(&required("MORUNA_NODE_CGROUP").to_string_lossy()).unwrap(),
    )
    .unwrap();
    let mut stream = node
        .connect(node.agent_port(), Instant::now() + Duration::from_secs(60))
        .unwrap();
    stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut hello = Vec::new();
    loop {
        let bytes = stream.fill_buf().unwrap();
        assert!(!bytes.is_empty(), "EOF before actual hello");
        let n = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        assert!(hello.len() + n <= 1024 * 1024, "unbounded hello");
        let done = bytes[n - 1] == b'\n';
        hello.extend_from_slice(&bytes[..n]);
        stream.consume(n);
        if done {
            break;
        }
    }
    let hello: serde_json::Value = serde_json::from_slice(&hello).unwrap();
    assert_eq!(hello.get("type").and_then(|v| v.as_str()), Some("hello"));
    drop(stream);
    node.shutdown().unwrap();
    assert!(node.try_wait().unwrap().is_some());
    assert!(!proof.exists(), "the run's directory goes with the run");
}
