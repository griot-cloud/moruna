//! Where an image comes from: the port ([`ImageSource`]), its one adapter ([`OciLayout`]), and
//! the documents an image is made of (SDD 19 d.1, e.1).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::digest::{Digest, verify};
use crate::error::{Error, Result};

/// The annotation an OCI layout's `index.json` names an image by.
pub const REF_NAME: &str = "org.opencontainers.image.ref.name";

/// The manifest media types read: OCI and Docker's schema 2.
const MANIFEST_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// Where an image's blobs are read from (SDD 19 d.1): the manifest a reference names, and any
/// blob by its digest. A registry would be a second adapter; this build has the layout.
pub trait ImageSource: Send + Sync {
    /// The manifest `reference` names: a digest, or a name the source knows.
    fn manifest(&self, reference: &str) -> Result<Manifest>;
    /// The bytes of the blob `digest` names. The reader is not trusted: the caller hashes what
    /// it reads.
    fn blob(&self, digest: &Digest) -> Result<Box<dyn Read>>;
}

/// A blob an image names: its media type, its digest and its size.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Descriptor {
    /// The blob's media type.
    #[serde(rename = "mediaType")]
    pub media_type: String,
    /// The blob's content address.
    pub digest: Digest,
    /// The blob's size in bytes.
    pub size: u64,
}

/// An image manifest, read and verified: its bytes hash to its digest (SDD 19 IM-I1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The manifest's digest, which names the cache entry.
    pub digest: Digest,
    /// The image config's descriptor.
    pub config: Descriptor,
    /// The layers' descriptors, bottom first.
    pub layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct ManifestDoc {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

impl Manifest {
    /// The manifest in `bytes`, which must hash to `digest`; refused when it does not, or when it
    /// is not an image manifest (an index of images needs a platform chosen, which a layout of
    /// one image has no use for).
    pub fn parse(digest: Digest, bytes: &[u8]) -> Result<Manifest> {
        verify(&digest, bytes)?;
        let doc: ManifestDoc = serde_json::from_slice(bytes)
            .map_err(|e| Error::Format(format!("{digest}: not an image manifest: {e}")))?;
        if let Some(media_type) = &doc.media_type
            && !MANIFEST_TYPES.contains(&media_type.as_str())
        {
            return Err(Error::Format(format!(
                "{digest}: the manifest's media type `{media_type}` is not an image manifest \
                 (an index of images is not read; name one image's manifest)"
            )));
        }
        Ok(Manifest {
            digest,
            config: doc.config,
            layers: doc.layers,
        })
    }
}

/// The part of an image config a run reads (SDD 19 e.1); every other field is ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// The image's `Entrypoint`.
    pub entrypoint: Vec<String>,
    /// The image's `Cmd`.
    pub cmd: Vec<String>,
    /// The image's `Env`, each `NAME=value`.
    pub env: Vec<String>,
    /// The image's `WorkingDir`; empty when it names none.
    pub working_dir: String,
    /// The image's `User`; empty when it names none.
    pub user: String,
}

#[derive(Deserialize)]
struct ConfigDoc {
    #[serde(default)]
    config: ContainerConfig,
}

#[derive(Deserialize, Default)]
struct ContainerConfig {
    #[serde(rename = "Entrypoint", default)]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd", default)]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Env", default)]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir", default)]
    working_dir: Option<String>,
    #[serde(rename = "User", default)]
    user: Option<String>,
}

impl Config {
    /// The config in the image config document `bytes`.
    pub fn parse(bytes: &[u8]) -> Result<Config> {
        let doc: ConfigDoc = serde_json::from_slice(bytes).map_err(|e| {
            Error::Format(format!("the image config is not JSON of its shape: {e}"))
        })?;
        let c = doc.config;
        Ok(Config {
            entrypoint: c.entrypoint.unwrap_or_default(),
            cmd: c.cmd.unwrap_or_default(),
            env: c.env.unwrap_or_default(),
            working_dir: c.working_dir.unwrap_or_default(),
            user: c.user.unwrap_or_default(),
        })
    }

    /// The image's own command: its entrypoint and then its cmd.
    pub fn command(&self) -> Vec<String> {
        self.entrypoint
            .iter()
            .chain(self.cmd.iter())
            .cloned()
            .collect()
    }
}

/// A local OCI image layout: `oci-layout`, `index.json` and `blobs/sha256/<hex>` (SDD 19 e.1).
#[derive(Clone, Debug)]
pub struct OciLayout {
    root: PathBuf,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    digest: Digest,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct LayoutMarker {
    #[serde(rename = "imageLayoutVersion")]
    version: String,
}

impl OciLayout {
    /// The layout at `root`, which must hold `oci-layout` of version 1.0.0.
    pub fn open(root: impl Into<PathBuf>) -> Result<OciLayout> {
        let root = root.into();
        let marker = read(&root.join("oci-layout"))?;
        let marker: LayoutMarker = serde_json::from_slice(&marker).map_err(|e| {
            Error::Format(format!("{}: not an OCI layout marker: {e}", root.display()))
        })?;
        if marker.version != "1.0.0" {
            return Err(Error::Format(format!(
                "{}: the layout's version is {}; only 1.0.0 is read",
                root.display(),
                marker.version
            )));
        }
        Ok(OciLayout { root })
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.root.join("blobs").join("sha256").join(digest.hex())
    }

    /// The digest `reference` names: itself when it is a digest, else the manifest `index.json`
    /// gives the annotation `org.opencontainers.image.ref.name`.
    fn resolve(&self, reference: &str) -> Result<Digest> {
        if Digest::is_digest(reference) {
            return reference.parse();
        }
        let path = self.root.join("index.json");
        let index: Index = serde_json::from_slice(&read(&path)?)
            .map_err(|e| Error::Format(format!("{}: not an OCI index: {e}", path.display())))?;
        let named: Vec<&IndexEntry> = index
            .manifests
            .iter()
            .filter(|m| m.annotations.get(REF_NAME).map(String::as_str) == Some(reference))
            .collect();
        match named.as_slice() {
            [one] => Ok(one.digest.clone()),
            [] => {
                let mut known: Vec<&str> = index
                    .manifests
                    .iter()
                    .filter_map(|m| m.annotations.get(REF_NAME).map(String::as_str))
                    .collect();
                known.sort_unstable();
                Err(Error::Format(format!(
                    "{}: no image is named `{reference}`; the layout names {}",
                    path.display(),
                    if known.is_empty() {
                        "none (name an image by its digest)".to_string()
                    } else {
                        known.join(", ")
                    }
                )))
            }
            _ => Err(Error::Format(format!(
                "{}: {} images are named `{reference}`; name one by its digest",
                path.display(),
                named.len()
            ))),
        }
    }
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::io(format!("reading {}", path.display()), e))
}

impl ImageSource for OciLayout {
    fn manifest(&self, reference: &str) -> Result<Manifest> {
        let digest = self.resolve(reference)?;
        let bytes = read(&self.blob_path(&digest))?;
        Manifest::parse(digest, &bytes)
    }

    fn blob(&self, digest: &Digest) -> Result<Box<dyn Read>> {
        let path = self.blob_path(digest);
        let file =
            File::open(&path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        Ok(Box::new(file))
    }
}
