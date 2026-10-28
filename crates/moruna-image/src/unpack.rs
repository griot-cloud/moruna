//! An image unpacked into the cache (SDD 19 f.1, f.3, f.4): every blob checked as it is read,
//! the layers laid over one another, the entry made whole in a directory of its own and renamed
//! into place.

use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::MOUNT_POINTS;
use crate::digest::{Digest, Hashing};
use crate::error::{Error, Result};
use crate::layer::{Layer, remove_any};
use crate::source::{Config, Descriptor, ImageSource, Manifest};

/// The file a cache entry holds when, and only when, it was made whole.
pub const COMPLETE: &str = "complete";

/// An image config is a few kilobytes; one longer than this is not one.
const CONFIG_MAX: u64 = 16 << 20;

/// An unpacked image: where its root is and what it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// The manifest's digest, which names the cache entry.
    pub digest: Digest,
    /// What the image config says to run.
    pub config: Config,
    /// The root filesystem, `<cache>/<digest hex>/rootfs`.
    pub rootfs: PathBuf,
    /// Character devices, block devices and fifos the layers held and the root does not
    /// (SDD 19 f.2).
    pub skipped: u64,
}

/// How a layer's bytes are compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Zstd,
}

fn compression(layer: &Descriptor) -> Result<Compression> {
    match layer.media_type.as_str() {
        "application/vnd.oci.image.layer.v1.tar" => Ok(Compression::None),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Ok(Compression::Gzip),
        "application/vnd.oci.image.layer.v1.tar+zstd" => Ok(Compression::Zstd),
        other => Err(Error::MediaType {
            digest: layer.digest.to_string(),
            media_type: other.to_string(),
        }),
    }
}

/// The image `reference` names in `source`, unpacked under `cache` (SDD 19 f.1): the manifest,
/// the config and every layer verified as read; returned as it is when the cache already holds it
/// whole, without a layer read; otherwise unpacked in a directory of its own and renamed into
/// place, and when another unpacking got there first, theirs is used and ours is dropped.
pub fn unpack(source: &dyn ImageSource, reference: &str, cache: &Path) -> Result<Image> {
    let manifest = source.manifest(reference)?;
    for layer in &manifest.layers {
        compression(layer)?;
    }
    fs::create_dir_all(cache)
        .map_err(|e| Error::io(format!("making the cache {}", cache.display()), e))?;
    let entry = cache.join(manifest.digest.hex());
    if let Some(image) = existing(&entry, &manifest.digest)? {
        return Ok(image);
    }
    let temp = cache.join(format!(".tmp-{}", random()?));
    fs::create_dir(&temp).map_err(|e| Error::io(format!("making {}", temp.display()), e))?;
    if let Err(e) = build(source, &manifest, &temp) {
        let _ = remove_any(&temp, "cleaning up");
        return Err(e);
    }
    match fs::rename(&temp, &entry) {
        Ok(()) => {}
        Err(e) => {
            // Another unpacking of the same image got there first, or this is a real failure.
            let _ = remove_any(&temp, "cleaning up");
            if !entry.join(COMPLETE).is_file() {
                return Err(Error::io(
                    format!(
                        "moving {} into place at {}",
                        temp.display(),
                        entry.display()
                    ),
                    e,
                ));
            }
        }
    }
    existing(&entry, &manifest.digest)?.ok_or_else(|| Error::Cache {
        path: entry,
        reason: "the entry was made whole and is gone".into(),
    })
}

/// The cache entry, when it is whole; refused when it is there and not.
fn existing(entry: &Path, digest: &Digest) -> Result<Option<Image>> {
    if !entry.exists() {
        return Ok(None);
    }
    let complete = entry.join(COMPLETE);
    let skipped = match fs::read_to_string(&complete) {
        Ok(text) => text.trim().parse::<u64>().map_err(|_| Error::Cache {
            path: complete.clone(),
            reason: format!("`{}` is not a count", text.trim()),
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Error::Cache {
                path: entry.to_path_buf(),
                reason: format!(
                    "the entry exists and holds no `{COMPLETE}` file: it was not made by an \
                     unpacking that finished; remove it"
                ),
            });
        }
        Err(e) => return Err(Error::io(format!("reading {}", complete.display()), e)),
    };
    let config_path = entry.join("config.json");
    let bytes = fs::read(&config_path)
        .map_err(|e| Error::io(format!("reading {}", config_path.display()), e))?;
    Ok(Some(Image {
        digest: digest.clone(),
        config: Config::parse(&bytes)?,
        rootfs: entry.join("rootfs"),
        skipped,
    }))
}

/// The whole image in `temp`: `config.json`, `rootfs/` and, last, `complete`.
fn build(source: &dyn ImageSource, manifest: &Manifest, temp: &Path) -> Result<()> {
    let config = blob_bytes(source, &manifest.config)?;
    Config::parse(&config)?;
    fs::write(temp.join("config.json"), &config)
        .map_err(|e| Error::io(format!("writing {}/config.json", temp.display()), e))?;
    let rootfs = temp.join("rootfs");
    fs::create_dir(&rootfs).map_err(|e| Error::io(format!("making {}", rootfs.display()), e))?;
    let mut skipped = 0u64;
    for layer in &manifest.layers {
        apply_layer(source, layer, &rootfs, &mut skipped)?;
    }
    mount_points(&rootfs)?;
    fs::write(temp.join(COMPLETE), format!("{skipped}\n"))
        .map_err(|e| Error::io(format!("writing {}/{COMPLETE}", temp.display()), e))
}

/// The bytes of a small blob, checked against its descriptor.
fn blob_bytes(source: &dyn ImageSource, descriptor: &Descriptor) -> Result<Vec<u8>> {
    let mut reader = Hashing::new(source.blob(&descriptor.digest)?);
    let mut bytes = Vec::new();
    (&mut reader)
        .take(CONFIG_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::io(format!("{}: reading the blob", descriptor.digest), e))?;
    if bytes.len() as u64 > CONFIG_MAX {
        return Err(Error::Format(format!(
            "{}: the blob is longer than {CONFIG_MAX} bytes, which no image config is",
            descriptor.digest
        )));
    }
    check(descriptor, reader)?;
    Ok(bytes)
}

/// The blob read through `reader` hashes to its descriptor's digest and has its size.
fn check(descriptor: &Descriptor, reader: Hashing<impl Read>) -> Result<()> {
    let (actual, read) = reader
        .finish()
        .map_err(|e| Error::io(format!("{}: reading the blob", descriptor.digest), e))?;
    if actual != descriptor.digest {
        return Err(Error::Digest {
            digest: descriptor.digest.to_string(),
            actual: actual.to_string(),
        });
    }
    if read != descriptor.size {
        return Err(Error::Size {
            digest: descriptor.digest.to_string(),
            declared: descriptor.size,
            actual: read,
        });
    }
    Ok(())
}

fn apply_layer(
    source: &dyn ImageSource,
    descriptor: &Descriptor,
    rootfs: &Path,
    skipped: &mut u64,
) -> Result<()> {
    let compression = compression(descriptor)?;
    let name = descriptor.digest.to_string();
    let mut hashing = Hashing::new(source.blob(&descriptor.digest)?);
    let applied = (|| -> Result<()> {
        let mut layer = Layer::new(&name, rootfs, skipped);
        let stream: Box<dyn Read + '_> = match compression {
            Compression::None => Box::new(&mut hashing),
            Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(&mut hashing)),
            Compression::Zstd => Box::new(
                zstd::stream::read::Decoder::new(&mut hashing)
                    .map_err(|e| Error::io(format!("{name}: opening the zstd stream"), e))?,
            ),
        };
        layer.apply(BufReader::new(stream))
    })();
    match applied {
        Ok(()) => check(descriptor, hashing),
        Err(e) => {
            // A blob that is not the bytes it is named by fails in whatever way those bytes
            // fail; the digest is the cause worth naming.
            match check(descriptor, hashing) {
                Err(mismatch @ (Error::Digest { .. } | Error::Size { .. })) => Err(mismatch),
                _ => Err(e),
            }
        }
    }
}

/// The directories the sandbox mounts onto exist, and are directories (SDD 19 f.3).
fn mount_points(rootfs: &Path) -> Result<()> {
    for name in MOUNT_POINTS {
        let path = rootfs.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(Error::Format(format!(
                    "the image holds `/{name}`, which is not a directory: the sandbox mounts \
                     there"
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&path)
                    .map_err(|e| Error::io(format!("making {}", path.display()), e))?;
            }
            Err(e) => return Err(Error::io(format!("looking at {}", path.display()), e)),
        }
    }
    Ok(())
}

/// Eight random bytes in hex: what keeps two unpackings' directories apart.
fn random() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::io("choosing a directory name", io::Error::other(e.to_string())))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
