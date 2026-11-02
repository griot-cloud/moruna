//! A scratch directory's bytes on the kernel (SDD 27 f.7, SDD 20 f.18): memory objects
//! charged to the directory's own account, so the kernel holds the directory to that
//! account's limit and a page the account has no room for is `NoSpace`. The account is
//! the one the task's starter gave (`scratch_account`), or, for a connection given none,
//! one of `scratch::DEFAULT_QUOTA` bytes the server carves out of its own when the
//! directory first needs a page, held until the connection ends.
//!
//! A file is a run of chunks of [`CHUNK`] bytes; a chunk is mapped into the server only
//! while a read or a write copies through it.

use alloc::{collections::BTreeMap, format, vec::Vec};

use moruna_imagefs::{
    error::{Error as FsError, Result},
    scratch::{DEFAULT_QUOTA, Id, Store},
};
use moruna_svc::{Account, Error, Mapping, Memory, map as mapflags};

/// A chunk of a scratch file: what each memory object holds.
pub const CHUNK: u64 = 16 * 1024;

struct File {
    size: u64,
    chunks: Vec<Memory>,
}

pub struct Pages {
    /// The account the chunks are charged to; `None` until one is given or needed.
    account: Option<Account>,
    files: BTreeMap<Id, File>,
}

fn kernel(what: &str, e: Error) -> FsError {
    match e {
        Error::QuotaExceeded => FsError::NoSpace,
        e => FsError::Io(format!("the scratch directory's {what}: {}", e.name())),
    }
}

/// Copies `data` into a mapping from `at`, eight bytes an access where it can.
fn copy_in(view: &Mapping<'_>, mut at: usize, data: &[u8]) -> Result<()> {
    let fail = |e: Error| kernel("chunk", e);
    let mut i = 0;
    while i < data.len() && !at.is_multiple_of(8) {
        view.write8(at, data[i]).map_err(fail)?;
        i += 1;
        at += 1;
    }
    while data.len() - i >= 8 {
        let mut word = [0u8; 8];
        word.copy_from_slice(&data[i..i + 8]);
        view.write64(at, u64::from_le_bytes(word)).map_err(fail)?;
        i += 8;
        at += 8;
    }
    while i < data.len() {
        view.write8(at, data[i]).map_err(fail)?;
        i += 1;
        at += 1;
    }
    Ok(())
}

impl Pages {
    /// A store charging `account`, or, when `None`, an account it carves when it first
    /// needs one.
    pub fn new(account: Option<Account>) -> Pages {
        Pages {
            account,
            files: BTreeMap::new(),
        }
    }

    fn account(&mut self) -> Result<&Account> {
        if self.account.is_none() {
            let carved = Account::carve(DEFAULT_QUOTA, "scratch")
                .map_err(|e| kernel(&format!("default account of {DEFAULT_QUOTA} bytes"), e))?;
            self.account = Some(carved);
        }
        self.account.as_ref().ok_or_else(|| {
            FsError::Io(alloc::string::String::from(
                "the scratch directory has no account",
            ))
        })
    }

    /// Each chunk of file `id` that `offset .. offset + len` touches, with the range of
    /// it: `(chunk, from, to)`, and where in the caller's buffer it begins.
    fn spans(offset: u64, len: usize) -> impl Iterator<Item = (usize, u64, u64, usize)> {
        let end = offset + len as u64;
        let mut at = offset;
        core::iter::from_fn(move || {
            if at >= end {
                return None;
            }
            let chunk = at / CHUNK;
            let from = at % CHUNK;
            let to = (end - chunk * CHUNK).min(CHUNK);
            let done = (at - offset) as usize;
            at = chunk * CHUNK + to;
            Some((chunk as usize, from, to, done))
        })
    }
}

impl Store for Pages {
    fn resize(&mut self, id: Id, size: u64) -> Result<()> {
        let need = usize::try_from(size.div_ceil(CHUNK)).map_err(|_| FsError::NoSpace)?;
        let have = self.files.get(&id).map_or(0, |f| f.chunks.len());
        let mut grown = Vec::new();
        if need > have {
            let account = self.account()?;
            for _ in have..need {
                // A refusal drops what this call made: nothing about the file changes.
                grown.push(Memory::create_in(CHUNK, &account.0).map_err(|e| kernel("pages", e))?);
            }
        }
        let file = self.files.entry(id).or_insert(File {
            size: 0,
            chunks: Vec::new(),
        });
        file.chunks.extend(grown);
        if size < file.size && !size.is_multiple_of(CHUNK) {
            // A later growth reads zeros where the cut bytes were.
            let last = &file.chunks[need - 1];
            let from = size % CHUNK;
            let view = last
                .map(0, CHUNK, mapflags::READ | mapflags::WRITE)
                .map_err(|e| kernel("chunk", e))?;
            let zeros = [0u8; 512];
            let mut at = from;
            while at < CHUNK {
                let n = (CHUNK - at).min(zeros.len() as u64) as usize;
                copy_in(&view, at as usize, &zeros[..n])?;
                at += n as u64;
            }
        }
        // Keep the old chunks and length if preparing the tail mapping is refused.
        // Only release truncated storage after all fallible work has succeeded.
        file.chunks.truncate(need);
        file.size = size;
        Ok(())
    }

    fn read(&self, id: Id, offset: u64, buf: &mut [u8]) -> Result<()> {
        let Some(file) = self.files.get(&id) else {
            return if buf.is_empty() {
                Ok(())
            } else {
                Err(FsError::BadRange)
            };
        };
        if offset + buf.len() as u64 > file.size {
            return Err(FsError::BadRange);
        }
        for (chunk, from, to, done) in Pages::spans(offset, buf.len()) {
            let view = file.chunks[chunk]
                .map(0, CHUNK, mapflags::READ)
                .map_err(|e| kernel("chunk", e))?;
            let n = (to - from) as usize;
            view.read(from as usize, &mut buf[done..done + n])
                .map_err(|e| kernel("chunk", e))?;
        }
        Ok(())
    }

    fn write(&mut self, id: Id, offset: u64, bytes: &[u8]) -> Result<()> {
        let file = self.files.get(&id).ok_or(FsError::BadRange)?;
        if offset + bytes.len() as u64 > file.size {
            return Err(FsError::BadRange);
        }
        for (chunk, from, to, done) in Pages::spans(offset, bytes.len()) {
            let view = file.chunks[chunk]
                .map(0, CHUNK, mapflags::READ | mapflags::WRITE)
                .map_err(|e| kernel("chunk", e))?;
            let n = (to - from) as usize;
            copy_in(&view, from as usize, &bytes[done..done + n])?;
        }
        Ok(())
    }

    fn free(&mut self, id: Id) {
        self.files.remove(&id);
    }
}
