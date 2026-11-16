//! A VM run's outputs, read back on the host (contract C3, stream D): the output disk holds, from
//! its first byte, one layer file of the engine's output root (`moruna_micro::blk::layer`), and
//! the host unpacks it into the job's output directory.
//!
//! The layer was written by the guest, so it is read as a hostile file would be:
//!
//! - through the image filesystem's own reader (`moruna-imagefs`), which checks every page
//!   against the layer's hash tree and every count, range and name against the format before it
//!   believes it, so no layer makes it loop, panic or allocate what it names;
//! - into a directory that is new or empty, creating every entry itself with `O_EXCL` and
//!   `O_NOFOLLOW`, under names the format allows (no `/`, no NUL, never `.` or `..`), so nothing
//!   is written outside it whatever the layer says;
//! - with symbolic links left out (counted), modes cut to `0o755` (no set-user-ID, nothing
//!   writable by others), and at most as many bytes as the disk holds, holes included.
//!
//! A layer that fails any of this leaves nothing behind: what was unpacked is removed.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use moruna_imagefs::cache::{Cache, Disk, Fetch};
use moruna_imagefs::format::{Kind, PAGE, valid_name};
use moruna_imagefs::volume::Pages;

/// The deepest path a layer may hold below the output directory.
pub const DEPTH_MAX: usize = 128;
/// Pages the reader keeps in memory.
const SLOTS: usize = 1024;

/// What an unpack wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unpacked {
    /// Regular files written.
    pub files: u64,
    /// Directories made, the output directory itself not counted.
    pub directories: u64,
    /// The files' bytes, holes included.
    pub bytes: u64,
    /// Symbolic links the layer held, which are not unpacked.
    pub links_skipped: u64,
}

/// The output disk as the reader's device: pages read from the file, slots in memory.
struct DiskFile {
    file: File,
    pages: u64,
    slots: Vec<[u8; PAGE]>,
}

impl Disk for DiskFile {
    fn fetch(&mut self, reads: &[Fetch]) -> moruna_imagefs::Result<()> {
        for r in reads {
            let slot = self
                .slots
                .get_mut(r.slot as usize)
                .ok_or(moruna_imagefs::Error::BadRange)?;
            if r.layer != 0 || r.page >= self.pages {
                return Err(moruna_imagefs::Error::BadRange);
            }
            self.file
                .read_exact_at(slot, r.page * PAGE as u64)
                .map_err(|e| moruna_imagefs::Error::Io(format!("page {}: {e}", r.page)))?;
        }
        Ok(())
    }

    fn copy(&mut self, slot: u32, offset: usize, out: &mut [u8]) -> moruna_imagefs::Result<()> {
        let page = self
            .slots
            .get(slot as usize)
            .ok_or(moruna_imagefs::Error::BadRange)?;
        let end = offset
            .checked_add(out.len())
            .filter(|end| *end <= PAGE)
            .ok_or(moruna_imagefs::Error::BadRange)?;
        out.copy_from_slice(&page[offset..end]);
        Ok(())
    }

    fn device_pages(&mut self, layer: usize) -> moruna_imagefs::Result<u64> {
        if layer != 0 {
            return Err(moruna_imagefs::Error::BadRange);
        }
        Ok(self.pages)
    }
}

fn read_err(e: moruna_imagefs::Error) -> String {
    format!("the output layer cannot be read: {e:?}")
}

/// Unpack the layer at the start of the output disk `disk` into `out`, a directory that does not
/// exist yet (its parent does) or is empty.
pub fn unpack(disk: &Path, out: &Path) -> Result<Unpacked, String> {
    let file = File::open(disk).map_err(|e| format!("the output disk {}: {e}", disk.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("the output disk {}: {e}", disk.display()))?
        .len();
    let mut cache = Cache::new(
        DiskFile {
            file,
            pages: len / PAGE as u64,
            slots: vec![[0u8; PAGE]; SLOTS],
        },
        SLOTS as u32,
    )
    .map_err(read_err)?;
    cache.mount().map_err(|e| match e {
        moruna_imagefs::Error::Corrupt(_) => format!(
            "the output disk holds no layer the engine wrote ({}); the run wrote no outputs",
            read_err(e)
        ),
        other => read_err(other),
    })?;
    let created = prepare(out)?;
    let result = walk(&mut cache, out, len);
    if result.is_err() {
        clear(out, created);
    }
    result
}

/// Make `out` ready: created when absent, accepted when an empty directory. True when created.
fn prepare(out: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(out) {
        Ok(m) if m.is_dir() => {
            let mut entries =
                std::fs::read_dir(out).map_err(|e| format!("{}: {e}", out.display()))?;
            if entries.next().is_some() {
                return Err(format!(
                    "the output directory {} is not empty; a run's outputs go into a new or \
                     empty one",
                    out.display()
                ));
            }
            Ok(false)
        }
        Ok(_) => Err(format!(
            "the output directory {} is not a directory",
            out.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o755)
                .create(out)
                .map_err(|e| format!("the output directory {}: {e}", out.display()))?;
            Ok(true)
        }
        Err(e) => Err(format!("the output directory {}: {e}", out.display())),
    }
}

/// Remove what an unpack that failed made.
fn clear(out: &Path, created: bool) {
    if created {
        let _ = std::fs::remove_dir_all(out);
    } else if let Ok(entries) = std::fs::read_dir(out) {
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => {
                    let _ = std::fs::remove_dir_all(&path);
                }
                _ => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

fn walk(cache: &mut Cache<DiskFile>, out: &Path, limit: u64) -> Result<Unpacked, String> {
    let layer = cache.layers()[0].clone();
    let root = layer.root(cache).map_err(read_err)?;
    if root.kind != Kind::Directory {
        return Err("the output layer's root is not a directory".into());
    }
    let mut done = Unpacked::default();
    let mut todo = vec![(root, PathBuf::from(out), 0usize)];
    let mut page = vec![0u8; PAGE];
    while let Some((dir, at, depth)) = todo.pop() {
        for (name, _, node) in layer.children(cache, &dir).map_err(read_err)? {
            if !valid_name(&name) {
                return Err("the output layer holds a name no file may have".into());
            }
            let path = at.join(OsStr::from_bytes(&name));
            match node.kind {
                Kind::Directory => {
                    if depth + 1 >= DEPTH_MAX {
                        return Err(format!(
                            "the output layer is deeper than {DEPTH_MAX} directories"
                        ));
                    }
                    std::fs::DirBuilder::new()
                        .mode(0o755)
                        .create(&path)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    done.directories += 1;
                    todo.push((node, path, depth + 1));
                }
                Kind::Regular => {
                    let map = layer.file(cache, &node).map_err(read_err)?;
                    done.bytes = done
                        .bytes
                        .checked_add(map.size)
                        .filter(|total| *total <= limit)
                        .ok_or_else(|| {
                            format!(
                                "the output layer's files are larger than its disk ({limit} \
                                 bytes)"
                            )
                        })?;
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode((node.mode & 0o755) | 0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&path)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    for p in 0..map.pages() {
                        let offset = p * PAGE as u64;
                        let n = (map.size - offset).min(PAGE as u64) as usize;
                        match map.layer_page(p) {
                            Some(Some(lp)) => {
                                cache
                                    .read(map.layer, lp, 0, &mut page[..n])
                                    .map_err(read_err)?;
                                file.write_all_at(&page[..n], offset)
                                    .map_err(|e| format!("{}: {e}", path.display()))?;
                            }
                            Some(None) => {}
                            None => return Err("the output layer's file map is short".into()),
                        }
                    }
                    file.set_len(map.size)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    done.files += 1;
                }
                Kind::Symlink => done.links_skipped += 1,
                Kind::Whiteout => {}
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_image::layerfile::{self, Data, Entry};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "moruna-outputs-{tag}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Scratch(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file(mode: u32, bytes: &[u8]) -> Entry {
        Entry::File {
            mode,
            data: Data::Bytes(Arc::from(bytes)),
            link: None,
        }
    }

    fn dir(children: Vec<(&str, Entry)>) -> Entry {
        Entry::Directory {
            mode: 0o755,
            opaque: false,
            children: children
                .into_iter()
                .map(|(n, e)| (n.as_bytes().to_vec(), e))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    /// A layer of `tree` written at the start of a disk of `disk_bytes`, as the engine writes it.
    fn disk(s: &Scratch, tree: &Entry, disk_bytes: u64) -> PathBuf {
        let layer = s.0.join("layer");
        layerfile::write(tree, &layer).unwrap();
        let bytes = std::fs::read(&layer).unwrap();
        let disk = s.0.join("out.disk");
        let f = File::create(&disk).unwrap();
        f.set_len(disk_bytes.max(bytes.len() as u64)).unwrap();
        f.write_all_at(&bytes, 0).unwrap();
        disk
    }

    #[test]
    fn a_layer_unpacks_into_a_new_directory() {
        let s = Scratch::new("tree");
        let big: Vec<u8> = (0..3 * PAGE + 17).map(|i| (i % 251) as u8).collect();
        let mut zeros = vec![0u8; 3 * PAGE];
        zeros[PAGE * 2 + 5] = 9;
        let tree = dir(vec![
            (
                "result",
                dir(vec![
                    ("part-0.parquet", file(0o644, &big)),
                    ("holes", file(0o4755, &zeros)),
                    ("empty", file(0o600, b"")),
                    ("deeper", dir(vec![("x", file(0o666, b"x"))])),
                ]),
            ),
            (
                "link",
                Entry::Symlink {
                    target: b"/etc/passwd".to_vec(),
                },
            ),
            ("_SUCCESS", file(0o644, b"")),
        ]);
        let disk = disk(&s, &tree, 1 << 20);
        let out = s.0.join("outputs");
        let done = unpack(&disk, &out).unwrap();
        assert_eq!(done.files, 5);
        assert_eq!(done.directories, 2);
        assert_eq!(done.links_skipped, 1);
        assert_eq!(done.bytes, (big.len() + zeros.len() + 1) as u64);
        assert_eq!(
            std::fs::read(out.join("result/part-0.parquet")).unwrap(),
            big
        );
        assert_eq!(std::fs::read(out.join("result/holes")).unwrap(), zeros);
        assert_eq!(std::fs::read(out.join("result/deeper/x")).unwrap(), b"x");
        assert!(std::fs::read(out.join("result/empty")).unwrap().is_empty());
        assert!(std::fs::symlink_metadata(out.join("link")).is_err());
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str| std::fs::metadata(out.join(p)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode("result/holes") & 0o7000, 0, "no set-user-ID");
        assert_eq!(
            mode("result/deeper/x") & 0o002,
            0,
            "nothing writable by others"
        );
        // An existing empty directory is accepted; a second unpack into a full one is refused.
        let again = s.0.join("again");
        std::fs::create_dir(&again).unwrap();
        unpack(&disk, &again).unwrap();
        let e = unpack(&disk, &out).unwrap_err();
        assert!(e.contains("not empty"), "{e}");
    }

    #[test]
    fn a_disk_without_a_layer_or_a_bad_target_is_refused_and_leaves_nothing() {
        let s = Scratch::new("refuse");
        let blank = s.0.join("blank.disk");
        File::create(&blank).unwrap().set_len(1 << 20).unwrap();
        let out = s.0.join("o");
        let e = unpack(&blank, &out).unwrap_err();
        assert!(e.contains("holds no layer"), "{e}");
        assert!(!out.exists(), "nothing made before the layer is known");
        assert!(unpack(&s.0.join("absent"), &out).is_err());
        let tree = dir(vec![("a", file(0o644, b"abc"))]);
        let good = disk(&s, &tree, 1 << 20);
        let not_dir = s.0.join("plain");
        std::fs::write(&not_dir, b"x").unwrap();
        assert!(
            unpack(&good, &not_dir)
                .unwrap_err()
                .contains("not a directory")
        );
        let link = s.0.join("link");
        std::os::unix::fs::symlink(&s.0, &link).unwrap();
        assert!(
            unpack(&good, &link).is_err(),
            "never through a symbolic link"
        );
        assert!(unpack(&good, &s.0.join("no/such/parent")).is_err());
    }

    #[test]
    fn a_layer_that_is_tampered_with_or_too_large_is_refused() {
        let s = Scratch::new("tamper");
        let body = vec![5u8; 2 * PAGE];
        let tree = dir(vec![("f", file(0o644, &body))]);
        let disk_path = disk(&s, &tree, 1 << 20);
        // One byte of the file's data changed: its page no longer matches its hash.
        let bytes = std::fs::read(&disk_path).unwrap();
        let at = bytes
            .windows(PAGE)
            .position(|w| w.iter().all(|b| *b == 5))
            .unwrap();
        let f = OpenOptions::new().write(true).open(&disk_path).unwrap();
        f.write_all_at(&[6], at as u64).unwrap();
        let out = s.0.join("o");
        let e = unpack(&disk_path, &out).unwrap_err();
        assert!(e.contains("cannot be read"), "{e}");
        assert!(!out.exists(), "a failed unpack leaves nothing");
        // Files larger than the disk that held them.
        let s2 = Scratch::new("large");
        let mut holes = vec![0u8; 64 * PAGE];
        holes[0] = 1;
        let tree = dir(vec![("f", file(0o644, &holes))]);
        let disk_path = disk(&s2, &tree, 0);
        let kept = s2.0.join("kept");
        std::fs::create_dir(&kept).unwrap();
        let e = unpack(&disk_path, &kept).unwrap_err();
        assert!(e.contains("larger than its disk"), "{e}");
        assert!(kept.exists() && std::fs::read_dir(&kept).unwrap().next().is_none());
    }

    #[test]
    fn the_reader_device_refuses_what_is_not_its_own() {
        let s = Scratch::new("device");
        let path = s.0.join("d");
        std::fs::write(&path, vec![1u8; PAGE]).unwrap();
        let mut d = DiskFile {
            file: File::open(&path).unwrap(),
            pages: 1,
            slots: vec![[0u8; PAGE]; 2],
        };
        let at = |layer, page, slot| Fetch { layer, page, slot };
        assert!(d.fetch(&[at(1, 0, 0)]).is_err());
        assert!(d.fetch(&[at(0, 1, 0)]).is_err());
        assert!(d.fetch(&[at(0, 0, 2)]).is_err());
        d.fetch(&[at(0, 0, 1)]).unwrap();
        let mut out = [0u8; 4];
        d.copy(1, PAGE - 4, &mut out).unwrap();
        assert_eq!(out, [1; 4]);
        assert!(d.copy(1, PAGE - 3, &mut out).is_err());
        assert!(d.copy(2, 0, &mut out).is_err());
        assert!(d.device_pages(1).is_err());
        assert_eq!(d.device_pages(0).unwrap(), 1);
    }
}
