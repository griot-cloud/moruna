//! A scratch directory's bytes on the kernel (SDD 27 f.7, SDD 20 f.18): memory objects
//! charged to the directory's own account, so the kernel holds the directory to that
//! account's limit and a page the account has no room for is `NoSpace`. The account is
//! the one the task's starter gave (`scratch_account`), or, for a connection given none,
//! one of `scratch::DEFAULT_QUOTA` bytes the server carves out of its own when the
//! directory first needs a page, held until the connection ends.
//!
//! A file is a run of chunks that grow with it (`moruna_imagefs::chunks`), each one
//! memory object, under the server's budget of handles. A read or a write maps only the
//! pages of a chunk it copies through, and copies them at once: the chunks are the
//! server's alone, since a scratch file is never mapped by a client.

use alloc::{format, string::String};

use moruna_imagefs::{
    chunks::{Backing, Budget, Chunked, Cost},
    error::{Error as FsError, Result},
    scratch::DEFAULT_QUOTA,
};
use moruna_svc::{Account, Error, Mapping, Memory, Usage, charge, map as mapflags};

/// What the server keeps of its own account when it carves a default one: its heap,
/// its messages and its replies are charged there.
const HEAP_RESERVE: u64 = 16 << 20;

/// Where a scratch directory's chunks are made: memory objects charged to its account.
pub struct Pages {
    /// `None` until one is given or needed.
    account: Option<Account>,
}

/// A scratch directory's store: `account`'s, or a default one's, under `budget`.
pub fn store(account: Option<Account>, budget: Budget) -> Chunked<Pages> {
    Chunked::new(Pages { account }, budget)
}

fn kernel(what: &str, e: Error) -> FsError {
    match e {
        // The account, the handle table or the machine is full: the client's NoSpace.
        Error::QuotaExceeded | Error::Limit | Error::NoMemory => FsError::NoSpace,
        e => FsError::Io(format!("the scratch directory's {what}: {}", e.name())),
    }
}

/// `len` bytes of `chunk` from `at`, mapped: the pages they lie on, and where they begin
/// in the mapping.
fn window(chunk: &Memory, at: u64, len: usize, flags: u64) -> Result<(Mapping<'_>, usize)> {
    const PAGE: u64 = moruna_imagefs::chunks::PAGE;
    let first = at / PAGE * PAGE;
    let end = (at + len as u64).next_multiple_of(PAGE);
    let view = chunk
        .map(first, end - first, flags)
        .map_err(|e| kernel("chunk", e))?;
    Ok((view, (at - first) as usize))
}

impl Pages {
    fn account(&mut self) -> Result<&Account> {
        if self.account.is_none() {
            let own = Usage::own().map_err(|e| kernel("server's account", e))?;
            if own.room() < DEFAULT_QUOTA + HEAP_RESERVE {
                return Err(FsError::NoSpace);
            }
            let carved = Account::carve(DEFAULT_QUOTA, "scratch")
                .map_err(|e| kernel(&format!("default account of {DEFAULT_QUOTA} bytes"), e))?;
            self.account = Some(carved);
        }
        self.account
            .as_ref()
            .ok_or_else(|| FsError::Io(String::from("the scratch directory has no account")))
    }
}

impl Backing for Pages {
    type Chunk = Memory;

    fn cost(&self) -> Cost {
        Cost {
            object: charge::OBJECT,
            page: charge::PAGE + charge::PAGE_RECORD,
        }
    }

    fn room(&mut self) -> Result<Option<u64>> {
        // Without READ on the account its figures are unknown, and the kernel alone
        // refuses what does not fit.
        Ok(self.account()?.usage().ok().map(|u| u.room()))
    }

    fn create(&mut self, len: u64) -> Result<Memory> {
        Memory::create_in(len, &self.account()?.0).map_err(|e| kernel("pages", e))
    }

    fn read(&self, chunk: &Memory, at: u64, buf: &mut [u8]) -> Result<()> {
        let (view, from) = window(chunk, at, buf.len(), mapflags::READ)?;
        // No other process maps a chunk and the server writes none while it reads.
        let bytes = view.bytes().get(from..from + buf.len());
        buf.copy_from_slice(bytes.ok_or(FsError::BadRange)?);
        Ok(())
    }

    fn write(&self, chunk: &Memory, at: u64, bytes: &[u8]) -> Result<()> {
        let (mut view, from) = window(chunk, at, bytes.len(), mapflags::READ | mapflags::WRITE)?;
        // The region is shared with no one: one copy hands the bytes over.
        let region = view.shared_region().map_err(|e| kernel("chunk", e))?;
        region
            .get_mut(from..from + bytes.len())
            .ok_or(FsError::BadRange)?
            .copy_from_slice(bytes);
        Ok(())
    }

    fn zero(&self, chunk: &Memory, from: u64, to: u64) -> Result<()> {
        let len = usize::try_from(to - from).map_err(|_| FsError::BadRange)?;
        let (mut view, at) = window(chunk, from, len, mapflags::READ | mapflags::WRITE)?;
        let region = view.shared_region().map_err(|e| kernel("chunk", e))?;
        region
            .get_mut(at..at + len)
            .ok_or(FsError::BadRange)?
            .fill(0);
        Ok(())
    }
}
