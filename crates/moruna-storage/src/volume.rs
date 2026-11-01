//! Durable contiguous object extents and receipt-backed publication on one explicit
//! block volume. Uses the sole no_std codec; file-backed fixtures do not prove raw IO.
#![forbid(unsafe_code)]
use crate::{
    Backend, Condition, Error, Metadata, Object, Operation, Result, Scope, Version,
    blocks::{Blocks, Transaction},
    staging::{Lease, Staging},
};
use moruna_kernel::{Allocator, Buffer, BufferView, Tier};
use moruna_storage_format::{
    BlockProof, Commit, Frame, Location, Superblock, journal::Journal, leaf_digest, tree_node, wire,
};
use std::sync::Arc;
#[path = "volume_compact.rs"]
mod compact;

/// Supplies the requesting run's actual charged host arena. No owner-scoped fallback.
pub trait Arenas: Send + Sync {
    /// Resolve a validated nonsecret requester identity to its admission-controlled allocator.
    fn allocator(&self, scope: &Scope) -> Result<Arc<dyn Allocator>>;
}
/// Durable raw home or explicit regular-file format proof. Replays on every transaction,
/// so a dropped publication cannot leave stale in-memory visibility or reuse live IO.
pub struct Volume {
    blocks: Arc<dyn Blocks>,
    arenas: Arc<dyn Arenas>,
    staging: Arc<dyn Staging>,
    admission: tokio::sync::Mutex<()>,
}
struct Replay {
    journal: Journal,
    root: moruna_storage_format::banks::Root,
    _charge: Box<dyn Lease>,
}
impl Volume {
    /// Attach explicit IO/accounting authorities. Does not format, read or erase a device.
    pub fn new(
        blocks: Arc<dyn Blocks>,
        arenas: Arc<dyn Arenas>,
        staging: Arc<dyn Staging>,
    ) -> Result<Self> {
        if blocks.role() == crate::Role::CacheScratch {
            return Err(Error::Unavailable(
                "plaintext volume cannot serve encrypted cache/scratch",
            ));
        }
        let a = blocks.alignment();
        if a < 512 || !a.is_power_of_two() {
            return Err(Error::Invalid("volume alignment"));
        }
        Ok(Self {
            blocks,
            arenas,
            staging,
            admission: tokio::sync::Mutex::new(()),
        })
    }
    fn arena(&self, scope: &Scope) -> Result<Arc<dyn Allocator>> {
        scope.validate()?;
        self.arenas.allocator(scope)
    }
    fn buffer(&self, arena: &dyn Allocator, bytes: usize) -> Result<Buffer> {
        let buffer = arena
            .alloc(bytes, Tier::Host)
            .map_err(|e| Error::Arena(Box::new(e)))?;
        if buffer.len() != bytes
            || buffer
                .host_ptr()
                .is_none_or(|p| !(p as usize).is_multiple_of(self.blocks.alignment()))
        {
            return Err(Error::Invalid("volume requires aligned charged arena"));
        }
        Ok(buffer)
    }
    async fn page(
        &self,
        tx: &dyn Transaction,
        arena: &dyn Allocator,
        at: u64,
        bytes: usize,
    ) -> Result<Buffer> {
        tx.read(at, self.buffer(arena, bytes)?).await
    }
    async fn encoded(
        &self,
        tx: &dyn Transaction,
        arena: &dyn Allocator,
        at: u64,
        bytes: usize,
        frame: &Frame,
    ) -> Result<Vec<u8>> {
        let encoded = frame.encode()?;
        if encoded.len() > bytes {
            return Err(Error::Invalid("volume metadata exceeds cell"));
        }
        let mut dst = self.buffer(arena, bytes)?;
        dst.as_mut().fill(0);
        dst.as_mut()[..encoded.len()].copy_from_slice(&encoded);
        tx.write(at, Arc::new(dst).view()).await?;
        Ok(encoded)
    }
    /// Initialize only an all-zero superblock. The caller must separately authorize the
    /// destructive provisioning action; no absent/corrupt mount automatically formats.
    pub async fn format(
        &self,
        scope: &Scope,
        workspace: String,
        bucket: String,
        log_bytes: u64,
    ) -> Result<Superblock> {
        let _admission = self.admission.lock().await;
        let arena = self.arena(scope)?;
        let tx = Arc::clone(&self.blocks).begin()?;
        let a = self.blocks.alignment();
        let root = moruna_storage_format::banks::Root::initial(
            self.blocks.capacity(),
            u32::try_from(a).map_err(|_| Error::Invalid("volume alignment width"))?,
            log_bytes,
            self.blocks.role(),
            workspace,
            bucket,
        )?;
        let geometry = root.geometry().clone();
        Journal::new(geometry.clone())?;
        for at in [0, a as u64] {
            let first = self.page(tx.as_ref(), arena.as_ref(), at, a).await?;
            if first.as_ref().iter().any(|b| *b != 0) {
                return Err(Error::Conflict);
            }
        }
        let mut zero = self.buffer(arena.as_ref(), a)?;
        zero.as_mut().fill(0);
        let zero = Arc::new(zero);
        let mut at = geometry.log_offset;
        while at < geometry.data_offset {
            tx.write(at, zero.view()).await?;
            at += a as u64;
        }
        tx.sync().await?;
        let encoded = root.encode()?;
        let mut cell = self.buffer(arena.as_ref(), a)?;
        cell.as_mut().fill(0);
        cell.as_mut()[..encoded.len()].copy_from_slice(&encoded);
        tx.write(root.slot(), Arc::new(cell).view()).await?;
        tx.sync().await?;
        Ok(geometry)
    }
    /// Validate geometry/replay visibility and finalize any provable interrupted root
    /// switch by retiring its older valid root before later mutation can be admitted.
    pub async fn mount(&self, scope: &Scope) -> Result<Superblock> {
        let _admission = self.admission.lock().await;
        let arena = self.arena(scope)?;
        let tx = Arc::clone(&self.blocks).begin()?;
        Ok(self
            .replay(tx.as_ref(), arena.as_ref(), scope)
            .await?
            .journal
            .geometry()
            .clone())
    }
    async fn replay(
        &self,
        tx: &dyn Transaction,
        arena: &dyn Allocator,
        scope: &Scope,
    ) -> Result<Replay> {
        let a = self.blocks.alignment();
        let first = self.page(tx, arena, 0, a).await?;
        let second = self.page(tx, arena, a as u64, a).await?;
        let recovery =
            moruna_storage_format::banks::Root::recover(first.as_ref(), second.as_ref())?;
        let root = recovery.root().clone();
        let geometry = root.geometry().clone();
        if root.physical_capacity() != self.blocks.capacity()
            || geometry.alignment as usize != a
            || geometry.role != self.blocks.role()
        {
            return Err(Error::Invalid("mounted volume geometry/role"));
        }
        let charge = self.staging.reserve(scope, geometry.log_bytes)?;
        let mut journal = Journal::new(geometry)?;
        let c = usize::try_from(journal.cell_bytes())
            .map_err(|_| Error::Invalid("volume cell width"))?;
        while journal.has_capacity() {
            let at = journal.sequence();
            let receipt = self.page(tx, arena, at + c as u64, c).await?;
            let record = self.page(tx, arena, at, c).await?;
            if !journal.replay(record.as_ref(), receipt.as_ref())? {
                break;
            }
        }
        if let Some(retire) = recovery.retirement() {
            let mut zero = self.buffer(arena, a)?;
            zero.as_mut().fill(0);
            tx.write(retire, Arc::new(zero).view()).await?;
            tx.sync().await?;
        }
        Ok(Replay {
            journal,
            root,
            _charge: charge,
        })
    }
    fn object(replay: &Replay, object: &Object) -> Result<()> {
        object.validate()?;
        if object.workspace != replay.journal.geometry().workspace
            || object.bucket != replay.journal.geometry().bucket
        {
            return Err(Error::Denied);
        }
        Ok(())
    }
    async fn put(
        &self,
        scope: &Scope,
        object: &Object,
        condition: Condition,
        src: BufferView,
    ) -> Result<Metadata> {
        let _admission = self.admission.lock().await;
        let arena = self.arena(scope)?;
        let tx = Arc::clone(&self.blocks).begin()?;
        let replay = self.replay(tx.as_ref(), arena.as_ref(), scope).await?;
        Self::object(&replay, object)?;
        if object.key.is_empty() {
            return Err(Error::Invalid("empty volume object"));
        }
        let a = self.blocks.alignment();
        let c = usize::try_from(replay.journal.cell_bytes())
            .map_err(|_| Error::Invalid("volume cell width"))?;
        let bytes = src
            .as_host_slice()
            .ok_or(Error::Invalid("volume host payload"))?;
        let condition = match condition {
            Condition::Absent => wire::Condition::Absent,
            Condition::Match(v) => wire::Condition::Match(v),
        };
        let mut location = replay
            .journal
            .plan(object, &condition, bytes.len() as u64)?
            .ok_or(Error::Conflict)?;
        let leaves = bytes.len().div_ceil(a);
        let tree_charge = (leaves as u64)
            .checked_mul(64)
            .ok_or(Error::Invalid("volume tree admission"))?;
        let _tree_charge = self.staging.reserve(scope, tree_charge)?;
        let hash = *blake3::hash(bytes).as_bytes();
        let mut identity = [0; 16];
        getrandom::fill(&mut identity).map_err(|_| Error::Unavailable("OS entropy"))?;
        let metadata = Metadata {
            object: object.clone(),
            bytes: bytes.len() as u64,
            version: Version {
                etag: Some(hex(&hash)),
                generation: Some(hex(&identity)),
            },
        };
        // Reject unrepresentable control facts before writing any payload or tree bytes.
        let preview = Frame::Commit(Commit {
            sequence: replay.journal.sequence(),
            metadata: metadata.clone(),
            content_hash: hash,
            location: location.clone(),
        })
        .encode()?;
        if preview.len() > c {
            return Err(Error::Invalid("volume metadata exceeds commit cell"));
        }
        let mut scratch = self.buffer(arena.as_ref(), a)?;
        let mut digests = Vec::with_capacity(leaves);
        if let Location::Extent {
            offset, tree, root, ..
        } = &mut location
        {
            for (i, chunk) in bytes.chunks(a).enumerate() {
                digests.push(leaf_digest(chunk, a)?);
                let at = *offset + (i * a) as u64;
                if chunk.len() == a
                    && src
                        .host_ptr()
                        .is_some_and(|p| (p as usize).is_multiple_of(a))
                {
                    tx.write(at, src.slice(i * a, a)).await?;
                } else {
                    scratch.as_mut().fill(0);
                    scratch.as_mut()[..chunk.len()].copy_from_slice(chunk);
                    tx.write(at, Arc::new(scratch).view()).await?;
                    scratch = self.buffer(arena.as_ref(), a)?;
                }
            }
            for level in tree {
                let mut parents = Vec::with_capacity(level.nodes as usize);
                for (i, children) in digests.chunks(a / 32).enumerate() {
                    parents.push(tree_node(scratch.as_mut(), children)?);
                    tx.write(level.offset + (i * a) as u64, Arc::new(scratch).view())
                        .await?;
                    scratch = self.buffer(arena.as_ref(), a)?;
                }
                digests = parents;
            }
            if let Some(value) = digests.first() {
                *root = *value;
            }
        }
        tx.sync().await?;
        let commit = Commit {
            sequence: replay.journal.sequence(),
            metadata: metadata.clone(),
            content_hash: hash,
            location,
        };
        replay.journal.validate_commit(&commit)?;
        let encoded = self
            .encoded(
                tx.as_ref(),
                arena.as_ref(),
                replay.journal.sequence(),
                c,
                &Frame::Commit(commit),
            )
            .await?;
        tx.sync().await?;
        self.encoded(
            tx.as_ref(),
            arena.as_ref(),
            replay.journal.sequence() + c as u64,
            c,
            &Frame::Receipt {
                sequence: replay.journal.sequence(),
                commit_hash: *blake3::hash(&encoded).as_bytes(),
            },
        )
        .await?;
        tx.sync().await?;
        Ok(metadata)
    }
    async fn checked_read(
        &self,
        tx: &dyn Transaction,
        arena: &dyn Allocator,
        commit: &Commit,
        geometry: &Superblock,
        offset: u64,
        mut dst: Buffer,
    ) -> Result<Buffer> {
        if offset
            .checked_add(dst.len() as u64)
            .is_none_or(|n| n > commit.metadata.bytes)
        {
            return Err(Error::Invalid("volume range"));
        }
        let (start, _) = match &commit.location {
            Location::Extent {
                offset,
                padded_bytes,
                ..
            } => (*offset, *padded_bytes),
            _ => return Err(Error::Integrity),
        };
        let a = self.blocks.alignment();
        let length = dst.len();
        if length == 0 {
            return Ok(dst);
        }
        // Aligned ranges land directly in caller-owned charged pages. Only unaligned
        // edge/range requests use one bounded charged halo page and a visible CPU copy.
        let direct = offset.is_multiple_of(a as u64)
            && length.is_multiple_of(a)
            && dst
                .host_ptr()
                .is_some_and(|p| (p as usize).is_multiple_of(a));
        if direct && length > 0 {
            dst = tx.read(start + offset, dst).await?;
        }
        let first = offset / a as u64;
        let end = offset
            .checked_add(length as u64)
            .ok_or(Error::Invalid("volume range"))?
            .div_ceil(a as u64);
        for block in first..end {
            let mut proof = BlockProof::new(commit, geometry, block)?;
            while let Some(step) = proof.next_node() {
                let node = self.page(tx, arena, step.offset, a).await?;
                proof.accept_node(node.as_ref())?;
            }
            if direct {
                let at = (block - first) as usize * a;
                proof.verify_data(&dst.as_ref()[at..at + a])?;
            } else {
                let page = self.page(tx, arena, start + block * a as u64, a).await?;
                proof.verify_data(page.as_ref())?;
                let lo = offset.max(block * a as u64);
                let hi = (offset + length as u64).min((block + 1) * a as u64);
                dst.as_mut()[(lo - offset) as usize..(hi - offset) as usize].copy_from_slice(
                    &page.as_ref()
                        [(lo - block * a as u64) as usize..(hi - block * a as u64) as usize],
                );
            }
        }
        Ok(dst)
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
impl Backend for Volume {
    fn role(&self) -> Option<moruna_storage_format::auth::SessionRole> {
        use moruna_storage_format::auth::SessionRole;
        match self.blocks.role() {
            crate::Role::FormatProof => None,
            crate::Role::SingleNodeHome => Some(SessionRole::SingleNodeHome),
            crate::Role::CheckpointMetadata => Some(SessionRole::CheckpointMetadata),
            crate::Role::CacheScratch => None,
        }
    }

    fn current<'a>(&'a self, scope: &'a Scope, object: &'a Object) -> Operation<'a, Metadata> {
        Box::pin(async move {
            let _admission = self.admission.lock().await;
            let arena = self.arena(scope)?;
            let tx = Arc::clone(&self.blocks).begin()?;
            let replay = self.replay(tx.as_ref(), arena.as_ref(), scope).await?;
            Self::object(&replay, object)?;
            Ok(replay
                .journal
                .current(&object.key)
                .ok_or(Error::Missing)?
                .metadata
                .clone())
        })
    }

    fn head<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
    ) -> Operation<'a, Metadata> {
        Box::pin(async move {
            let _admission = self.admission.lock().await;
            let arena = self.arena(scope)?;
            let tx = Arc::clone(&self.blocks).begin()?;
            let replay = self.replay(tx.as_ref(), arena.as_ref(), scope).await?;
            Self::object(&replay, object)?;
            let current = replay.journal.current(&object.key).ok_or(Error::Missing)?;
            if current.metadata.version != *version {
                return Err(Error::Conflict);
            }
            Ok(current.metadata.clone())
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
        Box::pin(async move {
            let _admission = self.admission.lock().await;
            let arena = self.arena(scope)?;
            let tx = Arc::clone(&self.blocks).begin()?;
            let replay = self.replay(tx.as_ref(), arena.as_ref(), scope).await?;
            Self::object(&replay, object)?;
            let commit = replay.journal.current(&object.key).ok_or(Error::Missing)?;
            if commit.metadata.version != *version {
                return Err(Error::Conflict);
            }
            self.checked_read(
                tx.as_ref(),
                arena.as_ref(),
                commit,
                replay.journal.geometry(),
                offset,
                dst,
            )
            .await
        })
    }
    fn write<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        Box::pin(self.put(scope, object, Condition::Absent, src))
    }
    fn list<'a>(&'a self, scope: &'a Scope, prefix: &'a Object) -> Operation<'a, Vec<Metadata>> {
        Box::pin(async move {
            let _admission = self.admission.lock().await;
            let arena = self.arena(scope)?;
            let tx = Arc::clone(&self.blocks).begin()?;
            let replay = self.replay(tx.as_ref(), arena.as_ref(), scope).await?;
            Self::object(&replay, prefix)?;
            Ok(replay
                .journal
                .into_metadata()
                .filter(|metadata| metadata.object.key.starts_with(&prefix.key))
                .collect())
        })
    }
    fn replace<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        condition: Condition,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        Box::pin(self.put(scope, object, condition, src))
    }
}
