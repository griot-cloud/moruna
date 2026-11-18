//! The files of the layer a volume holds (`architecture/moruna-os-system.md` 3C): the output
//! disk's format, `moruna_imagefs::format`, for sources too, written by `moruna volume pack`
//! or by a run's sinks. `disk://<serial>/<path>` names the file at `path` in it.
//!
//! The layer's index (its header, nodes, names and extents) is read through
//! `moruna_imagefs`'s page cache, so every page of it is checked against the layer's hash
//! tree before it is believed, as the image filesystem believes an image's. A file's bytes are
//! then read as they are, as any store's are: straight into the reader's memory, by the
//! volume's driver ([`Volume::read`]), never through the cache.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moruna_imagefs::Error as FsError;
use moruna_imagefs::cache::{Cache, Disk, Fetch};
use moruna_imagefs::format::{Kind, Node, PAGE, PAGE_U64};
use moruna_imagefs::volume::{FileMap, Layer};

use super::io::{Done, Join, Volume, zero};

/// Pages of the index the cache holds at once.
const CACHED_PAGES: u32 = 256;
/// The most files a listing names.
pub const LIST_MAX: usize = 1 << 20;

/// The cache's pages, read from the volume.
struct Fetched {
    volume: Volume,
    slots: Vec<Box<[u8; PAGE]>>,
}

impl Disk for Fetched {
    fn fetch(&mut self, reads: &[Fetch]) -> moruna_imagefs::Result<()> {
        for read in reads {
            let bytes = self
                .volume
                .read_bytes(read.page * PAGE_U64, PAGE)
                .map_err(FsError::Io)?;
            let slot = self
                .slots
                .get_mut(read.slot as usize)
                .ok_or(FsError::TooLarge)?;
            slot.copy_from_slice(&bytes);
        }
        Ok(())
    }

    fn copy(&mut self, slot: u32, offset: usize, out: &mut [u8]) -> moruna_imagefs::Result<()> {
        let page = self.slots.get(slot as usize).ok_or(FsError::TooLarge)?;
        let bytes = page
            .get(offset..offset + out.len())
            .ok_or(FsError::BadRange)?;
        out.copy_from_slice(bytes);
        Ok(())
    }

    fn device_pages(&mut self, _layer: usize) -> moruna_imagefs::Result<u64> {
        Ok(self.volume.capacity() / PAGE_U64)
    }
}

/// A run of a file's bytes: where in the file it starts, where on the volume (`None`: a hole,
/// which reads as zeros), and how long it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// Its first byte in the file.
    pub file: u64,
    /// Its first byte on the volume, or `None` for a hole.
    pub volume: Option<u64>,
    /// Its bytes.
    pub len: u64,
}

/// A file of the layer: its length and where its bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    /// Its bytes.
    pub size: u64,
    /// Its runs, in order, covering exactly `size` bytes.
    pub runs: Vec<Run>,
}

impl File {
    fn of(map: &FileMap) -> File {
        let mut runs: Vec<Run> = Vec::new();
        for page in 0..map.pages() {
            let at = page * PAGE_U64;
            let len = (map.size - at).min(PAGE_U64);
            let volume = map.layer_page(page).flatten().map(|p| p * PAGE_U64);
            match runs.last_mut() {
                Some(last)
                    if last.file + last.len == at
                        && match (last.volume, volume) {
                            (Some(a), Some(b)) => a + last.len == b,
                            (None, None) => true,
                            _ => false,
                        } =>
                {
                    last.len += len;
                }
                _ => runs.push(Run {
                    file: at,
                    volume,
                    len,
                }),
            }
        }
        File {
            size: map.size,
            runs,
        }
    }
}

/// The layer on one volume: its index, and the files looked up so far.
pub struct Files {
    volume: Volume,
    cache: Mutex<(Cache<Fetched>, Layer)>,
    found: Mutex<BTreeMap<String, Arc<File>>>,
}

impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Files")
            .field("volume", &self.volume)
            .finish()
    }
}

fn fs_error(serial: &str, e: FsError) -> String {
    match e {
        FsError::Corrupt(why) => format!(
            "volume {serial} does not hold a layer `moruna volume pack` or a run writes: {why}"
        ),
        FsError::Io(why) => why,
        other => format!("volume {serial}: {other:?}"),
    }
}

/// The names of a path in a layer, which are each a name a layer may hold.
fn names(path: &str) -> Option<Vec<&[u8]>> {
    let names: Vec<&[u8]> = path
        .split('/')
        .filter(|n| !n.is_empty())
        .map(str::as_bytes)
        .collect();
    names
        .iter()
        .all(|n| moruna_imagefs::format::valid_name(n))
        .then_some(names)
}

impl Files {
    /// The layer `volume` holds, its header checked.
    pub fn open(volume: Volume) -> Result<Files, String> {
        let serial = volume.serial().to_string();
        let pages = Fetched {
            volume: volume.clone(),
            slots: (0..CACHED_PAGES).map(|_| Box::new([0u8; PAGE])).collect(),
        };
        let mut cache = Cache::new(pages, CACHED_PAGES).map_err(|e| fs_error(&serial, e))?;
        let index = cache.mount().map_err(|e| fs_error(&serial, e))?;
        let layer = cache.layers()[index].clone();
        Ok(Files {
            volume,
            cache: Mutex::new((cache, layer)),
            found: Mutex::default(),
        })
    }

    /// The volume it is on.
    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// The node at `path`, or `None` when nothing is there.
    fn node(&self, path: &str) -> Result<Option<Node>, String> {
        let serial = self.volume.serial();
        let Some(names) = names(path) else {
            return Err(format!(
                "`{path}` is not a path in volume {serial}'s layer: names of 1 to 255 bytes, \
                 no `.` or `..`"
            ));
        };
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let (cache, layer) = &mut *guard;
        let mut node = layer.root(cache).map_err(|e| fs_error(serial, e))?;
        for name in names {
            if node.kind != Kind::Directory {
                return Ok(None);
            }
            match layer
                .child(cache, &node, name)
                .map_err(|e| fs_error(serial, e))?
            {
                Some((_, child)) => node = child,
                None => return Ok(None),
            }
        }
        Ok(Some(node))
    }

    /// The regular file at `path`, or `None` when there is none.
    pub fn file(&self, path: &str) -> Result<Option<Arc<File>>, String> {
        if let Some(found) = self
            .found
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
        {
            return Ok(Some(Arc::clone(found)));
        }
        let Some(node) = self.node(path)? else {
            return Ok(None);
        };
        if node.kind != Kind::Regular {
            return Ok(None);
        }
        let serial = self.volume.serial();
        let map = {
            let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let (cache, layer) = &mut *guard;
            layer.file(cache, &node).map_err(|e| fs_error(serial, e))?
        };
        let file = Arc::new(File::of(&map));
        self.found
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_string(), Arc::clone(&file));
        Ok(Some(file))
    }

    /// Every regular file whose path starts with `prefix`, with its size, in order: the
    /// files under the directory the prefix ends in, or whose name it begins.
    pub fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, String> {
        let (dir, start) = match prefix.rfind('/') {
            Some(at) => (&prefix[..at], &prefix[at + 1..]),
            None => ("", prefix),
        };
        let Some(node) = self.node(dir)? else {
            return Ok(Vec::new());
        };
        if node.kind != Kind::Directory {
            return Ok(Vec::new());
        }
        let base = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", dir.trim_matches('/'))
        };
        let serial = self.volume.serial();
        let mut out = Vec::new();
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let (cache, layer) = &mut *guard;
        let mut stack = vec![(base, node, true)];
        while let Some((at, dir, top)) = stack.pop() {
            let children = layer
                .children(cache, &dir)
                .map_err(|e| fs_error(serial, e))?;
            for (name, _, child) in children.into_iter().rev() {
                let name = String::from_utf8_lossy(&name).into_owned();
                if top && !name.starts_with(start) {
                    continue;
                }
                let path = format!("{at}{name}");
                match child.kind {
                    Kind::Directory => stack.push((format!("{path}/"), child, false)),
                    Kind::Regular => {
                        if out.len() >= LIST_MAX {
                            return Err(format!(
                                "volume {serial}: more than {LIST_MAX} files under `{prefix}`"
                            ));
                        }
                        out.push((path, child.size));
                    }
                    _ => {}
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Reads `len` bytes at byte `offset` of `file` into the bytes at `addr`: each run's
    /// bytes straight from the volume, a hole's as zeros.
    pub fn read(&self, file: &File, offset: u64, addr: u64, len: u64, done: Done) {
        if offset.checked_add(len).is_none_or(|end| end > file.size) {
            return done(Err(format!(
                "{len} bytes at {offset} are past the file's {} bytes",
                file.size
            )));
        }
        let end = offset + len;
        let mut reads = Vec::new();
        for run in &file.runs {
            let from = run.file.max(offset);
            let to = (run.file + run.len).min(end);
            if from >= to {
                continue;
            }
            match run.volume {
                Some(at) => reads.push((at + (from - run.file), addr + (from - offset), to - from)),
                None => zero(addr + (from - offset), (to - from) as usize),
            }
        }
        if reads.is_empty() {
            return done(Ok(()));
        }
        let join = Join::new(reads.len(), done);
        for (at, to, n) in reads {
            let join = join.clone();
            self.volume.read(at, to, n, Box::new(move |r| join.part(r)));
        }
    }
}
