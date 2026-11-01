//! Layer files (SDD 27, f.1): an image's layers as Moruna's microkernel reads them, one
//! read-only block device each, merged by the image filesystem server.
//!
//! A layer file is not the OCI tar: it is what the layer did to the layers below it,
//! as SDD 19's unpacking did it. [`convert`] applies each layer with that unpacking
//! (`unpack.rs`, `layer.rs`), which records the paths it wrote, the paths of the layers
//! below it removed and the directories it made opaque, and writes the difference from
//! that record: every entry it made or replaced (at the path the unpacking resolved,
//! symbolic links of lower layers followed), a whiteout for every name it removed, and
//! a directory it replaced or emptied of everything below marked opaque. Merging the
//! files top down (SDD 27 f.3) is therefore the unpacking itself, whatever quirk of the
//! rules a layer exercises (IF-I3, IF-T2); no two trees are compared, so what a
//! filesystem does with inode numbers cannot hide a change.
//!
//! [`write`] lays a tree out as `moruna_imagefs::format` describes: the header, the
//! node table in breadth-first order (each directory's children consecutive and in
//! name order), the names, the extents, every file's data from a page boundary (runs
//! of zero pages are holes and take no space), and the SHA-256 tree over all of it.
//! The layer's id is its seal: the SHA-256 of its header page, which carries the root
//! of that tree, so the id names every byte of the file and the image filesystem checks
//! it when it mounts the layer without reading the rest (SDD 30 f.12). [`read`] reads
//! files back with the
//! kernel's own reader, compiled for the host.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_imagefs::cache::{Cache, Disk, Fetch};
use moruna_imagefs::format::{
    self, EXTENT_BYTES, Extent, FANOUT, Geometry, Header, Kind, NODE_BYTES, OPAQUE, PAGE,
};
use moruna_imagefs::fs::{Place, Tree};
use moruna_imagefs::overlay::{Found, Overlay};
use moruna_imagefs::scratch::Scratch;
use moruna_imagefs::volume::Pages;
use sha2::{Digest as _, Sha256};

use crate::digest::Digest;
use crate::error::{Error, Result};
use crate::layer::{Applied, remove_any};
use crate::source::{Config, ImageSource};
use crate::unpack;

/// Where a file's bytes come from.
#[derive(Clone, Debug)]
pub enum Data {
    /// Bytes in memory.
    Bytes(Arc<[u8]>),
    /// A file on disk, of this length.
    Path(PathBuf, u64),
}

impl Data {
    fn len(&self) -> u64 {
        match self {
            Data::Bytes(b) => b.len() as u64,
            Data::Path(_, len) => *len,
        }
    }
}

/// A tree to write as a layer.
#[derive(Clone, Debug)]
pub enum Entry {
    /// A directory: its mode, whether it hides everything below it, its entries.
    Directory {
        /// The permission bits.
        mode: u32,
        /// Nothing of the layers below shows through it.
        opaque: bool,
        /// Its entries by name.
        children: BTreeMap<Vec<u8>, Entry>,
    },
    /// A regular file; files with the same `link` share their data (hard links).
    File {
        /// The permission bits.
        mode: u32,
        /// Its bytes.
        data: Data,
        /// Files with one link number are one file.
        link: Option<u64>,
    },
    /// A symbolic link.
    Symlink {
        /// Where it points, as written.
        target: Vec<u8>,
    },
    /// The name is gone from the layers below.
    Whiteout,
}

impl Entry {
    /// An empty directory with `mode`.
    pub fn directory(mode: u32) -> Entry {
        Entry::Directory {
            mode,
            opaque: false,
            children: BTreeMap::new(),
        }
    }
}

/// A layer file written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerFile {
    /// The layer's id: its seal, the SHA-256 of its header page.
    pub id: Digest,
    /// Where it is.
    pub path: PathBuf,
    /// Its length in pages.
    pub pages: u64,
}

fn bad(what: impl Into<String>) -> Error {
    Error::Format(what.into())
}

fn io_at(path: &Path, what: &str) -> impl Fn(io::Error) -> Error {
    let at = format!("{what} {}", path.display());
    move |e| Error::io(at.clone(), e)
}

/// The node table, the names and the files to lay out, breadth first.
struct Flat<'a> {
    nodes: Vec<format::Node>,
    names: Vec<u8>,
    /// Each regular file's node, in node order.
    files: Vec<(usize, &'a Data, Option<u64>)>,
}

fn flatten(root: &Entry) -> Result<Flat<'_>> {
    let blank = format::Node {
        name_offset: 0,
        name_len: 0,
        kind: Kind::Directory,
        flags: 0,
        mode: 0,
        first: 0,
        count: 0,
        size: 0,
        target_offset: 0,
        target_len: 0,
    };
    if !matches!(root, Entry::Directory { .. }) {
        return Err(bad("a layer's root is a directory"));
    }
    let mut flat = Flat {
        nodes: vec![blank],
        names: Vec::new(),
        files: Vec::new(),
    };
    let mut queue: VecDeque<(usize, &Entry)> = VecDeque::from([(0, root)]);
    while let Some((at, entry)) = queue.pop_front() {
        match entry {
            Entry::Directory {
                mode,
                opaque,
                children,
            } => {
                let first = flat.nodes.len();
                let n = &mut flat.nodes[at];
                n.mode = mode & 0o777;
                n.flags = if *opaque { OPAQUE } else { 0 };
                n.count =
                    u32::try_from(children.len()).map_err(|_| bad("a directory too large"))?;
                n.first = if children.is_empty() { 0 } else { first as u32 };
                for (name, child) in children {
                    if !format::valid_name(name) {
                        return Err(bad(format!(
                            "`{}` cannot be a name in a layer file",
                            String::from_utf8_lossy(name)
                        )));
                    }
                    let mut node = blank;
                    node.name_offset = flat.names.len() as u64;
                    node.name_len = name.len() as u16;
                    flat.names.extend_from_slice(name);
                    flat.nodes.push(node);
                    queue.push_back((flat.nodes.len() - 1, child));
                }
            }
            Entry::File { mode, data, link } => {
                let n = &mut flat.nodes[at];
                n.kind = Kind::Regular;
                n.mode = mode & 0o777;
                n.size = data.len();
                flat.files.push((at, data, *link));
            }
            Entry::Symlink { target } => {
                if !format::valid_target(target) {
                    return Err(bad(format!(
                        "a symbolic link to `{}` cannot be written",
                        String::from_utf8_lossy(target)
                    )));
                }
                let n = &mut flat.nodes[at];
                n.kind = Kind::Symlink;
                n.mode = 0o777;
                n.target_offset = flat.names.len() as u64;
                n.target_len = target.len() as u32;
                flat.names.extend_from_slice(target);
            }
            Entry::Whiteout => {
                flat.nodes[at].kind = Kind::Whiteout;
            }
        }
        if flat.nodes.len() > format::NODES_MAX as usize {
            return Err(bad("more entries than a layer file holds"));
        }
    }
    Ok(flat)
}

/// Writes every file's pages that are not all zero to `data`, and returns the extents
/// (a start of 0 is a hole; otherwise one more than the page's index in `data`).
fn lay_data(flat: &mut Flat<'_>, data: &mut impl Write) -> Result<Vec<Extent>> {
    let mut extents: Vec<Extent> = Vec::new();
    let mut written = 0u64;
    let mut linked: HashMap<u64, (u32, u32)> = HashMap::new();
    let files = std::mem::take(&mut flat.files);
    for (at, source, link) in &files {
        if let Some(link) = link
            && let Some((first, count)) = linked.get(link)
        {
            flat.nodes[*at].first = *first;
            flat.nodes[*at].count = *count;
            continue;
        }
        let first = extents.len() as u32;
        let mut count = 0u32;
        let mut reader: Box<dyn Read> = match source {
            Data::Bytes(b) => Box::new(io::Cursor::new(b.clone())),
            Data::Path(p, _) => Box::new(BufReader::new(open_readable(p)?)),
        };
        let mut total = 0u64;
        loop {
            let mut page = [0u8; PAGE];
            let mut filled = 0;
            while filled < PAGE {
                let n = reader
                    .read(&mut page[filled..])
                    .map_err(|e| Error::io("reading a file for a layer", e))?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            total += filled as u64;
            let zero = page.iter().all(|b| *b == 0);
            let start = if zero { 0 } else { written + 1 };
            match extents.last_mut() {
                Some(e)
                    if count > 0
                        && (e.start == 0) == zero
                        && (zero || e.start + e.pages == start) =>
                {
                    e.pages += 1
                }
                _ => {
                    extents.push(Extent { start, pages: 1 });
                    count += 1;
                }
            }
            if !zero {
                data.write_all(&page)
                    .map_err(|e| Error::io("writing a layer's data", e))?;
                written += 1;
            }
            if filled < PAGE {
                break;
            }
        }
        if total != source.len() {
            return Err(bad(format!(
                "a file changed while it was read: {total} bytes, not {}",
                source.len()
            )));
        }
        if extents.len() > format::EXTENTS_MAX as usize {
            return Err(bad("more extents than a layer file holds"));
        }
        flat.nodes[*at].first = if count == 0 { 0 } else { first };
        flat.nodes[*at].count = count;
        if let Some(link) = link {
            linked.insert(*link, (flat.nodes[*at].first, count));
        }
    }
    Ok(extents)
}

/// A file opened for reading even when its mode does not let its owner read it (a
/// layer may make a file of mode 0): the owner's read bit is set for the open and put
/// back.
fn open_readable(path: &Path) -> Result<File> {
    match File::open(path) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let mode = fs::metadata(path).map_err(io_at(path, "reading"))?.mode();
            fs::set_permissions(path, fs::Permissions::from_mode((mode | 0o400) & 0o7777))
                .map_err(io_at(path, "making readable"))?;
            let opened = File::open(path).map_err(io_at(path, "opening"));
            fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(io_at(path, "restoring the mode of"))?;
            opened
        }
        Err(e) => Err(io_at(path, "opening")(e)),
    }
}

/// Writes pages and keeps each one's SHA-256.
struct Pager<W: Write> {
    out: W,
    hashes: Vec<[u8; 32]>,
}

impl<W: Write> Pager<W> {
    fn page(&mut self, page: &[u8; PAGE]) -> Result<()> {
        self.hashes.push(Sha256::digest(page).into());
        self.out
            .write_all(page)
            .map_err(|e| Error::io("writing a layer file", e))
    }

    /// `bytes` as whole pages, the last one padded with zeros.
    fn region(&mut self, bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks(PAGE) {
            let mut page = [0u8; PAGE];
            page[..chunk.len()].copy_from_slice(chunk);
            self.page(&page)?;
        }
        Ok(())
    }
}

/// The tree of the directory `dir` on the host, as a layer adds it: every directory,
/// regular file and symbolic link below it, with its permission bits; anything else is
/// refused.
pub fn tree(dir: &Path) -> Result<Entry> {
    let meta = fs::symlink_metadata(dir).map_err(io_at(dir, "reading"))?;
    if !meta.is_dir() {
        return Err(bad(format!("{} is not a directory", dir.display())));
    }
    let mut children = BTreeMap::new();
    for child in fs::read_dir(dir).map_err(io_at(dir, "listing"))? {
        let child = child.map_err(io_at(dir, "listing"))?;
        let path = child.path();
        let meta = fs::symlink_metadata(&path).map_err(io_at(&path, "reading"))?;
        let name = child.file_name().as_encoded_bytes().to_vec();
        let entry = if meta.file_type().is_symlink() {
            Entry::Symlink {
                target: fs::read_link(&path)
                    .map_err(io_at(&path, "reading"))?
                    .into_os_string()
                    .into_encoded_bytes(),
            }
        } else if meta.is_dir() {
            tree(&path)?
        } else if meta.is_file() {
            Entry::File {
                mode: meta.permissions().mode() & 0o777,
                data: Data::Path(path.clone(), meta.len()),
                link: None,
            }
        } else {
            return Err(bad(format!(
                "{} is not a file, a directory or a link",
                path.display()
            )));
        };
        children.insert(name, entry);
    }
    Ok(Entry::Directory {
        mode: meta.permissions().mode() & 0o777,
        opaque: false,
        children,
    })
}

/// Writes `root` as a layer file at `out`.
pub fn write(root: &Entry, out: &Path) -> Result<LayerFile> {
    let mut flat = flatten(root)?;
    let spill = out.with_extension("data-part");
    let mut data = BufWriter::new(File::create(&spill).map_err(io_at(&spill, "creating"))?);
    let laid = lay_data(&mut flat, &mut data);
    let finished = data
        .into_inner()
        .map_err(|e| Error::io("writing a layer's data", e.into_error()));
    let result = laid.and_then(|extents| {
        finished?;
        assemble(&flat, &extents, &spill, out)
    });
    let _ = fs::remove_file(&spill);
    if result.is_err() {
        let _ = fs::remove_file(out);
    }
    result
}

fn assemble(flat: &Flat<'_>, extents: &[Extent], spill: &Path, out: &Path) -> Result<LayerFile> {
    let data_pages = fs::metadata(spill).map_err(io_at(spill, "reading"))?.len() / PAGE as u64;
    let g = Geometry::new(
        flat.nodes.len() as u32,
        extents.len() as u32,
        flat.names.len() as u64,
        data_pages,
    )
    .map_err(|e| bad(e.0))?;
    let file = File::create(out).map_err(io_at(out, "creating"))?;
    let mut pager = Pager {
        out: BufWriter::new(file),
        hashes: Vec::new(),
    };
    pager
        .out
        .write_all(&[0u8; PAGE])
        .map_err(|e| Error::io("writing a layer file", e))?;
    let mut nodes = Vec::with_capacity(flat.nodes.len() * NODE_BYTES);
    for n in &flat.nodes {
        nodes.extend_from_slice(&n.encode());
    }
    pager.region(&nodes)?;
    pager.region(&flat.names)?;
    let mut table = Vec::with_capacity(extents.len() * EXTENT_BYTES);
    for e in extents {
        let start = if e.start == 0 {
            0
        } else {
            e.start - 1 + g.data_page
        };
        table.extend_from_slice(
            &Extent {
                start,
                pages: e.pages,
            }
            .encode(),
        );
    }
    pager.region(&table)?;
    let mut spilled = BufReader::new(File::open(spill).map_err(io_at(spill, "opening"))?);
    for _ in 0..data_pages {
        let mut page = [0u8; PAGE];
        spilled
            .read_exact(&mut page)
            .map_err(|e| Error::io("reading a layer's data back", e))?;
        pager.page(&page)?;
    }
    if pager.hashes.len() as u64 != g.tree_page - 1 {
        return Err(bad("the layer's content is not the length its counts give"));
    }
    let mut level = std::mem::take(&mut pager.hashes);
    let mut root = [0u8; 32];
    for l in 0..g.levels {
        let mut next = Vec::new();
        for chunk in level.chunks(FANOUT as usize) {
            let mut page = [0u8; PAGE];
            for (i, h) in chunk.iter().enumerate() {
                page[i * 32..i * 32 + 32].copy_from_slice(h);
            }
            pager.page(&page)?;
            let h = std::mem::take(&mut pager.hashes);
            next.extend(h);
        }
        if next.len() as u64 != g.level[l].1 {
            return Err(bad("the tree is not the shape the layout gives"));
        }
        if l + 1 == g.levels {
            root = next[0];
        }
        level = next;
    }
    let header = Header {
        node_count: g.node_count,
        extent_count: g.extent_count,
        names_len: g.names_len,
        data_pages: g.data_pages,
        root,
    }
    .encode()
    .map_err(|e| bad(e.0))?;
    let mut file = pager
        .out
        .into_inner()
        .map_err(|e| Error::io("writing a layer file", e.into_error()))?;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&header))
        .and_then(|()| file.sync_all())
        .map_err(io_at(out, "writing the header of"))?;
    Ok(LayerFile {
        id: Digest::of(&header),
        path: out.to_path_buf(),
        pages: g.pages,
    })
}

fn name_of(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.file_name()
        .map(|n| n.as_bytes().to_vec())
        .unwrap_or_default()
}

/// What `at` is in the root, `None` when it is not there (nor is a directory on the way to it).
fn lookup(at: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(at) {
        Ok(meta) => Ok(Some(meta)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(io_at(at, "reading")(e)),
    }
}

/// The directory entry at `rel` in `root`, made (as a directory of its mode in the working root)
/// with its parents when missing.
fn dir_at<'a>(root: &'a mut Entry, rel: &Path, rootfs: &Path) -> Result<&'a mut Entry> {
    let mut at = root;
    let mut walked = PathBuf::new();
    for part in rel.iter() {
        walked.push(part);
        let here = rootfs.join(&walked);
        let meta = fs::symlink_metadata(&here).map_err(io_at(&here, "reading"))?;
        let Entry::Directory { children, .. } = at else {
            return Err(bad(format!("{} is under a file", walked.display())));
        };
        at = children
            .entry(name_of(&walked))
            .or_insert_with(|| Entry::directory(meta.mode() & 0o777));
    }
    Ok(at)
}

/// Every path under the directory `dir` of the working root, added to `paths`: what a
/// directory that hides the layers below keeps of them is what the layer has to state.
fn below(rootfs: &Path, dir: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut todo = vec![dir.to_path_buf()];
    while let Some(rel) = todo.pop() {
        let at = rootfs.join(&rel);
        for child in fs::read_dir(&at).map_err(io_at(&at, "listing"))? {
            let child = child.map_err(io_at(&at, "listing"))?;
            let path = rel.join(child.file_name());
            let kind = child.file_type().map_err(io_at(&at, "reading"))?;
            if kind.is_dir() {
                todo.push(path.clone());
            }
            paths.insert(path);
        }
    }
    Ok(())
}

/// What one layer did, from the unpacker's record of it (SDD 27, f.1): an entry for every path
/// it wrote, read from the working root as the layer left it, the directories it made opaque,
/// and a whiteout for every name of the layers below it removed.
fn difference(applied: &Applied, rootfs: &Path) -> Result<Entry> {
    let mut paths = applied.written.clone();
    for dir in &applied.opaque {
        paths.insert(dir.clone());
        below(rootfs, dir, &mut paths)?;
    }
    let mut root = Entry::directory(0o755);
    for p in &paths {
        let at = rootfs.join(p);
        let meta = lookup(&at)?.ok_or_else(|| {
            bad(format!(
                "the unpacking says it wrote {}, and the working root has nothing there",
                p.display()
            ))
        })?;
        let is_opaque = applied.opaque.contains(p);
        if p.as_os_str().is_empty() {
            if let Entry::Directory {
                mode, opaque: o, ..
            } = &mut root
            {
                *mode = meta.mode() & 0o777;
                *o = is_opaque;
            }
            continue;
        }
        let parent = dir_at(&mut root, p.parent().unwrap_or(Path::new("")), rootfs)?;
        let Entry::Directory { children, .. } = parent else {
            return Err(bad(format!("{} is under a file", p.display())));
        };
        let name = name_of(p);
        let entry = if meta.file_type().is_symlink() {
            use std::os::unix::ffi::OsStrExt;
            let target = fs::read_link(&at).map_err(io_at(&at, "reading the link"))?;
            Entry::Symlink {
                target: target.as_os_str().as_bytes().to_vec(),
            }
        } else if meta.is_dir() {
            // Parents sort before what they hold, so nothing is in it yet.
            Entry::Directory {
                mode: meta.mode() & 0o777,
                opaque: is_opaque,
                children: BTreeMap::new(),
            }
        } else if meta.is_file() {
            Entry::File {
                mode: meta.mode() & 0o777,
                data: Data::Path(at.clone(), meta.len()),
                link: Some(meta.ino() ^ (meta.dev() << 48)),
            }
        } else {
            return Err(bad(format!(
                "{} is not a file, directory or link",
                at.display()
            )));
        };
        children.insert(name, entry);
    }
    // Whiteouts: a name the layers below had and this layer removed and did not make again, in
    // a directory that is still there and does not hide everything below anyway.
    for p in &applied.removed {
        let Some(parent) = p.parent() else {
            continue;
        };
        if lookup(&rootfs.join(p))?.is_some() {
            continue;
        }
        let kept = lookup(&rootfs.join(parent))?.is_some_and(|m| m.is_dir());
        if !kept || parent.ancestors().any(|a| applied.opaque.contains(a)) {
            continue;
        }
        let dir = dir_at(&mut root, parent, rootfs)?;
        if let Entry::Directory { children, .. } = dir {
            children.insert(name_of(p), Entry::Whiteout);
        }
    }
    Ok(root)
}

/// An image converted: its config and its layer files, lowest first.
#[derive(Clone, Debug)]
pub struct Converted {
    /// What the image's config says to run.
    pub config: Config,
    /// Each layer's file, lowest first.
    pub layers: Vec<LayerFile>,
}

/// Converts the image `reference` names into layer files in `out`, each named by its
/// id (`<sha256 hex>.layer`): every layer unpacked over the ones below it with SDD
/// 19's rules, and what it did written as a layer file.
pub fn convert(source: &dyn ImageSource, reference: &str, out: &Path) -> Result<Converted> {
    fs::create_dir_all(out).map_err(io_at(out, "making"))?;
    let work = out.join(format!(".convert-{}", std::process::id()));
    let _ = remove_any(&work, "converting");
    let rootfs = work.join("rootfs");
    fs::create_dir_all(&rootfs).map_err(io_at(&rootfs, "making"))?;
    let result = convert_in(source, reference, out, &work, &rootfs);
    let _ = remove_any(&work, "converting");
    result
}

fn convert_in(
    source: &dyn ImageSource,
    reference: &str,
    out: &Path,
    work: &Path,
    rootfs: &Path,
) -> Result<Converted> {
    let manifest = source.manifest(reference)?;
    let config = Config::parse(&unpack::blob_bytes(source, &manifest.config)?)?;
    let mut skipped = 0;
    let mut layers = Vec::new();
    for (i, descriptor) in manifest.layers.iter().enumerate() {
        let applied = unpack::apply_layer(source, descriptor, rootfs, &mut skipped)?;
        let tree = difference(&applied, rootfs)?;
        let part = work.join(format!("layer-{i}"));
        let written = write(&tree, &part)?;
        let named = out.join(format!("{}.layer", written.id.hex()));
        fs::rename(&part, &named).map_err(io_at(&named, "moving into place"))?;
        layers.push(LayerFile {
            path: named,
            ..written
        });
    }
    // The image's own refusals of SDD 19 f.3, which the unpacking makes after its last
    // layer: a mount point that is not a directory.
    unpack::mount_points(rootfs)?;
    Ok(Converted { config, layers })
}

/// Layer files as a disk: pages read from the files, slots in memory.
struct FileDisk {
    files: Vec<File>,
    lengths: Vec<u64>,
    slots: Vec<[u8; PAGE]>,
}

impl Disk for FileDisk {
    fn fetch(&mut self, reads: &[Fetch]) -> moruna_imagefs::Result<()> {
        for r in reads {
            let file = self
                .files
                .get(r.layer)
                .ok_or_else(|| moruna_imagefs::Error::Io(format!("no layer {}", r.layer)))?;
            file.read_exact_at(&mut self.slots[r.slot as usize], r.page * PAGE as u64)
                .map_err(|e| {
                    moruna_imagefs::Error::Io(format!("layer {} page {}: {e}", r.layer, r.page))
                })?;
        }
        Ok(())
    }

    fn copy(&mut self, slot: u32, offset: usize, out: &mut [u8]) -> moruna_imagefs::Result<()> {
        out.copy_from_slice(&self.slots[slot as usize][offset..offset + out.len()]);
        Ok(())
    }

    fn device_pages(&mut self, layer: usize) -> moruna_imagefs::Result<u64> {
        Ok(self.lengths[layer] / PAGE as u64)
    }
}

/// One entry of a merged tree, as [`read`] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A directory and its mode.
    Directory(u32),
    /// A file: its mode and its bytes.
    File(u32, Vec<u8>),
    /// A symbolic link and its target.
    Symlink(Vec<u8>),
}

/// The tree `layers` (lowest first) merge to, read with the kernel's reader: every
/// path below the root (`/tmp`, `/proc` and `/dev`, which the server adds, left out),
/// with what it is. A layer that is not what its hashes say is an error.
pub fn read(layers: &[PathBuf]) -> Result<BTreeMap<String, Item>> {
    let mut files = Vec::new();
    let mut lengths = Vec::new();
    for p in layers {
        let f = File::open(p).map_err(io_at(p, "opening"))?;
        lengths.push(f.metadata().map_err(io_at(p, "reading"))?.len());
        files.push(f);
    }
    let fail = |e: moruna_imagefs::Error| bad(format!("reading the layer files: {e:?}"));
    let slots = 1024;
    let disk = FileDisk {
        files,
        lengths,
        slots: vec![[0u8; PAGE]; slots],
    };
    let mut cache = Cache::new(disk, slots as u32).map_err(fail)?;
    for _ in layers {
        cache.mount().map_err(fail)?;
    }
    let overlay = Overlay::new(cache.layers().to_vec());
    let scratch = Scratch::new();
    let tree = Tree {
        overlay: &overlay,
        scratch: &scratch,
    };
    let mut out = BTreeMap::new();
    let mut todo = vec![String::from("/")];
    while let Some(dir) = todo.pop() {
        let place = tree
            .resolve(&mut cache, dir.as_bytes(), false)
            .map_err(fail)?;
        for (name, kind, _) in tree.list(&mut cache, &place).map_err(fail)? {
            let name = String::from_utf8(name).map_err(|_| bad("a name that is not UTF-8"))?;
            let path = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            if dir == "/" && ["tmp", "proc", "dev"].contains(&name.as_str()) {
                continue;
            }
            let at = tree
                .resolve(&mut cache, path.as_bytes(), false)
                .map_err(fail)?;
            let item = match (kind, &at) {
                (Kind::Directory, Place::Image(Found::Directory(d))) => {
                    todo.push(path.clone());
                    Item::Directory(d.mode())
                }
                (Kind::Regular, Place::Image(Found::File { layer, node, .. })) => {
                    let map = overlay.file(&mut cache, *layer, node).map_err(fail)?;
                    let mut bytes = Vec::with_capacity(map.size as usize);
                    for page in 0..map.pages() {
                        let mut buf = [0u8; PAGE];
                        if let Some(Some(lp)) = map.layer_page(page) {
                            cache.read(*layer, lp, 0, &mut buf).map_err(fail)?;
                        }
                        bytes.extend_from_slice(&buf);
                    }
                    bytes.truncate(map.size as usize);
                    Item::File(node.mode, bytes)
                }
                (Kind::Symlink, _) => {
                    Item::Symlink(tree.readlink(&mut cache, path.as_bytes()).map_err(fail)?)
                }
                _ => {
                    return Err(bad(format!(
                        "{path} lists as one kind and opens as another"
                    )));
                }
            };
            out.insert(path, item);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "layerfile_tests.rs"]
mod tests;
