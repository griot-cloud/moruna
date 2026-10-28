//! The layers and layouts the image tests build: a tar of entries, a layout directory of
//! layers, a config and a manifest, every blob under its digest. Test support, never a build a
//! user runs (feature `testing`).

use std::fs;
use std::io::Write;
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
