//! A regular-file adapter for the durable extent/commit format. Explicitly a format
//! fixture, not a raw-device/NVMe/O_DIRECT proof or an implicit bucket fallback.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use moruna_kernel::{Buffer, BufferView};
use moruna_storage_format::{Commit, Frame, HEADER, Location, frame_size};

use crate::{
    Backend, Condition, Error, Metadata, Object, Operation, Result, Role, Scope, Version, io,
};

/// Immutable data plus one checksummed durable commit record, serialized across processes.
#[derive(Clone)]
pub struct FileStore {
    root: Arc<PathBuf>,
    workspace: String,
    bucket: String,
    role: Role,
}
impl FileStore {
    /// Open only an explicit directory and explicit role; never accepts a raw device.
    pub fn open(root: PathBuf, workspace: String, bucket: String, role: Role) -> Result<Self> {
        if !matches!(role, Role::FormatProof | Role::CheckpointMetadata)
            || workspace.is_empty()
            || bucket.is_empty()
        {
            return Err(Error::Invalid("local store scope"));
        }
        fs::create_dir_all(root.join("data")).map_err(|e| io("create local store", e))?;
        Ok(Self {
            root: Arc::new(root),
            workspace,
            bucket,
            role,
        })
    }
    /// The declared durability role, recorded independently from bucket portability.
    pub fn role(&self) -> Role {
        self.role
    }
    fn scope(&self, object: &Object) -> Result<()> {
        object.validate()?;
        if object.workspace != self.workspace || object.bucket != self.bucket {
            return Err(Error::Denied);
        }
        Ok(())
    }
    fn locked(&self) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("writer.lock"))
            .map_err(|e| io("open commit lock", e))?;
        file.lock().map_err(|e| io("lock local store", e))?;
        Ok(file)
    }
    fn replay(&self) -> Result<(BTreeMap<String, Commit>, u64)> {
        let mut file = match File::open(self.root.join("commits.log")) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((BTreeMap::new(), 0)),
            Err(e) => return Err(io("open commit log", e)),
        };
        let mut commits = BTreeMap::new();
        let total = file.metadata().map_err(|e| io("stat commit log", e))?.len();
        let mut at = 0u64;
        while total.saturating_sub(at) >= HEADER as u64 {
            let mut fixed = [0u8; HEADER];
            file.read_exact(&mut fixed)
                .map_err(|e| io("read commit header", e))?;
            let len = frame_size(&fixed)?;
            let end = at.checked_add(len as u64).ok_or(Error::Integrity)?;
            if end > total {
                break;
            }
            let mut bytes = vec![0; len];
            bytes[..HEADER].copy_from_slice(&fixed);
            file.read_exact(&mut bytes[HEADER..])
                .map_err(|e| io("read commit payload", e))?;
            let commit = match Frame::decode(&bytes)? {
                Frame::Commit(commit) => commit,
                _ => return Err(Error::Integrity),
            };
            self.scope(&commit.metadata.object)?;
            if commit.sequence != at || !matches!(commit.location, Location::File(_)) {
                return Err(Error::Integrity);
            }
            commits.insert(commit.metadata.object.key.clone(), commit);
            at = end;
        }
        Ok((commits, at))
    }
    fn put(&self, object: &Object, condition: Condition, src: BufferView) -> Result<Metadata> {
        self.scope(object)?;
        if object.key.is_empty() {
            return Err(Error::Invalid("empty object key"));
        }
        let bytes = src
            .as_host_slice()
            .ok_or(Error::Invalid("local write needs host view"))?;
        let _writer = self.locked()?;
        let (commits, valid) = self.replay()?;
        match (&condition, commits.get(&object.key)) {
            (Condition::Absent, Some(_)) => return Err(Error::Conflict),
            (Condition::Match(expected), Some(current))
                if *expected == current.metadata.version => {}
            (Condition::Match(_), _) => return Err(Error::Conflict),
            (Condition::Absent, None) => {}
        }
        let hash = blake3::hash(bytes).to_hex().to_string();
        let mut nonce = [0u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| Error::Unavailable("OS entropy"))?;
        let identity = *blake3::hash(&nonce).as_bytes();
        let data = hex(&identity);
        let path = self.root.join("data").join(&data);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| io("create immutable extent", e))?;
        file.write_all(bytes)
            .map_err(|e| io("write immutable extent", e))?;
        file.sync_all()
            .map_err(|e| io("sync immutable extent", e))?;
        File::open(self.root.join("data"))
            .and_then(|f| f.sync_all())
            .map_err(|e| io("sync extent directory", e))?;
        let metadata = Metadata {
            object: object.clone(),
            version: Version {
                etag: Some(hash.clone()),
                generation: Some(data.clone()),
            },
            bytes: bytes.len() as u64,
        };
        let commit = Commit {
            sequence: valid,
            metadata: metadata.clone(),
            content_hash: *blake3::hash(bytes).as_bytes(),
            location: Location::File(identity),
        };
        let record = Frame::Commit(commit).encode()?;
        let mut log = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("commits.log"))
            .map_err(|e| io("open commit writer", e))?;
        log.set_len(valid)
            .map_err(|e| io("discard uncommitted tail", e))?;
        log.seek(SeekFrom::Start(valid))
            .map_err(|e| io("seek commit", e))?;
        log.write_all(&record)
            .map_err(|e| io("write commit frame", e))?;
        log.sync_all().map_err(|e| io("sync commit", e))?;
        File::open(self.root.as_ref())
            .and_then(|f| f.sync_all())
            .map_err(|e| io("sync store directory", e))?;
        Ok(metadata)
    }
}
impl Backend for FileStore {
    fn role(&self) -> Option<moruna_storage_format::auth::SessionRole> {
        None
    }

    fn current<'a>(&'a self, scope: &'a Scope, object: &'a Object) -> Operation<'a, Metadata> {
        let store = self.clone();
        let object = object.clone();
        Box::pin(async move {
            scope.validate()?;
            tokio::task::spawn_blocking(move || {
                store.scope(&object)?;
                let _writer = store.locked()?;
                let (commits, _) = store.replay()?;
                Ok(commits
                    .get(&object.key)
                    .ok_or(Error::Missing)?
                    .metadata
                    .clone())
            })
            .await
            .map_err(|_| Error::Unavailable("local current metadata worker"))?
        })
    }

    fn head<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
    ) -> Operation<'a, Metadata> {
        let store = self.clone();
        let object = object.clone();
        let version = version.clone();
        Box::pin(async move {
            scope.validate()?;
            tokio::task::spawn_blocking(move || {
                store.scope(&object)?;
                let _writer = store.locked()?;
                let (commits, _) = store.replay()?;
                let current = commits.get(&object.key).ok_or(Error::Missing)?;
                if current.metadata.version != version {
                    return Err(Error::Conflict);
                }
                Ok(current.metadata.clone())
            })
            .await
            .map_err(|_| Error::Unavailable("local metadata worker"))?
        })
    }

    fn read<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        dst: Buffer,
    ) -> Operation<'a, Buffer> {
        let (store, object, version) = (self.clone(), object.clone(), version.clone());
        Box::pin(async move {
            scope.validate()?;
            tokio::task::spawn_blocking(move || {
                store.scope(&object)?;
                if dst.host_ptr().is_none() {
                    return Err(Error::Invalid("local read needs host buffer"));
                }
                let _writer = store.locked()?;
                let (commits, _) = store.replay()?;
                let commit = commits.get(&object.key).ok_or(Error::Missing)?;
                if commit.metadata.version != version {
                    return Err(Error::Conflict);
                }
                let end = offset
                    .checked_add(dst.len() as u64)
                    .ok_or(Error::Invalid("range overflow"))?;
                if end > commit.metadata.bytes {
                    return Err(Error::Invalid("range past object"));
                }
                let mut file = File::open(store.root.join("data").join(file_name(commit)?))
                    .map_err(|e| io("open immutable extent", e))?;
                // The format fixture checks the complete immutable extent with bounded
                // scratch. This is not the raw adapter's direct range/DMA performance path.
                let mut hash = blake3::Hasher::new();
                let mut scratch = [0u8; 65536];
                loop {
                    let n = file
                        .read(&mut scratch)
                        .map_err(|e| io("verify extent", e))?;
                    if n == 0 {
                        break;
                    }
                    hash.update(&scratch[..n]);
                }
                if hash.finalize().as_bytes() != &commit.content_hash {
                    return Err(Error::Integrity);
                }
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| io("seek extent range", e))?;
                let mut dst = dst;
                file.read_exact(dst.as_mut())
                    .map_err(|e| io("read extent range", e))?;
                Ok(dst)
            })
            .await
            .map_err(|_| Error::Unavailable("local read worker"))?
        })
    }
    fn write<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        self.replace(scope, object, Condition::Absent, src)
    }
    fn list<'a>(&'a self, scope: &'a Scope, prefix: &'a Object) -> Operation<'a, Vec<Metadata>> {
        let (store, prefix) = (self.clone(), prefix.clone());
        Box::pin(async move {
            scope.validate()?;
            tokio::task::spawn_blocking(move || {
                store.scope(&prefix)?;
                let _writer = store.locked()?;
                Ok(store
                    .replay()?
                    .0
                    .into_values()
                    .filter(|c| c.metadata.object.key.starts_with(&prefix.key))
                    .map(|c| c.metadata)
                    .collect())
            })
            .await
            .map_err(|_| Error::Unavailable("local list worker"))?
        })
    }
    fn replace<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        condition: Condition,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        let (store, object) = (self.clone(), object.clone());
        Box::pin(async move {
            scope.validate()?;
            tokio::task::spawn_blocking(move || store.put(&object, condition, src))
                .await
                .map_err(|_| Error::Unavailable("local commit worker"))?
        })
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn file_name(commit: &Commit) -> Result<String> {
    match &commit.location {
        Location::File(id) => Ok(hex(id)),
        Location::Extent { .. } => {
            Err(Error::Unavailable("raw extent requires raw volume adapter"))
        }
    }
}
