//! One layer's tar laid over the root (SDD 19 f.2): every path checked before anything is
//! written, whiteouts applied to what lower layers left, modes without their special bits.

use std::collections::{BTreeSet, HashSet, VecDeque};
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

/// What one layer did to the root, as the unpacking did it: the exact paths it touched, relative
/// to the root, with every symbolic link of a lower layer on the way already followed. A layer
/// file is written from this record (SDD 27 f.1) and never from a comparison of two trees.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// Created or replaced, of any kind: a file, a directory (also one the layer names that was
    /// there, whose mode it sets, and one made on the way to an entry), a symbolic link, a hard
    /// link. A path the layer made and later removed is not here.
    pub(crate) written: BTreeSet<PathBuf>,
    /// Removed from the layers below by a whiteout or an opaque marker, and everything under a
    /// removed directory. A path the layer made itself is never here, and one that was removed
    /// and made again is here and in `written`.
    pub(crate) removed: BTreeSet<PathBuf>,
    /// Directories that hide what the layers below held in them: one with an opaque marker, one
    /// made where a non-directory was, and one made where this layer had removed a path.
    pub(crate) opaque: BTreeSet<PathBuf>,
}

/// One layer being applied to a root.
pub(crate) struct Layer<'a> {
    digest: &'a str,
    root: &'a Path,
    /// The paths (under the root, symbolic links resolved) this layer made, which no whiteout of
    /// the same layer removes.
    added: HashSet<PathBuf>,
    /// What the layer has done so far.
    applied: Applied,
    /// Devices, block devices and fifos left out, over every layer of the image.
    skipped: &'a mut u64,
}

impl<'a> Layer<'a> {
    pub(crate) fn new(digest: &'a str, root: &'a Path, skipped: &'a mut u64) -> Layer<'a> {
        Layer {
            digest,
            root,
            added: HashSet::new(),
            applied: Applied::default(),
            skipped,
        }
    }

    /// Apply every entry of the tar in `archive`, in order, and say what was done.
    pub(crate) fn apply<R: Read>(&mut self, archive: R) -> Result<Applied> {
        let mut archive = tar::Archive::new(archive);
        let entries = archive
            .entries()
            .map_err(|e| Error::io(format!("{}: reading the layer", self.digest), e))?;
        for entry in entries {
            let mut entry = entry
                .map_err(|e| Error::io(format!("{}: reading a layer entry", self.digest), e))?;
            self.entry(&mut entry)?;
        }
        Ok(std::mem::take(&mut self.applied))
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
        let mut made = Vec::new();
        let resolved = self.resolve(&raw, parent, Some(&mut made))?;
        for dir in made {
            self.made_dir(dir);
        }
        let Some(parent_rel) = resolved else {
            return Err(self.refuse(&raw, "has no parent directory"));
        };
        let rel = parent_rel.join(name);
        let dest = self.root.join(&rel);
        let mode =
            entry.header().mode().map_err(|e| {
                Error::io(format!("{}: the mode of `{}`", self.digest, lossy(&raw)), e)
            })? & PERMISSIONS;
        match kind {
            EntryType::Directory => self.directory(&rel, mode)?,
            EntryType::Regular | EntryType::Continuous => {
                self.regular(&rel, mode, entry)?;
            }
            EntryType::Symlink => {
                let Some(target) = entry.link_name_bytes() else {
                    return Err(self.refuse(&raw, "is a symbolic link with no target"));
                };
                self.clear(&rel)?;
                symlink(OsStr::from_bytes(&target), &dest).map_err(|e| {
                    Error::io(format!("{}: linking `{}`", self.digest, lossy(&raw)), e)
                })?;
            }
            EntryType::Link => self.hard_link(&raw, &rel, entry)?,
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
        self.applied.written.insert(rel.clone());
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
    /// does not exist and `create` is not asked; when it is, the directories made are pushed to it.
    fn resolve(
        &self,
        raw: &[u8],
        comps: &[OsString],
        mut create: Option<&mut Vec<PathBuf>>,
    ) -> Result<Option<PathBuf>> {
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
                    let Some(made) = create.as_deref_mut() else {
                        return Ok(None);
                    };
                    make_dir(&at, 0o755, self.digest)?;
                    stack.push(component);
                    made.push(stack.iter().collect());
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

    fn directory(&mut self, rel: &Path, mode: u32) -> Result<()> {
        let dest = self.root.join(rel);
        match fs::symlink_metadata(&dest) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                // Where a non-directory was, nothing of the layers below shows through.
                self.clear(rel)?;
                make_dir(&dest, mode, self.digest)?;
                self.applied.opaque.insert(rel.to_path_buf());
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                make_dir(&dest, mode, self.digest)?;
                self.made_dir(rel.to_path_buf());
            }
            Err(e) => return Err(Error::io(format!("{}: {}", self.digest, dest.display()), e)),
        }
        // The owner can always write to a directory the unpacking is still filling.
        set_mode(&dest, mode | 0o700, self.digest)
    }

    /// A directory made where nothing is: it is written, and it hides what the layers below held
    /// at its path when this layer removed that.
    fn made_dir(&mut self, rel: PathBuf) {
        if self.applied.removed.contains(&rel) {
            self.applied.opaque.insert(rel.clone());
        }
        self.applied.written.insert(rel);
    }

    fn regular<R: Read>(
        &mut self,
        rel: &Path,
        mode: u32,
        entry: &mut tar::Entry<'_, R>,
    ) -> Result<()> {
        self.clear(rel)?;
        let dest = self.root.join(rel);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&dest)
            .map_err(|e| Error::io(format!("{}: creating {}", self.digest, dest.display()), e))?;
        io::copy(entry, &mut file)
            .map_err(|e| Error::io(format!("{}: writing {}", self.digest, dest.display()), e))?;
        set_mode(&dest, mode, self.digest)
    }

    fn hard_link<R: Read>(
        &mut self,
        raw: &[u8],
        rel: &Path,
        entry: &mut tar::Entry<'_, R>,
    ) -> Result<()> {
        let dest = self.root.join(rel);
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
        let Some(parent_rel) = self.resolve(raw, parent, None)? else {
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
        self.clear(rel)?;
        let linked = if meta.file_type().is_symlink() {
            // A link to a link is the same link made again, whatever the platform's `link` does.
            fs::read_link(&source).and_then(|t| symlink(t, &dest))
        } else {
            fs::hard_link(&source, &dest)
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
    fn whiteout(&mut self, raw: &[u8], parent: &[OsString], target: &OsStr) -> Result<()> {
        let Some(parent_rel) = self.resolve(raw, parent, None)? else {
            return Ok(());
        };
        let rel = parent_rel.join(target);
        if self.added.contains(&rel) {
            return Ok(());
        }
        self.remove(&rel)
    }

    /// `.wh..wh..opq` in `parent`: nothing the layers below put there stays.
    fn opaque(&mut self, raw: &[u8], parent: &[OsString]) -> Result<()> {
        let Some(parent_rel) = self.resolve(raw, parent, None)? else {
            return Ok(());
        };
        let dir = self.root.join(&parent_rel);
        let mut doomed = Vec::new();
        for child in fs::read_dir(&dir)
            .map_err(|e| Error::io(format!("{}: {}", self.digest, dir.display()), e))?
        {
            let child =
                child.map_err(|e| Error::io(format!("{}: {}", self.digest, dir.display()), e))?;
            let rel = parent_rel.join(child.file_name());
            if !self.added.contains(&rel) {
                doomed.push(rel);
            }
        }
        for rel in doomed {
            self.remove(&rel)?;
        }
        self.applied.opaque.insert(parent_rel);
        Ok(())
    }

    /// Remove `rel` and all under it from the root, and record what the layers below held there.
    fn remove(&mut self, rel: &Path) -> Result<()> {
        let mut seen = Vec::new();
        removal(&self.root.join(rel), rel, self.digest, &mut |p| {
            seen.push(p.to_path_buf());
        })?;
        for path in seen {
            if !self.applied.written.contains(&path) {
                self.applied.removed.insert(path);
            }
        }
        self.forget(rel);
        Ok(())
    }

    /// Make room for a new entry at `rel`: what is there, if anything, goes, and the layer's own
    /// record of it with it.
    fn clear(&mut self, rel: &Path) -> Result<()> {
        remove_any(&self.root.join(rel), self.digest)?;
        self.forget(rel);
        Ok(())
    }

    /// `rel` and everything under it is no longer there.
    fn forget(&mut self, rel: &Path) {
        for set in [&mut self.applied.written, &mut self.applied.opaque] {
            // A path sorts directly before the paths under it.
            let gone: Vec<PathBuf> = set
                .range(rel.to_path_buf()..)
                .take_while(|p| p.starts_with(rel))
                .cloned()
                .collect();
            for p in gone {
                set.remove(&p);
            }
        }
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
    removal(path, Path::new(""), digest, &mut |_| {})
}

/// Remove `path`, which is `rel` under the root, and call `seen` with `rel` and the path of
/// everything under it, relative to the root, before it goes.
fn removal(path: &Path, rel: &Path, digest: &str, seen: &mut dyn FnMut(&Path)) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(format!("{digest}: {}", path.display()), e)),
    };
    seen(rel);
    let removed = if meta.is_dir() {
        remove_tree(path, rel, seen)
    } else {
        fs::remove_file(path)
    };
    removed.map_err(|e| Error::io(format!("{digest}: removing {}", path.display()), e))
}

/// A tree removed with every directory made writable first: a layer may have said a directory
/// is not the owner's to write.
fn remove_tree(path: &Path, rel: &Path, seen: &mut dyn FnMut(&Path)) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    for child in fs::read_dir(path)? {
        let child = child?;
        let child_rel = rel.join(child.file_name());
        seen(&child_rel);
        if child.file_type()?.is_dir() {
            remove_tree(&child.path(), &child_rel, seen)?;
        } else {
            fs::remove_file(child.path())?;
        }
    }
    fs::remove_dir(path)
}
