//! The monitor's configuration: what one guest is booted with.
//!
//! There is no network device in the monitor and no field for one here (MH H10). The type
//! refuses unknown fields when read from JSON, so a configuration that names a NIC is refused
//! rather than ignored, and `vm_t2_no_network_field` destructures the type exhaustively so
//! that a field added later has to be looked at by the test that forbids a network.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, VmmError};

/// One MiB.
pub const MIB: u64 = 1 << 20;
/// One GiB.
pub const GIB: u64 = 1 << 30;

/// Smallest boot memory: the pinned guest kernel, the initramfs and the page cache CPython's
/// first import needs fit in it with room for a small run.
pub const MIN_MEMORY_BYTES: u64 = 256 * MIB;
/// Memory sizes are multiples of this: the virtio-mem block size, so that the boot RAM end and
/// every hot-plugged block share one granularity.
pub const MEMORY_ALIGN_BYTES: u64 = 2 * MIB;
/// Largest `--memory-max`: 1 TiB, far above any host this runs on, and small enough that the
/// guest-physical map of both architectures stays below 41 bits.
pub const MAX_MEMORY_BYTES: u64 = 1 << 40;
/// Most vCPUs a guest may have: the aarch64 GICv3 redistributor window is sized for 64, and
/// x86 xAPIC ids stay below 255 with room for the IOAPIC id.
pub const MAX_VCPUS: u32 = 64;
/// Most block devices a guest may have, the image's root filesystem included: on x86_64 the
/// block devices take GSIs 8 to 23.
pub const MAX_DISKS: usize = 16;
/// The host's vsock CID; a guest CID must be above it (vsock(7): 0, 1 and 2 are reserved).
pub const HOST_CID: u32 = 2;

/// One block device the host names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskConfig {
    /// The backing file or block device.
    pub path: PathBuf,
    /// Read-only: the guest sees `VIRTIO_BLK_F_RO` and every write is refused.
    pub read_only: bool,
}

/// The guest's vsock device and its host side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VsockConfig {
    /// The guest's context id (above [`HOST_CID`]).
    pub cid: u32,
    /// The host Unix socket: a host process connects here and writes `CONNECT <port>\n` to
    /// reach a guest port; a guest connecting to host port `P` reaches `<uds_path>_P`
    /// (Firecracker's convention).
    pub uds_path: PathBuf,
}

/// Everything one boot is given. No field describes a network device (MH H10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    /// The guest image: a directory or an OCI image layout.
    pub image: PathBuf,
    /// The block devices the host names, in the order the guest sees them after the image's
    /// root filesystem.
    pub disks: Vec<DiskConfig>,
    /// Memory at boot, bytes.
    pub memory_bytes: u64,
    /// The most memory `resize` may reach, bytes; the difference is the virtio-mem region.
    pub memory_max_bytes: u64,
    /// vCPUs online at boot.
    pub cpus: u32,
    /// The most vCPUs `resize` may reach.
    pub cpus_max: u32,
    /// The vsock device.
    pub vsock: VsockConfig,
    /// The monitor's own control socket.
    pub control_socket: PathBuf,
}

impl VmConfig {
    /// A configuration with the default socket paths for `cid` under the system temp dir.
    pub fn with_defaults(
        image: PathBuf,
        disks: Vec<DiskConfig>,
        memory_bytes: u64,
        cpus: u32,
        cid: u32,
    ) -> Self {
        VmConfig {
            image,
            disks,
            memory_bytes,
            memory_max_bytes: memory_bytes,
            cpus,
            cpus_max: cpus,
            vsock: VsockConfig {
                cid,
                uds_path: default_vsock_path(cid),
            },
            control_socket: default_control_path(cid),
        }
    }

    /// Check every field; the error names the flag that is wrong.
    pub fn validate(&self) -> Result<()> {
        if self.memory_bytes < MIN_MEMORY_BYTES {
            return Err(VmmError::config(
                "--memory",
                format!(
                    "{} bytes is below the minimum {MIN_MEMORY_BYTES}",
                    self.memory_bytes
                ),
            ));
        }
        check_aligned("--memory", self.memory_bytes)?;
        check_aligned("--memory-max", self.memory_max_bytes)?;
        if self.memory_max_bytes < self.memory_bytes {
            return Err(VmmError::config(
                "--memory-max",
                format!(
                    "{} is below --memory {}",
                    self.memory_max_bytes, self.memory_bytes
                ),
            ));
        }
        if self.memory_max_bytes > MAX_MEMORY_BYTES {
            return Err(VmmError::config(
                "--memory-max",
                format!(
                    "{} is above the limit {MAX_MEMORY_BYTES}",
                    self.memory_max_bytes
                ),
            ));
        }
        if self.cpus == 0 {
            return Err(VmmError::config("--cpus", "must be at least 1"));
        }
        if self.cpus_max < self.cpus {
            return Err(VmmError::config(
                "--cpus-max",
                format!("{} is below --cpus {}", self.cpus_max, self.cpus),
            ));
        }
        if self.cpus_max > MAX_VCPUS {
            return Err(VmmError::config(
                "--cpus-max",
                format!("{} is above the limit {MAX_VCPUS}", self.cpus_max),
            ));
        }
        if self.vsock.cid <= HOST_CID || self.vsock.cid == u32::MAX {
            return Err(VmmError::config(
                "--vsock",
                format!(
                    "cid {} is reserved; a guest cid is 3 to {}",
                    self.vsock.cid,
                    u32::MAX - 1
                ),
            ));
        }
        // The image's root filesystem takes one slot.
        if self.disks.len() + 1 > MAX_DISKS {
            return Err(VmmError::config(
                "--disk",
                format!(
                    "{} disks; at most {} besides the image's root filesystem",
                    self.disks.len(),
                    MAX_DISKS - 1
                ),
            ));
        }
        for d in &self.disks {
            let meta = std::fs::metadata(&d.path)
                .map_err(|e| VmmError::config("--disk", format!("{}: {e}", d.path.display())))?;
            if meta.is_dir() {
                return Err(VmmError::config(
                    "--disk",
                    format!("{} is a directory", d.path.display()),
                ));
            }
        }
        for (flag, p) in [
            ("--vsock-uds", &self.vsock.uds_path),
            ("--control", &self.control_socket),
        ] {
            check_socket_path(flag, p)?;
        }
        if self.vsock.uds_path == self.control_socket {
            return Err(VmmError::config(
                "--control",
                "the control socket and the vsock socket are the same path",
            ));
        }
        Ok(())
    }

    /// Bytes the virtio-mem device may plug beyond boot memory.
    pub fn hotplug_bytes(&self) -> u64 {
        self.memory_max_bytes - self.memory_bytes
    }
}

fn check_aligned(field: &'static str, v: u64) -> Result<()> {
    if !v.is_multiple_of(MEMORY_ALIGN_BYTES) {
        return Err(VmmError::config(
            field,
            format!("{v} is not a multiple of {MEMORY_ALIGN_BYTES} (2 MiB)"),
        ));
    }
    Ok(())
}

/// `sun_path` is 108 bytes on Linux and 104 on macOS, both with the terminating NUL; the
/// shorter limit is enforced everywhere so a path that works here works on the host.
pub const MAX_SOCKET_PATH: usize = 103;

fn check_socket_path(field: &'static str, p: &Path) -> Result<()> {
    let len = p.as_os_str().len();
    // `_<port>` is appended for guest-initiated vsock connections: leave room for 11 bytes.
    let limit = if field == "--vsock-uds" {
        MAX_SOCKET_PATH - 11
    } else {
        MAX_SOCKET_PATH
    };
    if len == 0 || len > limit {
        return Err(VmmError::config(
            field,
            format!(
                "{} is {len} bytes; a socket path is 1 to {limit}",
                p.display()
            ),
        ));
    }
    match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() && !dir.is_dir() => Err(VmmError::config(
            field,
            format!("directory {} does not exist", dir.display()),
        )),
        _ => Ok(()),
    }
}

/// The default vsock socket for `cid`: `<tmp>/moruna-vmm-<cid>.vsock`.
pub fn default_vsock_path(cid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("moruna-vmm-{cid}.vsock"))
}

/// The default control socket for `cid`: `<tmp>/moruna-vmm-<cid>.ctl`.
pub fn default_control_path(cid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("moruna-vmm-{cid}.ctl"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testing::scratch_dir;

    pub(crate) fn valid(dir: &Path) -> VmConfig {
        let disk = dir.join("data.img");
        std::fs::write(&disk, vec![0u8; 4096]).unwrap();
        VmConfig {
            image: dir.join("image"),
            disks: vec![DiskConfig {
                path: disk,
                read_only: true,
            }],
            memory_bytes: 512 * MIB,
            memory_max_bytes: 2 * GIB,
            cpus: 1,
            cpus_max: 4,
            vsock: VsockConfig {
                cid: 3,
                uds_path: dir.join("v.sock"),
            },
            control_socket: dir.join("c.sock"),
        }
    }

    fn refused(c: &VmConfig, field: &str) {
        match c.validate() {
            Err(VmmError::Config { field: f, .. }) => assert_eq!(f, field),
            other => panic!("expected a refusal naming {field}, got {other:?}"),
        }
    }

    #[test]
    fn vm_t1_config_refusals_name_the_field() {
        let dir = scratch_dir("cfg");
        let ok = valid(&dir);
        ok.validate().unwrap();
        assert_eq!(ok.hotplug_bytes(), 2 * GIB - 512 * MIB);

        let mut c = ok.clone();
        c.memory_bytes = 128 * MIB;
        refused(&c, "--memory");
        let mut c = ok.clone();
        c.memory_bytes = 512 * MIB + 4096;
        refused(&c, "--memory");
        let mut c = ok.clone();
        c.memory_max_bytes = 3 * GIB + 1;
        refused(&c, "--memory-max");
        let mut c = ok.clone();
        c.memory_max_bytes = 256 * MIB;
        refused(&c, "--memory-max");
        let mut c = ok.clone();
        c.memory_max_bytes = 2 * MAX_MEMORY_BYTES;
        refused(&c, "--memory-max");
        let mut c = ok.clone();
        c.cpus = 0;
        refused(&c, "--cpus");
        let mut c = ok.clone();
        c.cpus_max = 0;
        refused(&c, "--cpus-max");
        let mut c = ok.clone();
        c.cpus_max = MAX_VCPUS + 1;
        refused(&c, "--cpus-max");
        for cid in [0, 1, 2, u32::MAX] {
            let mut c = ok.clone();
            c.vsock.cid = cid;
            refused(&c, "--vsock");
        }
        let mut c = ok.clone();
        c.disks = (0..MAX_DISKS).map(|_| ok.disks[0].clone()).collect();
        refused(&c, "--disk");
        let mut c = ok.clone();
        c.disks[0].path = dir.join("absent.img");
        refused(&c, "--disk");
        let mut c = ok.clone();
        c.disks[0].path = dir.clone();
        refused(&c, "--disk");
        let mut c = ok.clone();
        c.control_socket = dir.join("x".repeat(200));
        refused(&c, "--control");
        let mut c = ok.clone();
        c.control_socket = PathBuf::new();
        refused(&c, "--control");
        let mut c = ok.clone();
        c.vsock.uds_path = PathBuf::from("/nonexistent-moruna-dir/v.sock");
        refused(&c, "--vsock-uds");
        let mut c = ok.clone();
        c.control_socket = c.vsock.uds_path.clone();
        refused(&c, "--control");
        // A bare file name has no parent directory to check and is accepted.
        let mut c = ok.clone();
        c.control_socket = PathBuf::from("c.sock");
        c.validate().unwrap();
    }

    #[test]
    fn vm_t1_defaults_derive_from_the_cid() {
        let c = VmConfig::with_defaults(PathBuf::from("img"), vec![], 512 * MIB, 2, 7);
        assert_eq!(c.memory_max_bytes, c.memory_bytes);
        assert_eq!(c.cpus_max, 2);
        assert!(c.vsock.uds_path.ends_with("moruna-vmm-7.vsock"));
        assert!(c.control_socket.ends_with("moruna-vmm-7.ctl"));
    }

    /// H10, structurally: the configuration type has no field a network device could live
    /// in, and a configuration naming one is refused, not ignored.
    #[test]
    fn vm_t2_no_network_field() {
        let dir = scratch_dir("cfg-h10");
        let c = valid(&dir);
        // Exhaustive destructuring: adding a field to VmConfig fails to compile here until
        // this list, and the assertion below, have been looked at.
        let VmConfig {
            image: _,
            disks: _,
            memory_bytes: _,
            memory_max_bytes: _,
            cpus: _,
            cpus_max: _,
            vsock: _,
            control_socket: _,
        } = &c;
        let json = serde_json::to_value(&c).unwrap();
        let mut names: Vec<String> = Vec::new();
        collect_keys(&json, &mut names);
        for n in &names {
            let n = n.to_ascii_lowercase();
            for banned in ["net", "nic", "tap", "mac", "ip", "bridge", "iface"] {
                assert!(
                    !n.split('_').any(|w| w == banned),
                    "field {n} could describe a network device"
                );
            }
        }
        for extra in [
            r#""net": {"tap": "tap0"}"#,
            r#""network": true"#,
            r#""nic": "virtio""#,
        ] {
            let mut text = serde_json::to_string(&c).unwrap();
            text.pop();
            text.push(',');
            text.push_str(extra);
            text.push('}');
            let err = serde_json::from_str::<VmConfig>(&text).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
        let back: VmConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    fn collect_keys(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    out.push(k.clone());
                    collect_keys(v, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|v| collect_keys(v, out)),
            _ => {}
        }
    }
}
