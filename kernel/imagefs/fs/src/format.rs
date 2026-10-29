//! The layer file, byte by byte (SDD 27, e.1 to e.4): a header page, the node table,
//! the names, the extent table, the file data with every file starting on a page, and
//! the hash tree over all of it. Every integer is little endian. This module parses
//! and validates; the host writer (`moruna-image`) uses [`Geometry`] and
//! [`Header::encode`] so both sides compute one layout.

use alloc::{format, string::String};

use crate::sha256;

/// The page: the unit of the layout, the cache and a mapping.
pub const PAGE: usize = 4096;
pub const PAGE_U64: u64 = PAGE as u64;
/// A layer file's first eight bytes.
pub const MAGIC: [u8; 8] = *b"MRNLAYR1";
pub const VERSION: u32 = 1;
pub const NODE_BYTES: usize = 64;
pub const EXTENT_BYTES: usize = 16;
pub const HASH_BYTES: usize = 32;
/// Hashes in one page of the tree.
pub const FANOUT: u64 = (PAGE / HASH_BYTES) as u64;
/// The longest name of a directory entry, and of a symbolic link's target.
pub const NAME_MAX: usize = 255;
pub const TARGET_MAX: usize = 4095;
/// Bounds a reader holds a header to, so no count it reads can make it allocate or
/// loop without limit.
pub const NODES_MAX: u32 = 1 << 24;
pub const EXTENTS_MAX: u32 = 1 << 24;
pub const NAMES_MAX: u64 = 1 << 30;
pub const DATA_PAGES_MAX: u64 = 1 << 36;
/// Tree levels: 128 to the 7th covers more pages than `DATA_PAGES_MAX` allows.
pub const LEVELS_MAX: usize = 7;

/// Where the header keeps its own checksum.
const CHECKSUM_AT: usize = 88;
const HEADER_USED: usize = CHECKSUM_AT + HASH_BYTES;

/// Why bytes are not a layer file. The reader turns it into a refusal naming the layer
/// and the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatError(pub String);

fn bad<T>(why: impl Into<String>) -> Result<T, FormatError> {
    Err(FormatError(why.into()))
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// The whole number of pages `bytes` fills.
pub const fn pages_for(bytes: u64) -> u64 {
    bytes.div_ceil(PAGE_U64)
}

/// Where everything is, derived from the four counts a header carries: nothing in the
/// layout is stored that can be computed, so a header cannot describe overlapping or
/// misplaced regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub node_count: u32,
    pub extent_count: u32,
    pub names_len: u64,
    pub data_pages: u64,
    pub nodes_page: u64,
    pub names_page: u64,
    pub extents_page: u64,
    pub data_page: u64,
    /// The first page of the tree; pages `1 .. tree_page` are the content it covers.
    pub tree_page: u64,
    /// Each level's first page and page count, level 0 (the hashes of content pages)
    /// first; `levels` of them are used.
    pub level: [(u64, u64); LEVELS_MAX],
    pub levels: usize,
    /// The file's length in pages.
    pub pages: u64,
}

/// What a page's hash is checked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    /// The header page, which carries its own checksum.
    Header,
    /// The top tree page, checked against the header's root.
    Root,
    /// Hash `slot` of tree page `page`.
    Tree { page: u64, slot: usize },
}

impl Geometry {
    pub fn new(
        node_count: u32,
        extent_count: u32,
        names_len: u64,
        data_pages: u64,
    ) -> Result<Geometry, FormatError> {
        if node_count == 0 || node_count > NODES_MAX {
            return bad(format!(
                "{node_count} nodes; a layer holds 1 to {NODES_MAX}"
            ));
        }
        if extent_count > EXTENTS_MAX {
            return bad(format!("{extent_count} extents; at most {EXTENTS_MAX}"));
        }
        if names_len > NAMES_MAX {
            return bad(format!("{names_len} bytes of names; at most {NAMES_MAX}"));
        }
        if data_pages > DATA_PAGES_MAX {
            return bad(format!("{data_pages} data pages; at most {DATA_PAGES_MAX}"));
        }
        let nodes_page = 1;
        let names_page = nodes_page + pages_for(node_count as u64 * NODE_BYTES as u64);
        let extents_page = names_page + pages_for(names_len);
        let data_page = extents_page + pages_for(extent_count as u64 * EXTENT_BYTES as u64);
        let tree_page = data_page + data_pages;
        let mut level = [(0u64, 0u64); LEVELS_MAX];
        let mut levels = 0;
        let mut covered = tree_page - 1;
        let mut at = tree_page;
        loop {
            let count = covered.div_ceil(FANOUT);
            level[levels] = (at, count);
            levels += 1;
            at += count;
            if count == 1 {
                break;
            }
            covered = count;
        }
        Ok(Geometry {
            node_count,
            extent_count,
            names_len,
            data_pages,
            nodes_page,
            names_page,
            extents_page,
            data_page,
            tree_page,
            level,
            levels,
            pages: at,
        })
    }

    /// The top tree page, whose hash is the root.
    pub fn top(&self) -> u64 {
        self.level[self.levels - 1].0
    }

    /// What page `page`'s hash is checked against; `None` past the file's end.
    pub fn parent(&self, page: u64) -> Option<Parent> {
        if page == 0 {
            return Some(Parent::Header);
        }
        if page < self.tree_page {
            let i = page - 1;
            return Some(Parent::Tree {
                page: self.level[0].0 + i / FANOUT,
                slot: (i % FANOUT) as usize,
            });
        }
        for l in 0..self.levels {
            let (first, count) = self.level[l];
            if page >= first && page < first + count {
                if l + 1 == self.levels {
                    return Some(Parent::Root);
                }
                let i = page - first;
                return Some(Parent::Tree {
                    page: self.level[l + 1].0 + i / FANOUT,
                    slot: (i % FANOUT) as usize,
                });
            }
        }
        None
    }

    /// Whether `start .. start + pages` lies inside the data region.
    pub fn holds_data(&self, start: u64, pages: u64) -> bool {
        start >= self.data_page
            && start
                .checked_add(pages)
                .is_some_and(|end| end <= self.tree_page)
    }
}

/// The header page's fields that are not derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub node_count: u32,
    pub extent_count: u32,
    pub names_len: u64,
    pub data_pages: u64,
    /// The hash of the top tree page.
    pub root: [u8; 32],
}

impl Header {
    /// The header page:
    ///
    /// | offset | field |
    /// |---|---|
    /// | 0 | magic `MRNLAYR1` |
    /// | 8 | `u32` version, 1 |
    /// | 12 | `u32` page size, 4096 |
    /// | 16 | `u64` the file's pages |
    /// | 24 | `u32` nodes |
    /// | 28 | `u32` extents |
    /// | 32 | `u64` bytes of names |
    /// | 40 | `u64` data pages |
    /// | 48 | `u32` tree levels |
    /// | 52 | `u32` flags, 0 |
    /// | 56 | the root: SHA-256 of the top tree page |
    /// | 88 | SHA-256 of the page with these 32 bytes zero |
    /// | 120 | zero to the end of the page |
    pub fn encode(&self) -> Result<[u8; PAGE], FormatError> {
        let g = Geometry::new(
            self.node_count,
            self.extent_count,
            self.names_len,
            self.data_pages,
        )?;
        let mut p = [0u8; PAGE];
        p[0..8].copy_from_slice(&MAGIC);
        p[8..12].copy_from_slice(&VERSION.to_le_bytes());
        p[12..16].copy_from_slice(&(PAGE as u32).to_le_bytes());
        p[16..24].copy_from_slice(&g.pages.to_le_bytes());
        p[24..28].copy_from_slice(&self.node_count.to_le_bytes());
        p[28..32].copy_from_slice(&self.extent_count.to_le_bytes());
        p[32..40].copy_from_slice(&self.names_len.to_le_bytes());
        p[40..48].copy_from_slice(&self.data_pages.to_le_bytes());
        p[48..52].copy_from_slice(&(g.levels as u32).to_le_bytes());
        p[56..88].copy_from_slice(&self.root);
        let sum = sha256::digest(&p);
        p[CHECKSUM_AT..HEADER_USED].copy_from_slice(&sum);
        Ok(p)
    }

    /// Parses and checks a header page: its checksum, its constants, and that the
    /// layout its counts give is the one it states.
    pub fn parse(p: &[u8]) -> Result<(Header, Geometry), FormatError> {
        if p.len() != PAGE {
            return bad(format!("a header is one page, not {} bytes", p.len()));
        }
        if p[0..8] != MAGIC {
            return bad("the first eight bytes are not MRNLAYR1: this is not a layer file");
        }
        let mut zeroed = [0u8; PAGE];
        zeroed.copy_from_slice(p);
        zeroed[CHECKSUM_AT..HEADER_USED].fill(0);
        if sha256::digest(&zeroed)[..] != p[CHECKSUM_AT..HEADER_USED] {
            return bad("the header's checksum does not match its bytes");
        }
        let version = u32_at(p, 8);
        if version != VERSION {
            return bad(format!("version {version}; this reader reads {VERSION}"));
        }
        if u32_at(p, 12) != PAGE as u32 {
            return bad(format!("a page size of {}, not {PAGE}", u32_at(p, 12)));
        }
        if u32_at(p, 52) != 0 || p[HEADER_USED..].iter().any(|b| *b != 0) {
            return bad("bytes the format keeps zero are not");
        }
        let mut root = [0u8; 32];
        root.copy_from_slice(&p[56..88]);
        let header = Header {
            node_count: u32_at(p, 24),
            extent_count: u32_at(p, 28),
            names_len: u64_at(p, 32),
            data_pages: u64_at(p, 40),
            root,
        };
        let g = Geometry::new(
            header.node_count,
            header.extent_count,
            header.names_len,
            header.data_pages,
        )?;
        if u64_at(p, 16) != g.pages || u32_at(p, 48) as usize != g.levels {
            return bad("the file's length or tree depth is not the one its counts give");
        }
        Ok((header, g))
    }
}

/// What a node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory = 1,
    Regular = 2,
    Symlink = 3,
    /// The name is gone from the layers below (SDD 19 f.2's whiteout, as converted).
    Whiteout = 4,
}

/// A directory's flag: it keeps nothing of the layers below.
pub const OPAQUE: u8 = 1;

/// One 64-byte entry of the node table:
///
/// | offset | field |
/// |---|---|
/// | 0 | `u64` the name's offset in the names |
/// | 8 | `u16` the name's length (0 for the root, 1 to 255 otherwise) |
/// | 10 | `u8` kind |
/// | 11 | `u8` flags (`OPAQUE` on a directory) |
/// | 12 | `u32` mode, the permission bits |
/// | 16 | `u32` a directory's first child, a file's first extent |
/// | 20 | `u32` a directory's children, a file's extents |
/// | 24 | `u64` a file's size in bytes |
/// | 32 | `u64` a symbolic link's target's offset in the names |
/// | 40 | `u32` the target's length |
/// | 44 | zero to 64 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub name_offset: u64,
    pub name_len: u16,
    pub kind: Kind,
    pub flags: u8,
    pub mode: u32,
    pub first: u32,
    pub count: u32,
    pub size: u64,
    pub target_offset: u64,
    pub target_len: u32,
}

impl Node {
    pub fn encode(&self) -> [u8; NODE_BYTES] {
        let mut b = [0u8; NODE_BYTES];
        b[0..8].copy_from_slice(&self.name_offset.to_le_bytes());
        b[8..10].copy_from_slice(&self.name_len.to_le_bytes());
        b[10] = self.kind as u8;
        b[11] = self.flags;
        b[12..16].copy_from_slice(&self.mode.to_le_bytes());
        b[16..20].copy_from_slice(&self.first.to_le_bytes());
        b[20..24].copy_from_slice(&self.count.to_le_bytes());
        b[24..32].copy_from_slice(&self.size.to_le_bytes());
        b[32..40].copy_from_slice(&self.target_offset.to_le_bytes());
        b[40..44].copy_from_slice(&self.target_len.to_le_bytes());
        b
    }

    /// Decodes node `index` and checks it against the layout: every field its kind
    /// does not use is zero, every range it names is inside its table, and a
    /// directory's children come after it, so following children always ends.
    pub fn decode(b: &[u8], index: u32, g: &Geometry) -> Result<Node, FormatError> {
        if b.len() != NODE_BYTES {
            return bad("a node is 64 bytes");
        }
        let kind = match b[10] {
            1 => Kind::Directory,
            2 => Kind::Regular,
            3 => Kind::Symlink,
            4 => Kind::Whiteout,
            k => return bad(format!("node {index} is of kind {k}, which no node is")),
        };
        let node = Node {
            name_offset: u64_at(b, 0),
            name_len: u16_at(b, 8),
            kind,
            flags: b[11],
            mode: u32_at(b, 12),
            first: u32_at(b, 16),
            count: u32_at(b, 20),
            size: u64_at(b, 24),
            target_offset: u64_at(b, 32),
            target_len: u32_at(b, 40),
        };
        let fail = |why: &str| bad(format!("node {index}: {why}"));
        if b[44..].iter().any(|x| *x != 0) {
            return fail("bytes the format keeps zero are not");
        }
        if index == 0 {
            if node.kind != Kind::Directory || node.name_len != 0 || node.name_offset != 0 {
                return fail("the first node is the root: a directory with no name");
            }
        } else if node.name_len == 0 || node.name_len as usize > NAME_MAX {
            return fail("a name is 1 to 255 bytes");
        }
        if node
            .name_offset
            .checked_add(node.name_len as u64)
            .is_none_or(|end| end > g.names_len)
        {
            return fail("its name reaches past the names");
        }
        if node.mode > 0o777 {
            return fail("a mode is permission bits only");
        }
        if node.flags != 0 && !(node.kind == Kind::Directory && node.flags == OPAQUE) {
            return fail("only a directory carries a flag, and only OPAQUE");
        }
        let no_target = node.target_offset == 0 && node.target_len == 0;
        match node.kind {
            Kind::Directory => {
                if node.size != 0 || !no_target {
                    return fail("a directory has no size or target");
                }
                if node.count > 0
                    && (node.first <= index
                        || (node.first as u64 + node.count as u64) > g.node_count as u64)
                {
                    return fail("a directory's children are later nodes inside the table");
                }
                if node.count == 0 && node.first != 0 {
                    return fail("a directory with no children names no first child");
                }
            }
            Kind::Regular => {
                if !no_target {
                    return fail("a file has no target");
                }
                if node.count as u64 != 0 && node.size == 0 {
                    return fail("an empty file has no extents");
                }
                if (node.first as u64 + node.count as u64) > g.extent_count as u64 {
                    return fail("a file's extents reach past the extent table");
                }
                if node.count == 0 && (node.first != 0 || node.size != 0) {
                    return fail("a file with data has extents");
                }
            }
            Kind::Symlink => {
                if node.first != 0 || node.count != 0 || node.size != 0 {
                    return fail("a symbolic link has no children, extents or size");
                }
                if node.target_len == 0 || node.target_len as usize > TARGET_MAX {
                    return fail("a target is 1 to 4095 bytes");
                }
                if node
                    .target_offset
                    .checked_add(node.target_len as u64)
                    .is_none_or(|end| end > g.names_len)
                {
                    return fail("its target reaches past the names");
                }
            }
            Kind::Whiteout => {
                if node.first != 0 || node.count != 0 || node.size != 0 || !no_target {
                    return fail("a whiteout is a name and nothing else");
                }
                if node.mode != 0 {
                    return fail("a whiteout has no mode");
                }
            }
        }
        Ok(node)
    }
}

/// One 16-byte entry of the extent table: `u64` the first page in the file (0: a hole
/// of zero pages, which reads nothing), `u64` the pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub start: u64,
    pub pages: u64,
}

impl Extent {
    pub fn encode(&self) -> [u8; EXTENT_BYTES] {
        let mut b = [0u8; EXTENT_BYTES];
        b[0..8].copy_from_slice(&self.start.to_le_bytes());
        b[8..16].copy_from_slice(&self.pages.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8], index: u32, g: &Geometry) -> Result<Extent, FormatError> {
        if b.len() != EXTENT_BYTES {
            return bad("an extent is 16 bytes");
        }
        let e = Extent {
            start: u64_at(b, 0),
            pages: u64_at(b, 8),
        };
        if e.pages == 0 {
            return bad(format!("extent {index} holds no pages"));
        }
        if e.start != 0 && !g.holds_data(e.start, e.pages) {
            return bad(format!("extent {index} is not inside the data region"));
        }
        Ok(e)
    }
}

/// Whether `name` may be a directory entry's name: 1 to 255 bytes, no `/` and no
/// NUL, and not `.` or `..`.
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != b"."
        && name != b".."
        && !name.iter().any(|b| *b == b'/' || *b == 0)
}

/// Whether `target` may be a symbolic link's target: 1 to 4095 bytes with no NUL.
pub fn valid_target(target: &[u8]) -> bool {
    !target.is_empty() && target.len() <= TARGET_MAX && !target.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_t5_the_layout_follows_from_the_counts() {
        let g = Geometry::new(65, 3, 4097, 10).unwrap();
        assert_eq!(g.nodes_page, 1);
        assert_eq!(g.names_page, 3, "65 nodes are two pages");
        assert_eq!(g.extents_page, 5, "4097 bytes of names are two pages");
        assert_eq!(g.data_page, 6);
        assert_eq!(g.tree_page, 16);
        assert_eq!(g.levels, 1);
        assert_eq!(g.level[0], (16, 1));
        assert_eq!(g.pages, 17);
        assert_eq!(g.parent(0), Some(Parent::Header));
        assert_eq!(g.parent(1), Some(Parent::Tree { page: 16, slot: 0 }));
        assert_eq!(g.parent(15), Some(Parent::Tree { page: 16, slot: 14 }));
        assert_eq!(g.parent(16), Some(Parent::Root));
        assert_eq!(g.parent(17), None);
        assert!(g.holds_data(6, 10));
        assert!(!g.holds_data(5, 1));
        assert!(!g.holds_data(15, 2));
        assert!(!g.holds_data(u64::MAX, 2));
    }

    #[test]
    fn if_t5_a_deep_tree() {
        // 100 MiB of data: 25,600 pages and some metadata, three levels.
        let g = Geometry::new(4, 2, 30, 25_600).unwrap();
        assert_eq!(g.levels, 3);
        let content = g.tree_page - 1;
        assert_eq!(g.level[0].1, content.div_ceil(128));
        assert_eq!(g.level[1].1, g.level[0].1.div_ceil(128));
        assert_eq!(g.level[2].1, 1);
        assert_eq!(g.top(), g.level[2].0);
        let last = g.tree_page - 1;
        assert_eq!(
            g.parent(last),
            Some(Parent::Tree {
                page: g.level[0].0 + (last - 1) / 128,
                slot: ((last - 1) % 128) as usize
            })
        );
        assert_eq!(
            g.parent(g.level[0].0 + 130),
            Some(Parent::Tree {
                page: g.level[1].0 + 1,
                slot: 2
            })
        );
        assert_eq!(
            g.parent(g.level[1].0 + 1),
            Some(Parent::Tree {
                page: g.top(),
                slot: 1
            })
        );
        assert_eq!(g.parent(g.top()), Some(Parent::Root));
        assert_eq!(g.pages, g.top() + 1);
    }

    #[test]
    fn if_t5_counts_out_of_bounds_are_refused() {
        assert!(Geometry::new(0, 0, 0, 0).is_err());
        assert!(Geometry::new(NODES_MAX + 1, 0, 0, 0).is_err());
        assert!(Geometry::new(1, EXTENTS_MAX + 1, 0, 0).is_err());
        assert!(Geometry::new(1, 0, NAMES_MAX + 1, 0).is_err());
        assert!(Geometry::new(1, 0, 0, DATA_PAGES_MAX + 1).is_err());
        assert!(Geometry::new(NODES_MAX, EXTENTS_MAX, NAMES_MAX, DATA_PAGES_MAX).is_ok());
    }

    fn header() -> Header {
        Header {
            node_count: 3,
            extent_count: 1,
            names_len: 9,
            data_pages: 2,
            root: [7; 32],
        }
    }

    #[test]
    fn if_t5_a_header_round_trips_and_checks_itself() {
        let h = header();
        let page = h.encode().unwrap();
        let (back, g) = Header::parse(&page).unwrap();
        assert_eq!(back, h);
        assert_eq!(g, Geometry::new(3, 1, 9, 2).unwrap());
        for at in [0, 8, 12, 16, 24, 40, 48, 52, 60, 90, 200, 4095] {
            let mut bad = page;
            bad[at] ^= 1;
            assert!(Header::parse(&bad).is_err(), "byte {at} flipped");
        }
        assert!(Header::parse(&page[..100]).is_err());
    }

    fn resealed(mut p: [u8; PAGE]) -> [u8; PAGE] {
        p[CHECKSUM_AT..HEADER_USED].fill(0);
        let sum = sha256::digest(&p);
        p[CHECKSUM_AT..HEADER_USED].copy_from_slice(&sum);
        p
    }

    #[test]
    fn if_t5_a_header_whose_checksum_holds_is_still_checked() {
        let page = header().encode().unwrap();
        let mut version = page;
        version[8] = 2;
        let mut size = page;
        size[13] = 0x20;
        let mut flags = page;
        flags[52] = 1;
        let mut tail = page;
        tail[300] = 1;
        let mut length = page;
        length[16] += 1;
        let mut levels = page;
        levels[48] = 2;
        let mut nodes = page;
        nodes[24..28].copy_from_slice(&0u32.to_le_bytes());
        for p in [version, size, flags, tail, length, levels, nodes] {
            assert!(Header::parse(&resealed(p)).is_err());
        }
        assert!(Header::parse(&resealed(page)).is_ok());
    }

    fn g() -> Geometry {
        Geometry::new(10, 4, 100, 8).unwrap()
    }

    fn node(kind: Kind) -> Node {
        Node {
            name_offset: 0,
            name_len: 3,
            kind,
            flags: 0,
            mode: 0o644,
            first: 0,
            count: 0,
            size: 0,
            target_offset: 0,
            target_len: 0,
        }
    }

    #[test]
    fn if_t5_nodes_round_trip() {
        let g = g();
        let mut dir = node(Kind::Directory);
        dir.first = 3;
        dir.count = 2;
        dir.flags = OPAQUE;
        let mut file = node(Kind::Regular);
        file.first = 1;
        file.count = 2;
        file.size = 5000;
        let mut link = node(Kind::Symlink);
        link.target_offset = 10;
        link.target_len = 4;
        let mut wh = node(Kind::Whiteout);
        wh.mode = 0;
        let mut root = node(Kind::Directory);
        root.name_len = 0;
        root.first = 1;
        root.count = 2;
        for (i, n) in [(1, dir), (2, file), (2, link), (2, wh), (0, root)] {
            assert_eq!(Node::decode(&n.encode(), i, &g).unwrap(), n);
        }
    }

    #[test]
    fn if_t5_bad_nodes_are_refused() {
        let g = g();
        let ok = node(Kind::Regular);
        let mut cases = alloc::vec::Vec::new();
        let mut b = ok.encode();
        b[10] = 9;
        cases.push((1, b));
        let mut b = ok.encode();
        b[50] = 1;
        cases.push((1, b));
        let mut n = ok;
        n.name_len = 0;
        cases.push((1, n.encode()));
        let mut n = ok;
        n.name_offset = 99;
        cases.push((1, n.encode()));
        let mut n = ok;
        n.mode = 0o4755;
        cases.push((1, n.encode()));
        let mut n = ok;
        n.flags = OPAQUE;
        cases.push((1, n.encode()));
        let mut n = ok;
        n.size = 4;
        cases.push((1, n.encode()));
        let mut n = ok;
        n.count = 1;
        cases.push((1, n.encode()));
        let mut n = ok;
        n.first = 4;
        n.count = 1;
        n.size = 1;
        cases.push((1, n.encode()));
        let mut n = ok;
        n.target_len = 1;
        cases.push((1, n.encode()));
        let mut n = node(Kind::Directory);
        n.first = 1;
        n.count = 1;
        cases.push((1, n.encode()));
        let mut n = node(Kind::Directory);
        n.first = 9;
        n.count = 2;
        cases.push((1, n.encode()));
        let mut n = node(Kind::Directory);
        n.first = 5;
        cases.push((1, n.encode()));
        let mut n = node(Kind::Directory);
        n.size = 1;
        cases.push((1, n.encode()));
        let mut n = node(Kind::Symlink);
        cases.push((1, n.encode()));
        n.target_len = 5000;
        cases.push((1, n.encode()));
        n.target_len = 3;
        n.target_offset = 98;
        cases.push((1, n.encode()));
        n.target_offset = 0;
        n.count = 1;
        cases.push((1, n.encode()));
        let mut n = node(Kind::Whiteout);
        cases.push((1, n.encode()));
        n.mode = 0;
        n.size = 1;
        cases.push((1, n.encode()));
        cases.push((0, node(Kind::Directory).encode()));
        cases.push((0, node(Kind::Regular).encode()));
        for (i, (index, b)) in cases.iter().enumerate() {
            assert!(Node::decode(b, *index, &g).is_err(), "case {i}");
        }
        assert!(Node::decode(&[0; 10], 1, &g).is_err());
    }

    #[test]
    fn if_t5_extents_are_checked() {
        let g = g();
        let e = Extent {
            start: g.data_page,
            pages: 8,
        };
        assert_eq!(Extent::decode(&e.encode(), 0, &g).unwrap(), e);
        let hole = Extent {
            start: 0,
            pages: 1000,
        };
        assert_eq!(Extent::decode(&hole.encode(), 0, &g).unwrap(), hole);
        for bad in [
            Extent {
                start: g.data_page,
                pages: 0,
            },
            Extent {
                start: g.data_page,
                pages: 9,
            },
            Extent { start: 1, pages: 1 },
            Extent {
                start: u64::MAX,
                pages: 1,
            },
        ] {
            assert!(Extent::decode(&bad.encode(), 0, &g).is_err());
        }
        assert!(Extent::decode(&[0; 3], 0, &g).is_err());
    }

    #[test]
    fn if_t5_names_and_targets() {
        assert!(valid_name(b"a"));
        assert!(valid_name(&[b'x'; 255]));
        for bad in [&b""[..], b".", b"..", b"a/b", b"a\0", &[b'x'; 256]] {
            assert!(!valid_name(bad));
        }
        assert!(valid_target(b"../x"));
        assert!(!valid_target(b""));
        assert!(!valid_target(b"a\0"));
        assert!(!valid_target(&[b'x'; 4096]));
    }
}
