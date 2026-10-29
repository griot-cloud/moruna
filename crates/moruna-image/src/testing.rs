//! The layers and layouts the image tests build: a tar of entries, a layout directory of
//! layers, a config and a manifest, every blob under its digest. Test support, never a build a
//! user runs (feature `testing`).

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{Digest, ImageSource, Manifest, OciLayout, Result};

/// One entry of a layer.
#[derive(Clone, Debug)]
pub enum Entry {
    /// A regular file: path, bytes, mode.
    File(&'static str, Vec<u8>, u32),
    /// A directory: path, mode.
    Dir(&'static str, u32),
    /// A symbolic link: path, target.
    Symlink(&'static str, &'static str),
    /// A hard link: path, target.
    Hardlink(&'static str, &'static str),
    /// A fifo: path.
    Fifo(&'static str),
    /// A character device: path.
    Char(&'static str),
}

impl Entry {
    /// A file with the mode 0o644.
    pub fn file(path: &'static str, bytes: &[u8]) -> Entry {
        Entry::File(path, bytes.to_vec(), 0o644)
    }

    /// A directory with the mode 0o755.
    pub fn dir(path: &'static str) -> Entry {
        Entry::Dir(path, 0o755)
    }
}

/// The tar of `entries`, written as they are: a path may be absolute or hold `..`, which the
/// tar builder's own `set_path` would refuse, because the unpacking must be shown to.
pub fn tar_of(entries: &[Entry]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        let (path, kind, mode, link, data): (&str, tar::EntryType, u32, Option<&str>, &[u8]) =
            match entry {
                Entry::File(p, d, m) => (p, tar::EntryType::Regular, *m, None, d),
                Entry::Dir(p, m) => (p, tar::EntryType::Directory, *m, None, &[]),
                Entry::Symlink(p, t) => (p, tar::EntryType::Symlink, 0o777, Some(t), &[]),
                Entry::Hardlink(p, t) => (p, tar::EntryType::Link, 0o644, Some(t), &[]),
                Entry::Fifo(p) => (p, tar::EntryType::Fifo, 0o644, None, &[]),
                Entry::Char(p) => (p, tar::EntryType::Char, 0o644, None, &[]),
            };
        let mut header = tar::Header::new_gnu();
        {
            let old = header.as_old_mut();
            old.name[..path.len()].copy_from_slice(path.as_bytes());
            if let Some(link) = link {
                old.linkname[..link.len()].copy_from_slice(link.as_bytes());
            }
        }
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder
            .append(&header, data)
            .expect("a tar entry is appended to memory");
    }
    builder.into_inner().expect("a tar in memory is finished")
}

/// `bytes`, gzip compressed.
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).expect("gzip to memory");
    encoder.finish().expect("gzip to memory")
}

/// `bytes`, zstd compressed.
pub fn zstd(bytes: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(bytes, 0).expect("zstd to memory")
}

/// The media types a layer is written with.
pub mod media {
    /// An uncompressed OCI layer.
    pub const TAR: &str = "application/vnd.oci.image.layer.v1.tar";
    /// A gzip OCI layer.
    pub const GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
    /// A zstd OCI layer.
    pub const ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
    /// Docker's gzip layer.
    pub const DOCKER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
}

/// A layout directory being built: layers, a config, and one manifest under a name.
pub struct LayoutBuilder {
    root: PathBuf,
    layers: Vec<(String, Digest, u64)>,
    config: serde_json::Value,
}

impl LayoutBuilder {
    /// A layout in `root`, which is made.
    pub fn new(root: &Path) -> LayoutBuilder {
        fs::create_dir_all(root.join("blobs").join("sha256")).expect("the layout's blobs");
        fs::write(
            root.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .expect("the layout marker");
        LayoutBuilder {
            root: root.to_path_buf(),
            layers: Vec::new(),
            config: serde_json::json!({"config": {}}),
        }
    }

    /// The image config's `config` object.
    pub fn config(mut self, config: serde_json::Value) -> LayoutBuilder {
        self.config = serde_json::json!({"config": config});
        self
    }

    fn put(&self, bytes: &[u8]) -> Digest {
        let digest = Digest::of(bytes);
        self.put_as(&digest, bytes);
        digest
    }

    /// `bytes` stored under `digest`, whatever they hash to.
    pub fn put_as(&self, digest: &Digest, bytes: &[u8]) {
        fs::write(
            self.root.join("blobs").join("sha256").join(digest.hex()),
            bytes,
        )
        .expect("a blob");
    }

    /// A layer of `entries`, compressed as `media_type` says.
    pub fn layer(mut self, media_type: &str, entries: &[Entry]) -> LayoutBuilder {
        let tar = tar_of(entries);
        let bytes = match media_type {
            media::GZIP | media::DOCKER_GZIP => gzip(&tar),
            media::ZSTD => zstd(&tar),
            _ => tar,
        };
        self = self.layer_bytes(media_type, &bytes);
        self
    }

    /// A layer whose blob is `bytes`, as given.
    pub fn layer_bytes(mut self, media_type: &str, bytes: &[u8]) -> LayoutBuilder {
        let digest = self.put(bytes);
        self.layers
            .push((media_type.to_string(), digest, bytes.len() as u64));
        self
    }

    /// A layer that claims `digest` and `size` and holds `bytes`, which hash to something else.
    pub fn layer_claiming(
        mut self,
        media_type: &str,
        digest: &Digest,
        bytes: &[u8],
        size: u64,
    ) -> LayoutBuilder {
        self.put_as(digest, bytes);
        self.layers
            .push((media_type.to_string(), digest.clone(), size));
        self
    }

    /// The manifest, config and index written, the image named `name` in the index; the
    /// manifest's digest.
    pub fn finish(self, name: Option<&str>) -> Digest {
        let config = serde_json::to_vec(&self.config).expect("a config");
        let config_digest = self.put(&config);
        let layers: Vec<serde_json::Value> = self
            .layers
            .iter()
            .map(|(media_type, digest, size)| {
                serde_json::json!({"mediaType": media_type, "digest": digest.to_string(),
                                   "size": size})
            })
            .collect();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": config_digest.to_string(), "size": config.len()},
            "layers": layers,
        }))
        .expect("a manifest");
        let digest = self.put(&manifest);
        let mut entry = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest.to_string(), "size": manifest.len(),
        });
        if let Some(name) = name {
            entry["annotations"] = serde_json::json!({crate::source::REF_NAME: name});
        }
        fs::write(
            self.root.join("index.json"),
            serde_json::to_vec(&serde_json::json!({"schemaVersion": 2, "manifests": [entry]}))
                .expect("an index"),
        )
        .expect("the index");
        digest
    }
}

/// A source that counts what is asked of it.
pub struct Counting {
    inner: OciLayout,
    /// How many layers or configs were opened.
    pub blobs: AtomicUsize,
    /// How many manifests were asked for.
    pub manifests: AtomicUsize,
}

impl Counting {
    /// `inner`, counted.
    pub fn new(inner: OciLayout) -> Counting {
        Counting {
            inner,
            blobs: AtomicUsize::new(0),
            manifests: AtomicUsize::new(0),
        }
    }
}

impl ImageSource for Counting {
    fn manifest(&self, reference: &str) -> Result<Manifest> {
        self.manifests.fetch_add(1, Ordering::SeqCst);
        self.inner.manifest(reference)
    }

    fn blob(&self, digest: &Digest) -> Result<Box<dyn std::io::Read>> {
        self.blobs.fetch_add(1, Ordering::SeqCst);
        self.inner.blob(digest)
    }
}

/// A scratch directory that is removed with it.
pub struct Scratch(PathBuf);

impl Scratch {
    /// A new empty directory named for `name` and this process.
    pub fn new(name: &str) -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "moruna-image-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a scratch directory");
        Scratch(dir)
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = crate::layer::remove_any(&self.0, "scratch");
    }
}

/// Bytes no compression shrinks, the same every run: a xorshift stream from `seed`, with
/// no zero byte (so no page of it is a hole).
pub fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8 | 1
        })
        .collect()
}

/// What [`fixture`] wrote.
#[derive(Clone, Debug)]
pub struct Fixture {
    /// The layer files, lowest first, and their ids.
    pub layers: Vec<(PathBuf, Digest)>,
    /// The list of the merged tree the boot's test checks against.
    pub checks: PathBuf,
    /// The layer list the image filesystem server starts with.
    pub list: PathBuf,
}

/// The file the cold start reads, in the 100 MiB layer.
pub const COLD: &str = "/cold/a.txt";
/// The 100 MiB file.
pub const BIG: &str = "/data/big.bin";

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The CI boot's image (SDD 27, k): two layers converted to layer files (`layer-1.img`,
/// the lower, and `layer-2.img`, over 100 MiB), the server's layer list naming them
/// `block1` and `block2`, and the list of what SDD 19's unpacking of the same image
/// holds, with each file's SHA-256, which the boot's test checks the merged tree
/// against. Whiteouts, an opaque directory, replacements both ways, a hard link, a
/// lower layer's link followed by an upper layer's path, a file of mode 0, holes, and
/// eighty small files of varied lengths.
pub fn fixture(out: &Path) -> Result<Fixture> {
    fs::create_dir_all(out)
        .map_err(|e| crate::Error::io(format!("making {}", out.display()), e))?;
    let mut lower = vec![
        Entry::dir("bin"),
        Entry::File("bin/sh", noise(1, 20_000), 0o755),
        Entry::Hardlink("bin/sh2", "bin/sh"),
        Entry::File("bin/ls", b"ls\n".to_vec(), 0o755),
        Entry::Symlink("bin/cat", "sh"),
        Entry::dir("etc"),
        Entry::file("etc/passwd", b"root:x:0:0:root:/root:/bin/sh\n"),
        Entry::file("etc/old", b"old\n"),
        Entry::file("etc/hostname", b"lower\n"),
        Entry::dir("gone"),
        Entry::file("gone/a", b"a"),
        Entry::dir("gone/b"),
        Entry::file("gone/b/c", b"c"),
        Entry::dir("opq"),
        Entry::file("opq/lower1", b"1"),
        Entry::file("opq/lower2", b"2"),
        Entry::file("replaced", b"a file below"),
        Entry::Symlink("lib", "usr/lib"),
        Entry::dir("usr"),
        Entry::dir("usr/lib"),
        Entry::File("usr/lib/libc.so", noise(2, 300 * 1024), 0o755),
        Entry::file("usr/lib/empty", b""),
        Entry::dir("usr/share"),
        Entry::dir("usr/share/doc"),
        Entry::file("usr/share/doc/readme.txt", b"a document\n"),
        Entry::File("data", Vec::new(), 0o755),
    ];
    // `data` is a file below and a directory above.
    let mut sparse = vec![0u8; 1 << 20];
    sparse.extend(noise(3, 8192));
    sparse.extend(vec![0u8; 3000]);
    lower.push(Entry::file("sparse.img", &sparse));
    lower.push(Entry::File("locked", b"mode 0".to_vec(), 0o000));
    let mut upper = vec![
        Entry::file(
            "etc/passwd",
            b"root:x:0:0:root:/root:/bin/sh\nmoruna:x:1:1::/:/bin/sh\n",
        ),
        Entry::file("etc/.wh.old", b""),
        Entry::file("etc/new", b"new\n"),
        Entry::file(".wh.gone", b""),
        Entry::file("opq/.wh..wh..opq", b""),
        Entry::file("opq/upper", b"upper"),
        Entry::dir("replaced"),
        Entry::file("replaced/inside", b"now a directory"),
        Entry::File("lib/libextra.so", noise(4, 50_000), 0o755),
        Entry::file("usr/lib/.wh.empty", b""),
        Entry::Symlink("bin/ls", "sh"),
        Entry::dir("cold"),
        Entry::File("cold/a.txt", noise(5, 4096), 0o644),
        Entry::dir("data"),
        Entry::File("data/big.bin", noise(6, 100 << 20), 0o644),
        Entry::dir("data/small"),
    ];
    for i in 0..80usize {
        let len = (i * 1361) % 9000;
        upper.push(Entry::File(
            leak(format!("data/small/f{i:03}")),
            noise(100 + i as u64, len),
            0o640,
        ));
    }
    let layout = out.join("layout");
    let _ = crate::layer::remove_any(&layout, "fixture");
    LayoutBuilder::new(&layout)
        .layer(media::TAR, &lower)
        .layer(media::TAR, &upper)
        .finish(Some("kc2:1"));
    let source = OciLayout::open(&layout)?;
    let converted = crate::layerfile::convert(&source, "kc2:1", &out.join("converted"))?;
    let mut layers = Vec::new();
    let mut list = String::from("moruna-layers 1\n");
    for (i, layer) in converted.layers.iter().enumerate() {
        let to = out.join(format!("layer-{}.img", i + 1));
        fs::rename(&layer.path, &to)
            .map_err(|e| crate::Error::io(format!("moving {}", to.display()), e))?;
        list.push_str(&format!("layer block{} {}\n", i + 1, layer.id.hex()));
        layers.push((to, layer.id.clone()));
    }
    let cache = out.join("cache");
    let _ = crate::layer::remove_any(&cache, "fixture");
    let image = crate::unpack(&source, "kc2:1", &cache)?;
    let mut lines = vec![format!("cold {COLD}"), format!("big {BIG}")];
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        let dir = image.rootfs.join(&rel);
        let mut children: Vec<_> = fs::read_dir(&dir)
            .map_err(|e| crate::Error::io(format!("listing {}", dir.display()), e))?
            .collect::<std::io::Result<_>>()
            .map_err(|e| crate::Error::io(format!("listing {}", dir.display()), e))?;
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let rel = rel.join(child.file_name());
            let path = format!("/{}", rel.display());
            if crate::MOUNT_POINTS.iter().any(|m| path == format!("/{m}")) {
                continue;
            }
            let meta = fs::symlink_metadata(child.path())
                .map_err(|e| crate::Error::io(format!("reading {path}"), e))?;
            let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777;
            if meta.file_type().is_symlink() {
                let target = fs::read_link(child.path())
                    .map_err(|e| crate::Error::io(format!("reading {path}"), e))?;
                lines.push(format!("l {} {path}", target.display()));
            } else if meta.is_dir() {
                lines.push(format!("d {mode:o} {path}"));
                todo.push(rel);
            } else {
                if mode & 0o400 == 0 {
                    fs::set_permissions(child.path(), fs::Permissions::from_mode(0o600))
                        .map_err(|e| crate::Error::io(format!("reading {path}"), e))?;
                }
                let bytes = fs::read(child.path())
                    .map_err(|e| crate::Error::io(format!("reading {path}"), e))?;
                lines.push(format!(
                    "f {mode:o} {} {} {path}",
                    bytes.len(),
                    Digest::of(&bytes).hex()
                ));
            }
        }
    }
    let body = lines.join("\n") + "\n";
    // The header carries the list's length, so the test knows how much to map: its
    // own length is counted, digits and all.
    let header = (1..20)
        .map(|digits| "moruna-checks 1 ".len() + digits + 1 + body.len())
        .find(|total| format!("moruna-checks 1 {total}\n").len() + body.len() == *total)
        .map(|total| format!("moruna-checks 1 {total}\n"))
        .ok_or_else(|| crate::Error::Format("the check list's length has no fixed point".into()))?;
    let checks = out.join("imagefs.checks");
    fs::write(&checks, header + &body)
        .map_err(|e| crate::Error::io(format!("writing {}", checks.display()), e))?;
    let list_path = out.join("imagefs.layers");
    fs::write(&list_path, list)
        .map_err(|e| crate::Error::io(format!("writing {}", list_path.display()), e))?;
    let _ = crate::layer::remove_any(&cache, "fixture");
    let _ = crate::layer::remove_any(&layout, "fixture");
    let _ = crate::layer::remove_any(&out.join("converted"), "fixture");
    Ok(Fixture {
        layers,
        checks,
        list: list_path,
    })
}
