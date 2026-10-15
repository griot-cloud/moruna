//! A `.pex` extracted, with its bytecode (16 e.4, f.6, PK-I7).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Format, MORUNA_VERSION, Package, PackageError, hex};

/// The marker file at a tree's root: `<fingerprint> <moruna version>\n`, written last.
pub const MARKER: &str = "MORUNA-TREE";

/// Distinguishes the temporary directories of one process.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// An extracted `.pex`.
#[derive(Debug)]
pub struct Tree {
    /// The tree's root: `src/`, `deps/<target>/`, `moruna.json`.
    pub root: PathBuf,
    /// What happened that a run should say: a private directory, a fresh extraction.
    pub notes: Vec<String>,
    /// True when the tree is this process's own and is removed when dropped.
    private: bool,
}

impl Tree {
    /// The project's files.
    pub fn src(&self) -> PathBuf {
        self.root.join("src")
    }

    /// The installed wheels for `target`.
    pub fn deps(&self, target: &str) -> PathBuf {
        self.root.join("deps").join(target)
    }

    /// True when the tree is a private directory of this process.
    pub fn is_private(&self) -> bool {
        self.private
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if self.private {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// `<package>.tree`, beside the package.
pub fn beside(package: &Path) -> PathBuf {
    let mut name = package.as_os_str().to_os_string();
    name.push(".tree");
    PathBuf::from(name)
}

fn marker_text(package: &Package) -> String {
    format!("sha256:{} {MORUNA_VERSION}\n", hex(&package.fingerprint))
}

/// Whether `dir` is a complete tree of `package` for this Moruna.
pub fn is_current(dir: &Path, package: &Package) -> bool {
    fs::read_to_string(dir.join(MARKER)).is_ok_and(|text| text == marker_text(package))
}

fn not_pex(package: &Package) -> Result<(), PackageError> {
    if package.format != Format::Pex {
        return Err(PackageError::format(
            "format",
            "only a .pex has a tree; a .wasm is compiled",
        ));
    }
    Ok(())
}

/// The tree of `package` (16 f.6): the one beside it when its marker matches, else extracted
/// there, else, when that place cannot be written, extracted into a private directory that is
/// removed with the returned `Tree`.
pub fn tree(package: &Package) -> Result<Tree, PackageError> {
    not_pex(package)?;
    let place = beside(&package.path);
    if is_current(&place, package) {
        return Ok(Tree {
            root: place,
            notes: Vec::new(),
            private: false,
        });
    }
    match extract_beside(package) {
        Ok(root) => Ok(Tree {
            root,
            notes: vec![format!(
                "{}: extracted beside the package",
                package.path.display()
            )],
            private: false,
        }),
        Err(PackageError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::PermissionDenied
                || source.kind() == std::io::ErrorKind::ReadOnlyFilesystem =>
        {
            let root = std::env::temp_dir().join(format!(
                "moruna-tree-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            extract(package, &root)?;
            Ok(Tree {
                notes: vec![format!(
                    "{} cannot be written beside the package; extracted to a private directory \
                     for this process",
                    place.display()
                )],
                root,
                private: true,
            })
        }
        Err(e) => Err(e),
    }
}

/// Write the tree beside the package (`moruna aot`, 16 f.10), refusing when that place cannot
/// be written. Returns the tree's path.
pub fn write_beside(package: &Package) -> Result<PathBuf, PackageError> {
    not_pex(package)?;
    let place = beside(&package.path);
    if is_current(&place, package) {
        return Ok(place);
    }
    extract_beside(package)
}

fn extract_beside(package: &Package) -> Result<PathBuf, PackageError> {
    let place = beside(&package.path);
    let mut tmp = place.as_os_str().to_os_string();
    tmp.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    if let Err(e) = extract(package, &tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if is_current(&place, package) {
        // Another process finished first (16 g); its tree is the same tree.
        let _ = fs::remove_dir_all(&tmp);
        return Ok(place);
    }
    if place.exists() {
        fs::remove_dir_all(&place).map_err(|e| PackageError::io(&place, e))?;
    }
    if let Err(e) = fs::rename(&tmp, &place) {
        let _ = fs::remove_dir_all(&tmp);
        if is_current(&place, package) {
            return Ok(place);
        }
        return Err(PackageError::io(&place, e));
    }
    Ok(place)
}

/// Extract every entry of the package's zip under `root` and write the marker last.
fn extract(package: &Package, root: &Path) -> Result<(), PackageError> {
    fs::create_dir_all(root).map_err(|e| PackageError::io(root, e))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(package.bytes()))
        .map_err(|e| PackageError::format("pex", format!("not a zip archive: {e}")))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| PackageError::format("pex", format!("entry {i}: {e}")))?;
        let name = entry
            .name()
            .map(|n| n.to_string())
            .map_err(|e| PackageError::format("pex", format!("entry {i}: {e}")))?;
        let relative = entry.enclosed_name().ok_or_else(|| {
            PackageError::format("pex", format!("`{name}` would be written outside the tree"))
        })?;
        let path = root.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&path).map_err(|e| PackageError::io(&path, e))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| PackageError::io(parent, e))?;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| PackageError::format("pex", format!("`{name}`: {e}")))?;
        let mut file = fs::File::create(&path).map_err(|e| PackageError::io(&path, e))?;
        file.write_all(&bytes)
            .map_err(|e| PackageError::io(&path, e))?;
    }
    let marker = root.join(MARKER);
    fs::write(&marker, marker_text(package)).map_err(|e| PackageError::io(&marker, e))?;
    Ok(())
}
