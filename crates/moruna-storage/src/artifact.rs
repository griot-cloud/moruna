//! Explicit, budgeted historical task rows, independent from fusion, logs and spill.
//! Opaque fused kinds must expose a logical boundary tap or refuse required capture before
//! execution. This module does not pretend to instrument opaque executable code.
#![forbid(unsafe_code)]

use crate::{Access, Condition, Error, Governed, Metadata, Object, Result, VERSION};
use moruna_kernel::{Allocator, BufferView, Tier};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A logical task output identity, independent of fused executable placement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// Producing workspace; a delegated target object may have a different resource owner.
    pub workspace: String,
    /// Saved run identity.
    pub run: String,
    /// Attempt identity, preventing a retry from mutating another attempt's history.
    pub attempt: String,
    /// Logical task identity, preserved through executable fusion.
    pub task: String,
    /// Logical output port identity.
    pub port: String,
}
/// Declared kind capability, checked before the fused executable starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Boundary {
    /// The kind exposes exact logical task/output-port rows to a publisher.
    LogicalRows,
    /// The executable is opaque; disabling fusion is not an implicit substitute.
    Opaque,
}
/// Explicit orchestrator decision; no capture is inferred from spilling or logging.
#[derive(Clone, Debug)]
pub struct Request {
    /// Identity to retain.
    pub identity: Identity,
    /// Required capture must fail the run on refusal/failure, never silently truncate.
    pub required: bool,
    /// Explicit admitted encoded-byte ceiling. The engine reserves its real queue/arena
    /// budget before prepare; this number is not a physical bucket-capacity lease.
    pub byte_budget: u64,
    /// Immutable policy decision identifier; policy is enforced by the injected verifier.
    pub policy: String,
}
/// Publication/completeness is explicit; a partial object is never labelled complete.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum State {
    /// Capture was approved but has not completed.
    Capturing,
    /// All rows at the logical output boundary were captured successfully.
    Complete,
    /// Required or optional capture was refused with a nonsecret reason.
    Refused(String),
}
/// Self-contained retained-row encoding. Unknown formats are refused during decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Encoding {
    /// Arrow IPC stream with schema, record batches and end-of-stream marker.
    ArrowIpcStream,
}
/// One immutable batch object, with exact schema/content identity and measured counts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    /// Independently decodable chunk encoding, with no external dictionary fragments.
    pub encoding: Encoding,
    /// Immutable governed object/version.
    pub metadata: Metadata,
    /// Hash over the complete encoded Arrow record.
    pub content_hash: String,
    /// Schema identity, supplied by the engine's checked logical boundary.
    pub schema_identity: String,
    /// Verified row count at that boundary.
    pub rows: u64,
}
/// The public artifact fact, shared by CLI/SDK/MCP adapters rather than separate UI state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Storage manifest format.
    pub version: u32,
    /// Exact task/run/attempt/output identity.
    pub identity: Identity,
    /// Policy under which publication was requested.
    pub policy: String,
    /// Explicit publication state.
    pub state: State,
    /// Every complete immutable chunk admitted so far.
    pub chunks: Vec<Chunk>,
    /// Actual retained rows.
    pub rows: u64,
    /// Actual retained encoded bytes.
    pub bytes: u64,
}
/// Stateful publisher held by the engine's logical-boundary tap.
pub struct Capture {
    request: Request,
    manifest: Manifest,
    pointer: Option<Metadata>,
    target: Object,
    store: Arc<Governed>,
    alloc: Arc<dyn Allocator>,
    metadata_limit: usize,
}
impl Capture {
    /// Check kind support and reserved capacity before execution. The engine must propagate
    /// any error for required capture; this never rewrites the job to disable fusion.
    pub fn prepare(
        request: Request,
        boundary: Boundary,
        target: Object,
        store: Arc<Governed>,
        alloc: Arc<dyn Allocator>,
        metadata_limit: usize,
    ) -> Result<Self> {
        if boundary != Boundary::LogicalRows {
            return Err(Error::Unavailable(
                "kind has no logical row boundary publisher",
            ));
        }
        if request.byte_budget == 0
            || metadata_limit == 0
            || request.policy.is_empty()
            || [
                &request.identity.workspace,
                &request.identity.run,
                &request.identity.attempt,
                &request.identity.task,
                &request.identity.port,
            ]
            .iter()
            .any(|v| v.is_empty())
        {
            return Err(Error::Invalid(
                "artifact identity, policy or reserved budget",
            ));
        }
        let manifest = Manifest {
            version: VERSION,
            identity: request.identity.clone(),
            policy: request.policy.clone(),
            state: State::Capturing,
            chunks: Vec::new(),
            rows: 0,
            bytes: 0,
        };
        Ok(Self {
            request,
            manifest,
            pointer: None,
            target,
            store,
            alloc,
            metadata_limit,
        })
    }
    /// Begin by publishing the explicit incomplete state before any rows execute.
    pub async fn begin(&mut self, access: &Access) -> Result<()> {
        if self.pointer.is_some() || self.manifest.state != State::Capturing {
            return Err(Error::Invalid("artifact already begun"));
        }
        self.publish(access).await
    }
    /// Append one engine-verified complete Arrow IPC stream (schema, batches, EOS) at
    /// the declared logical task boundary. Fragments are not independent historical chunks.
    /// Schema and row count are the engine's checked facts, never stdout or spill metadata.
    pub async fn append(
        &mut self,
        access: &Access,
        schema_identity: String,
        rows: u64,
        encoded: BufferView,
    ) -> Result<()> {
        self.check_access(access)?;
        if self.pointer.is_none() || self.manifest.state != State::Capturing {
            return Err(Error::Invalid("artifact is not capturing"));
        }
        let bytes = encoded
            .as_host_slice()
            .ok_or(Error::Invalid("artifact needs host record"))?;
        let next = self
            .manifest
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or(Error::Invalid("artifact count overflow"))?;
        if next > self.request.byte_budget {
            let cause = Error::CaptureCapacity {
                need: next,
                budget: self.request.byte_budget,
            };
            return Err(self
                .refuse_after(access, "artifact byte budget exceeded", cause)
                .await);
        }
        if schema_identity.is_empty() {
            return Err(Error::Invalid("artifact schema identity"));
        }
        let content_hash = blake3::hash(bytes).to_hex().to_string();
        let key = format!(
            "{}.chunks/{:020}-{}",
            self.target.key,
            self.manifest.chunks.len(),
            content_hash
        );
        let object = Object {
            key,
            ..self.target.clone()
        };
        let length = encoded.len() as u64;
        let publication = match self.store.write(access, &object, encoded).await {
            Err(Error::Conflict) => {
                self.reuse_chunk(access, &object, length, &content_hash)
                    .await
            }
            other => other,
        };
        let metadata = match publication {
            Ok(metadata) => metadata,
            Err(cause) => {
                return Err(self
                    .refuse_after(access, "artifact object publication failed", cause)
                    .await);
            }
        };
        self.manifest.rows = self
            .manifest
            .rows
            .checked_add(rows)
            .ok_or(Error::Invalid("artifact row overflow"))?;
        self.manifest.bytes = next;
        self.manifest.chunks.push(Chunk {
            encoding: Encoding::ArrowIpcStream,
            metadata,
            content_hash,
            schema_identity,
            rows,
        });
        self.publish(access).await
    }
    /// Complete only at the engine's end-of-output event after all appends succeeded.
    pub async fn complete(&mut self, access: &Access) -> Result<Metadata> {
        self.check_access(access)?;
        if self.manifest.state == State::Complete {
            let pointer = self.pointer.as_ref().ok_or(Error::Integrity)?;
            let reader = Reader::new(self.store.clone(), self.alloc.clone(), self.metadata_limit)?;
            if reader.manifest(access, pointer).await? != self.manifest {
                return Err(Error::Integrity);
            }
            return Ok(pointer.clone());
        }

        if self.pointer.is_none() || self.manifest.state != State::Capturing {
            return Err(Error::Invalid("artifact cannot complete"));
        }
        self.manifest.state = State::Complete;
        if let Err(e) = self.publish(access).await {
            self.manifest.state = State::Refused("completion publication failed".into());
            return Err(e);
        }
        self.pointer
            .clone()
            .ok_or(Error::Invalid("artifact completion missing"))
    }
    /// Publish refusal explicitly; partial chunks remain visibly incomplete.
    pub async fn refuse(&mut self, access: &Access, reason: &'static str) -> Result<()> {
        self.manifest.state = State::Refused(reason.into());
        self.publish(access).await
    }
    /// Persist an immutable acknowledged checkpoint prefix after the engine FIFO drain.
    /// Capturing remains Capturing; an already observed real EOF may preserve Complete.
    pub async fn snapshot(
        &mut self,
        access: &Access,
        immutable_target: &Object,
    ) -> Result<Metadata> {
        self.check_access(access)?;
        immutable_target.validate()?;
        if immutable_target == &self.target || matches!(self.manifest.state, State::Refused(_)) {
            return Err(Error::Invalid("artifact snapshot state/target"));
        }
        let pointer = self
            .pointer
            .as_ref()
            .ok_or(Error::Invalid("artifact prefix not acknowledged"))?;
        let reader = Reader::new(self.store.clone(), self.alloc.clone(), self.metadata_limit)?;
        let actual = reader.manifest(access, pointer).await?;
        if actual != self.manifest {
            return Err(Error::Integrity);
        }
        self.verify_prefix(access, &actual).await?;
        let encoded = self.encode_manifest(&actual)?;
        self.store
            .write(access, immutable_target, Arc::new(encoded).view())
            .await
    }
    /// Restore a pinned immutable prefix into an ALREADY prepared capture. Keep this
    /// actual identity/target/allocator/budget; verify chunks, then CAS current progress.
    pub async fn resume(&mut self, access: &Access, snapshot: &Metadata) -> Result<()> {
        self.check_access(access)?;
        if self.pointer.is_some()
            || !self.manifest.chunks.is_empty()
            || self.manifest.state != State::Capturing
            || snapshot.object == self.target
        {
            return Err(Error::Invalid(
                "artifact resume needs prepared capture and immutable prefix",
            ));
        }
        let reader = Reader::new(self.store.clone(), self.alloc.clone(), self.metadata_limit)?;
        let prefix = reader.manifest(access, snapshot).await?;
        if prefix.identity != self.request.identity
            || prefix.policy != self.request.policy
            || matches!(prefix.state, State::Refused(_))
        {
            return Err(Error::Integrity);
        }
        if prefix.bytes > self.request.byte_budget {
            return Err(Error::CaptureCapacity {
                need: prefix.bytes,
                budget: self.request.byte_budget,
            });
        }
        self.verify_prefix(access, &prefix).await?;
        let current = match self.store.current(access, &self.target).await {
            Ok(metadata) => Some(metadata),
            Err(Error::Missing) => None,
            Err(cause) => return Err(cause),
        };
        if let Some(current) = &current {
            let actual = reader.manifest(access, current).await?;
            if actual.identity != prefix.identity || actual.policy != prefix.policy {
                return Err(Error::Integrity);
            }
            let shared = actual.chunks.len().min(prefix.chunks.len());
            if actual.chunks[..shared] != prefix.chunks[..shared] {
                return Err(Error::Conflict);
            }
        }
        let encoded = self.encode_manifest(&prefix)?;
        let condition = current
            .map(|metadata| Condition::Match(metadata.version))
            .unwrap_or(Condition::Absent);
        let pointer = self
            .store
            .replace(access, &self.target, condition, Arc::new(encoded).view())
            .await?;
        self.manifest = prefix;
        self.pointer = Some(pointer);
        Ok(())
    }
    async fn reuse_chunk(
        &self,
        access: &Access,
        object: &Object,
        length: u64,
        hash: &str,
    ) -> Result<Metadata> {
        // Replay after checkpoint may encounter an already-acknowledged identical
        // immutable chunk. Reuse ONLY its actual pinned bytes, never assume key==hash.
        let metadata = self.store.current(access, object).await?;
        if metadata.bytes != length {
            return Err(Error::Integrity);
        }
        let size = usize::try_from(length).map_err(|_| Error::Integrity)?;
        let buffer = crate::allocate_exact(self.alloc.as_ref(), size)?;
        let bytes = self
            .store
            .read(access, object, &metadata.version, 0, buffer)
            .await?;
        if blake3::hash(bytes.as_ref()).to_hex().as_str() != hash {
            return Err(Error::Integrity);
        }
        Ok(metadata)
    }
    async fn refuse_after(
        &mut self,
        access: &Access,
        reason: &'static str,
        primary: Error,
    ) -> Error {
        match self.refuse(access, reason).await {
            Ok(()) => primary,
            Err(cleanup) => Error::Cleanup {
                primary: Box::new(primary),
                cleanup: Box::new(cleanup),
            },
        }
    }
    async fn verify_prefix(&self, access: &Access, manifest: &Manifest) -> Result<()> {
        let reader = Reader::new(self.store.clone(), self.alloc.clone(), self.metadata_limit)?;
        for (index, chunk) in manifest.chunks.iter().enumerate() {
            let size = usize::try_from(chunk.metadata.bytes).map_err(|_| Error::Integrity)?;
            let buffer = crate::allocate_exact(self.alloc.as_ref(), size)?;
            let _verified = reader.chunk(access, manifest, index, buffer).await?;
        }
        Ok(())
    }
    fn encode_manifest(&self, manifest: &Manifest) -> Result<moruna_kernel::Buffer> {
        let bytes = serde_json::to_vec(manifest)
            .map_err(|_| Error::Invalid("artifact metadata serialization"))?;
        if bytes.len() > self.metadata_limit {
            return Err(Error::Invalid("artifact metadata budget"));
        }
        let mut buffer = crate::allocate_exact(self.alloc.as_ref(), bytes.len())?;
        buffer.as_mut().copy_from_slice(&bytes);
        Ok(buffer)
    }
    /// Actual last successfully published pointer, including Capturing/Refused facts.
    /// In-memory state without a successful write never invents a metadata reference.
    pub fn current_metadata(&self) -> Option<&Metadata> {
        self.pointer.as_ref()
    }
    /// Current engine/public-port fact, including incomplete/refused state.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    /// Whether failure must terminate the run, as decided by the orchestrator.
    pub fn required(&self) -> bool {
        self.request.required
    }
    fn check_access(&self, access: &Access) -> Result<()> {
        if access.workspace != self.request.identity.workspace
            || access.run != self.request.identity.run
        {
            return Err(Error::Denied);
        }
        Ok(())
    }
    async fn publish(&mut self, access: &Access) -> Result<()> {
        self.check_access(access)?;
        let bytes = serde_json::to_vec(&self.manifest)
            .map_err(|_| Error::Invalid("artifact metadata serialization"))?;
        if bytes.len() > self.metadata_limit {
            return Err(Error::Invalid("artifact metadata budget"));
        }
        let mut buffer = self
            .alloc
            .alloc(bytes.len(), Tier::Host)
            .map_err(|_| Error::Unavailable("artifact metadata arena"))?;
        buffer.as_mut().copy_from_slice(&bytes);
        let condition = self
            .pointer
            .as_ref()
            .map(|m| Condition::Match(m.version.clone()))
            .unwrap_or(Condition::Absent);
        self.pointer = Some(
            self.store
                .replace(access, &self.target, condition, Arc::new(buffer).view())
                .await?,
        );
        Ok(())
    }
}

/// Stateless historical reader shared by CLI/SDK/MCP surfaces. Reading never re-executes
/// a task, queries current source data, or mistakes spill/logs for retained row objects.
pub struct Reader {
    store: Arc<Governed>,
    alloc: Arc<dyn Allocator>,
    metadata_limit: usize,
}
impl Reader {
    /// Use the same live verifier and charged allocator as any other object consumer.
    pub fn new(
        store: Arc<Governed>,
        alloc: Arc<dyn Allocator>,
        metadata_limit: usize,
    ) -> Result<Self> {
        if metadata_limit == 0 {
            return Err(Error::Invalid("artifact metadata budget"));
        }
        Ok(Self {
            store,
            alloc,
            metadata_limit,
        })
    }
    /// Read exact persisted publication/completeness facts under their pinned version.
    pub async fn manifest(&self, access: &Access, metadata: &Metadata) -> Result<Manifest> {
        let len = usize::try_from(metadata.bytes)
            .map_err(|_| Error::Invalid("artifact manifest length"))?;
        if len > self.metadata_limit {
            return Err(Error::Invalid("artifact metadata budget"));
        }
        let dst = self
            .alloc
            .alloc(len, Tier::Host)
            .map_err(|_| Error::Unavailable("artifact manifest arena"))?;
        let bytes = self
            .store
            .read(access, &metadata.object, &metadata.version, 0, dst)
            .await?;
        let manifest: Manifest =
            serde_json::from_slice(bytes.as_ref()).map_err(|_| Error::Integrity)?;
        if manifest.version != VERSION {
            return Err(Error::Unavailable("artifact manifest version"));
        }
        if [
            &manifest.identity.workspace,
            &manifest.identity.run,
            &manifest.identity.attempt,
            &manifest.identity.task,
            &manifest.identity.port,
        ]
        .iter()
        .any(|v| v.is_empty())
        {
            return Err(Error::Integrity);
        }
        let (mut rows, mut bytes) = (0u64, 0u64);
        for chunk in &manifest.chunks {
            if chunk.schema_identity.is_empty()
                || chunk.content_hash.len() != 64
                || !chunk.content_hash.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(Error::Integrity);
            }
            rows = rows.checked_add(chunk.rows).ok_or(Error::Integrity)?;
            bytes = bytes
                .checked_add(chunk.metadata.bytes)
                .ok_or(Error::Integrity)?;
        }
        if rows != manifest.rows || bytes != manifest.bytes {
            return Err(Error::Integrity);
        }
        Ok(manifest)
    }
    /// Read a retained immutable encoded record into the caller's exact-size destination,
    /// re-verifying authority and content. Incomplete/refused publication remains explicit
    /// in the supplied manifest; this function never labels it complete.
    pub async fn chunk(
        &self,
        access: &Access,
        manifest: &Manifest,
        index: usize,
        destination: moruna_kernel::Buffer,
    ) -> Result<moruna_kernel::Buffer> {
        if manifest.version != VERSION {
            return Err(Error::Unavailable("artifact manifest version"));
        }
        if [
            &manifest.identity.workspace,
            &manifest.identity.run,
            &manifest.identity.attempt,
            &manifest.identity.task,
            &manifest.identity.port,
        ]
        .iter()
        .any(|v| v.is_empty())
        {
            return Err(Error::Integrity);
        }
        let chunk = manifest
            .chunks
            .get(index)
            .ok_or(Error::Invalid("artifact chunk index"))?;
        if destination.len() as u64 != chunk.metadata.bytes {
            return Err(Error::Invalid("artifact destination length"));
        }
        let mut bytes = self
            .store
            .read(
                access,
                &chunk.metadata.object,
                &chunk.metadata.version,
                0,
                destination,
            )
            .await?;
        if blake3::hash(bytes.as_ref()).to_hex().as_str() != chunk.content_hash {
            bytes.as_mut().fill(0);
            return Err(Error::Integrity);
        }
        Ok(bytes)
    }
}
