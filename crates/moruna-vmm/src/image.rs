//! The guest image: a kernel, an initramfs and a read-only root filesystem, given either as a
//! plain directory or as an OCI image layout whose layers are those three files.
//!
//! The OCI form is what a release publishes (MH 4.8.7) and what a host pins by digest. The
//! monitor reads the layout from disk; it never pulls. Blobs up to [`VERIFY_MAX_BYTES`] are
//! hashed on every boot and refused on a mismatch; the root filesystem is larger than that and
//! is checked by size, because hashing a few hundred megabytes on every boot would spend the
//! whole of H11's 300 ms, and the host verified the artefact's digest and signature when it
//! pulled it.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Result, VmmError};

/// The OCI media type of the kernel layer.
pub const MEDIA_KERNEL: &str = "application/vnd.moruna.guest.kernel.v1";
/// The OCI media type of the initramfs layer (a gzip-compressed newc cpio archive).
pub const MEDIA_INITRAMFS: &str = "application/vnd.moruna.guest.initramfs.v1+gzip";
/// The OCI media type of the root filesystem layer (an EROFS image).
pub const MEDIA_ROOTFS: &str = "application/vnd.moruna.guest.rootfs.v1.erofs";
/// The OCI artifact type of the whole image.
pub const ARTIFACT_TYPE: &str = "application/vnd.moruna.guest.v1";
/// Blobs at most this large are hashed at every boot: the kernel and the initramfs.
pub const VERIFY_MAX_BYTES: u64 = 64 << 20;

/// The kernel file names a directory image may use, in order of preference for this
/// architecture.
#[cfg(target_arch = "x86_64")]
pub const KERNEL_NAMES: &[&str] = &["vmlinux", "bzImage"];
/// The kernel file names a directory image may use, in order of preference for this
/// architecture.
#[cfg(not(target_arch = "x86_64"))]
pub const KERNEL_NAMES: &[&str] = &["Image"];
/// The initramfs file names a directory image may use.
pub const INITRAMFS_NAMES: &[&str] = &["initramfs.cpio.gz", "initramfs.cpio"];
/// The root filesystem file names a directory image may use.
pub const ROOTFS_NAMES: &[&str] = &["rootfs.erofs"];

/// This architecture's name in an OCI platform.
#[cfg(target_arch = "x86_64")]
pub const OCI_ARCH: &str = "amd64";
/// This architecture's name in an OCI platform.
#[cfg(not(target_arch = "x86_64"))]
pub const OCI_ARCH: &str = "arm64";

/// A resolved image: three files on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestImage {
    /// The kernel.
    pub kernel: PathBuf,
    /// The initramfs, if the image has one.
    pub initramfs: Option<PathBuf>,
    /// The root filesystem, attached read-only as the first block device, if the image has one.
    pub rootfs: Option<PathBuf>,
    /// The manifest digest, when the image is an OCI layout.
    pub digest: Option<String>,
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "artifactType", default)]
    artifact_type: Option<String>,
    layers: Vec<Descriptor>,
}

fn err(path: &Path, msg: impl Into<String>) -> VmmError {
    VmmError::Image {
        path: path.display().to_string(),
        msg: msg.into(),
    }
}

/// Resolve `path` into a [`GuestImage`].
pub fn resolve(path: &Path) -> Result<GuestImage> {
    if !path.is_dir() {
        return Err(err(
            path,
            "not a directory (an image directory or an OCI layout)",
        ));
    }
    if path.join("oci-layout").is_file() {
        resolve_oci(path)
    } else {
        resolve_dir(path)
    }
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

fn resolve_dir(dir: &Path) -> Result<GuestImage> {
    let kernel = first_existing(dir, KERNEL_NAMES).ok_or_else(|| {
        err(
            dir,
            format!("no kernel; expected one of {}", KERNEL_NAMES.join(", ")),
        )
    })?;
    Ok(GuestImage {
        kernel,
        initramfs: first_existing(dir, INITRAMFS_NAMES),
        rootfs: first_existing(dir, ROOTFS_NAMES),
        digest: None,
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Result<T> {
    let bytes = std::fs::read(p).map_err(|e| err(p, e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| err(p, e.to_string()))
}

fn blob_path(dir: &Path, digest: &str) -> Result<PathBuf> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(err(dir, format!("digest {digest} is not sha256")));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(err(dir, format!("digest {digest} is malformed")));
    }
    Ok(dir.join("blobs").join("sha256").join(hex))
}

/// Check a blob's size, and its digest when it is small enough.
fn verify_blob(dir: &Path, d: &Descriptor) -> Result<PathBuf> {
    let p = blob_path(dir, &d.digest)?;
    let meta = std::fs::metadata(&p).map_err(|e| err(&p, e.to_string()))?;
    if meta.len() != d.size {
        return Err(err(
            &p,
            format!("size {} but the manifest says {}", meta.len(), d.size),
        ));
    }
    if d.size <= VERIFY_MAX_BYTES {
        let got = sha256_file(&p)?;
        if got != d.digest {
            return Err(err(
                &p,
                format!("digest {got} but the manifest says {}", d.digest),
            ));
        }
    }
    Ok(p)
}

/// `sha256:<hex>` of a file.
pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = File::open(p).map_err(|e| err(p, e.to_string()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| err(p, e.to_string()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("sha256:{}", hex(&h.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn resolve_oci(dir: &Path) -> Result<GuestImage> {
    let index: Index = read_json(&dir.join("index.json"))?;
    let candidates: Vec<&Descriptor> = index
        .manifests
        .iter()
        .filter(|m| match &m.platform {
            Some(p) => p.architecture == OCI_ARCH && p.os == "linux",
            None => true,
        })
        .collect();
    let desc = match candidates.as_slice() {
        [one] => *one,
        [] => {
            return Err(err(
                dir,
                format!("index.json has no manifest for linux/{OCI_ARCH}"),
            ));
        }
        _ => {
            return Err(err(
                dir,
                format!("index.json has several manifests for linux/{OCI_ARCH}"),
            ));
        }
    };
    let manifest_path = verify_blob(dir, desc)?;
    let manifest: Manifest = read_json(&manifest_path)?;
    if manifest.artifact_type.as_deref() != Some(ARTIFACT_TYPE) {
        return Err(err(
            &manifest_path,
            format!(
                "artifactType {:?} is not {ARTIFACT_TYPE}",
                manifest.artifact_type
            ),
        ));
    }
    let mut kernel = None;
    let mut initramfs = None;
    let mut rootfs = None;
    for layer in &manifest.layers {
        let slot = match layer.media_type.as_str() {
            MEDIA_KERNEL => &mut kernel,
            MEDIA_INITRAMFS => &mut initramfs,
            MEDIA_ROOTFS => &mut rootfs,
            other => {
                return Err(err(&manifest_path, format!("unknown layer type {other}")));
            }
        };
        if slot.replace(verify_blob(dir, layer)?).is_some() {
            return Err(err(
                &manifest_path,
                format!("two layers of type {}", layer.media_type),
            ));
        }
    }
    Ok(GuestImage {
        kernel: kernel.ok_or_else(|| err(&manifest_path, "no kernel layer"))?,
        initramfs,
        rootfs,
        digest: Some(desc.digest.clone()),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testing::scratch_dir;

    fn put_blob(dir: &Path, bytes: &[u8]) -> (String, u64) {
        let digest = format!("sha256:{}", hex(&Sha256::digest(bytes)));
        let p = blob_path(dir, &digest).unwrap();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        (digest, bytes.len() as u64)
    }

    /// Build an OCI layout with the given layers; returns the layout dir.
    pub(crate) fn oci_layout(name: &str, layers: &[(&str, &[u8])], arch: &str) -> PathBuf {
        let dir = scratch_dir(name);
        std::fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        let (cfg_digest, cfg_size) = put_blob(&dir, b"{}");
        let layers: Vec<serde_json::Value> = layers
            .iter()
            .map(|(mt, bytes)| {
                let (d, s) = put_blob(&dir, bytes);
                serde_json::json!({"mediaType": mt, "digest": d, "size": s})
            })
            .collect();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": ARTIFACT_TYPE,
            "config": {"mediaType": "application/vnd.oci.empty.v1+json",
                       "digest": cfg_digest, "size": cfg_size},
            "layers": layers,
        });
        let (md, ms) = put_blob(&dir, manifest.to_string().as_bytes());
        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                           "digest": md, "size": ms,
                           "platform": {"architecture": arch, "os": "linux"}}],
        });
        std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
        dir
    }

    fn image_err<T: std::fmt::Debug>(r: Result<T>) -> String {
        match r {
            Err(VmmError::Image { msg, .. }) => msg,
            other => panic!("expected an image error, got {other:?}"),
        }
    }

    #[test]
    fn vm_t4_directory_image() {
        let dir = scratch_dir("img-dir");
        assert!(image_err(resolve(&dir)).contains("no kernel"));
        std::fs::write(dir.join(KERNEL_NAMES[0]), b"k").unwrap();
        let img = resolve(&dir).unwrap();
        assert_eq!(img.kernel, dir.join(KERNEL_NAMES[0]));
        assert_eq!((img.initramfs.clone(), img.rootfs.clone()), (None, None));
        std::fs::write(dir.join("initramfs.cpio"), b"i").unwrap();
        std::fs::write(dir.join("rootfs.erofs"), b"r").unwrap();
        let img = resolve(&dir).unwrap();
        assert_eq!(img.initramfs, Some(dir.join("initramfs.cpio")));
        assert_eq!(img.rootfs, Some(dir.join("rootfs.erofs")));
        assert!(img.digest.is_none());
        assert!(image_err(resolve(&dir.join("absent"))).contains("not a directory"));
    }

    #[test]
    fn vm_t4_oci_layout_resolves_and_verifies() {
        let dir = oci_layout(
            "img-oci",
            &[
                (MEDIA_KERNEL, b"kernel-bytes"),
                (MEDIA_INITRAMFS, b"initramfs-bytes"),
                (MEDIA_ROOTFS, b"rootfs-bytes"),
            ],
            OCI_ARCH,
        );
        let img = resolve(&dir).unwrap();
        assert_eq!(std::fs::read(&img.kernel).unwrap(), b"kernel-bytes");
        assert_eq!(
            std::fs::read(img.initramfs.as_ref().unwrap()).unwrap(),
            b"initramfs-bytes"
        );
        assert_eq!(
            std::fs::read(img.rootfs.as_ref().unwrap()).unwrap(),
            b"rootfs-bytes"
        );
        assert!(img.digest.as_ref().unwrap().starts_with("sha256:"));

        // A tampered kernel blob of the same size is refused by digest.
        std::fs::write(&img.kernel, b"KERNEL-BYTES").unwrap();
        assert!(image_err(resolve(&dir)).contains("digest"));
        // A truncated one by size.
        std::fs::write(&img.kernel, b"k").unwrap();
        assert!(image_err(resolve(&dir)).contains("size"));
    }

    #[test]
    fn vm_t4_oci_layout_refusals() {
        let other = if OCI_ARCH == "amd64" {
            "arm64"
        } else {
            "amd64"
        };
        let dir = oci_layout("img-arch", &[(MEDIA_KERNEL, b"k")], other);
        assert!(image_err(resolve(&dir)).contains("no manifest for linux"));

        let dir = oci_layout("img-nok", &[(MEDIA_INITRAMFS, b"i")], OCI_ARCH);
        assert!(image_err(resolve(&dir)).contains("no kernel layer"));

        let dir = oci_layout(
            "img-two",
            &[(MEDIA_KERNEL, b"k"), (MEDIA_KERNEL, b"k2")],
            OCI_ARCH,
        );
        assert!(image_err(resolve(&dir)).contains("two layers"));

        let dir = oci_layout("img-unk", &[("application/x-net-config", b"n")], OCI_ARCH);
        assert!(image_err(resolve(&dir)).contains("unknown layer type"));

        // Two manifests for this platform are ambiguous.
        let dir = oci_layout("img-amb", &[(MEDIA_KERNEL, b"k")], OCI_ARCH);
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
        let m = index["manifests"][0].clone();
        index["manifests"].as_array_mut().unwrap().push(m);
        std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
        assert!(image_err(resolve(&dir)).contains("several manifests"));

        // A manifest that is not a Moruna guest.
        let dir = scratch_dir("img-type");
        std::fs::write(dir.join("oci-layout"), "{}").unwrap();
        let (md, ms) = put_blob(&dir, br#"{"layers":[]}"#);
        std::fs::write(
            dir.join("index.json"),
            serde_json::json!({"manifests":[{"mediaType":"m","digest":md,"size":ms}]}).to_string(),
        )
        .unwrap();
        assert!(image_err(resolve(&dir)).contains("artifactType"));

        // Malformed digests and a missing or broken index.
        assert!(image_err(blob_path(&dir, "md5:00")).contains("not sha256"));
        assert!(image_err(blob_path(&dir, "sha256:XYZ")).contains("malformed"));
        std::fs::remove_file(dir.join("index.json")).unwrap();
        assert!(!image_err(resolve(&dir)).is_empty());
        std::fs::write(dir.join("index.json"), "not json").unwrap();
        assert!(!image_err(resolve(&dir)).is_empty());
    }

    #[test]
    fn vm_t4_large_blobs_are_checked_by_size_only() {
        let dir = oci_layout("img-big", &[(MEDIA_KERNEL, b"k")], OCI_ARCH);
        // A descriptor claiming a size above the hashing limit is only size-checked.
        let p = dir.join("big");
        let f = File::create(&p).unwrap();
        f.set_len(VERIFY_MAX_BYTES + 1).unwrap();
        let d = Descriptor {
            media_type: MEDIA_ROOTFS.into(),
            digest: format!("sha256:{}", "0".repeat(64)),
            size: VERIFY_MAX_BYTES + 1,
            platform: None,
        };
        let target = blob_path(&dir, &d.digest).unwrap();
        std::fs::rename(&p, &target).unwrap();
        assert_eq!(verify_blob(&dir, &d).unwrap(), target);
    }
}
