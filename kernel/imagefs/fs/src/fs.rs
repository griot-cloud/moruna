//! A task's whole tree (SDD 27, f.6): the merged image, read only, with the task's
//! scratch directory at `/tmp`, and `/proc` and `/dev` as empty directories when the
//! image has none (SDD 19 f.3 makes the same three). Paths are resolved here as Linux
//! resolves them: absolute, `.` and `..` taken physically (`..` from a directory a
//! link led to is that directory's parent), every symbolic link on the way followed
//! (at most 40), the last one only when asked.

use alloc::{collections::VecDeque, vec::Vec};

use crate::error::{Error, Result};
use crate::format::{Kind, Node, valid_name};
use crate::overlay::{Dir, Found, Overlay};
use crate::scratch::{self, Scratch};
use crate::volume::Pages;

/// Symbolic links one path may pass through.
pub const LINKS_MAX: usize = 40;
/// The longest path a request may carry.
pub const PATH_MAX: usize = 4096;
/// The scratch directory's name in the root.
pub const SCRATCH: &[u8] = b"tmp";
/// The directories the root always has.
pub const MOUNT_POINTS: [&[u8]; 2] = [b"dev", b"proc"];

/// What a path names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Image(Found),
    Scratch(scratch::Id),
    /// `/proc` or `/dev` where the image has neither: an empty directory.
    MountPoint(u8),
}

/// What `stat` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub mode: u32,
    pub size: u64,
    pub ino: u64,
}

/// Inode numbers of what the image does not hold.
const SCRATCH_INO: u64 = 1 << 48;
const MOUNT_POINT_INO: u64 = 2 << 48;

impl Place {
    pub fn is_directory(&self, scratch: &Scratch) -> bool {
        match self {
            Place::Image(Found::Directory(_)) | Place::MountPoint(_) => true,
            Place::Image(_) => false,
            Place::Scratch(id) => scratch.info(*id).is_ok_and(|i| i.directory),
        }
    }

    pub fn stat(&self, scratch: &Scratch) -> Result<Stat> {
        Ok(match self {
            Place::Image(Found::Directory(d)) => Stat {
                kind: Kind::Directory,
                mode: d.mode(),
                size: 0,
                ino: d.ino(),
            },
            Place::Image(f @ Found::File { node, .. }) => Stat {
                kind: Kind::Regular,
                mode: node.mode,
                size: node.size,
                ino: f.ino(),
            },
            Place::Image(f @ Found::Symlink { node, .. }) => Stat {
                kind: Kind::Symlink,
                mode: 0o777,
                size: node.target_len as u64,
                ino: f.ino(),
            },
            Place::Scratch(id) => {
                let info = scratch.info(*id)?;
                Stat {
                    kind: if info.directory {
                        Kind::Directory
                    } else {
                        Kind::Regular
                    },
                    mode: info.mode,
                    size: info.size,
                    ino: SCRATCH_INO | *id as u64,
                }
            }
            Place::MountPoint(i) => Stat {
                kind: Kind::Directory,
                mode: 0o755,
                size: 0,
                ino: MOUNT_POINT_INO | *i as u64,
            },
        })
    }
}

enum Frame {
    Image(Dir),
    Scratch(scratch::Id),
    MountPoint(u8),
}

/// The components of a path that must be absolute and at most `PATH_MAX` bytes.
fn components(path: &[u8]) -> Result<VecDeque<Vec<u8>>> {
    if path.first() != Some(&b'/') || path.len() > PATH_MAX || path.contains(&0) {
        return Err(Error::BadPath);
    }
    Ok(path
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
        .collect())
}

/// The tree one task sees: the image's overlay and the task's scratch directory.
pub struct Tree<'a> {
    pub overlay: &'a Overlay,
    pub scratch: &'a Scratch,
}

impl Tree<'_> {
    /// What `path` names; a symbolic link at its end is followed when `follow`.
    pub fn resolve(&self, p: &mut impl Pages, path: &[u8], follow: bool) -> Result<Place> {
        let queue = components(path)?;
        self.walk(p, queue, follow, None)
    }
    /// Resolve within an actual pinned directory, never a path-prefix alias.
    /// Parent traversal cannot escape this capability root; absolute symlinks
    /// restart at it rather than at the source task's ambient namespace.
    pub fn resolve_at(
        &self,
        p: &mut impl Pages,
        root: Option<&Place>,
        path: &[u8],
        follow: bool,
    ) -> Result<Place> {
        self.walk(p, components(path)?, follow, root)
    }

    /// The directory `path`'s last component would be in, and that component, which
    /// must be a name (not `.` or `..`).
    pub fn parent(&self, p: &mut impl Pages, path: &[u8]) -> Result<(Place, Vec<u8>)> {
        let mut queue = components(path)?;
        let Some(name) = queue.pop_back() else {
            return Err(Error::Exists);
        };
        if !valid_name(&name) {
            return Err(Error::BadPath);
        }
        let dir = self.walk(p, queue, true, None)?;
        if !dir.is_directory(self.scratch) {
            return Err(Error::NotDirectory);
        }
        Ok((dir, name))
    }

    fn walk(
        &self,
        p: &mut impl Pages,
        mut queue: VecDeque<Vec<u8>>,
        follow: bool,
        anchor: Option<&Place>,
    ) -> Result<Place> {
        let root = self.overlay.root(p)?;
        let global_root = Place::Image(Found::Directory(root.clone()));
        let ambient = anchor.is_none_or(|a| *a == global_root);
        let first = match anchor {
            None => Frame::Image(root.clone()),
            Some(Place::Image(Found::Directory(dir))) => Frame::Image(dir.clone()),
            Some(Place::Scratch(id)) if self.scratch.info(*id)?.directory => Frame::Scratch(*id),
            Some(Place::MountPoint(i)) => Frame::MountPoint(*i),
            _ => return Err(Error::NotDirectory),
        };
        let mut stack = alloc::vec![first];
        let mut links = 0;
        while let Some(name) = queue.pop_front() {
            if name == b"." {
                continue;
            }
            if name == b".." {
                if stack.len() > 1 {
                    stack.pop();
                } else if !ambient {
                    return Err(Error::BadPath);
                }
                continue;
            }
            if name.len() > crate::format::NAME_MAX {
                return Err(Error::BadPath);
            }
            let last = queue.is_empty();
            let at_root = ambient && stack.len() == 1;
            let top = stack.last().ok_or(Error::NotFound)?;
            let next = match top {
                Frame::Image(_) if at_root && name == SCRATCH => Frame::Scratch(0),
                Frame::Image(dir) => match self.overlay.lookup(p, dir, &name)? {
                    Some(Found::Directory(d)) => Frame::Image(d),
                    Some(Found::Symlink { layer, node, .. }) if follow || !last => {
                        links += 1;
                        if links > LINKS_MAX {
                            return Err(Error::Loop);
                        }
                        let target = self.overlay.target(p, layer, &node)?;
                        if target.first() == Some(&b'/') {
                            stack.truncate(1);
                        }
                        for c in target.split(|b| *b == b'/').rev() {
                            if !c.is_empty() {
                                queue.push_front(c.to_vec());
                            }
                        }
                        continue;
                    }
                    Some(found) => {
                        if !last {
                            return Err(Error::NotDirectory);
                        }
                        return Ok(Place::Image(found));
                    }
                    None => match MOUNT_POINTS.iter().position(|m| *m == name.as_slice()) {
                        Some(i) if at_root => Frame::MountPoint(i as u8),
                        _ => return Err(Error::NotFound),
                    },
                },
                Frame::Scratch(dir) => {
                    let id = self.scratch.lookup(*dir, &name)?;
                    if self.scratch.info(id)?.directory {
                        Frame::Scratch(id)
                    } else if last {
                        return Ok(Place::Scratch(id));
                    } else {
                        return Err(Error::NotDirectory);
                    }
                }
                Frame::MountPoint(_) => return Err(Error::NotFound),
            };
            stack.push(next);
        }
        Ok(match stack.pop() {
            Some(Frame::Image(d)) => Place::Image(Found::Directory(d)),
            Some(Frame::Scratch(id)) => Place::Scratch(id),
            Some(Frame::MountPoint(i)) => Place::MountPoint(i),
            None => Place::Image(Found::Directory(root)),
        })
    }

    /// A directory's entries, in name order: the name, what it is, its inode number.
    /// The root lists `tmp` as the scratch directory, and `proc` and `dev`.
    pub fn list(&self, p: &mut impl Pages, dir: &Place) -> Result<Vec<(Vec<u8>, Kind, u64)>> {
        match dir {
            Place::Image(Found::Directory(d)) => {
                let mut entries = self.overlay.list(p, d)?;
                let root = self.overlay.root(p)?;
                if *d == root {
                    entries.retain(|(name, _, _)| name.as_slice() != SCRATCH);
                    entries.push((SCRATCH.to_vec(), Kind::Directory, SCRATCH_INO));
                    for (i, m) in MOUNT_POINTS.iter().enumerate() {
                        if !entries.iter().any(|(n, _, _)| n.as_slice() == *m) {
                            entries.push((m.to_vec(), Kind::Directory, MOUNT_POINT_INO | i as u64));
                        }
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                }
                Ok(entries)
            }
            Place::Scratch(id) => Ok(self
                .scratch
                .list(*id)?
                .into_iter()
                .map(|(name, child, directory)| {
                    let kind = if directory {
                        Kind::Directory
                    } else {
                        Kind::Regular
                    };
                    (name, kind, SCRATCH_INO | child as u64)
                })
                .collect()),
            Place::MountPoint(_) => Ok(Vec::new()),
            Place::Image(_) => Err(Error::NotDirectory),
        }
    }

    /// A symbolic link's target.
    pub fn readlink(&self, p: &mut impl Pages, path: &[u8]) -> Result<Vec<u8>> {
        self.readlink_at(p, None, path)
    }
    /// Read a link inside an actual capability root without resolving its target.
    pub fn readlink_at(
        &self,
        p: &mut impl Pages,
        root: Option<&Place>,
        path: &[u8],
    ) -> Result<Vec<u8>> {
        match self.resolve_at(p, root, path, false)? {
            Place::Image(Found::Symlink { layer, node, .. }) => {
                self.overlay.target(p, layer, &node)
            }
            _ => Err(Error::BadPath),
        }
    }
}

/// The node of a regular file the image holds.
pub fn image_file(place: &Place) -> Option<(usize, Node)> {
    match place {
        Place::Image(Found::File { layer, node, .. }) => Some((*layer, *node)),
        _ => None,
    }
}
