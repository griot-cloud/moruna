//! Actual secret-Memory key capability, fresh epoch entropy and sole shared Cipher.
//! No environment key, owner-equality grant, derived token or permissive verifier exists.
use crate::memory::Loan;
use alloc::{rc::Rc, string::String};
use core::cell::{Cell, RefCell};
use moruna_storage_format::{
    MAX_KEY_PAGE,
    auth::{Authorization, Verifier},
    cache::{Cipher, WorkspaceKey},
    wire::Refusal,
};
use moruna_svc::{Handle, Memory, PrivateMemory};

/// One explicit key-page capability plus private requester-charged scratch and epoch.
/// The source may be revoked/rotated by its real authority; every use verifies live access
/// and rereads the actual granted page. Source bytes never become an aliased Rust slice.
pub struct Keys {
    source: Loan,
    requester: String,
    scratch: RefCell<PrivateMemory>,
    epoch: RefCell<PrivateMemory>,
    usable: bool,
    issued: Rc<Cell<usize>>,
}
impl Keys {
    /// Accept only an explicitly granted key Memory and original child-account budget.
    /// A malformed/wrong-requester page refuses when first used; no fallback key is made.
    pub fn new(
        source: Memory,
        bytes: u64,
        requester: String,
        account: &Handle,
    ) -> Result<Self, Refusal> {
        if bytes == 0 || bytes > MAX_KEY_PAGE as u64 || requester.is_empty() {
            return Err(Refusal::Invalid);
        }
        let source = Loan::new(source, 0, bytes, false).map_err(|_| Refusal::Invalid)?;
        let scratch = PrivateMemory::create_in(bytes, account).map_err(|_| Refusal::Capacity)?;
        let mut epoch = PrivateMemory::create_in(32, account).map_err(|_| Refusal::Capacity)?;
        epoch
            .with_bytes(|bytes| moruna_svc::random(&mut bytes[..32]))
            .map_err(|_| Refusal::Unavailable)?
            .map_err(|_| Refusal::Unavailable)?;
        Ok(Self {
            source,
            requester,
            scratch: RefCell::new(scratch),
            epoch: RefCell::new(epoch),
            usable: true,
            issued: Rc::new(Cell::new(0)),
        })
    }
    /// Nonsecret cryptographic epoch fingerprint used ONLY for local namespace isolation.
    /// Missing/poisoned epoch refuses; raw epoch secret never leaves private memory.
    pub fn epoch_id(&self) -> Result<[u8; 32], Refusal> {
        if !self.usable {
            return Err(Refusal::Unavailable);
        }
        self.epoch
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?
            .with_bytes(|bytes| *blake3::hash(&bytes[..32]).as_bytes())
            .map_err(|_| Refusal::Unavailable)
    }
    /// Reverify this exact action BEFORE reading key bytes or returning any cached-key
    /// cipher. Foreign resource owners are allowed only by that actual injected authority.
    pub fn cipher(
        &self,
        verifier: &dyn Verifier,
        authorization: &Authorization<'_>,
    ) -> Result<(String, IssuedCipher), Refusal> {
        if !self.usable {
            return Err(Refusal::Unavailable);
        }
        if authorization.scope().workspace != self.requester {
            return Err(Refusal::Denied);
        }
        verifier.verify(authorization)?;
        let mut scratch = self
            .scratch
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?;
        let mut epoch = self
            .epoch
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?;
        let (id, cipher) = scratch
            .with_bytes(|bytes| {
                let result = (|| {
                    let length =
                        usize::try_from(self.source.len()).map_err(|_| Refusal::Invalid)?;
                    self.source
                        .read(0, &mut bytes[..length])
                        .map_err(|_| Refusal::Unavailable)?;
                    let key = WorkspaceKey::decode_secret(&bytes[..length], &self.requester)
                        .map_err(|_| Refusal::Invalid)?;
                    let cipher = epoch
                        .with_bytes(|epoch| {
                            let node: &[u8; 32] =
                                epoch[..32].try_into().map_err(|_| Refusal::Invalid)?;
                            Cipher::new(node, &key.bytes, &self.requester)
                                .map_err(|_| Refusal::Integrity)
                        })
                        .map_err(|_| Refusal::Unavailable)??;
                    Ok((key.id.clone(), cipher))
                })();
                bytes.fill(0);
                result
            })
            .map_err(|_| Refusal::Unavailable)??;
        let count = self.issued.get().checked_add(1).ok_or(Refusal::Capacity)?;
        self.issued.set(count);
        Ok((
            id,
            IssuedCipher {
                cipher: Some(cipher),
                issued: self.issued.clone(),
            },
        ))
    }
    /// Whether every actual issued cipher/private plaintext owner has ended.
    pub fn quiescent(&self) -> bool {
        self.issued.get() == 0
    }
    /// Replace the epoch using real entropy. Old ciphertext becomes unreadable once all
    /// issued ciphers finish; caller must quiesce those owners before recycling the disk.
    /// This is cryptographic invalidation, not a physical erase or retention guarantee.
    pub fn recycle_epoch(&mut self) -> Result<(), Refusal> {
        if self.issued.get() != 0 {
            return Err(Refusal::Unavailable);
        }
        self.usable = false;
        self.epoch
            .get_mut()
            .with_bytes(|bytes| {
                bytes.fill(0);
                moruna_svc::random(&mut bytes[..32])
            })
            .map_err(|_| Refusal::Unavailable)?
            .map_err(|_| Refusal::Unavailable)?;
        self.usable = true;
        Ok(())
    }
}
/// Owned derived cipher that prevents cache-epoch recycle until its actual user drops.
/// There is no clone or raw-cipher escape that could evade the lifetime count.
pub struct IssuedCipher {
    cipher: Option<Cipher>,
    issued: Rc<Cell<usize>>,
}
impl IssuedCipher {
    /// Encrypt with the sole core implementation into explicitly private charged bytes.
    pub fn seal(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        payload: &mut [u8],
    ) -> moruna_storage_format::Result<[u8; 16]> {
        self.cipher
            .as_ref()
            .ok_or(moruna_storage_format::Error::Invalid)?
            .seal(nonce, aad, payload)
    }
    /// Authenticate/decrypt with the sole core implementation, wiping on refusal.
    pub fn open(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        tag: &[u8; 16],
        payload: &mut [u8],
    ) -> moruna_storage_format::Result<()> {
        self.cipher
            .as_ref()
            .ok_or(moruna_storage_format::Error::Invalid)?
            .open(nonce, aad, tag, payload)
    }
}
impl Drop for IssuedCipher {
    fn drop(&mut self) {
        drop(self.cipher.take());
        self.issued.set(self.issued.get() - 1);
    }
}
