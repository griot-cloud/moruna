//! One layer read through verified pages (SDD 27, f.2): nodes, names, a directory's
//! children by binary search, a file's extents. Every read goes through [`Pages`],
//! which hands out only bytes whose page matched its hash, and everything read is
//! checked against the format before it is believed, so a layer that is not what it
//! says is refused (`Error::Corrupt`) and never followed into a loop, a panic or an
//! allocation it names.

use alloc::{format, vec::Vec};
use core::cmp::Ordering;

use crate::error::{Error, Result};
use crate::format::{
    EXTENT_BYTES, Extent, Geometry, Header, Kind, NODE_BYTES, Node, PAGE, PAGE_U64, valid_name,
    valid_target,
};

/// The most entries one directory may hold, merged over every layer.
pub const DIRECTORY_MAX: usize = 1 << 20;

/// Verified bytes of the layers' pages.
pub trait Pages {
    /// Copies `out.len()` bytes from `offset` of page `page` of layer `layer`, after the
    /// page has been checked against its hash. The bytes lie inside the one page.
    fn read(&mut self, layer: usize, page: u64, offset: usize, out: &mut [u8]) -> Result<()>;
}

/// A mounted layer: its place in the image and its header.
#[derive(Clone, Debug)]
pub struct Layer {
    pub index: usize,
    pub header: Header,
    pub geometry: Geometry,
}

/// A regular file's pages: where in the layer each lies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMap {
    pub layer: usize,
    pub size: u64,
    /// Each extent with the file page it starts at, in order.
    runs: Vec<(u64, Extent)>,
}

impl FileMap {
    /// The file's length in pages.
    pub fn pages(&self) -> u64 {
        self.size.div_ceil(PAGE_U64)
    }

    /// The layer page holding file page `page`: `Some(None)` for a page of a hole,
    /// `None` past the file's end.
    pub fn layer_page(&self, page: u64) -> Option<Option<u64>> {
        if page >= self.pages() {
            return None;
        }
        let at = self.runs.partition_point(|(first, _)| *first <= page);
        let (first, extent) = self.runs[at.checked_sub(1)?];
        if page >= first + extent.pages {
            return None;
        }
        Some((extent.start != 0).then(|| extent.start + (page - first)))
    }
}

impl Layer {
    pub fn new(index: usize, header: Header, geometry: Geometry) -> Layer {
        Layer {
            index,
            header,
            geometry,
        }
    }

    fn corrupt(&self, why: impl core::fmt::Display) -> Error {
        Error::Corrupt(format!("layer {}: {why}", self.index))
    }

    /// Node `index`.
    pub fn node(&self, p: &mut impl Pages, index: u32) -> Result<Node> {
        if index >= self.geometry.node_count {
            return Err(self.corrupt(format!("node {index} is past the node table")));
        }
        let at = index as u64 * NODE_BYTES as u64;
        let mut b = [0u8; NODE_BYTES];
        p.read(
            self.index,
            self.geometry.nodes_page + at / PAGE_U64,
            (at % PAGE_U64) as usize,
            &mut b,
        )?;
        Node::decode(&b, index, &self.geometry).map_err(|e| self.corrupt(e.0))
    }

    /// `len` bytes of the names from `offset`, across pages.
    fn names(&self, p: &mut impl Pages, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut out = alloc::vec![0u8; len];
        let mut done = 0;
        while done < len {
            let at = offset + done as u64;
            let within = (at % PAGE_U64) as usize;
            let n = (PAGE - within).min(len - done);
            p.read(
                self.index,
                self.geometry.names_page + at / PAGE_U64,
                within,
                &mut out[done..done + n],
            )?;
            done += n;
        }
        Ok(out)
    }

    /// A node's name: empty for the root.
    pub fn name(&self, p: &mut impl Pages, node: &Node) -> Result<Vec<u8>> {
        let name = self.names(p, node.name_offset, node.name_len as usize)?;
        if node.name_len != 0 && !valid_name(&name) {
            return Err(self.corrupt("a name holds `/` or NUL, or is `.` or `..`"));
        }
        Ok(name)
    }

    /// A symbolic link's target.
    pub fn target(&self, p: &mut impl Pages, node: &Node) -> Result<Vec<u8>> {
        if node.kind != Kind::Symlink {
            return Err(Error::BadPath);
        }
        let target = self.names(p, node.target_offset, node.target_len as usize)?;
        if !valid_target(&target) {
            return Err(self.corrupt("a symbolic link's target holds NUL"));
        }
        Ok(target)
    }

    /// The child of directory `dir` named `name`, by binary search over its sorted
    /// children: reads about log2(children) nodes and names.
    pub fn child(
        &self,
        p: &mut impl Pages,
        dir: &Node,
        name: &[u8],
    ) -> Result<Option<(u32, Node)>> {
        if dir.kind != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        let (mut lo, mut hi) = (dir.first, dir.first + dir.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let node = self.node(p, mid)?;
            let here = self.name(p, &node)?;
            match here.as_slice().cmp(name) {
                Ordering::Equal => return Ok(Some((mid, node))),
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
            }
        }
        Ok(None)
    }

    /// Every child of `dir`, in order; refused unless their names strictly increase.
    pub fn children(&self, p: &mut impl Pages, dir: &Node) -> Result<Vec<(Vec<u8>, u32, Node)>> {
        if dir.kind != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        let count = dir.count as usize;
        if count > DIRECTORY_MAX {
            return Err(Error::TooLarge);
        }
        let mut out: Vec<(Vec<u8>, u32, Node)> = Vec::new();
        out.try_reserve_exact(count).map_err(|_| Error::TooLarge)?;
        for i in dir.first..dir.first + dir.count {
            let node = self.node(p, i)?;
            let name = self.name(p, &node)?;
            if out.last().is_some_and(|(prev, _, _)| *prev >= name) {
                return Err(self.corrupt("a directory's children are not in order"));
            }
            out.push((name, i, node));
        }
        Ok(out)
    }

    /// A regular file's map from its extents, checked to cover exactly its pages.
    pub fn file(&self, p: &mut impl Pages, node: &Node) -> Result<FileMap> {
        if node.kind != Kind::Regular {
            return Err(Error::IsDirectory);
        }
        let want = node.size.div_ceil(PAGE_U64);
        let mut runs: Vec<(u64, Extent)> = Vec::new();
        runs.try_reserve_exact(node.count as usize)
            .map_err(|_| self.corrupt("a file names more extents than memory holds"))?;
        let mut covered = 0u64;
        for i in node.first..node.first + node.count {
            let at = i as u64 * EXTENT_BYTES as u64;
            let mut b = [0u8; EXTENT_BYTES];
            p.read(
                self.index,
                self.geometry.extents_page + at / PAGE_U64,
                (at % PAGE_U64) as usize,
                &mut b,
            )?;
            let e = Extent::decode(&b, i, &self.geometry).map_err(|e| self.corrupt(e.0))?;
            runs.push((covered, e));
            covered = covered
                .checked_add(e.pages)
                .ok_or_else(|| self.corrupt("a file's extents overflow"))?;
        }
        if covered != want {
            return Err(self.corrupt(format!(
                "a file of {} bytes has extents of {covered} pages, not {want}",
                node.size
            )));
        }
        Ok(FileMap {
            layer: self.index,
            size: node.size,
            runs,
        })
    }

    /// The root directory.
    pub fn root(&self, p: &mut impl Pages) -> Result<Node> {
        self.node(p, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_t5_a_file_map_finds_pages_and_holes() {
        let map = FileMap {
            layer: 0,
            size: 5 * 4096 + 1,
            runs: alloc::vec![
                (
                    0,
                    Extent {
                        start: 10,
                        pages: 2
                    }
                ),
                (2, Extent { start: 0, pages: 3 }),
                (
                    5,
                    Extent {
                        start: 40,
                        pages: 1
                    }
                ),
            ],
        };
        assert_eq!(map.pages(), 6);
        assert_eq!(map.layer_page(0), Some(Some(10)));
        assert_eq!(map.layer_page(1), Some(Some(11)));
        assert_eq!(map.layer_page(2), Some(None));
        assert_eq!(map.layer_page(4), Some(None));
        assert_eq!(map.layer_page(5), Some(Some(40)));
        assert_eq!(map.layer_page(6), None);
        let empty = FileMap {
            layer: 0,
            size: 0,
            runs: Vec::new(),
        };
        assert_eq!(empty.layer_page(0), None);
    }
}
