//! The layers as one tree (SDD 27, f.3): a name is looked for from the top layer down.
//! The first layer that names it decides: a whiteout hides it; a file or a symbolic
//! link is it, and hides whatever the layers below have; a directory is it, merged
//! with the same directory in the layers below until one of them is opaque or names
//! something else there. These are the rules of SDD 19 f.2, which the writer has
//! already applied to each layer against the layers below it (SDD 27, f.1), so a
//! merge here is the unpacking SDD 19 makes (IF-I3).

use alloc::{vec, vec::Vec};

use crate::error::{Error, Result};
use crate::format::{Kind, Node, OPAQUE};
use crate::volume::{Layer, Pages};

/// A directory of the merged tree: the layers' directories it is made of, top first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dir {
    pub parts: Vec<(usize, u32, Node)>,
}

impl Dir {
    /// The mode is the top layer's.
    pub fn mode(&self) -> u32 {
        self.parts.first().map_or(0o755, |(_, _, n)| n.mode)
    }

    /// The inode number: the top layer's node's.
    pub fn ino(&self) -> u64 {
        self.parts.first().map_or(1, |(l, i, _)| ino(*l, *i))
    }
}

/// What a name is in the merged tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Directory(Dir),
    File {
        layer: usize,
        index: u32,
        node: Node,
    },
    Symlink {
        layer: usize,
        index: u32,
        node: Node,
    },
}

/// An inode number unique over the image: the layer and the node.
pub fn ino(layer: usize, index: u32) -> u64 {
    ((layer as u64 + 1) << 32) | index as u64
}

impl Found {
    pub fn ino(&self) -> u64 {
        match self {
            Found::Directory(d) => d.ino(),
            Found::File { layer, index, .. } | Found::Symlink { layer, index, .. } => {
                ino(*layer, *index)
            }
        }
    }
}

/// The image's layers, lowest first.
#[derive(Clone, Debug)]
pub struct Overlay {
    layers: Vec<Layer>,
}

impl Overlay {
    pub fn new(layers: Vec<Layer>) -> Overlay {
        Overlay { layers }
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// The merged root.
    pub fn root(&self, p: &mut impl Pages) -> Result<Dir> {
        let mut parts = Vec::new();
        for layer in self.layers.iter().rev() {
            let node = layer.root(p)?;
            parts.push((layer.index, 0, node));
            if node.flags & OPAQUE != 0 {
                break;
            }
        }
        Ok(Dir { parts })
    }

    fn layer(&self, index: usize) -> Result<&Layer> {
        self.layers.get(index).ok_or(Error::NotFound)
    }

    /// `name` in `dir`, or `None`.
    pub fn lookup(&self, p: &mut impl Pages, dir: &Dir, name: &[u8]) -> Result<Option<Found>> {
        let mut parts = dir.parts.iter();
        let found = loop {
            let Some((l, _, node)) = parts.next() else {
                return Ok(None);
            };
            let layer = self.layer(*l)?;
            if let Some((index, child)) = layer.child(p, node, name)? {
                break (*l, index, child);
            }
        };
        let (l, index, child) = found;
        match child.kind {
            Kind::Whiteout => Ok(None),
            Kind::Regular => Ok(Some(Found::File {
                layer: l,
                index,
                node: child,
            })),
            Kind::Symlink => Ok(Some(Found::Symlink {
                layer: l,
                index,
                node: child,
            })),
            Kind::Directory => {
                let mut merged = vec![(l, index, child)];
                if child.flags & OPAQUE == 0 {
                    for (l, _, node) in parts {
                        let layer = self.layer(*l)?;
                        match layer.child(p, node, name)? {
                            None => continue,
                            Some((i, n)) if n.kind == Kind::Directory => {
                                merged.push((*l, i, n));
                                if n.flags & OPAQUE != 0 {
                                    break;
                                }
                            }
                            Some(_) => break,
                        }
                    }
                }
                Ok(Some(Found::Directory(Dir { parts: merged })))
            }
        }
    }

    /// Every name in `dir`, in order, with what it is: the top layer's entry of each
    /// name decides, and a whiteout removes the name.
    pub fn list(&self, p: &mut impl Pages, dir: &Dir) -> Result<Vec<(Vec<u8>, Kind, u64)>> {
        let mut merged: alloc::collections::BTreeMap<Vec<u8>, Option<(Kind, u64)>> =
            alloc::collections::BTreeMap::new();
        for (l, _, node) in &dir.parts {
            let layer = self.layer(*l)?;
            for (name, index, child) in layer.children(p, node)? {
                if merged.len() >= crate::volume::DIRECTORY_MAX && !merged.contains_key(&name) {
                    return Err(Error::TooLarge);
                }
                merged.entry(name).or_insert_with(|| {
                    (child.kind != Kind::Whiteout).then(|| (child.kind, ino(*l, index)))
                });
            }
        }
        Ok(merged
            .into_iter()
            .filter_map(|(name, entry)| entry.map(|(kind, ino)| (name, kind, ino)))
            .collect())
    }

    /// A file's map of its pages.
    pub fn file(
        &self,
        p: &mut impl Pages,
        layer: usize,
        node: &Node,
    ) -> Result<crate::volume::FileMap> {
        self.layer(layer)?.file(p, node)
    }

    /// A symbolic link's target.
    pub fn target(&self, p: &mut impl Pages, layer: usize, node: &Node) -> Result<Vec<u8>> {
        self.layer(layer)?.target(p, node)
    }
}
