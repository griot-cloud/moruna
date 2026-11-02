//! A task-local mount table. The assets endpoint is an actual imagefs readonly
//! directory capability, never a source path interpreted by this process.
use alloc::{collections::BTreeMap, vec::Vec};
use imagefs_protocol::{Bytes, Dirent, Stat, imagefs, kind, open};
use moruna_svc::{CallError, Handle, Memory};

type Result<T> = core::result::Result<T, CallError<imagefs::Error>>;
fn refusal<T>(e: imagefs::Error) -> Result<T> {
    Err(CallError::Remote(e))
}
struct File {
    assets: bool,
    remote: u32,
    root: bool,
    readonly_mapping: Option<ReadonlyMapping>,
}

/// Immutable image bytes retained for this exact open file, not a reusable fd.
struct ReadonlyMapping {
    memory: Memory,
    offset: u64,
    len: u64,
}

impl ReadonlyMapping {
    fn contains(&self, offset: u64, len: u64) -> bool {
        offset >= self.offset
            && offset
                .checked_add(len)
                .zip(self.offset.checked_add(self.len))
                .is_some_and(|(end, mapped_end)| end <= mapped_end)
    }
}
pub struct Namespace {
    root: imagefs::Client,
    assets: Option<imagefs::Client>,
    files: BTreeMap<u32, File>,
    next: u32,
}
impl Namespace {
    pub fn new(root: imagefs::Client, assets: Option<imagefs::Client>) -> Self {
        Self {
            root,
            assets,
            files: BTreeMap::new(),
            next: 1,
        }
    }
    fn client(&mut self, assets: bool) -> Result<&mut imagefs::Client> {
        if assets {
            self.assets
                .as_mut()
                .ok_or(CallError::Remote(imagefs::Error::NotFound))
        } else {
            Ok(&mut self.root)
        }
    }
    fn route(&self, path: &[u8]) -> (bool, Vec<u8>) {
        if self.assets.is_some() && (path == b"/assets" || path.starts_with(b"/assets/")) {
            (
                true,
                if path.len() == 7 {
                    b"/".to_vec()
                } else {
                    path[7..].to_vec()
                },
            )
        } else {
            (false, path.to_vec())
        }
    }
    // Resolve components before choosing the final endpoint. Root-image links may
    // enter the mount. Links inside a capability stay inside it, including absolute
    // targets; '..' cannot turn a source capability into ambient root authority.
    fn resolve(&mut self, path: &[u8], follow: bool, missing: bool) -> Result<Vec<u8>> {
        if path.first() != Some(&b'/') || path.len() > 4096 || path.contains(&0) {
            return refusal(imagefs::Error::BadPath);
        }
        let mut todo: Vec<Vec<u8>> = path
            .split(|b| *b == b'/')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_vec())
            .collect();
        let mut done: Vec<Vec<u8>> = Vec::new();
        let mut links = 0;
        while !todo.is_empty() {
            let part = todo.remove(0);
            if part == b"." {
                continue;
            }
            if part == b".." {
                if self.assets.is_some() && done.len() == 1 && done[0] == b"assets" {
                    return refusal(imagefs::Error::BadPath);
                }
                done.pop();
                continue;
            }
            done.push(part);
            let current = absolute(&done);
            let (assets, remote) = self.route(&current);
            let stat = match self.client(assets)?.stat(Bytes(remote.clone()), false) {
                Err(CallError::Remote(imagefs::Error::NotFound)) if missing && todo.is_empty() => {
                    return Ok(current);
                }
                other => other?,
            };
            if stat.kind == kind::SYMLINK && (follow || !todo.is_empty()) {
                links += 1;
                if links > 40 {
                    return refusal(imagefs::Error::Loop);
                }
                let Bytes(target) = self.client(assets)?.readlink(Bytes(remote))?;
                if target.is_empty() || target.contains(&0) {
                    return refusal(imagefs::Error::BadPath);
                }
                done.pop();
                if target[0] == b'/' {
                    done.clear();
                    if assets {
                        done.push(b"assets".to_vec());
                    }
                }
                let mut next: Vec<Vec<u8>> = target
                    .split(|b| *b == b'/')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_vec())
                    .collect();
                next.extend(todo);
                todo = next;
            } else if !todo.is_empty() && stat.kind != kind::DIRECTORY {
                return refusal(imagefs::Error::NotDirectory);
            }
            if absolute(&done).len() + todo.iter().map(|p| p.len() + 1).sum::<usize>() > 4096 {
                return refusal(imagefs::Error::BadPath);
            }
        }
        Ok(absolute(&done))
    }
    pub fn open(&mut self, path: Bytes, flags: u32, mode: u32) -> Result<(u32, Stat)> {
        let path = self.resolve(
            &path.0,
            flags & open::NOFOLLOW == 0,
            flags & open::CREATE != 0,
        )?;
        let (assets, remote) = self.route(&path);
        if assets && flags & (open::WRITE | open::CREATE | open::TRUNCATE) != 0 {
            return refusal(imagefs::Error::ReadOnly);
        }
        if self.files.len() >= imagefs_protocol::FILES_MAX {
            return refusal(imagefs::Error::TooManyFiles);
        }
        let next = self
            .next
            .checked_add(1)
            .ok_or(CallError::Remote(imagefs::Error::TooManyFiles))?;
        let (remote, stat) =
            self.client(assets)?
                .open(Bytes(remote), flags | open::NOFOLLOW, mode)?;
        let local = self.next;
        self.next = next;
        self.files.insert(
            local,
            File {
                assets,
                remote,
                root: path == b"/",
                readonly_mapping: None,
            },
        );
        Ok((local, stat))
    }
    pub fn stat(&mut self, path: Bytes, follow: bool) -> Result<Stat> {
        let path = self.resolve(&path.0, follow, false)?;
        let (assets, remote) = self.route(&path);
        self.client(assets)?.stat(Bytes(remote), false)
    }
    pub fn readlink(&mut self, path: Bytes) -> Result<Bytes> {
        let path = self.resolve(&path.0, false, false)?;
        let (assets, remote) = self.route(&path);
        self.client(assets)?.readlink(Bytes(remote))
    }
    fn file(&self, n: u32) -> Result<(bool, u32)> {
        self.files
            .get(&n)
            .map(|f| (f.assets, f.remote))
            .ok_or(CallError::Remote(imagefs::Error::BadFile))
    }
    pub fn is_assets(&self, n: u32) -> bool {
        self.files.get(&n).is_some_and(|f| f.assets)
    }
    pub fn fstat(&mut self, n: u32) -> Result<Stat> {
        let (a, n) = self.file(n)?;
        self.client(a)?.fstat(n)
    }
    pub fn read(&mut self, n: u32, at: u64, len: u32) -> Result<Bytes> {
        let (a, n) = self.file(n)?;
        self.client(a)?.read(n, at, len)
    }
    /// A readonly view of immutable file bytes. A contained range shares the
    /// existing backing; the caller must pass the returned memory offset to map.
    /// Writable MAP_PRIVATE mappings use the separate read-and-copy path.
    pub fn map_readonly(&mut self, n: u32, at: u64, len: u64) -> Result<(&Memory, u64)> {
        let (assets, remote) = self.file(n)?;
        if len == 0 || !at.is_multiple_of(4096) || at.checked_add(len).is_none() {
            return refusal(imagefs::Error::BadRange);
        }
        let contained = self
            .files
            .get(&n)
            .and_then(|file| file.readonly_mapping.as_ref())
            .is_some_and(|mapping| mapping.contains(at, len));
        if !contained {
            let memory = self.client(assets)?.map(remote, at, len)?;
            let file = self
                .files
                .get_mut(&n)
                .ok_or(CallError::Remote(imagefs::Error::BadFile))?;
            // Active process mappings independently retain their old backing.
            // Replacing this one file-local handle cannot retire those mappings.
            file.readonly_mapping = Some(ReadonlyMapping {
                memory,
                offset: at,
                len,
            });
        }
        let mapping = self
            .files
            .get(&n)
            .and_then(|file| file.readonly_mapping.as_ref())
            .ok_or(CallError::Remote(imagefs::Error::BadFile))?;
        Ok((&mapping.memory, at - mapping.offset))
    }
    pub fn write(&mut self, n: u32, at: u64, data: Bytes) -> Result<u32> {
        let (a, n) = self.file(n)?;
        if a {
            return refusal(imagefs::Error::ReadOnly);
        }
        self.root.write(n, at, data)
    }
    pub fn truncate(&mut self, n: u32, len: u64) -> Result<()> {
        let (a, n) = self.file(n)?;
        if a {
            return refusal(imagefs::Error::ReadOnly);
        }
        self.root.truncate(n, len)
    }
    pub fn close(&mut self, n: u32) -> Result<()> {
        let (a, remote) = self.file(n)?;
        self.client(a)?.close(remote)?;
        self.files.remove(&n);
        Ok(())
    }
    pub fn readdir(&mut self, n: u32, cookie: u32) -> Result<(Vec<Dirent>, u32)> {
        let (a, remote) = self.file(n)?;
        if self.assets.is_none() || !self.files.get(&n).is_some_and(|f| f.root) {
            return self.client(a)?.readdir(remote, cookie);
        }
        // A root listing is bounded by the same service directory bound. Rebuild
        // its stable immutable entries, hide a shadowed name, add the real mount.
        let mut all = Vec::new();
        let mut cursor = 0;
        loop {
            let (batch, next) = self.root.readdir(remote, cursor)?;
            all.extend(batch.into_iter().filter(|e| e.0.0 != b"assets"));
            if next == cursor || next == 0 {
                break;
            }
            cursor = next;
            if all.len() > 4096 {
                return refusal(imagefs::Error::TooLarge);
            }
        }
        let stat = self.client(true)?.stat(Bytes(b"/".to_vec()), false)?;
        all.push((Bytes(b"assets".to_vec()), kind::DIRECTORY, stat.ino));
        all.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        let start = cookie as usize;
        if start >= all.len() {
            return Ok((Vec::new(), cookie));
        }
        let end = (start + 128).min(all.len());
        Ok((all[start..end].to_vec(), end as u32))
    }
    fn writable(&mut self, path: Bytes, missing: bool) -> Result<Bytes> {
        let p = self.resolve(&path.0, false, missing)?;
        if self.route(&p).0 {
            return refusal(imagefs::Error::ReadOnly);
        }
        Ok(Bytes(p))
    }
    pub fn mkdir(&mut self, p: Bytes, mode: u32) -> Result<()> {
        let p = self.writable(p, true)?;
        self.root.mkdir(p, mode)
    }
    pub fn remove(&mut self, p: Bytes) -> Result<()> {
        let p = self.writable(p, false)?;
        self.root.remove(p)
    }
    pub fn rename(&mut self, from: Bytes, to: Bytes) -> Result<()> {
        let from = self.writable(from, false)?;
        let to = self.writable(to, true)?;
        self.root.rename(from, to)
    }
    pub fn scratch_account(&mut self, a: Handle) -> Result<()> {
        self.root.scratch_account(a)
    }
    pub fn export_directory(&mut self, p: Bytes) -> Result<moruna_svc::Channel> {
        let p = self.resolve(&p.0, true, false)?;
        let (a, p) = self.route(&p);
        self.client(a)?.export_directory(Bytes(p))
    }
}
fn absolute(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::from(&b"/"[..]);
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push(b'/')
        }
        out.extend(p)
    }
    out
}
