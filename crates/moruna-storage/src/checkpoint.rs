//! Portable checkpoint publication. Local spill references are never silently portable:
//! callers must publish immutable dependencies or describe durable source lineage.
#![forbid(unsafe_code)]

use moruna_kernel::{Allocator, Buffer, BufferView};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{Access, Condition, Error, Governed, Metadata, Object, Result, VERSION};

/// A durable immutable dependency, whose bytes and authorization are checked on restore.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dependency {
    /// Exact immutable object/version/length.
    pub metadata: Metadata,
    /// Hash over the complete persisted bytes.
    pub content_hash: String,
}
/// Recovery metadata portable across nodes. It contains the existing engine manifest bytes
/// unchanged; it must not contain credentials, keys or unresolved node-local spill paths.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Portable {
    /// Storage envelope version.
    pub version: u32,
    /// Explicit workspace/run identity.
    pub workspace: String,
    /// Run identity.
    pub run: String,
    /// Monotonic checkpoint sequence, owned by the run's checkpoint writer.
    pub sequence: u64,
    /// The existing recovery manifest, with durable lineage/dependency references.
    pub manifest: serde_json::Value,
    /// Every immutable dependency required by this checkpoint.
    pub dependencies: Vec<Dependency>,
}
/// Bounded checkpoint publication through the same governed port as task data.
pub struct Checkpoints {
    local: Arc<Governed>,
    store: Arc<Governed>,
    alloc: Arc<dyn Allocator>,
    metadata_limit: usize,
}
impl Checkpoints {
    /// Construct with a metadata ceiling and the caller's charged allocator.
    pub fn new(
        local: Arc<Governed>,
        store: Arc<Governed>,
        alloc: Arc<dyn Allocator>,
        metadata_limit: usize,
    ) -> Result<Self> {
        if metadata_limit == 0 {
            return Err(Error::Invalid("checkpoint metadata ceiling"));
        }
        Ok(Self {
            local,
            store,
            alloc,
            metadata_limit,
        })
    }
    /// Publish actual trusted engine-owned segment/shard bytes into immutable bucket home.
    /// The caller owns their truthful local source identity and actual charged view; no fake
    /// Governed source metadata or filesystem URL is synthesized by this storage boundary.
    pub async fn export_bytes(
        &self,
        access: &Access,
        target: &Object,
        bytes: BufferView,
    ) -> Result<Dependency> {
        let content_hash = blake3::hash(
            bytes
                .as_host_slice()
                .ok_or(Error::Invalid("checkpoint host dependency"))?,
        )
        .to_hex()
        .to_string();
        let metadata = self.store.write(access, target, bytes).await?;
        Ok(Dependency {
            metadata,
            content_hash,
        })
    }
    /// Copy one actual local/source object into immutable bucket home, using an explicitly
    /// supplied charged whole-object buffer. No local path/JSON reference is relabelled as
    /// portable. Oversized dependencies must be partitioned by the engine before this call.
    pub async fn export_dependency(
        &self,
        access: &Access,
        source: &Governed,
        original: &Metadata,
        target: &Object,
        destination: Buffer,
    ) -> Result<Dependency> {
        if destination.len() as u64 != original.bytes {
            return Err(Error::Invalid("checkpoint dependency destination"));
        }
        let actual = source
            .head(access, &original.object, &original.version)
            .await?;
        if actual != *original {
            return Err(Error::Integrity);
        }
        let bytes = source
            .read(access, &original.object, &original.version, 0, destination)
            .await?;
        let content_hash = blake3::hash(bytes.as_ref()).to_hex().to_string();
        let metadata = self
            .store
            .write(access, target, Arc::new(bytes).view())
            .await?;
        Ok(Dependency {
            metadata,
            content_hash,
        })
    }
    /// Restore one complete immutable dependency into the caller's charged destination,
    /// freshly verifying exact identity and full content. This never reruns historical data.
    pub async fn dependency(
        &self,
        access: &Access,
        dependency: &Dependency,
        destination: Buffer,
    ) -> Result<Buffer> {
        if destination.len() as u64 != dependency.metadata.bytes {
            return Err(Error::Invalid("checkpoint dependency destination"));
        }
        let actual = self
            .store
            .head(
                access,
                &dependency.metadata.object,
                &dependency.metadata.version,
            )
            .await?;
        if actual != dependency.metadata {
            return Err(Error::Integrity);
        }
        let mut bytes = self
            .store
            .read(access, &actual.object, &actual.version, 0, destination)
            .await?;
        if blake3::hash(bytes.as_ref()).to_hex().as_str() != dependency.content_hash {
            bytes.as_mut().fill(0);
            return Err(Error::Integrity);
        }
        Ok(bytes)
    }
    /// Publish only already-written immutable dependencies, then conditionally update the
    /// checkpoint object. Every dependency is re-read and hashed through the live verifier.
    pub async fn publish(
        &self,
        access: &Access,
        local_target: &Object,
        target: &Object,
        condition: Condition,
        checkpoint: &Portable,
    ) -> Result<Metadata> {
        if checkpoint.workspace != access.workspace || checkpoint.run != access.run {
            return Err(Error::Denied);
        }
        self.validate(checkpoint)?;
        self.verify_dependencies(access, checkpoint).await?;
        let encoded = serde_json::to_vec(checkpoint)
            .map_err(|_| Error::Invalid("checkpoint serialization"))?;
        if encoded.len() > self.metadata_limit {
            return Err(Error::Invalid("checkpoint metadata budget"));
        }
        let mut bytes = self.allocate(encoded.len())?;
        bytes.as_mut().copy_from_slice(&encoded);
        let bytes = Arc::new(bytes);
        let existing = match self.local.current(access, local_target).await {
            Ok(metadata) => Some(metadata),
            Err(Error::Missing) => None,
            Err(cause) => return Err(cause),
        };
        let local_condition = existing
            .map(|m| Condition::Match(m.version))
            .unwrap_or(Condition::Absent);
        let local = self
            .local
            .replace(access, local_target, local_condition, bytes.view())
            .await?;
        match self
            .store
            .replace(access, target, condition, bytes.view())
            .await
        {
            Ok(metadata) => Ok(metadata),
            Err(cause) => Err(Error::PortablePending {
                local: Box::new(local),
                cause: Box::new(cause),
            }),
        }
    }
    /// Read a checkpoint under its exact version and authorize/hash its dependencies on
    /// this node; the same method is used by CLI/SDK/public-port consumers.
    pub async fn restore(&self, access: &Access, metadata: &Metadata) -> Result<Portable> {
        let len =
            usize::try_from(metadata.bytes).map_err(|_| Error::Invalid("checkpoint length"))?;
        if len > self.metadata_limit {
            return Err(Error::Invalid("checkpoint metadata budget"));
        }
        let bytes = self
            .store
            .read(
                access,
                &metadata.object,
                &metadata.version,
                0,
                self.allocate(len)?,
            )
            .await?;
        let checkpoint: Portable =
            serde_json::from_slice(bytes.as_ref()).map_err(|_| Error::Integrity)?;
        self.validate(&checkpoint)?;
        self.verify_dependencies(access, &checkpoint).await?;
        Ok(checkpoint)
    }
    fn validate(&self, checkpoint: &Portable) -> Result<()> {
        if checkpoint.version != VERSION {
            return Err(Error::Unavailable("portable checkpoint version"));
        }
        if checkpoint.workspace.is_empty() || checkpoint.run.is_empty() {
            return Err(Error::Integrity);
        }
        // Old manifests with local segment/lineage references do not become portable
        // simply by copying their JSON to a bucket.
        if checkpoint
            .manifest
            .get("segments")
            .and_then(|v| v.as_array())
            .is_some_and(|v| !v.is_empty())
            || checkpoint
                .manifest
                .get("lineage")
                .and_then(|v| v.as_array())
                .is_some_and(|rows| {
                    rows.iter()
                        .any(|r| r.get("disk").is_some_and(|d| !d.is_null()))
                })
        {
            return Err(Error::Invalid(
                "checkpoint still depends on node-local segments",
            ));
        }
        Ok(())
    }
    async fn verify_dependencies(&self, access: &Access, checkpoint: &Portable) -> Result<()> {
        for dependency in &checkpoint.dependencies {
            if self
                .store
                .head(
                    access,
                    &dependency.metadata.object,
                    &dependency.metadata.version,
                )
                .await?
                != dependency.metadata
            {
                return Err(Error::Integrity);
            }
            let mut hash = blake3::Hasher::new();
            let mut offset = 0;
            // The scratch is charged, bounded and reused one range at a time.
            while offset < dependency.metadata.bytes {
                let len =
                    (dependency.metadata.bytes - offset).min(self.metadata_limit as u64) as usize;
                let bytes = self
                    .store
                    .read(
                        access,
                        &dependency.metadata.object,
                        &dependency.metadata.version,
                        offset,
                        self.allocate(len)?,
                    )
                    .await?;
                hash.update(bytes.as_ref());
                offset += len as u64;
            }
            // Even empty objects require a current read authorization and exact version.
            if dependency.metadata.bytes == 0 {
                self.store
                    .read(
                        access,
                        &dependency.metadata.object,
                        &dependency.metadata.version,
                        0,
                        self.allocate(0)?,
                    )
                    .await?;
            }
            if hash.finalize().to_hex().as_str() != dependency.content_hash {
                return Err(Error::Integrity);
            }
        }
        Ok(())
    }
    fn allocate(&self, bytes: usize) -> Result<Buffer> {
        crate::allocate_exact(self.alloc.as_ref(), bytes)
    }
}
