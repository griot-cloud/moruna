//! The scratch directory (SDD 27, f.7): a task's only writable path, `/tmp`. Its files'
//! bytes are kept by a [`Store`], which meters them: on the kernel, memory objects
//! charged to the directory's own account, which the kernel holds it to (SDD 20 f.18);
//! on a host, the server's memory under a byte limit ([`Heap`]). Its entries are the
//! server's own memory, at most [`ENTRIES_MAX`] of them, so neither large files nor many
//! small ones pass what the directory was given. A file removed while open lives until
//! its last close, as on Linux, and a rename moves an entry with Linux's `rename` meaning
//! (imagefs 1.1, SDD 30).

use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};

use crate::error::{Error, Result};

/// The most entries a scratch directory holds, `/tmp` itself included.
pub const ENTRIES_MAX: usize = 4096;
/// The bytes a scratch directory may hold when its task's starter gave it no account of
/// its own: SDD 19's `TMP_BYTES`.
pub const DEFAULT_QUOTA: u64 = 64 << 20;

/// An entry's number in its scratch directory; 0 is `/tmp` itself.
pub type Id = u32;

/// Where a scratch directory keeps its files' bytes, by entry number, and what meters
/// them: a file that cannot grow is `NoSpace`, and nothing about it changes.
pub trait Store {
    /// Sets file `id`'s length to `size`: zeros past its old end, its bytes past `size`
    /// gone. A file the store has not seen has length zero.
    fn resize(&mut self, id: Id, size: u64) -> Result<()>;
    /// `buf.len()` bytes of file `id` from `offset`, which lie within its length.
    fn read(&self, id: Id, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// `bytes` at `offset` of file `id`, which lie within its length.
    fn write(&mut self, id: Id, offset: u64, bytes: &[u8]) -> Result<()>;
    /// File `id` is gone: its bytes, and what metered them, go back.
    fn free(&mut self, id: Id);
}

/// Bytes in the server's memory under a limit: a host's store, and the tests'.
pub struct Heap {
    files: BTreeMap<Id, Vec<u8>>,
    limit: u64,
    used: u64,
}

impl Heap {
    pub fn new(limit: u64) -> Heap {
        Heap {
            files: BTreeMap::new(),
            limit,
            used: 0,
        }
    }

    /// The bytes held now.
    pub fn used(&self) -> u64 {
        self.used
    }
}

impl Store for Heap {
    fn resize(&mut self, id: Id, size: u64) -> Result<()> {
        let have = self.files.get(&id).map_or(0, |f| f.len() as u64);
        let used = (self.used - have)
            .checked_add(size)
            .filter(|u| *u <= self.limit)
            .ok_or(Error::NoSpace)?;
        let size = usize::try_from(size).map_err(|_| Error::NoSpace)?;
        let file = self.files.entry(id).or_default();
        if size > file.len() && file.try_reserve_exact(size - file.len()).is_err() {
            return Err(Error::NoSpace);
        }
        file.resize(size, 0);
        file.shrink_to_fit();
        self.used = used;
        Ok(())
    }

    fn read(&self, id: Id, offset: u64, buf: &mut [u8]) -> Result<()> {
        let file = self.files.get(&id).map_or(&[][..], |f| &f[..]);
        let at = usize::try_from(offset).map_err(|_| Error::BadRange)?;
        let src = file.get(at..at + buf.len()).ok_or(Error::BadRange)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write(&mut self, id: Id, offset: u64, bytes: &[u8]) -> Result<()> {
        let file = self.files.get_mut(&id).ok_or(Error::BadRange)?;
        let at = usize::try_from(offset).map_err(|_| Error::BadRange)?;
        let dst = file.get_mut(at..at + bytes.len()).ok_or(Error::BadRange)?;
        dst.copy_from_slice(bytes);
        Ok(())
    }

    fn free(&mut self, id: Id) {
        if let Some(file) = self.files.remove(&id) {
            self.used -= file.len() as u64;
        }
    }
}

enum Body {
    Directory(BTreeMap<Vec<u8>, Id>),
    /// A file of this many bytes, which the store keeps.
    File(u64),
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
    store: Box<dyn Store>,
    /// Whether anything has been made or written, after which the store is fixed.
    touched: bool,
}

impl Default for Scratch {
    fn default() -> Scratch {
        Scratch::new()
    }
}

impl Scratch {
    /// A scratch directory whose bytes are in the server's memory, at most
    /// [`DEFAULT_QUOTA`] of them.
    pub fn new() -> Scratch {
        Scratch::with_store(Box::new(Heap::new(DEFAULT_QUOTA)))
    }

    /// A scratch directory whose bytes `store` keeps.
    pub fn with_store(store: Box<dyn Store>) -> Scratch {
        Scratch {
            entries: alloc::vec![Some(Entry {
                mode: 0o777,
                body: Body::Directory(BTreeMap::new()),
                linked: true,
                opens: 0,
            })],
            free: Vec::new(),
            store,
            touched: false,
        }
    }

    /// Gives the directory another store, before anything is made or written in it
    /// (`Unsupported` after).
    pub fn set_store(&mut self, store: Box<dyn Store>) -> Result<()> {
        if self.touched {
            return Err(Error::Unsupported);
        }
        self.store = store;
        Ok(())
    }

    /// The entries it holds, `/tmp` itself included.
    pub fn entries(&self) -> usize {
        self.entries.len() - self.free.len()
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
        if self.entries() >= ENTRIES_MAX {
            return Err(Error::NoSpace);
        }
        self.touched = true;
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
        self.make(dir, name, mode, Body::File(0))
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
        self.entry_mut(id)?.linked = false;
        self.release(id);
        Ok(())
    }

    /// Moves `from_name` of `from_dir` to `to_name` of `to_dir`, as Linux's `rename`: a
    /// file replaces a file there, a directory an empty directory; a file over a
    /// directory is `IsDirectory`, a directory over a file `NotDirectory`, a directory
    /// over one that is not empty `NotEmpty`; a directory cannot move into itself or
    /// below itself (`BadPath`). Renaming an entry to itself changes nothing. An entry
    /// replaced while open lives until its last close.
    pub fn rename(
        &mut self,
        from_dir: Id,
        from_name: &[u8],
        to_dir: Id,
        to_name: &[u8],
    ) -> Result<()> {
        if !crate::format::valid_name(to_name) {
            return Err(Error::BadPath);
        }
        let id = self.lookup(from_dir, from_name)?;
        if !matches!(self.entry(to_dir)?.body, Body::Directory(_)) {
            return Err(Error::NotDirectory);
        }
        let moving_dir = matches!(self.entry(id)?.body, Body::Directory(_));
        if moving_dir && self.is_within(to_dir, id) {
            return Err(Error::BadPath);
        }
        let replaced = match self.lookup(to_dir, to_name) {
            Ok(existing) if existing == id => return Ok(()),
            Ok(existing) => {
                match (&self.entry(existing)?.body, moving_dir) {
                    (Body::Directory(_), false) => return Err(Error::IsDirectory),
                    (Body::File(_), true) => return Err(Error::NotDirectory),
                    (Body::Directory(children), true) if !children.is_empty() => {
                        return Err(Error::NotEmpty);
                    }
                    _ => {}
                }
                Some(existing)
            }
            Err(Error::NotFound) => None,
            Err(e) => return Err(e),
        };
        if let Body::Directory(children) = &mut self.entry_mut(from_dir)?.body {
            children.remove(from_name);
        }
        if let Some(old) = replaced {
            self.entry_mut(old)?.linked = false;
            self.release(old);
        }
        if let Body::Directory(children) = &mut self.entry_mut(to_dir)?.body {
            children.insert(to_name.to_vec(), id);
        }
        self.touched = true;
        Ok(())
    }

    /// Whether `dir` is `ancestor` or below it.
    fn is_within(&self, dir: Id, ancestor: Id) -> bool {
        let mut stack = alloc::vec![ancestor];
        while let Some(at) = stack.pop() {
            if at == dir {
                return true;
            }
            if let Ok(Entry {
                body: Body::Directory(children),
                ..
            }) = self.entry(at)
            {
                stack.extend(children.values().copied());
            }
        }
        false
    }

    /// Frees an entry nothing names and nothing holds open, and its bytes.
    fn release(&mut self, id: Id) {
        let Some(Some(entry)) = self.entries.get(id as usize) else {
            return;
        };
        if entry.linked || entry.opens > 0 {
            return;
        }
        if let Some(Entry {
            body: Body::File(_),
            ..
        }) = self.entries[id as usize].take()
        {
            self.store.free(id);
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
            Body::File(size) => Info {
                directory: false,
                mode: e.mode,
                size: *size,
            },
        })
    }

    /// The length of file `id`; `IsDirectory` for a directory.
    fn size(&self, id: Id) -> Result<u64> {
        match &self.entry(id)?.body {
            Body::File(size) => Ok(*size),
            Body::Directory(_) => Err(Error::IsDirectory),
        }
    }

    /// Up to `len` bytes of file `id` from `offset`.
    pub fn read(&self, id: Id, offset: u64, len: usize) -> Result<Vec<u8>> {
        let size = self.size(id)?;
        let start = offset.min(size);
        let end = start.saturating_add(len as u64).min(size);
        let mut out = alloc::vec![0u8; (end - start) as usize];
        self.store.read(id, start, &mut out)?;
        Ok(out)
    }

    /// Writes `bytes` at `offset`, growing the file (with zeros between its end and
    /// `offset`) as far as the store has room.
    pub fn write(&mut self, id: Id, offset: u64, bytes: &[u8]) -> Result<u64> {
        let size = self.size(id)?;
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Error::BadRange)?;
        if end > size {
            self.set_size(id, end)?;
        }
        self.store.write(id, offset, bytes)?;
        self.touched = true;
        Ok(bytes.len() as u64)
    }

    fn set_size(&mut self, id: Id, size: u64) -> Result<()> {
        self.store.resize(id, size)?;
        if let Body::File(s) = &mut self.entry_mut(id)?.body {
            *s = size;
        }
        self.touched = true;
        Ok(())
    }

    /// Sets file `id`'s length.
    pub fn truncate(&mut self, id: Id, size: u64) -> Result<()> {
        self.size(id)?;
        self.set_size(id, size)
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
    use alloc::rc::Rc;
    use core::cell::Cell;

    use super::*;

    /// A [`Heap`] whose bytes held the test can see after the scratch directory owns it.
    struct Metered {
        heap: Heap,
        used: Rc<Cell<u64>>,
    }

    impl Store for Metered {
        fn resize(&mut self, id: Id, size: u64) -> Result<()> {
            let r = self.heap.resize(id, size);
            self.used.set(self.heap.used());
            r
        }
        fn read(&self, id: Id, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.heap.read(id, offset, buf)
        }
        fn write(&mut self, id: Id, offset: u64, bytes: &[u8]) -> Result<()> {
            self.heap.write(id, offset, bytes)
        }
        fn free(&mut self, id: Id) {
            self.heap.free(id);
            self.used.set(self.heap.used());
        }
    }

    fn metered(limit: u64) -> (Scratch, Rc<Cell<u64>>) {
        let used = Rc::new(Cell::new(0));
        let store = Metered {
            heap: Heap::new(limit),
            used: used.clone(),
        };
        (Scratch::with_store(Box::new(store)), used)
    }

    #[test]
    fn if_t11_files_and_directories() {
        let (mut s, used) = metered(DEFAULT_QUOTA);
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
        assert_eq!(used.get(), 6);
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
        assert_eq!(s.entries(), 3);
        s.remove(d, b"f").unwrap();
        s.remove(0, b"d").unwrap();
        assert_eq!(used.get(), 0, "every byte went back");
        assert_eq!(s.entries(), 1);
        assert_eq!(s.info(f), Err(Error::NotFound));
        assert_eq!(s.write(5, 0, b""), Err(Error::NotFound));
    }

    #[test]
    fn if_t11_the_store_meters_the_bytes() {
        let (mut s, used) = metered(4096);
        let f = s.create(0, b"f", 0o600).unwrap();
        s.write(f, 0, &alloc::vec![1u8; 4096]).unwrap();
        assert_eq!(used.get(), 4096);
        assert_eq!(s.write(f, 4096, b"x"), Err(Error::NoSpace));
        assert_eq!(s.truncate(f, 4097), Err(Error::NoSpace));
        assert_eq!(
            s.info(f).unwrap().size,
            4096,
            "a refused write changes nothing"
        );
        assert_eq!(s.read(f, 4090, 10).unwrap(), [1; 6]);
        assert_eq!(s.write(f, u64::MAX, b"x"), Err(Error::BadRange));
        assert_eq!(s.write(f, 1 << 40, b"x"), Err(Error::NoSpace));
        // Entries are bounded by count, not by the store.
        s.create(0, b"g", 0).unwrap();
        s.truncate(f, 0).unwrap();
        assert_eq!(used.get(), 0);
        // The store is fixed once anything is made.
        assert_eq!(
            s.set_store(Box::new(Heap::new(1 << 20))),
            Err(Error::Unsupported)
        );
        let mut fresh = Scratch::new();
        fresh.set_store(Box::new(Heap::new(8))).unwrap();
        let g = fresh.create(0, b"g", 0).unwrap();
        assert_eq!(fresh.write(g, 0, &[0; 9]), Err(Error::NoSpace));
        // At most ENTRIES_MAX entries, `/tmp` itself one of them.
        let mut t = Scratch::new();
        for i in 1..ENTRIES_MAX {
            t.create(0, alloc::format!("f{i}").as_bytes(), 0).unwrap();
        }
        assert_eq!(t.entries(), ENTRIES_MAX);
        assert_eq!(t.create(0, b"one-more", 0), Err(Error::NoSpace));
        assert_eq!(t.mkdir(0, b"one-more", 0), Err(Error::NoSpace));
        t.remove(0, b"f1").unwrap();
        t.mkdir(0, b"one-more", 0).unwrap();
    }

    #[test]
    fn if_t20_rename_moves_and_replaces_as_linux_does() {
        let (mut s, used) = metered(DEFAULT_QUOTA);
        let d = s.mkdir(0, b"d", 0o755).unwrap();
        let f = s.create(d, b"part.tmp", 0o644).unwrap();
        s.write(f, 0, b"rows").unwrap();
        // A file to a new name in another directory.
        s.rename(d, b"part.tmp", 0, b"part").unwrap();
        assert_eq!(s.lookup(0, b"part"), Ok(f));
        assert_eq!(s.lookup(d, b"part.tmp"), Err(Error::NotFound));
        assert_eq!(s.read(f, 0, 10).unwrap(), b"rows");
        // Over a file: the old one goes, with its bytes and its entry.
        let g = s.create(0, b"g", 0o644).unwrap();
        s.write(g, 0, &[1; 100]).unwrap();
        assert_eq!(used.get(), 104);
        s.rename(0, b"g", 0, b"part").unwrap();
        assert_eq!(s.lookup(0, b"part"), Ok(g));
        assert_eq!(
            s.info(f),
            Err(Error::NotFound),
            "the replaced file is freed"
        );
        assert_eq!(used.get(), 100, "with its bytes");
        // To itself: nothing changes.
        s.rename(0, b"part", 0, b"part").unwrap();
        assert_eq!(s.lookup(0, b"part"), Ok(g));
        // A file over a directory, a directory over a file, over a full directory,
        // into itself, a bad name, a missing source, a file as the target directory.
        let e = s.mkdir(0, b"e", 0o755).unwrap();
        s.create(e, b"x", 0o644).unwrap();
        assert_eq!(s.rename(0, b"part", 0, b"d"), Err(Error::IsDirectory));
        assert_eq!(s.rename(0, b"d", 0, b"part"), Err(Error::NotDirectory));
        assert_eq!(s.rename(0, b"d", 0, b"e"), Err(Error::NotEmpty));
        assert_eq!(s.rename(0, b"d", d, b"inner"), Err(Error::BadPath));
        assert_eq!(s.rename(0, b"e", e, b"deeper"), Err(Error::BadPath));
        assert_eq!(s.rename(0, b"part", 0, b".."), Err(Error::BadPath));
        assert_eq!(s.rename(0, b"nope", 0, b"x"), Err(Error::NotFound));
        assert_eq!(s.rename(0, b"part", g, b"x"), Err(Error::NotDirectory));
        // A directory over an empty directory; then it moves with its contents.
        s.remove(e, b"x").unwrap();
        s.rename(0, b"d", 0, b"e").unwrap();
        assert_eq!(s.lookup(0, b"e"), Ok(d));
        assert_eq!(s.lookup(0, b"d"), Err(Error::NotFound));
        // An open file replaced lives until its last close.
        let h = s.create(0, b"h", 0o644).unwrap();
        s.write(h, 0, b"held").unwrap();
        s.open(g).unwrap();
        s.rename(0, b"h", 0, b"part").unwrap();
        assert_eq!(s.read(g, 0, 4).unwrap(), [1, 1, 1, 1]);
        s.close(g);
        assert_eq!(s.info(g), Err(Error::NotFound));
        // Every entry gone, nothing is held.
        s.remove(0, b"part").unwrap();
        s.remove(0, b"e").unwrap();
        assert_eq!(used.get(), 0);
        assert_eq!(s.entries(), 1);
    }

    #[test]
    fn if_t11_a_removed_open_file_lives_until_closed() {
        let (mut s, used) = metered(10_000);
        let f = s.create(0, b"f", 0o600).unwrap();
        s.write(f, 0, &[7; 1000]).unwrap();
        s.open(f).unwrap();
        s.remove(0, b"f").unwrap();
        assert_eq!(s.lookup(0, b"f"), Err(Error::NotFound));
        assert_eq!(s.read(f, 0, 2).unwrap(), [7, 7]);
        assert_eq!(used.get(), 1000);
        s.close(f);
        assert_eq!(used.get(), 0);
        assert_eq!(s.info(f), Err(Error::NotFound));
        let g = s.create(0, b"g", 0o600).unwrap();
        assert_eq!(g, f, "the number is used again");
        assert_eq!(s.read(g, 0, 10).unwrap(), b"", "with none of the old bytes");
        s.close(99);
        assert_eq!(s.open(99), Err(Error::NotFound));
        assert_eq!(s.remove(0, b"nope"), Err(Error::NotFound));
    }
}
