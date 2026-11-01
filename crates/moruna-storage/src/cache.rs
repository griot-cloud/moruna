//! Authenticated workspace-scoped cache. Plaintext exists only in the supplied charged
//! buffer; local cache files contain AES-GCM ciphertext. Recycle destroys an ephemeral node
//! secret, not a fictitious guarantee that unlink physically sanitizes NVMe cells.
#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_kernel::Buffer;
pub use moruna_storage_format::cache::WorkspaceKey;
use moruna_storage_format::cache::{Binding, Cipher, PREFIX, Prefix, path_identity};
use zeroize::Zeroizing;

use crate::{Access, Action, Error, Governed, Object, Operation, Result, VERSION, Version, io};

/// The orchestrator's workspace key service. It must enforce run/key policy and rotation;
/// no environment-selected or test key ships as a default.
pub trait Keys: Send + Sync {
    /// Current key for insertion, or the specified recorded key for lookup.
    fn workspace<'a>(
        &'a self,
        access: &'a Access,
        key_id: Option<&'a str>,
    ) -> Operation<'a, WorkspaceKey>;
}

struct Node {
    secret: Mutex<Option<Zeroizing<[u8; 32]>>>,
    used: AtomicU64,
    limit: u64,
    cleanup_failed: std::sync::atomic::AtomicBool,
}
/// An encrypted node cache, separate from any durable home.
pub struct Cache {
    root: PathBuf,
    node: Arc<Node>,
    keys: Arc<dyn Keys>,
    governed: Arc<Governed>,
}
/// A miss retains the caller's charged destination for the home read.
pub enum Lookup {
    /// Authenticated plaintext in the original destination.
    Hit(Buffer),
    /// No cache entry exists; no plaintext was read locally.
    Miss(Buffer),
}
impl Cache {
    /// Create a new private node cache directory. An existing directory is refused so a
    /// recycled node cannot reuse another node epoch's files under a fresh key.
    pub fn create(
        root: PathBuf,
        keys: Arc<dyn Keys>,
        governed: Arc<Governed>,
        byte_budget: u64,
    ) -> Result<Self> {
        if byte_budget == 0 {
            return Err(Error::Invalid("cache byte budget"));
        }
        let mut secret = Zeroizing::new([0u8; 32]);
        getrandom::fill(secret.as_mut()).map_err(|_| Error::Unavailable("OS entropy"))?;
        fs::create_dir(&root).map_err(|e| io("create cache epoch", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .map_err(|e| io("cache permissions", e))?;
        }
        Ok(Self {
            root,
            node: Arc::new(Node {
                secret: Mutex::new(Some(secret)),
                used: AtomicU64::new(0),
                limit: byte_budget,
                cleanup_failed: std::sync::atomic::AtomicBool::new(false),
            }),
            keys,
            governed,
        })
    }
    /// Revoke this epoch before removing files. A failed unlink still drops the only node
    /// secret; no further cache operation can decrypt or write in this epoch.
    pub fn recycle(&self) -> Result<()> {
        let mut secret = self
            .node
            .secret
            .lock()
            .map_err(|_| Error::Unavailable("cache state poisoned"))?;
        drop(secret.take());
        fs::remove_dir_all(&self.root).map_err(|e| io("remove recycled cache", e))
    }
    /// Freshly authorize before touching disk, even on a hit. Crypto and file IO run on the
    /// blocking pool, not the async caller. Corruption is an error, never a silent miss.
    pub async fn read(
        &self,
        access: &Access,
        object: &Object,
        version: &Version,
        offset: u64,
        dst: Buffer,
    ) -> Result<Lookup> {
        self.governed
            .authorize(access, object, Action::Read, Some(version))
            .await?;
        check_range(version, offset, &dst)?;
        let path = self.path(&access.workspace, object, version, offset, dst.len())?;
        let header_path = path.clone();
        // Never retain a file across an asynchronous key-provider call: a replacement
        // could otherwise leave an unlinked old ciphertext outside the cache budget.
        let opened = tokio::task::spawn_blocking(move || {
            Ok::<_, Error>(open(&header_path)?.map(|(file, binding, nonce, tag)| {
                drop(file);
                (binding, nonce, tag)
            }))
        })
        .await
        .map_err(|_| Error::Unavailable("cache read worker"))??;
        let Some((binding, nonce, tag)) = opened else {
            return Ok(Lookup::Miss(dst));
        };
        if binding.version != VERSION
            || binding.requester != access.workspace
            || binding.object != *object
            || binding.object_version != *version
            || binding.offset != offset
            || binding.bytes != dst.len() as u64
        {
            return Err(Error::Integrity);
        }
        let key = self.keys.workspace(access, Some(&binding.key_id)).await?;
        if key.id != binding.key_id {
            return Err(Error::Integrity);
        }
        let node = Arc::clone(&self.node);
        tokio::task::spawn_blocking(move || {
            let secret = node
                .secret
                .lock()
                .map_err(|_| Error::Unavailable("cache state poisoned"))?;
            let secret = secret
                .as_ref()
                .ok_or(Error::Unavailable("recycled cache"))?;
            let mut dst = dst;
            let result = (|| {
                let Some((mut file, current, current_nonce, current_tag)) = open(&path)? else {
                    return Err(Error::Integrity);
                };
                if current != binding || current_nonce != nonce || current_tag != tag {
                    return Err(Error::Integrity);
                }
                file.read_exact(dst.as_mut())
                    .map_err(|e| io("read cache ciphertext", e))?;
                let aad = binding_bytes(&binding)?;
                let cipher = Cipher::new(secret, &key.bytes, &binding.requester)
                    .map_err(|_| Error::Integrity)?;
                cipher
                    .open(&nonce, &aad, &tag, dst.as_mut())
                    .map_err(|_| Error::Integrity)?;
                Ok(())
            })();
            if let Err(e) = result {
                dst.as_mut().fill(0);
                return Err(e);
            }
            Ok(Lookup::Hit(dst))
        })
        .await
        .map_err(|_| Error::Unavailable("cache decrypt worker"))?
    }
    /// Governed read-through path for sources/prefetch. A real cache miss reads the pinned
    /// bucket/home version into the same charged destination, then encrypts that range.
    /// Corruption, missing authority/key or cache admission failure remains an explicit
    /// error; there is no silent bypass of required encryption or verifier policy.
    pub async fn read_through(
        &self,
        access: &Access,
        object: &Object,
        version: &Version,
        offset: u64,
        destination: Buffer,
    ) -> Result<Buffer> {
        match self
            .read(access, object, version, offset, destination)
            .await?
        {
            Lookup::Hit(buffer) => Ok(buffer),
            Lookup::Miss(destination) => {
                let buffer = self
                    .governed
                    .read(access, object, version, offset, destination)
                    .await?;
                self.insert(access, object, version, offset, buffer).await
            }
        }
    }
    /// Encrypt the caller's buffer in place for disk, then restore it to plaintext before
    /// returning that same buffer. This consumes no second payload-sized heap allocation.
    pub async fn insert(
        &self,
        access: &Access,
        object: &Object,
        version: &Version,
        offset: u64,
        buffer: Buffer,
    ) -> Result<Buffer> {
        self.store(access, object, version, offset, buffer, Action::Read)
            .await
    }
    pub(crate) async fn store(
        &self,
        access: &Access,
        object: &Object,
        version: &Version,
        offset: u64,
        buffer: Buffer,
        action: Action,
    ) -> Result<Buffer> {
        self.governed
            .authorize(
                access,
                object,
                action,
                if action == Action::Read {
                    Some(version)
                } else {
                    None
                },
            )
            .await?;
        check_range(version, offset, &buffer)?;
        let key = self.keys.workspace(access, None).await?;
        if key.id.is_empty() {
            return Err(Error::Invalid("empty workspace key identity"));
        }
        let binding = Binding {
            version: VERSION,
            requester: access.workspace.clone(),
            object: object.clone(),
            object_version: version.clone(),
            offset,
            bytes: buffer.len() as u64,
            key_id: key.id,
        };
        let path = self.path(&access.workspace, object, version, offset, buffer.len())?;
        let node = Arc::clone(&self.node);
        tokio::task::spawn_blocking(move || {
            let secret = node
                .secret
                .lock()
                .map_err(|_| Error::Unavailable("cache state poisoned"))?;
            let secret = secret
                .as_ref()
                .ok_or(Error::Unavailable("recycled cache"))?;
            if node.cleanup_failed.load(Ordering::Acquire) {
                return Err(Error::Unavailable(
                    "cache temporary cleanup requires recycle",
                ));
            }
            let aad = binding_bytes(&binding)?;
            let old = match fs::metadata(&path) {
                Ok(metadata) => metadata.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
                Err(e) => return Err(io("stat cache budget", e)),
            };
            let size = 40u64
                .checked_add(aad.len() as u64)
                .and_then(|n| n.checked_add(buffer.len() as u64))
                .ok_or(Error::Invalid("cache size overflow"))?;
            let after = node
                .used
                .load(Ordering::Relaxed)
                .checked_sub(old)
                .and_then(|n| n.checked_add(size))
                .ok_or(Error::Integrity)?;
            // Both old and new files coexist until atomic rename. Reads never retain
            // an old descriptor outside this same node mutex.
            let peak = node
                .used
                .load(Ordering::Relaxed)
                .checked_add(size)
                .ok_or(Error::Invalid("cache temporary budget overflow"))?;
            if after > node.limit || peak > node.limit {
                return Err(Error::Unavailable("cache byte budget"));
            }
            let cipher = Cipher::new(secret, &key.bytes, &binding.requester)
                .map_err(|_| Error::Integrity)?;
            let mut nonce = [0u8; 12];
            getrandom::fill(&mut nonce).map_err(|_| Error::Unavailable("OS entropy"))?;
            let mut buffer = buffer;
            let tag = cipher
                .seal(&nonce, &aad, buffer.as_mut())
                .map_err(|_| Error::Integrity)?;
            let written = write(&path, &aad, &nonce, tag.as_slice(), buffer.as_ref());
            if matches!(&written, Err(Error::Cleanup { .. })) {
                node.cleanup_failed.store(true, Ordering::Release);
            }
            if written.is_ok() {
                node.used.store(after, Ordering::Relaxed);
            }
            let restored = cipher.open(&nonce, &aad, &tag, buffer.as_mut());
            if restored.is_err() {
                buffer.as_mut().fill(0);
                return Err(Error::Integrity);
            }
            written?;
            Ok(buffer)
        })
        .await
        .map_err(|_| Error::Unavailable("cache encrypt worker"))?
    }
    fn path(
        &self,
        requester: &str,
        object: &Object,
        version: &Version,
        offset: u64,
        bytes: usize,
    ) -> Result<PathBuf> {
        Ok(self.root.join(path_identity(
            requester,
            object,
            version,
            offset,
            bytes as u64,
        )?))
    }
}
fn check_range(version: &Version, offset: u64, dst: &Buffer) -> Result<()> {
    version.validate()?;
    if dst.host_ptr().is_none() || offset.checked_add(dst.len() as u64).is_none() {
        return Err(Error::Invalid("cache host range"));
    }
    Ok(())
}
fn binding_bytes(binding: &Binding) -> Result<Vec<u8>> {
    Ok(binding.encode()?)
}
fn open(path: &Path) -> Result<Option<(File, Binding, [u8; 12], [u8; 16])>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io("open cache", e)),
    };
    let mut fixed = [0u8; PREFIX];
    file.read_exact(&mut fixed)
        .map_err(|e| io("read cache header", e))?;
    let prefix = Prefix::decode(&fixed).map_err(|_| Error::Integrity)?;
    let size = prefix.binding_bytes as usize;
    let mut aad = vec![0; size];
    file.read_exact(&mut aad)
        .map_err(|e| io("read cache identity", e))?;
    let binding = Binding::decode(&aad).map_err(|_| Error::Integrity)?;
    if binding_bytes(&binding)? != aad {
        return Err(Error::Integrity);
    }
    let len = 40u64
        .checked_add(size as u64)
        .and_then(|n| n.checked_add(binding.bytes))
        .ok_or(Error::Integrity)?;
    if file.metadata().map_err(|e| io("stat cache", e))?.len() != len {
        return Err(Error::Integrity);
    }
    Ok(Some((file, binding, prefix.nonce, prefix.tag)))
}
fn write(path: &Path, aad: &[u8], nonce: &[u8; 12], tag: &[u8], ciphertext: &[u8]) -> Result<()> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| Error::Unavailable("OS entropy"))?;
    let temp = path.with_extension(blake3::hash(&random).to_hex().as_str());
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|e| io("create cache ciphertext", e))?;
        let tag: [u8; 16] = tag.try_into().map_err(|_| Error::Integrity)?;
        let prefix = Prefix {
            binding_bytes: u32::try_from(aad.len())
                .map_err(|_| Error::Invalid("cache identity width"))?,
            nonce: *nonce,
            tag,
        }
        .encode()?;
        for part in [prefix.as_slice(), aad, ciphertext] {
            file.write_all(part)
                .map_err(|e| io("write cache ciphertext", e))?;
        }
        fs::rename(&temp, path).map_err(|e| io("publish cache", e))
    })();
    if result.is_err() {
        if let Err(e) = fs::remove_file(&temp) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(Error::Cleanup {
                    primary: Box::new(result.err().ok_or(Error::Integrity)?),
                    cleanup: Box::new(io("remove cache temporary", e)),
                });
            }
        }
    }
    result
}
