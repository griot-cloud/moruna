//! One layer's tar laid over the root (SDD 19 f.2): every path checked before anything is
//! written, whiteouts applied to what lower layers left, modes without their special bits.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use tar::EntryType;

use crate::error::{Error, Result};

/// The prefix of a whiteout file: `.wh.<name>` deletes `<name>` from the layers below.
const WHITEOUT: &[u8] = b".wh.";
/// The opaque whiteout: the directory holding it keeps nothing of the layers below.
const OPAQUE: &[u8] = b".wh..wh..opq";
/// The prefix the image spec reserves for a layer's own bookkeeping, which is never a whiteout.
const RESERVED: &[u8] = b".wh..wh.";
/// Symbolic links followed while resolving one entry's parent directory.
const MAX_LINKS: usize = 40;
/// What a mode keeps: the permission bits, never setuid, setgid or sticky.
const PERMISSIONS: u32 = 0o777;

/// One layer being applied to a root.
pub(crate) struct Layer<'a> {
    digest: &'a str,
    root: &'a Path,
    /// The paths (under the root, symbolic links resolved) this layer made, which no whiteout of
    /// the same layer removes.
    added: HashSet<PathBuf>,
    /// Devices, block devices and fifos left out, over every layer of the image.
    skipped: &'a mut u64,
}

impl<'a> Layer<'a> {
    pub(crate) fn new(digest: &'a str, root: &'a Path, skipped: &'a mut u64) -> Layer<'a> {
        Layer {
            digest,
            root,
            added: HashSet::new(),
            skipped,
        }
    }

    /// Apply every entry of the tar in `archive`, in order.
    pub(crate) fn apply<R: Read>(&mut self, archive: R) -> Result<()> {
        let mut archive = tar::Archive::new(archive);
        let entries = archive
            .entries()
            .map_err(|e| Error::io(format!("{}: reading the layer", self.digest), e))?;
        for entry in entries {
            let mut entry = entry
                .map_err(|e| Error::io(format!("{}: reading a layer entry", self.digest), e))?;
            self.entry(&mut entry)?;
        }
        Ok(())
    }

    fn refuse(&self, path: &[u8], reason: impl Into<String>) -> Error {
        Error::entry(self.digest, &lossy(path), reason)
    }

    fn entry<R: Read>(&mut self, entry: &mut tar::Entry<'_, R>) -> Result<()> {
        let raw = entry.path_bytes().into_owned();
        let kind = entry.header().entry_type();
        if kind == EntryType::XGlobalHeader {
            return Ok(());
        }
        let comps = self.normalise(&raw)?;
        let Some((name, parent)) = comps.split_last() else {
            // The archive's root directory, `.`: nothing to make.
            return Ok(());
        };
        if let Some(target) = name.as_bytes().strip_prefix(WHITEOUT) {
            if name.as_bytes() == OPAQUE {
                return self.opaque(&raw, parent);
            }
            if name.as_bytes().starts_with(RESERVED) {
                return Ok(());
            }
            let target = OsStr::from_bytes(target).to_os_string();
            return self.whiteout(&raw, parent, &target);
        }
        let Some(parent_rel) = self.resolve(&raw, parent, true)? else {
            return Err(self.refuse(&raw, "has no parent directory"));
        };
        let rel = parent_rel.join(name);
        let dest = self.root.join(&rel);
        let mode =
            entry.header().mode().map_err(|e| {
                Error::io(format!("{}: the mode of `{}`", self.digest, lossy(&raw)), e)
            })? & PERMISSIONS;
        match kind {
            EntryType::Directory => self.directory(&dest, mode)?,
            EntryType::Regular | EntryType::Continuous => {
                self.regular(&dest, mode, entry)?;
            }
            EntryType::Symlink => {
                let Some(target) = entry.link_name_bytes() else {
                    return Err(self.refuse(&raw, "is a symbolic link with no target"));
                };
                self.clear(&dest)?;
                symlink(OsStr::from_bytes(&target), &dest).map_err(|e| {
                    Error::io(format!("{}: linking `{}`", self.digest, lossy(&raw)), e)
                })?;
            }
            EntryType::Link => self.hard_link(&raw, &dest, entry)?,
            EntryType::Char | EntryType::Block | EntryType::Fifo => {
                *self.skipped += 1;
                tracing::debug!(layer = self.digest, path = %lossy(&raw), "a device or fifo left out");
                return Ok(());
            }
            other => {
                return Err(self.refuse(
                    &raw,
                    format!("is of the entry type {other:?}, which an image layer does not hold"),
                ));
            }
        }
        self.added.insert(rel);
        Ok(())
    }

    /// The components of an archive path: relative, with no `..`; `.` and doubled slashes gone.
    fn normalise(&self, raw: &[u8]) -> Result<Vec<OsString>> {
        let path = Path::new(OsStr::from_bytes(raw));
        if path.has_root() {
            return Err(self.refuse(raw, "is an absolute path"));
        }
        let mut out = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(c) => out.push(c.to_os_string()),
                Component::CurDir => {}
                Component::ParentDir => return Err(self.refuse(raw, "has a `..` component")),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(self.refuse(raw, "is an absolute path"));
                }
            }
        }
        Ok(out)
    }

    /// The directory `comps` names, as a path under the root with every symbolic link resolved
    /// inside it, made when `create`. A link whose target is absolute, or climbs out of the root,
    /// is refused: a later entry would write where the layer must not. `None` when a component
    /// does not exist and `create` is not asked.
    fn resolve(&self, raw: &[u8], comps: &[OsString], create: bool) -> Result<Option<PathBuf>> {
        let mut stack: Vec<OsString> = Vec::new();
        let mut queue: VecDeque<OsString> = comps.iter().cloned().collect();
        let mut followed = 0;
        while let Some(component) = queue.pop_front() {
            if component.as_bytes() == b".." {
                if stack.pop().is_none() {
                    return Err(self.refuse(raw, "passes through a link that leaves the root"));
                }
                continue;
            }
            let at = stack
                .iter()
                .fold(self.root.to_path_buf(), |p, c| p.join(c))
                .join(&component);
            match fs::symlink_metadata(&at) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    followed += 1;
                    if followed > MAX_LINKS {
                        return Err(self.refuse(raw, "passes through too many symbolic links"));
                    }
                    let target = fs::read_link(&at).map_err(|e| {
                        Error::io(
                            format!("{}: reading the link {}", self.digest, at.display()),
                            e,
                        )
                    })?;
                    if target.has_root() {
                        return Err(self.refuse(
                            raw,
                            format!(
                                "passes through the link `{}` to `{}`, which leaves the root",
                                component.to_string_lossy(),
                                target.display()
                            ),
                        ));
                    }
                    for part in target.components().rev() {
                        match part {
                            Component::Normal(c) => queue.push_front(c.to_os_string()),
                            Component::ParentDir => queue.push_front(OsString::from("..")),
                            Component::CurDir => {}
                            Component::RootDir | Component::Prefix(_) => {
                                return Err(self.refuse(raw, "passes through an absolute link"));
                            }
                        }
                    }
                }
                Ok(meta) if meta.is_dir() => stack.push(component),
                Ok(_) => {
                    return Err(self.refuse(
                        raw,
                        format!(
                            "has `{}` among its directories, which is not one",
                            component.to_string_lossy()
                        ),
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if !create {
                        return Ok(None);
                    }
                    make_dir(&at, 0o755, self.digest)?;
                    stack.push(component);
                }
                Err(e) => {
                    return Err(Error::io(
                        format!("{}: looking at {}", self.digest, at.display()),
                        e,
                    ));
                }
            }
        }
        Ok(Some(stack.iter().collect()))
    }

    fn directory(&self, dest: &Path, mode: u32) -> Result<()> {
        match fs::symlink_metadata(dest) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                self.clear(dest)?;
                make_dir(dest, mode, self.digest)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => make_dir(dest, mode, self.digest)?,
            Err(e) => return Err(Error::io(format!("{}: {}", self.digest, dest.display()), e)),
        }
        // The owner can always write to a directory the unpacking is still filling.
        set_mode(dest, mode | 0o700, self.digest)
    }

    fn regular<R: Read>(
        &self,
        dest: &Path,
        mode: u32,
        entry: &mut tar::Entry<'_, R>,
    ) -> Result<()> {
        self.clear(dest)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dest)
            .map_err(|e| Error::io(format!("{}: creating {}", self.digest, dest.display()), e))?;
        io::copy(entry, &mut file)
            .map_err(|e| Error::io(format!("{}: writing {}", self.digest, dest.display()), e))?;
        set_mode(dest, mode, self.digest)
    }

    fn hard_link<R: Read>(
        &self,
        raw: &[u8],
        dest: &Path,
        entry: &mut tar::Entry<'_, R>,
    ) -> Result<()> {
        let Some(target) = entry.link_name_bytes() else {
            return Err(self.refuse(raw, "is a hard link with no target"));
        };
        let outside = || {
            self.refuse(
                raw,
                format!(
                    "is a hard link to `{}`, which is not inside the root",
                    lossy(&target)
                ),
            )
        };
        let missing = || {
            self.refuse(
                raw,
                format!(
                    "is a hard link to `{}`, which an earlier entry must make",
                    lossy(&target)
                ),
            )
        };
        let comps = self.normalise(&target).map_err(|_| outside())?;
        let Some((name, parent)) = comps.split_last() else {
            return Err(missing());
        };
        let Some(parent_rel) = self.resolve(raw, parent, false)? else {
            return Err(missing());
        };
        let source = self.root.join(parent_rel).join(name);
        let meta = match fs::symlink_metadata(&source) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(missing()),
            Err(e) => {
                return Err(Error::io(
                    format!("{}: {}", self.digest, source.display()),
                    e,
                ));
            }
        };
        if meta.is_dir() {
            return Err(self.refuse(raw, "is a hard link to a directory"));
        }
        self.clear(dest)?;
        let linked = if meta.file_type().is_symlink() {
            // A link to a link is the same link made again, whatever the platform's `link` does.
            fs::read_link(&source).and_then(|t| symlink(t, dest))
        } else {
            fs::hard_link(&source, dest)
        };
        linked.map_err(|e| {
            Error::io(
                format!(
                    "{}: linking {} to {}",
                    self.digest,
                    dest.display(),
                    source.display()
                ),
                e,
            )
        })
    }

    /// `.wh.<target>` in `parent`: `target` is gone, unless this layer made it.
    fn whiteout(&self, raw: &[u8], parent: &[OsString], target: &OsStr) -> Result<()> {
        let Some(parent_rel) = self.resolve(raw, parent, false)? else {
            return Ok(());
        };
        let rel = parent_rel.join(target);
        if self.added.contains(&rel) {
            return Ok(());
        }
        remove_any(&self.root.join(rel), self.digest)
    }

    /// `.wh..wh..opq` in `parent`: nothing the layers below put there stays.
    fn opaque(&self, raw: &[u8], parent: &[OsString]) -> Result<()> {
        let Some(parent_rel) = self.resolve(raw, parent, false)? else {
            return Ok(());
        };
        let dir = self.root.join(&parent_rel);
        for child in fs::read_dir(&dir)
            .map_err(|e| Error::io(format!("{}: {}", self.digest, dir.display()), e))?
        {
            let child =
                child.map_err(|e| Error::io(format!("{}: {}", self.digest, dir.display()), e))?;
            if !self.added.contains(&parent_rel.join(child.file_name())) {
                remove_any(&child.path(), self.digest)?;
            }
        }
        Ok(())
    }

    /// Make room for a new entry at `dest`: what is there, if anything, goes.
    fn clear(&self, dest: &Path) -> Result<()> {
        remove_any(dest, self.digest)
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn make_dir(path: &Path, mode: u32, digest: &str) -> Result<()> {
    fs::create_dir(path)
        .map_err(|e| Error::io(format!("{digest}: creating {}", path.display()), e))?;
    set_mode(path, mode | 0o700, digest)
}

fn set_mode(path: &Path, mode: u32, digest: &str) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777)).map_err(|e| {
        Error::io(
            format!("{digest}: setting the mode of {}", path.display()),
            e,
        )
    })
}

/// Remove `path`, a file, a link or a whole tree, when there is one.
pub(crate) fn remove_any(path: &Path, digest: &str) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(format!("{digest}: {}", path.display()), e)),
    };
    let removed = if meta.is_dir() {
        remove_tree(path)
    } else {
        fs::remove_file(path)
    };
    removed.map_err(|e| Error::io(format!("{digest}: removing {}", path.display()), e))
}

/// A tree removed with every directory made writable first: a layer may have said a directory
/// is not the owner's to write.
fn remove_tree(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    for child in fs::read_dir(path)? {
        let child = child?;
        if child.file_type()?.is_dir() {
            remove_tree(&child.path())?;
        } else {
            fs::remove_file(child.path())?;
        }
    }
    fs::remove_dir(path)
}
