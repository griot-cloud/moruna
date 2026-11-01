//! Memory accounts (SDD 20, f.18; ABI 2.3).
//!
//! Every page a process causes to exist is charged to an account: the pages of the
//! memory objects it makes and the kernel's record of each, its zero pages when they are
//! first touched, its address space's areas and page tables, its threads' kernel stacks,
//! the messages it has queued, its handle table, and each kernel object it makes at a
//! fixed charge (`moruna_abi::charge`). A charge is held by what it pays for and given
//! back when that is freed, whoever holds it by then: a memory object stays charged to
//! the account it was made in until its last handle and its last mapping are gone.
//!
//! An account has a limit. Carving a child out of an account holds the child's whole
//! limit in the parent until the child is gone, so what is charged to an account and
//! the limits of its children never pass its own limit together, and the machine's
//! account, whose limit is the physical memory the kernel found less its own reserve,
//! is never overcommitted. A charge that does not fit is refused (`QuotaExceeded`); a
//! page fault whose page cannot be charged ends its process with the account named.
//!
//! Lock order: an account's ledger is a leaf, taken under any other lock and holding
//! none.

use alloc::{format, string::String, sync::Arc};

use moruna_abi::{Error, account_info, valid_name, write_u64};
use moruna_quota::{Exceeded, Ledger, Usage};
use ostd::{early_println, sync::SpinLock};

pub struct Account {
    name: String,
    ledger: SpinLock<Ledger>,
    /// The account its limit is held in; `None` for the machine's.
    parent: Option<Arc<Account>>,
    limit: u64,
    /// Its own record, charged to the account of the process that made it.
    _record: Option<Charge>,
}

impl Account {
    /// The machine's account, of `limit` bytes: what processes may have of the physical
    /// memory the kernel found at boot.
    pub fn machine(limit: u64) -> Arc<Account> {
        Arc::new(Account {
            name: String::from("machine"),
            ledger: SpinLock::new(Ledger::new(limit)),
            parent: None,
            limit,
            _record: None,
        })
    }

    /// A child of this account named `name`, of `limit` bytes (whole pages, not zero),
    /// held here until the child is gone; its record charged to `payer`.
    pub fn carve(
        self: &Arc<Self>,
        name: &[u8],
        limit: u64,
        payer: &Arc<Account>,
    ) -> Result<Arc<Account>, Error> {
        if !valid_name(name) || limit == 0 || !limit.is_multiple_of(moruna_abi::PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        let name = core::str::from_utf8(name).map_err(|_| Error::InvalidArgs)?;
        self.ledger
            .lock()
            .reserve(limit)
            .map_err(|_| Error::QuotaExceeded)?;
        let record = match Charge::new(payer, moruna_abi::charge::OBJECT) {
            Ok(record) => record,
            Err(error) => {
                // The account record does not exist yet. Return the held ceiling
                // without charging a speculative record or inflating a failed
                // creator's peak. No ledger lock is held while charging another.
                if self.ledger.lock().unreserve(limit).is_err() {
                    early_println!(
                        "kk: accounting broken: account {} lost a child reservation of {}",
                        self.name,
                        limit
                    );
                    return Err(Error::BadState);
                }
                return Err(error);
            }
        };
        Ok(Arc::new(Account {
            name: String::from(name),
            ledger: SpinLock::new(Ledger::new(limit)),
            parent: Some(self.clone()),
            limit,
            _record: Some(record),
        }))
    }

    pub fn usage(&self) -> Usage {
        self.ledger.lock().usage()
    }

    /// Charges `bytes`; `QuotaExceeded`, with nothing charged, when they do not fit.
    pub fn charge(&self, bytes: u64) -> Result<(), Error> {
        self.try_charge(bytes).map_err(|_| Error::QuotaExceeded)
    }

    /// Charges `bytes`, or says why they do not fit.
    pub fn try_charge(&self, bytes: u64) -> Result<(), Exceeded> {
        if bytes == 0 {
            return Ok(());
        }
        self.ledger.lock().charge(bytes)
    }

    /// Gives back `bytes` charged before. More than is charged is a broken invariant of
    /// the kernel's: it is said on the console and nothing is given back.
    pub fn uncharge(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        if let Err(u) = self.ledger.lock().uncharge(bytes) {
            early_println!(
                "kk: accounting broken: account {} gave back {} bytes with {} charged",
                self.name,
                u.returned,
                u.held
            );
        }
    }

    /// Why a charge of `asked` bytes found no room, for a fault's reason.
    pub fn refusal(&self, asked: u64) -> String {
        let u = self.usage();
        format!(
            "account {} has no room for {} bytes (limit {} bytes, {} charged, {} held for the accounts carved from it)",
            self.name, asked, u.limit, u.used, u.reserved
        )
    }

    /// `account_info`'s 64 bytes.
    pub fn info(&self) -> [u8; account_info::SIZE] {
        let u = self.usage();
        let mut out = [0u8; account_info::SIZE];
        write_u64(&mut out, account_info::LIMIT, u.limit);
        write_u64(&mut out, account_info::USED, u.used);
        write_u64(&mut out, account_info::PEAK, u.peak);
        write_u64(&mut out, account_info::RESERVED, u.reserved);
        let name = self.name.as_bytes();
        let n = name.len().min(moruna_abi::limits::NAME_MAX);
        out[account_info::NAME..account_info::NAME + n].copy_from_slice(&name[..n]);
        out
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        let left = self.ledger.lock().usage().used;
        if left != 0 {
            // Every charge holds its account, so none can outlive it.
            early_println!(
                "kk: accounting broken: account {} ended with {} bytes charged",
                self.name,
                left
            );
        }
        if let Some(parent) = &self.parent
            && let Err(u) = parent.ledger.lock().unreserve(self.limit)
        {
            early_println!(
                "kk: accounting broken: account {} gave back a limit of {} bytes to {}, which held {}",
                self.name,
                u.returned,
                parent.name,
                u.held
            );
        }
    }
}

/// Bytes charged to an account, given back when this is dropped: held by whatever the
/// bytes pay for, so the charge lives exactly as long as it does.
pub struct Charge {
    account: Arc<Account>,
    bytes: u64,
}

impl Charge {
    /// Charges `bytes` to `account`; `QuotaExceeded` when they do not fit.
    pub fn new(account: &Arc<Account>, bytes: u64) -> Result<Charge, Error> {
        account.charge(bytes)?;
        Ok(Charge {
            account: account.clone(),
            bytes,
        })
    }

    /// The immutable original owner, never a usage/name/ancestor comparison.
    pub fn matches(&self, account: &Arc<Account>) -> bool {
        Arc::ptr_eq(&self.account, account)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Charges `bytes` more.
    pub fn grow(&mut self, bytes: u64) -> Result<(), Error> {
        self.account.charge(bytes)?;
        self.bytes += bytes;
        Ok(())
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.account.uncharge(self.bytes);
    }
}
