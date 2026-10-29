//! The scratch directory (SDD 27, f.7): a task's only writable path, `/tmp`, held in
//! the server's memory, under a byte quota. Every file's bytes count against it, and
//! every entry a fixed amount, so neither large files nor many small ones pass it. A
//! file removed while open lives until its last close, as on Linux.

use alloc::{collections::BTreeMap, vec::Vec};

use crate::error::{Error, Result};

/// What an entry costs against the quota besides its bytes.
pub const ENTRY_BYTES: u64 = 256;
/// The quota a scratch directory has until its task sets one: SDD 19's `TMP_BYTES`.
pub const DEFAULT_QUOTA: u64 = 64 << 20;
/// The largest quota a task may set.
pub const QUOTA_MAX: u64 = 1 << 30;

/// An entry's number in its scratch directory; 0 is `/tmp` itself.
pub type Id = u32;

enum Body {
    Directory(BTreeMap<Vec<u8>, Id>),
    File(Vec<u8>),
}

struct Entry {
    mode: u32,
    body: Body,
    /// Whether a directory still names it.
    linked: bool,
    /// Handles open on it.
    opens: u32,
}

/// What `stat` says of a scratch entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub directory: bool,
    pub mode: u32,
    pub size: u64,
}

pub struct Scratch {
    entries: Vec<Option<Entry>>,
    free: Vec<Id>,
    used: u64,
    quota: u64,
    /// Whether anything has been written, after which the quota is fixed.
    touched: bool,
}

impl Default for Scratch {
    fn default() -> Scratch {
        Scratch::new()
    }
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch {
            entries: alloc::vec![Some(Entry {
                mode: 0o777,
                body: Body::Directory(BTreeMap::new()),
                linked: true,
                opens: 0,
            })],
            free: Vec::new(),
            used: 0,
            quota: DEFAULT_QUOTA,
            touched: false,
        }
    }

    /// Bytes counted against the quota.
    pub fn used(&self) -> u64 {
        self.used
    }

    pub fn quota(&self) -> u64 {
        self.quota
    }

    /// Sets the quota, once, before anything is made.
    pub fn set_quota(&mut self, bytes: u64) -> Result<()> {
        if self.touched {
            return Err(Error::Unsupported);
        }
        if bytes > QUOTA_MAX {
            return Err(Error::TooLarge);
        }
        self.quota = bytes;
        Ok(())
    }

    fn entry(&self, id: Id) -> Result<&Entry> {
        self.entries
            .get(id as usize)
            .and_then(Option::as_ref)
            .ok_or(Error::NotFound)
    }

    fn entry_mut(&mut self, id: Id) -> Result<&mut Entry> {
        self.entries
            .get_mut(id as usize)
            .and_then(Option::as_mut)
            .ok_or(Error::NotFound)
    }

    fn charge(&mut self, bytes: u64) -> Result<()> {
        let used = self.used.checked_add(bytes).ok_or(Error::NoSpace)?;
        if used > self.quota {
            return Err(Error::NoSpace);
        }
        self.used = used;
        self.touched = true;
        Ok(())
    }

    /// The entry `name` in directory `dir`.
    pub fn lookup(&self, dir: Id, name: &[u8]) -> Result<Id> {
        match &self.entry(dir)?.body {
            Body::Directory(children) => children.get(name).copied().ok_or(Error::NotFound),
            Body::File(_) => Err(Error::NotDirectory),
        }
    }

    fn make(&mut self, dir: Id, name: &[u8], mode: u32, body: Body) -> Result<Id> {
        if !crate::format::valid_name(name) {
            return Err(Error::BadPath);
        }
        match &self.entry(dir)?.body {
            Body::Directory(children) if children.contains_key(name) => return Err(Error::Exists),
            Body::Directory(_) => {}
            Body::File(_) => return Err(Error::NotDirectory),
        }
        self.charge(ENTRY_BYTES + name.len() as u64)?;
        let entry = Entry {
            mode: mode & 0o777,
            body,
            linked: true,
            opens: 0,
        };
        let id = match self.free.pop() {
            Some(id) => {
                self.entries[id as usize] = Some(entry);
                id
            }
            None => {
                self.entries.push(Some(entry));
                (self.entries.len() - 1) as Id
            }
        };
        if let Body::Directory(children) = &mut self.entry_mut(dir)?.body {
            children.insert(name.to_vec(), id);
        }
        Ok(id)
    }

    /// Makes an empty file `name` in `dir`.
    pub fn create(&mut self, dir: Id, name: &[u8], mode: u32) -> Result<Id> {
        self.make(dir, name, mode, Body::File(Vec::new()))
    }

    /// Makes a directory `name` in `dir`.
    pub fn mkdir(&mut self, dir: Id, name: &[u8], mode: u32) -> Result<Id> {
        self.make(dir, name, mode, Body::Directory(BTreeMap::new()))
    }

    /// Removes `name` from `dir`: a file, or an empty directory. An open file's bytes
    /// stay until its last close.
    pub fn remove(&mut self, dir: Id, name: &[u8]) -> Result<()> {
        let id = self.lookup(dir, name)?;
        if let Body::Directory(children) = &self.entry(id)?.body
            && !children.is_empty()
        {
            return Err(Error::NotEmpty);
        }
        if let Body::Directory(children) = &mut self.entry_mut(dir)?.body {
            children.remove(name);
        }
        self.used = self.used.saturating_sub(ENTRY_BYTES + name.len() as u64);
        self.entry_mut(id)?.linked = false;
        self.release(id);
        Ok(())
    }

    /// Frees an entry nothing names and nothing holds open.
    fn release(&mut self, id: Id) {
        let Some(Some(entry)) = self.entries.get(id as usize) else {
            return;
        };
        if entry.linked || entry.opens > 0 {
            return;
        }
        if let Some(Entry {
            body: Body::File(data),
            ..
        }) = self.entries[id as usize].take()
        {
            self.used = self.used.saturating_sub(data.len() as u64);
        }
        self.free.push(id);
    }

    /// Notes a handle opened on `id`.
    pub fn open(&mut self, id: Id) -> Result<()> {
        let e = self.entry_mut(id)?;
        e.opens = e.opens.checked_add(1).ok_or(Error::TooLarge)?;
        Ok(())
    }

    /// Notes a handle on `id` closed; frees it if it was removed.
    pub fn close(&mut self, id: Id) {
        if let Ok(e) = self.entry_mut(id) {
            e.opens = e.opens.saturating_sub(1);
        }
        self.release(id);
    }

    pub fn info(&self, id: Id) -> Result<Info> {
        let e = self.entry(id)?;
        Ok(match &e.body {
            Body::Directory(_) => Info {
                directory: true,
                mode: e.mode,
                size: 0,
            },
            Body::File(data) => Info {
                directory: false,
                mode: e.mode,
                size: data.len() as u64,
            },
        })
    }

    /// Up to `len` bytes of file `id` from `offset`.
    pub fn read(&self, id: Id, offset: u64, len: usize) -> Result<Vec<u8>> {
        match &self.entry(id)?.body {
            Body::File(data) => {
                let start = (offset.min(data.len() as u64)) as usize;
                let end = start.saturating_add(len).min(data.len());
                Ok(data[start..end].to_vec())
            }
            Body::Directory(_) => Err(Error::IsDirectory),
        }
    }

    /// Writes `bytes` at `offset`, growing the file (with zeros between its end and
    /// `offset`) within the quota.
    pub fn write(&mut self, id: Id, offset: u64, bytes: &[u8]) -> Result<u64> {
        let len = match &self.entry(id)?.body {
            Body::File(data) => data.len() as u64,
            Body::Directory(_) => return Err(Error::IsDirectory),
        };
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Error::BadRange)?;
        if end > len {
            self.grow(id, end)?;
        }
        if let Body::File(data) = &mut self.entry_mut(id)?.body {
            data[offset as usize..end as usize].copy_from_slice(bytes);
        }
        self.touched = true;
        Ok(bytes.len() as u64)
    }

    fn grow(&mut self, id: Id, size: u64) -> Result<()> {
        let len = self.info(id)?.size;
        let size_usize = usize::try_from(size).map_err(|_| Error::NoSpace)?;
        self.charge(size - len)?;
        if let Body::File(data) = &mut self.entry_mut(id)?.body
            && data.try_reserve_exact(size_usize - data.len()).is_ok()
        {
            data.resize(size_usize, 0);
            return Ok(());
        }
        self.used -= size - len;
        Err(Error::NoSpace)
    }

    /// Sets file `id`'s length.
    pub fn truncate(&mut self, id: Id, size: u64) -> Result<()> {
        let info = self.info(id)?;
        if info.directory {
            return Err(Error::IsDirectory);
        }
        if size > info.size {
            return self.grow(id, size);
        }
        if let Body::File(data) = &mut self.entry_mut(id)?.body {
            data.truncate(size as usize);
            data.shrink_to_fit();
        }
        self.used -= info.size - size;
        self.touched = true;
        Ok(())
    }

    /// Directory `id`'s entries, in name order, with whether each is a directory.
    pub fn list(&self, id: Id) -> Result<Vec<(Vec<u8>, Id, bool)>> {
        match &self.entry(id)?.body {
            Body::Directory(children) => Ok(children
                .iter()
                .map(|(name, child)| {
                    let directory = matches!(
                        self.entries[*child as usize].as_ref().map(|e| &e.body),
                        Some(Body::Directory(_))
                    );
                    (name.clone(), *child, directory)
                })
                .collect()),
            Body::File(_) => Err(Error::NotDirectory),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_t11_files_and_directories() {
        let mut s = Scratch::new();
        let d = s.mkdir(0, b"d", 0o755).unwrap();
        let f = s.create(d, b"f", 0o644).unwrap();
        assert_eq!(s.lookup(0, b"d"), Ok(d));
        assert_eq!(s.lookup(d, b"f"), Ok(f));
        assert_eq!(s.lookup(f, b"x"), Err(Error::NotDirectory));
        assert_eq!(s.lookup(d, b"x"), Err(Error::NotFound));
        assert_eq!(s.create(d, b"f", 0), Err(Error::Exists));
        assert_eq!(s.create(f, b"g", 0), Err(Error::NotDirectory));
        assert_eq!(s.create(d, b"..", 0), Err(Error::BadPath));
        assert_eq!(s.write(f, 3, b"abc"), Ok(3));
        assert_eq!(s.read(f, 0, 100).unwrap(), b"\0\0\0abc");
        assert_eq!(s.read(f, 4, 1).unwrap(), b"b");
        assert_eq!(s.read(f, 99, 1).unwrap(), b"");
        assert_eq!(s.read(d, 0, 1), Err(Error::IsDirectory));
        assert_eq!(s.write(d, 0, b"x"), Err(Error::IsDirectory));
        assert_eq!(
            s.info(f).unwrap(),
            Info {
                directory: false,
                mode: 0o644,
                size: 6
            }
        );
        assert!(s.info(d).unwrap().directory);
        assert_eq!(s.list(d).unwrap(), alloc::vec![(b"f".to_vec(), f, false)]);
        assert_eq!(s.list(0).unwrap(), alloc::vec![(b"d".to_vec(), d, true)]);
        assert_eq!(s.list(f), Err(Error::NotDirectory));
        assert_eq!(s.remove(0, b"d"), Err(Error::NotEmpty));
        s.truncate(f, 2).unwrap();
        assert_eq!(s.read(f, 0, 10).unwrap(), b"\0\0");
        s.truncate(f, 5).unwrap();
        assert_eq!(s.info(f).unwrap().size, 5);
        assert_eq!(s.truncate(d, 0), Err(Error::IsDirectory));
        s.remove(d, b"f").unwrap();
        s.remove(0, b"d").unwrap();
        assert_eq!(s.used(), 0);
        assert_eq!(s.info(f), Err(Error::NotFound));
        assert_eq!(s.write(5, 0, b""), Err(Error::NotFound));
    }

    #[test]
    fn if_t11_the_quota_is_held() {
        let mut s = Scratch::new();
        assert_eq!(s.quota(), DEFAULT_QUOTA);
        s.set_quota(4096).unwrap();
        let f = s.create(0, b"f", 0o600).unwrap();
        assert_eq!(s.used(), ENTRY_BYTES + 1);
        assert_eq!(
            s.set_quota(1 << 20),
            Err(Error::Unsupported),
            "fixed once used"
        );
        let room = 4096 - s.used();
        s.write(f, 0, &alloc::vec![1u8; room as usize]).unwrap();
        assert_eq!(s.used(), 4096);
        assert_eq!(s.write(f, room, b"x"), Err(Error::NoSpace));
        assert_eq!(s.truncate(f, room + 1), Err(Error::NoSpace));
        assert_eq!(s.create(0, b"g", 0), Err(Error::NoSpace));
        assert_eq!(
            s.info(f).unwrap().size,
            room,
            "a refused write changes nothing"
        );
        assert_eq!(s.write(f, u64::MAX, b"x"), Err(Error::BadRange));
        assert_eq!(s.write(f, 1 << 40, b"x"), Err(Error::NoSpace));
        s.truncate(f, 0).unwrap();
        assert_eq!(s.used(), ENTRY_BYTES + 1);
        let mut t = Scratch::default();
        assert_eq!(t.set_quota(QUOTA_MAX + 1), Err(Error::TooLarge));
    }

    #[test]
    fn if_t11_a_removed_open_file_lives_until_closed() {
        let mut s = Scratch::new();
        s.set_quota(10_000).unwrap();
        let f = s.create(0, b"f", 0o600).unwrap();
        s.write(f, 0, &[7; 1000]).unwrap();
        s.open(f).unwrap();
        s.remove(0, b"f").unwrap();
        assert_eq!(s.lookup(0, b"f"), Err(Error::NotFound));
        assert_eq!(s.read(f, 0, 2).unwrap(), [7, 7]);
        assert_eq!(s.used(), 1000);
        s.close(f);
        assert_eq!(s.used(), 0);
        assert_eq!(s.info(f), Err(Error::NotFound));
        let g = s.create(0, b"g", 0o600).unwrap();
        assert_eq!(g, f, "the number is used again");
        s.close(99);
        assert_eq!(s.open(99), Err(Error::NotFound));
        assert_eq!(s.remove(0, b"nope"), Err(Error::NotFound));
    }
}
