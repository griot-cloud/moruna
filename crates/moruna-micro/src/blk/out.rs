//! The output layer, written onto `disk.out` as the sinks write it (contract C3; data-os §3a:
//! none of it in the engine's scratch).
//!
//! A sink whose URL is under [`URL`] commits each file as one `write_object`, which
//! [`DiskReactor`](super::reactor::DiskReactor) hands to [`Output::put`]. The file takes the
//! next free pages of the disk after the [`RESERVE`]. Its whole pages go there by DMA straight
//! from the sink's buffer, its last partial page through a zeroed bounce page, and each page is
//! hashed for the layer's tree on the way. At the run's end [`Output::finish`] lays the layer
//! out around the data already on the disk, in `moruna_imagefs::format`'s format, unchanged:
//!
//! - the header, the nodes, the names and the extents fill the [`RESERVE`] before the data. The
//!   names are padded with zeros so the data starts exactly there: a reader checks only that
//!   each name lies inside the names;
//! - the hash tree goes after the data;
//! - the header goes last, after a flush, so a run that ends early leaves no layer.
//!
//! A file the run left under the output root in its scratch (a sink or task that wrote
//! locally) joins the layer at the end, copied once onto the disk.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};

use moruna_imagefs::format::{
    self, EXTENT_BYTES, Extent, FANOUT, Geometry, Header, Kind, NODE_BYTES, PAGE, pages_for,
};
use moruna_kernel::{Allocator, Buffer, BufferView, Completion, Dispatch, MorunaError, Tier};
use sha2::{Digest as _, Sha256};

use super::disk::Disk;
use super::engine::{Kind as OpKind, Op};

/// The URL prefix a sink writes the output layer's files under: `disk://out/<path>`.
pub const URL: &str = "disk://out/";
/// The bytes the layer's header and tables have before its data: 16 MiB, room for about
/// 150,000 files.
pub const RESERVE: u64 = 16 << 20;
/// The bytes one bounce piece carries, for a file that is not in the arena.
pub const PIECE: usize = 1 << 20;

const PAGE64: u64 = PAGE as u64;
const FILE_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o755;

type Hash = [u8; 32];

/// What finishing the layer did, for the run's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    /// The layer's bytes on the disk.
    pub bytes: u64,
    /// Its files' bytes.
    pub data: u64,
    /// Its files.
    pub files: u64,
    /// Its directories, the root included.
    pub directories: u64,
    /// Files found under the output root in the scratch, copied in at the end.
    pub picked_up: u64,
}

/// One committed file: its size and the page its data starts at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placed {
    size: u64,
    page: u64,
}

struct State {
    /// The next free byte of the disk, a page boundary past the reserve.
    next: u64,
    /// Each data page's hash: the `i`th page after the reserve at `i`.
    hashes: Vec<Hash>,
    /// Each committed file by its path under the root.
    files: BTreeMap<Vec<u8>, Placed>,
    /// Directories that hold nothing, found under the output root.
    empty_dirs: BTreeSet<Vec<u8>>,
    /// Why a write failed: no layer is written after one did.
    failed: Option<String>,
    /// No file is taken any more.
    finished: bool,
    /// Writes on their way to the disk.
    pending: u64,
}

/// The output layer of one run on `disk.out`.
pub struct Output {
    disk: Disk,
    /// The bytes before the data: [`RESERVE`].
    reserve: u64,
    alloc: Arc<dyn Allocator>,
    tier: Tier,
    dispatch: Dispatch,
    state: Mutex<State>,
    idle: Condvar,
}

fn io(op: &'static str, target: &str, msg: impl Into<String>) -> MorunaError {
    MorunaError::Io {
        op,
        target: target.to_string(),
        msg: msg.into(),
    }
}

fn hash(page: &[u8]) -> Hash {
    Sha256::digest(page).into()
}

/// The SHA-256 of a page whose first bytes are `bytes` and the rest zero.
fn page_hash(bytes: &[u8]) -> Hash {
    if bytes.len() == PAGE {
        return hash(bytes);
    }
    let mut page = [0u8; PAGE];
    page[..bytes.len()].copy_from_slice(bytes);
    hash(&page)
}

/// The names on the way to a file: each one a name a layer may hold.
fn components(path: &[u8]) -> Result<Vec<&[u8]>, String> {
    let parts: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
    if parts.iter().any(|p| !format::valid_name(p)) {
        return Err(format!(
            "`{}` is not a path a layer holds: names of 1 to 255 bytes, no `.` or `..`",
            String::from_utf8_lossy(path)
        ));
    }
    Ok(parts)
}

/// The tree pages a layer of `content` pages before its tree needs, at most.
fn tree_pages(content: u64) -> u64 {
    content.div_ceil(FANOUT - 1) + format::LEVELS_MAX as u64
}

impl Output {
    /// The output layer of a run on `disk`, its bounce buffers from `alloc` in `tier`, its
    /// completions resolved through `dispatch`.
    pub fn new(
        disk: Disk,
        alloc: Arc<dyn Allocator>,
        tier: Tier,
        dispatch: Dispatch,
    ) -> Arc<Output> {
        Output::with_reserve(disk, alloc, tier, dispatch, RESERVE)
    }

    /// As [`Output::new`], with `reserve` bytes (whole pages) before the data.
    pub(crate) fn with_reserve(
        disk: Disk,
        alloc: Arc<dyn Allocator>,
        tier: Tier,
        dispatch: Dispatch,
        reserve: u64,
    ) -> Arc<Output> {
        Arc::new(Output {
            disk,
            reserve,
            alloc,
            tier,
            dispatch,
            state: Mutex::new(State {
                next: reserve,
                hashes: Vec::new(),
                files: BTreeMap::new(),
                empty_dirs: BTreeSet::new(),
                failed: None,
                finished: false,
                pending: 0,
            }),
            idle: Condvar::new(),
        })
    }

    /// The disk it writes.
    pub fn disk(&self) -> &Disk {
        &self.disk
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes `pages` pages after the files written so far, and counts a write on its way; the
    /// first page's index.
    fn take(&self, pages: u64) -> Result<u64, String> {
        let mut s = self.lock();
        if let Some(why) = &s.failed {
            return Err(format!("an earlier write to the output disk failed: {why}"));
        }
        if s.finished {
            return Err("the output layer is finished".into());
        }
        let end = s.next + pages * PAGE64;
        let content = (end - PAGE64) / PAGE64;
        if end + tree_pages(content) * PAGE64 > self.disk.capacity() {
            return Err(format!(
                "the outputs do not fit {}'s {} bytes: {} bytes of data written, {} more asked",
                self.disk.name(),
                self.disk.capacity(),
                s.next - self.reserve,
                pages * PAGE64
            ));
        }
        let first = s.next / PAGE64;
        s.next = end;
        s.hashes
            .resize(((end - self.reserve) / PAGE64) as usize, [0; 32]);
        s.pending += 1;
        Ok(first)
    }

    /// Records the hashes of the pages from `first`.
    fn hashed(&self, first: u64, hashes: &[Hash]) {
        let at = (first - self.reserve / PAGE64) as usize;
        self.lock().hashes[at..at + hashes.len()].copy_from_slice(hashes);
    }

    /// A write is over: its file is committed when it is done, and no layer is written when it
    /// failed.
    fn done(&self, path: Vec<u8>, placed: Placed, result: &Result<(), String>) {
        let mut s = self.lock();
        match result {
            Ok(()) => {
                s.files.insert(path, placed);
            }
            Err(why) => {
                s.failed.get_or_insert_with(|| why.clone());
            }
        }
        s.pending -= 1;
        if s.pending == 0 {
            self.idle.notify_all();
        }
    }

    /// Commits `src` as the file at `path` (under [`URL`]): resolved when its bytes are on the
    /// disk. A path written again is the later file.
    pub fn put(self: &Arc<Self>, path: &str, src: BufferView) -> Completion<()> {
        let target = format!("{URL}{path}");
        let fail = |why: String| {
            Completion::resolved_on(Err(io("write_object", &target, why)), self.dispatch.clone())
        };
        if let Err(why) = components(path.as_bytes()) {
            return fail(why);
        }
        let Some(bytes) = src.as_host_slice() else {
            return fail("the output disk is written from host memory only".into());
        };
        let size = bytes.len() as u64;
        let first = match self.take(pages_for(size)) {
            Ok(first) => first,
            Err(why) => return fail(why),
        };
        let hashes: Vec<Hash> = bytes.chunks(PAGE).map(page_hash).collect();
        self.hashed(first, &hashes);
        let placed = Placed { size, page: first };
        let name = path.as_bytes().to_vec();
        let ops = match self.pieces(bytes, first * PAGE64) {
            Ok(ops) => ops,
            Err(why) => {
                let why = format!("a bounce buffer for the output disk: {why}");
                self.done(name, placed, &Err(why.clone()));
                return fail(why);
            }
        };
        let (tx, rx) = Completion::channel_on(self.dispatch.clone());
        let me = Arc::clone(self);
        let join = Join::new(ops.len(), move |result: Result<(), String>| {
            // The sink's bytes stay alive until the device has read them.
            drop(src);
            me.done(name, placed, &result);
            tx.resolve(result.map_err(|why| io("write_object", &target, why)));
        });
        for piece in ops {
            self.submit(piece, join.clone());
        }
        rx
    }

    /// Takes the file at `path` out of the layer, if it is in it; its pages stay on the disk,
    /// unnamed.
    pub fn remove(&self, path: &str) -> bool {
        self.lock().files.remove(path.as_bytes()).is_some()
    }

    /// The writes that put `bytes` on the disk from `offset`: whole pages straight from the
    /// sink's memory when the arena holds it, the last partial page through a zeroed bounce
    /// page; everything through bounce pieces otherwise.
    fn pieces(&self, bytes: &[u8], offset: u64) -> Result<Vec<Piece>, MorunaError> {
        let mut ops = Vec::new();
        let whole = bytes.len() / PAGE * PAGE;
        let direct = !bytes.is_empty() && self.alloc.contains(bytes.as_ptr());
        let mut at = 0;
        if direct && whole > 0 {
            ops.push(Piece {
                offset,
                addr: bytes.as_ptr() as u64,
                len: whole as u64,
                keep: None,
            });
            at = whole;
        }
        while at < bytes.len() {
            let n = (bytes.len() - at).min(PIECE);
            let len = n.next_multiple_of(PAGE);
            let mut buffer = self.alloc.alloc(len, self.tier)?;
            buffer[..n].copy_from_slice(&bytes[at..at + n]);
            buffer[n..len].fill(0);
            ops.push(Piece {
                offset: offset + at as u64,
                addr: buffer.as_ptr() as u64,
                len: len as u64,
                keep: Some(buffer),
            });
            at += n;
        }
        Ok(ops)
    }

    fn submit(&self, piece: Piece, join: Join) {
        let keep = piece.keep;
        self.disk.submit(Op {
            kind: OpKind::Write,
            offset: piece.offset,
            addr: piece.addr,
            len: piece.len,
            done: Box::new(move |r| {
                drop(keep);
                join.one(r);
            }),
        });
    }

    /// Commits the file at `path` on the host, under `name`: read in pieces through the arena
    /// and written once.
    fn put_file(self: &Arc<Self>, name: &[u8], path: &Path, size: u64) -> Result<(), String> {
        use std::io::Read as _;
        let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let first = self.take(pages_for(size))?;
        let placed = Placed { size, page: first };
        let mut hashes = Vec::with_capacity(pages_for(size) as usize);
        let (tx, rx) = mpsc::channel();
        let join = Join::new(size.div_ceil(PIECE as u64) as usize, move |r| {
            let _ = tx.send(r);
        });
        let mut at = 0u64;
        let mut failed = None;
        while at < size {
            let n = ((size - at) as usize).min(PIECE);
            let len = n.next_multiple_of(PAGE);
            let mut buffer = match self.alloc.alloc(len, self.tier) {
                Ok(b) => b,
                Err(e) => {
                    failed = Some(format!("a buffer for {}: {e}", path.display()));
                    break;
                }
            };
            if let Err(e) = file.read_exact(&mut buffer[..n]) {
                failed = Some(format!("{}: {e}", path.display()));
                break;
            }
            buffer[n..len].fill(0);
            hashes.extend(buffer[..len].chunks(PAGE).map(hash));
            self.submit(
                Piece {
                    offset: first * PAGE64 + at,
                    addr: buffer.as_ptr() as u64,
                    len: len as u64,
                    keep: Some(buffer),
                },
                join.clone(),
            );
            at += n as u64;
        }
        // Pieces never submitted are told now, so the join ends.
        let left = size.div_ceil(PIECE as u64) - at.div_ceil(PIECE as u64);
        for _ in 0..left {
            join.one(Err(failed.clone().unwrap_or_default()));
        }
        drop(join);
        let result = rx
            .recv()
            .unwrap_or_else(|_| Err("the output disk dropped a write".into()));
        let result = match failed {
            Some(why) => Err(why),
            None => result,
        };
        if result.is_ok() {
            self.hashed(first, &hashes);
        }
        self.done(name.to_vec(), placed, &result);
        result
    }

    /// Adds what is under `root` in the scratch, if anything: its files and its empty
    /// directories.
    fn pick_up(self: &Arc<Self>, root: &Path) -> Result<u64, String> {
        let mut found = 0;
        let mut todo = vec![(root.to_path_buf(), Vec::new())];
        while let Some((dir, prefix)) = todo.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && prefix.is_empty() => {
                    return Ok(0);
                }
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            };
            let mut empty = true;
            for entry in entries {
                let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
                empty = false;
                let path = entry.path();
                let mut name = prefix.clone();
                if !name.is_empty() {
                    name.push(b'/');
                }
                name.extend_from_slice(entry.file_name().as_encoded_bytes());
                let meta = std::fs::symlink_metadata(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if meta.is_dir() {
                    todo.push((path, name));
                } else if meta.is_file() {
                    components(&name)?;
                    if self.lock().files.contains_key(&name) {
                        return Err(format!(
                            "`{}` is both a file the sinks wrote to the output disk and a file \
                             under {}",
                            String::from_utf8_lossy(&name),
                            root.display()
                        ));
                    }
                    self.put_file(&name, &path, meta.len())?;
                    found += 1;
                } else {
                    return Err(format!(
                        "{} is not a file or a directory; the outputs are files and directories",
                        path.display()
                    ));
                }
            }
            if empty && !prefix.is_empty() {
                components(&prefix)?;
                self.lock().empty_dirs.insert(prefix);
            }
        }
        Ok(found)
    }

    /// Lays the layer out on the disk around the files written: picks up what is under `root`
    /// in the scratch, waits for every write, then writes the tables, the tree and, after a
    /// flush, the header. An error leaves no layer.
    pub fn finish(self: &Arc<Self>, root: &Path) -> Result<Written, String> {
        let picked_up = self.pick_up(root)?;
        let mut s = self.lock();
        while s.pending > 0 {
            s = self.idle.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        s.finished = true;
        if let Some(why) = &s.failed {
            return Err(format!("a write to the output disk failed: {why}"));
        }
        let tree = Tree::of(&s.files, &s.empty_dirs)?;
        let laid = tree.flatten()?;
        let data_pages = (s.next - self.reserve) / PAGE64;
        let reserve_pages = self.reserve / PAGE64;
        let fixed = 1
            + pages_for((laid.nodes.len() * NODE_BYTES) as u64)
            + pages_for((laid.extents.len() * EXTENT_BYTES) as u64);
        if fixed + pages_for(laid.names.len() as u64) > reserve_pages {
            return Err(format!(
                "the outputs are {} files and {} directories, whose tables need more than the {} \
                 bytes the output disk keeps for them",
                laid.files, laid.directories, self.reserve
            ));
        }
        let names_len = (reserve_pages - fixed) * PAGE64;
        let node_count = u32::try_from(laid.nodes.len()).map_err(|_| "too many nodes")?;
        let extent_count = u32::try_from(laid.extents.len()).map_err(|_| "too many extents")?;
        let g = Geometry::new(node_count, extent_count, names_len, data_pages).map_err(|e| e.0)?;
        if g.data_page != reserve_pages {
            return Err("the tables do not end where the data starts".into());
        }
        if g.pages * PAGE64 > self.disk.capacity() {
            return Err(format!(
                "the layer's {} bytes do not fit {}'s {} bytes",
                g.pages * PAGE64,
                self.disk.name(),
                self.disk.capacity()
            ));
        }
        let mut tables = vec![0u8; ((reserve_pages - 1) * PAGE64) as usize];
        let names_at = ((g.names_page - 1) * PAGE64) as usize;
        let extents_at = ((g.extents_page - 1) * PAGE64) as usize;
        for (i, node) in laid.nodes.iter().enumerate() {
            tables[i * NODE_BYTES..(i + 1) * NODE_BYTES].copy_from_slice(&node.encode());
        }
        tables[names_at..names_at + laid.names.len()].copy_from_slice(&laid.names);
        for (i, e) in laid.extents.iter().enumerate() {
            let at = extents_at + i * EXTENT_BYTES;
            tables[at..at + EXTENT_BYTES].copy_from_slice(&e.encode());
        }
        let mut level: Vec<Hash> = tables.chunks(PAGE).map(hash).collect();
        level.extend_from_slice(&s.hashes);
        let data = laid.data;
        drop(s);
        if level.len() as u64 != g.tree_page - 1 {
            return Err("the layer's content is not the length its counts give".into());
        }
        let mut tree_bytes = Vec::with_capacity(((g.pages - g.tree_page) * PAGE64) as usize);
        let mut root_hash = [0u8; 32];
        for l in 0..g.levels {
            let mut next = Vec::new();
            for chunk in level.chunks(FANOUT as usize) {
                let mut page = [0u8; PAGE];
                for (i, h) in chunk.iter().enumerate() {
                    page[i * 32..i * 32 + 32].copy_from_slice(h);
                }
                next.push(hash(&page));
                tree_bytes.extend_from_slice(&page);
            }
            if next.len() as u64 != g.level[l].1 {
                return Err("the tree is not the shape the layout gives".into());
            }
            if l + 1 == g.levels {
                root_hash = next[0];
            }
            level = next;
        }
        self.write(PAGE64, &tables)?;
        self.write(g.tree_page * PAGE64, &tree_bytes)?;
        self.flush()?;
        let header = Header {
            node_count,
            extent_count,
            names_len,
            data_pages,
            root: root_hash,
        }
        .encode()
        .map_err(|e| e.0)?;
        self.write(0, &header)?;
        self.flush()?;
        Ok(Written {
            bytes: g.pages * PAGE64,
            data,
            files: laid.files,
            directories: laid.directories,
            picked_up,
        })
    }

    /// Writes `bytes` (whole pages) at `offset` through two arena buffers, one on the device's
    /// ring while the next is filled.
    fn write(&self, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let mut pending: VecDeque<mpsc::Receiver<Result<(), String>>> = VecDeque::new();
        let mut at = 0;
        let mut failed = None;
        while at < bytes.len() {
            if pending.len() == 2
                && let Some(rx) = pending.pop_front()
                && let Err(why) = wait(rx)
            {
                failed = Some(why);
                break;
            }
            let n = (bytes.len() - at).min(PIECE);
            let mut buffer = match self.alloc.alloc(n, self.tier) {
                Ok(b) => b,
                Err(e) => {
                    failed = Some(format!("a buffer for the layer: {e}"));
                    break;
                }
            };
            buffer.copy_from_slice(&bytes[at..at + n]);
            let (tx, rx) = mpsc::channel();
            let addr = buffer.as_ptr() as u64;
            self.disk.submit(Op {
                kind: OpKind::Write,
                offset: offset + at as u64,
                addr,
                len: n as u64,
                done: Box::new(move |r| {
                    drop(buffer);
                    let _ = tx.send(r);
                }),
            });
            pending.push_back(rx);
            at += n;
        }
        for rx in pending {
            if let Err(why) = wait(rx) {
                failed.get_or_insert(why);
            }
        }
        failed.map_or(Ok(()), Err)
    }

    fn flush(&self) -> Result<(), String> {
        self.disk.run(Op {
            kind: OpKind::Flush,
            offset: 0,
            addr: 0,
            len: 0,
            done: Box::new(|_| {}),
        })
    }
}

fn wait(rx: mpsc::Receiver<Result<(), String>>) -> Result<(), String> {
    rx.recv()
        .unwrap_or_else(|_| Err("the output disk dropped a write".into()))
}

/// One write of a file's bytes: `len` bytes at `addr` onto the disk at `offset`, and the
/// bounce buffer that holds them, if any.
struct Piece {
    offset: u64,
    addr: u64,
    len: u64,
    keep: Option<Buffer>,
}

/// Told by each of a file's writes; tells `then` once, when the last is done, with the first
/// failure if any.
#[derive(Clone)]
struct Join(Arc<Mutex<JoinState>>);

type Then = Box<dyn FnOnce(Result<(), String>) + Send>;

struct JoinState {
    left: usize,
    failed: Option<String>,
    then: Option<Then>,
}

impl Join {
    fn new(count: usize, then: impl FnOnce(Result<(), String>) + Send + 'static) -> Join {
        let join = Join(Arc::new(Mutex::new(JoinState {
            left: count,
            failed: None,
            then: Some(Box::new(then)),
        })));
        if count == 0 {
            join.finish();
        }
        join
    }

    fn one(&self, result: Result<(), String>) {
        let last = {
            let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(why) = result {
                s.failed.get_or_insert(why);
            }
            s.left = s.left.saturating_sub(1);
            s.left == 0
        };
        if last {
            self.finish();
        }
    }

    fn finish(&self) {
        let (then, failed) = {
            let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
            (s.then.take(), s.failed.take())
        };
        if let Some(then) = then {
            then(failed.map_or(Ok(()), Err));
        }
    }
}

/// The layer's tree, as the files' paths make it.
enum Tree {
    Dir(BTreeMap<Vec<u8>, Tree>),
    File(Placed),
}

/// The tree laid out as the format lays it: the nodes in breadth-first order, each
/// directory's children consecutive and in name order, the names in node order, and one
/// extent per file with data.
struct Laid {
    nodes: Vec<format::Node>,
    names: Vec<u8>,
    extents: Vec<Extent>,
    files: u64,
    directories: u64,
    data: u64,
}

impl Tree {
    fn of(files: &BTreeMap<Vec<u8>, Placed>, empty: &BTreeSet<Vec<u8>>) -> Result<Tree, String> {
        let mut root = Tree::Dir(BTreeMap::new());
        let clash = |path: &[u8]| {
            format!(
                "`{}` is both a file and a directory in the outputs",
                String::from_utf8_lossy(path)
            )
        };
        let paths = files
            .iter()
            .map(|(p, f)| (p, Some(*f)))
            .chain(empty.iter().map(|p| (p, None)));
        for (path, file) in paths {
            let parts = components(path)?;
            let (last, dirs) = parts.split_last().ok_or_else(|| clash(path))?;
            let mut at = &mut root;
            for part in dirs {
                let Tree::Dir(children) = at else {
                    return Err(clash(path));
                };
                at = children
                    .entry(part.to_vec())
                    .or_insert_with(|| Tree::Dir(BTreeMap::new()));
            }
            let Tree::Dir(children) = at else {
                return Err(clash(path));
            };
            let leaf = match file {
                Some(f) => Tree::File(f),
                None => Tree::Dir(BTreeMap::new()),
            };
            if children.insert(last.to_vec(), leaf).is_some() {
                return Err(clash(path));
            }
        }
        Ok(root)
    }

    fn flatten(&self) -> Result<Laid, String> {
        let blank = format::Node {
            name_offset: 0,
            name_len: 0,
            kind: Kind::Directory,
            flags: 0,
            mode: DIR_MODE,
            first: 0,
            count: 0,
            size: 0,
            target_offset: 0,
            target_len: 0,
        };
        let mut laid = Laid {
            nodes: vec![blank],
            names: Vec::new(),
            extents: Vec::new(),
            files: 0,
            directories: 1,
            data: 0,
        };
        let mut queue: VecDeque<(usize, &Tree)> = VecDeque::from([(0, self)]);
        while let Some((at, tree)) = queue.pop_front() {
            match tree {
                Tree::Dir(children) => {
                    let first = laid.nodes.len();
                    let node = &mut laid.nodes[at];
                    node.count =
                        u32::try_from(children.len()).map_err(|_| "a directory too large")?;
                    node.first = if children.is_empty() { 0 } else { first as u32 };
                    for (name, child) in children {
                        let mut node = blank;
                        node.name_offset = laid.names.len() as u64;
                        node.name_len = name.len() as u16;
                        laid.names.extend_from_slice(name);
                        if matches!(child, Tree::Dir(_)) {
                            laid.directories += 1;
                        }
                        laid.nodes.push(node);
                        queue.push_back((laid.nodes.len() - 1, child));
                    }
                }
                Tree::File(placed) => {
                    let node = &mut laid.nodes[at];
                    node.kind = Kind::Regular;
                    node.mode = FILE_MODE;
                    node.size = placed.size;
                    if placed.size > 0 {
                        node.first =
                            u32::try_from(laid.extents.len()).map_err(|_| "too many extents")?;
                        node.count = 1;
                        laid.extents.push(Extent {
                            start: placed.page,
                            pages: pages_for(placed.size),
                        });
                    }
                    laid.files += 1;
                    laid.data += placed.size;
                }
            }
            if laid.nodes.len() > format::NODES_MAX as usize {
                return Err("more entries than a layer holds".into());
            }
        }
        Ok(laid)
    }
}
