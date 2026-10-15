//! Moruna component 16, the function package: one file per kernel, `.pex` or `.wasm`, with its
//! dependencies and its declaration inside and a fingerprint that is the file.
//!
//! Design: `architecture/sdd/16-package.md`, from `architecture/function-packages.md`. This
//! crate reads a package (once, PK-I1), validates its manifest (PK-I3), checks a job document's
//! pin (PK-I2), reads and writes the manifest section of a component (16 f.3) and extracts a
//! `.pex` into its tree (16 f.6). How a package is run is SDD 17's.

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod host;
pub mod manifest;
pub mod tree;
pub mod wasm;

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub use host::{compatible, host_target};
pub use manifest::{DeclarationDoc, Hints, Manifest};
pub use tree::{Tree, tree, write_beside};
pub use wasm::{embed_manifest, wasm_manifest};

/// The manifest's name: the `.pex` zip entry and the `.wasm` custom section (16 e.1, e.5).
pub const MANIFEST_NAME: &str = "moruna.json";

/// A `.wasm` manifest's entry: the world it exports (16 e.5).
pub const WORLD: &str = "moruna:kernel";

/// A `.wasm` manifest's only platform (16 e.1).
pub const WASM_PLATFORM: &str = "wasm32";

/// The Moruna version that writes manifests and markers.
pub const MORUNA_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `moruna:kernel` world (16 e.5), byte for byte.
pub const WIT: &str = "package moruna:kernel@0.1.0;

world kernel {
  /// A file of the assets directory the host named, by its path relative to it; refused when
  /// there is none, or the path leaves it.
  import read-asset: func(name: string) -> result<list<u8>, string>;
  /// The package's manifest, moruna.json, as text.
  export declare: func() -> string;
  /// One Arrow record batch as an IPC stream in, one out; an error refuses this batch, named.
  export process: func(batch: list<u8>) -> result<list<u8>, string>;
  /// A stateful kernel's state in: empty at a fresh start, the bytes `close` returned on resume.
  export open: func(state: list<u8>) -> result<_, string>;
  /// A stateful kernel's state out.
  export close: func() -> list<u8>;
  /// A kernel built by arguments: the arguments as JSON, once, before anything else.
  export configure: func(args: string) -> result<_, string>;
}
";

/// Why a package was refused (16 d.1).
#[derive(Debug)]
pub enum PackageError {
    /// A file could not be read or written.
    Io {
        /// The file.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// The package or its manifest is not what 16 e.1 to e.6 describe.
    Format {
        /// The field, or the part of the file.
        field: String,
        /// What is wrong.
        reason: String,
    },
    /// A job document's pin does not hold (PK-I2).
    Pin {
        /// What the document pinned.
        pinned: String,
        /// The file's fingerprint.
        actual: String,
    },
    /// The package cannot run here.
    Unsupported(String),
}

impl PackageError {
    /// A refusal of `field` for `reason`.
    pub fn format(field: impl Into<String>, reason: impl Into<String>) -> PackageError {
        PackageError::Format {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// An IO failure on `path`.
    pub fn io(path: &Path, source: std::io::Error) -> PackageError {
        PackageError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageError::Io { path, source } => {
                write!(f, "package: {}: {source}", path.display())
            }
            PackageError::Format { field, reason } => write!(f, "package: {field}: {reason}"),
            PackageError::Pin { pinned, actual } => write!(
                f,
                "package: the document pins {pinned} and the file's fingerprint is {actual}"
            ),
            PackageError::Unsupported(why) => write!(f, "package: {why}"),
        }
    }
}

impl std::error::Error for PackageError {}

impl From<PackageError> for moruna_kernel::MorunaError {
    fn from(e: PackageError) -> moruna_kernel::MorunaError {
        match e {
            PackageError::Pin { .. } => moruna_kernel::MorunaError::Plan(e.to_string()),
            other => moruna_kernel::MorunaError::Config {
                name: "package",
                msg: other.to_string(),
            },
        }
    }
}

/// A package's kind, which is its extension (16 b).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Format {
    /// A Python executable zip with its wheels.
    Pex,
    /// A WebAssembly component exporting `moruna:kernel`.
    Wasm,
}

impl Format {
    /// The format of the package at `path`, from its extension.
    pub fn of_path(path: &Path) -> Result<Format, PackageError> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("pex") => Ok(Format::Pex),
            Some("wasm") => Ok(Format::Wasm),
            _ => Err(PackageError::format(
                path.display().to_string(),
                "a package is a .pex or a .wasm file",
            )),
        }
    }

    /// `pex` or `wasm`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Pex => "pex",
            Format::Wasm => "wasm",
        }
    }
}

/// `sha256` of a package's bytes (16 b).
pub fn fingerprint_of(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// 64 lowercase hex characters.
pub fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a host gives one packaged stage besides the file (16 b, d.4): its arguments and its
/// assets directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inputs {
    /// The arguments, a JSON object; only for a kernel with the hint `args`.
    pub args: Option<serde_json::Value>,
    /// A directory the kernel reads, read only.
    pub assets: Option<PathBuf>,
}

impl Inputs {
    /// Check the inputs against the package (16 f.8): arguments only for a kernel built by them,
    /// and an object; assets an existing directory.
    pub fn check(&self, manifest: &Manifest) -> Result<(), PackageError> {
        if let Some(args) = &self.args {
            if !manifest.hints().args {
                return Err(PackageError::format(
                    "args",
                    format!(
                        "{} is not built from arguments (its manifest has no hint `args`)",
                        manifest.name
                    ),
                ));
            }
            if !args.is_object() {
                return Err(PackageError::format(
                    "args",
                    "the arguments are a JSON object",
                ));
            }
        }
        if let Some(dir) = &self.assets
            && !dir.is_dir()
        {
            return Err(PackageError::format(
                "assets",
                format!("{} is not a directory", dir.display()),
            ));
        }
        Ok(())
    }

    /// The arguments a kernel built by them receives, as JSON: the given object, or `{}`.
    pub fn args_json(&self) -> String {
        self.args
            .as_ref()
            .map_or_else(|| "{}".to_string(), serde_json::Value::to_string)
    }
}

/// A package, read once (16 f.1, PK-I1).
pub struct Package {
    /// Where it was read from.
    pub path: PathBuf,
    /// Its kind.
    pub format: Format,
    /// `sha256` of its bytes.
    pub fingerprint: [u8; 32],
    /// Its manifest.
    pub manifest: Manifest,
    bytes: Vec<u8>,
}

impl std::fmt::Debug for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Package")
            .field("path", &self.path)
            .field("format", &self.format)
            .field("fingerprint", &self.fingerprint_string())
            .finish()
    }
}

impl Package {
    /// Read the package at `path` once, hash those bytes and read its manifest from them.
    pub fn open(path: &Path) -> Result<Package, PackageError> {
        let format = Format::of_path(path)?;
        let bytes = std::fs::read(path).map_err(|e| PackageError::io(path, e))?;
        Package::from_bytes(path, format, bytes)
    }

    /// A package from bytes already read (the bytes are the package; `path` names it).
    pub fn from_bytes(
        path: &Path,
        format: Format,
        bytes: Vec<u8>,
    ) -> Result<Package, PackageError> {
        let fingerprint = fingerprint_of(&bytes);
        let manifest_bytes = match format {
            Format::Pex => pex_manifest(&bytes)?,
            Format::Wasm => wasm_manifest(&bytes)?.ok_or_else(|| {
                PackageError::format(
                    MANIFEST_NAME,
                    "the component has no moruna.json section; `moruna package --wasm` embeds it",
                )
            })?,
        };
        let manifest = Manifest::parse(&manifest_bytes)?;
        if manifest.format != format {
            return Err(PackageError::format(
                "format",
                format!(
                    "the manifest says {} and the file is a .{}",
                    manifest.format.as_str(),
                    format.as_str()
                ),
            ));
        }
        Ok(Package {
            path: path.to_path_buf(),
            format,
            fingerprint,
            manifest,
            bytes,
        })
    }

    /// The bytes that were hashed, which are the bytes a runtime is given.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `sha256:<hex>`.
    pub fn fingerprint_string(&self) -> String {
        format!("sha256:{}", hex(&self.fingerprint))
    }

    /// The fingerprint as the kernel contract's type: a profile row is keyed by the file
    /// (16 f.9).
    pub fn kernel_fingerprint(&self) -> moruna_kernel::Fingerprint {
        moruna_kernel::Fingerprint(self.fingerprint)
    }

    /// A job document's pin (16 f.2, PK-I2): `sha256:<hex>` or `<hex>`.
    pub fn check_pin(&self, pinned: &str) -> Result<(), PackageError> {
        let want = match pinned.split_once(':') {
            Some(("sha256", hex)) => hex,
            Some((algo, _)) => {
                return Err(PackageError::format(
                    "fingerprint",
                    format!("`{algo}` is not the package fingerprint's algorithm, sha256"),
                ));
            }
            None => pinned,
        };
        let actual = hex(&self.fingerprint);
        if want.to_ascii_lowercase() != actual {
            return Err(PackageError::Pin {
                pinned: pinned.to_string(),
                actual: format!("sha256:{actual}"),
            });
        }
        Ok(())
    }
}

/// The `moruna.json` entry of a `.pex`.
fn pex_manifest(bytes: &[u8]) -> Result<Vec<u8>, PackageError> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| PackageError::format("pex", format!("not a zip archive: {e}")))?;
    let mut entry = archive.by_name(MANIFEST_NAME).map_err(|_| {
        PackageError::format(MANIFEST_NAME, "the .pex has no moruna.json at its root")
    })?;
    if entry.size() > manifest::MANIFEST_MAX_BYTES as u64 {
        return Err(PackageError::format(
            MANIFEST_NAME,
            format!(
                "{} bytes is over the {} a manifest may have",
                entry.size(),
                manifest::MANIFEST_MAX_BYTES
            ),
        ));
    }
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut out)
        .map_err(|e| PackageError::format(MANIFEST_NAME, format!("unreadable: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests;
