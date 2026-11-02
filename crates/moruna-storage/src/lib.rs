//! KS1's versioned governed storage boundary (SDD 32). Buckets are home; node-local
//! encrypted cache and scratch do not become a durable home implicitly.
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod artifact;
pub mod blocks;
pub mod bucket;
pub mod cache;
pub mod checkpoint;
pub mod conformance;
pub mod local;
#[cfg(all(feature = "native", target_os = "linux", target_arch = "x86_64"))]
pub mod native;
pub mod port;
pub mod prefetch;
pub mod provision;
pub mod proxy;
#[cfg(target_os = "linux")]
pub mod raw;
pub mod scratch;
pub mod staging;
pub mod volume;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use moruna_kernel::{Buffer, BufferView};
use zeroize::Zeroizing;

/// The major version of the storage operations and persisted identities.
pub const VERSION: u32 = 1;
/// An operation's typed result.
pub type Result<T> = std::result::Result<T, Error>;
/// An object-safe asynchronous operation, retaining its arguments until completion.
pub type Operation<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// A diagnosed storage refusal. Secret credentials and keys never appear in its fields.
#[derive(thiserror::Error)]
pub enum Error {
    /// The shared no_std codec refused an identity, version or durable frame.
    #[error(transparent)]
    Format(#[from] moruna_storage_format::Error),
    /// An owned operation stopped after safely draining its current bounded IO.
    #[error("storage operation cancelled")]
    Cancelled,
    /// The verifier refused this exact operation.
    #[error("storage access denied")]
    Denied,
    /// Budget admission has initiated room-making but no staging bytes are granted yet.
    #[error("SDK staging admission pending for {need} bytes")]
    StagingPending {
        /// Bytes that do not fit yet.
        need: u64,
    },
    /// SDK payload staging cannot fit the run's actual host budget.
    #[error("SDK staging refused: {need} bytes need room in budget {budget}")]
    StagingRefused {
        /// Unadmitted bytes.
        need: u64,
        /// Host budget.
        budget: u64,
    },
    /// Explicit retained-row queue/encoding admission refusal, never silent truncation.
    #[error("retained-row capture needs {need} bytes in budget {budget}")]
    CaptureCapacity {
        /// Requested charged capacity.
        need: u64,
        /// Available configured budget.
        budget: u64,
    },
    /// Typed remote refusal from the sole versioned storage object port.
    #[error("remote storage refused: {0:?}")]
    Remote(moruna_storage_format::wire::Refusal),
    /// Multiple admitted parallel operations failed; preserve every original diagnosis.
    #[error("parallel storage operations failed")]
    Concurrent {
        /// Earlier observed failure.
        first: Box<Error>,
        /// Another admitted operation's actual failure.
        next: Box<Error>,
    },
    /// A malformed identity, range or state.
    #[error("invalid storage request: {0}")]
    Invalid(&'static str),
    /// The observed version no longer owns the publication slot.
    #[error("storage version conflict")]
    Conflict,
    /// The requested immutable object does not exist.
    #[error("storage object missing")]
    Missing,
    /// Authentication of encrypted or hashed content failed.
    #[error("storage content authentication failed")]
    Integrity,
    /// An explicitly required facility is unavailable.
    #[error("storage facility unavailable: {0}")]
    Unavailable(&'static str),
    /// Local checkpoint is durable, but portability has not been acknowledged.
    #[error("local checkpoint committed; bucket checkpoint publication pending")]
    PortablePending {
        /// Actual durable local fact, usable by the orchestrator for recovery/status.
        local: Box<Metadata>,
        /// The bucket publication failure.
        cause: Box<Error>,
    },
    /// The reactor's original typed failure, retained without pretending it is cancellation.
    #[error("storage reactor operation failed")]
    Reactor(#[source] Box<moruna_kernel::MorunaError>),
    /// The charged arena refused an allocation.
    #[error("storage arena allocation failed")]
    Arena(#[source] Box<moruna_kernel::MorunaError>),
    /// A local IO error, without credentials or object URLs.
    #[error("storage {operation}: {source}")]
    Io {
        /// The operation being attempted.
        operation: &'static str,
        /// The local system's error.
        #[source]
        source: std::io::Error,
    },
    /// A bucket backend failed. The source is retained for explicit inspection; automatic
    /// display deliberately omits signed request URLs and backend response text.
    #[error("bucket {operation} failed")]
    Bucket {
        /// The operation being attempted.
        operation: &'static str,
        /// The backend's typed error.
        #[source]
        source: Box<object_store::Error>,
    },
    /// An operation failed and its temporary-object cleanup also failed.
    #[error("storage operation and cleanup both failed")]
    Cleanup {
        /// The initial error.
        primary: Box<Error>,
        /// The cleanup error.
        cleanup: Box<Error>,
    },
    /// An upload failed and its attempted abort also failed.
    #[error("multipart upload and abort both failed")]
    UploadAbort {
        /// The initial failure.
        primary: Box<Error>,
        /// The abort failure.
        abort: Box<Error>,
    },
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

pub(crate) fn io(operation: &'static str, source: std::io::Error) -> Error {
    Error::Io { operation, source }
}

pub use moruna_storage_format::{Metadata, Object, Role, Scope, Version};

/// An opaque run capability. Debug never exposes its bytes.
pub struct Credential(CredentialKind);
enum CredentialKind {
    Application(Zeroizing<Vec<u8>>),
    #[cfg(all(feature = "native", target_os = "linux", target_arch = "x86_64"))]
    Native(Vec<Arc<native::Connection>>),
}
impl Credential {
    /// The surface supplies a credential; the storage layer assumes no token syntax.
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.is_empty() {
            return Err(Error::Denied);
        }
        Ok(Self(CredentialKind::Application(Zeroizing::new(bytes))))
    }
    /// Actual application-issued bytes only. Native session authority is an actual
    /// connection grant and can never be serialized as fabricated opaque identity.
    pub fn application(&self) -> Option<&[u8]> {
        match &self.0 {
            CredentialKind::Application(bytes) => Some(bytes),
            #[cfg(all(feature = "native", target_os = "linux", target_arch = "x86_64"))]
            CredentialKind::Native(_) => None,
        }
    }
    /// Combine only real consumed/handshaken native connection grants for explicit roles
    /// under the same actual account/scope. No bytes or wire marker can mint a grant.
    #[cfg(all(feature = "native", target_os = "linux", target_arch = "x86_64"))]
    pub fn native_clients(clients: &[Arc<native::Client>]) -> Result<Self> {
        let first = clients.first().ok_or(Error::Denied)?;
        let mut grants = Vec::new();
        for client in clients {
            if client.scope() != first.scope() || !Arc::ptr_eq(client.account(), first.account()) {
                return Err(Error::Denied);
            }
            if !grants
                .iter()
                .any(|grant| Arc::ptr_eq(grant, client.connection()))
            {
                grants.push(Arc::clone(client.connection()));
            }
        }
        Ok(Self(CredentialKind::Native(grants)))
    }
    #[cfg(all(feature = "native", target_os = "linux", target_arch = "x86_64"))]
    pub(crate) fn from_native(connection: Arc<native::Connection>) -> Self {
        Self(CredentialKind::Native(vec![connection]))
    }
    #[cfg(all(feature = "native", target_os = "linux", target_arch = "x86_64"))]
    pub(crate) fn contains_native(&self, connection: &Arc<native::Connection>) -> bool {
        match &self.0 {
            CredentialKind::Native(grants) => {
                grants.iter().any(|grant| Arc::ptr_eq(grant, connection))
            }
            _ => false,
        }
    }
}
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential([redacted])")
    }
}

/// The run making an operation; workspace binding is checked before the verifier.
#[derive(Debug)]
pub struct Access {
    /// Explicit workspace.
    pub workspace: String,
    /// Run identity, supplied by the orchestrator.
    pub run: String,
    /// Opaque per-run authority.
    pub credential: Credential,
}

pub use moruna_storage_format::auth::Action;

/// Verifies the live run credential for every exact operation. Implementations must check
/// revocation/expiry and resource policy, and must never include credential bytes in errors.
pub trait Authorizer: Send + Sync {
    /// Validate this operation; no success is cached by `Governed`.
    fn verify<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        action: Action,
        version: Option<&'a Version>,
    ) -> Operation<'a, ()>;
}

/// Conditional visibility, never emulated by an unconditional write.
#[derive(Clone, Debug)]
pub enum Condition {
    /// Publish only if the key has never been created or is absent.
    Absent,
    /// Replace only the supplied observed version.
    Match(Version),
}

/// Storage backend major 1. Implementations perform no implicit local/cloud fallback.
/// Only a `Governed` instance is handed to run consumers.
pub trait Backend: Send + Sync {
    /// Actual configured backend class; unknown/format fixtures cannot attest bucket home.
    fn role(&self) -> Option<moruna_storage_format::auth::SessionRole>;

    /// Exact current object metadata; absence is Missing, not an enumeration operation.
    fn current<'a>(&'a self, scope: &'a Scope, object: &'a Object) -> Operation<'a, Metadata>;

    /// Protocol major; a peer with another major is refused before IO.
    fn version(&self) -> u32 {
        VERSION
    }
    /// Exact object metadata under an immutable version, with read authority.
    fn head<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
    ) -> Operation<'a, Metadata>;
    /// Read exactly `dst.len()` bytes at `offset` of the pinned object version.
    fn read<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        dst: Buffer,
    ) -> Operation<'a, Buffer>;
    /// Publish a whole immutable object; creation refuses an existing key.
    fn write<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        src: BufferView,
    ) -> Operation<'a, Metadata>;
    /// Recursively list keys under this exact prefix.
    fn list<'a>(&'a self, scope: &'a Scope, prefix: &'a Object) -> Operation<'a, Vec<Metadata>>;
    /// One atomic conditional whole-object replacement.
    fn replace<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        condition: Condition,
        src: BufferView,
    ) -> Operation<'a, Metadata>;
}

/// The run-facing port: every operation is authorized, including cache reads.
pub struct Governed {
    backend: Arc<dyn Backend>,
    authorizer: Arc<dyn Authorizer>,
}
impl Governed {
    /// Refuse an unsupported major at construction.
    pub fn new(backend: Arc<dyn Backend>, authorizer: Arc<dyn Authorizer>) -> Result<Self> {
        if backend.version() != VERSION {
            return Err(Error::Unavailable("storage protocol major"));
        }
        Ok(Self {
            backend,
            authorizer,
        })
    }
    /// Nonsecret actual backend class; conveys no resource authority or cloud proof.
    pub fn role(&self) -> Option<moruna_storage_format::auth::SessionRole> {
        self.backend.role()
    }
    /// Live authorization shared by encrypted cache and artifact/checkpoint boundaries.
    pub async fn authorize(
        &self,
        access: &Access,
        object: &Object,
        action: Action,
        version: Option<&Version>,
    ) -> Result<()> {
        object.validate()?;
        Scope::validate_parts(&access.workspace, &access.run)?;
        self.authorizer
            .verify(access, object, action, version)
            .await
    }
    /// Read current exact-object facts without exposing neighboring prefix metadata.
    pub async fn current(&self, access: &Access, object: &Object) -> Result<Metadata> {
        self.authorize(access, object, Action::Read, None).await?;
        let metadata = self
            .backend
            .current(
                &Scope {
                    workspace: access.workspace.clone(),
                    run: access.run.clone(),
                },
                object,
            )
            .await?;
        metadata.version.validate()?;
        if metadata.object != *object {
            return Err(Error::Integrity);
        }
        Ok(metadata)
    }
    /// Read exact pinned metadata under the same per-object read verifier, without
    /// requiring a prefix-list grant or exposing neighboring resource metadata.
    pub async fn head(
        &self,
        access: &Access,
        object: &Object,
        version: &Version,
    ) -> Result<Metadata> {
        version.validate()?;
        self.authorize(access, object, Action::Read, Some(version))
            .await?;
        self.backend
            .head(
                &Scope {
                    workspace: access.workspace.clone(),
                    run: access.run.clone(),
                },
                object,
                version,
            )
            .await
    }
    /// Read into the caller's charged allocation, pinned to an immutable version.
    pub async fn read(
        &self,
        access: &Access,
        object: &Object,
        version: &Version,
        offset: u64,
        dst: Buffer,
    ) -> Result<Buffer> {
        version.validate()?;
        if dst.host_ptr().is_none() || offset.checked_add(dst.len() as u64).is_none() {
            return Err(Error::Invalid("host range"));
        }
        self.authorize(access, object, Action::Read, Some(version))
            .await?;
        let scope = Scope {
            workspace: access.workspace.clone(),
            run: access.run.clone(),
        };
        self.backend
            .read(&scope, object, version, offset, dst)
            .await
    }
    /// Write immutable content, never overwrite an existing name.
    pub async fn write(
        &self,
        access: &Access,
        object: &Object,
        src: BufferView,
    ) -> Result<Metadata> {
        self.authorize(access, object, Action::Write, None).await?;
        self.backend
            .write(
                &Scope {
                    workspace: access.workspace.clone(),
                    run: access.run.clone(),
                },
                object,
                src,
            )
            .await
    }
    /// Prefix listing under a separately verified capability.
    pub async fn list(&self, access: &Access, prefix: &Object) -> Result<Vec<Metadata>> {
        self.authorize(access, prefix, Action::List, None).await?;
        self.backend
            .list(
                &Scope {
                    workspace: access.workspace.clone(),
                    run: access.run.clone(),
                },
                prefix,
            )
            .await
    }
    /// Atomic publication under a separately verified capability.
    pub async fn replace(
        &self,
        access: &Access,
        object: &Object,
        condition: Condition,
        src: BufferView,
    ) -> Result<Metadata> {
        if let Condition::Match(version) = &condition {
            version.validate()?;
        }
        self.authorize(
            access,
            object,
            Action::Publish,
            match &condition {
                Condition::Match(version) => Some(version),
                Condition::Absent => None,
            },
        )
        .await?;
        self.backend
            .replace(
                &Scope {
                    workspace: access.workspace.clone(),
                    run: access.run.clone(),
                },
                object,
                condition,
                src,
            )
            .await
    }
}

// Allocators may retain a minimum backing allocation even for zero logical bytes.
// Split through the kernel's safe ownership API so empty objects remain truly empty.
pub(crate) fn allocate_exact(
    allocator: &dyn moruna_kernel::Allocator,
    bytes: usize,
) -> Result<moruna_kernel::Buffer> {
    let buffer = allocator
        .alloc(bytes, moruna_kernel::Tier::Host)
        .map_err(|e| Error::Arena(Box::new(e)))?;
    if buffer.host_ptr().is_none() || buffer.len() < bytes {
        return Err(Error::Invalid("charged host allocation length"));
    }
    if buffer.len() == bytes {
        return Ok(buffer);
    }
    let (exact, excess) = buffer.split_at(bytes);
    drop(excess);
    Ok(exact)
}

impl Error {
    /// Actual capacity demand/budget when this refusal supplied both values; no fake zero
    /// budget is synthesized for an unavailable/pending provider.
    pub fn capacity(&self) -> Option<(u64, u64)> {
        match self {
            Self::CaptureCapacity { need, budget } | Self::StagingRefused { need, budget } => {
                Some((*need, *budget))
            }
            _ => None,
        }
    }
    /// Actual durable local receipt and bucket-publication cause for an explicitly pending
    /// portable checkpoint. This is not a portable-success receipt or a rerun instruction.
    pub fn local_checkpoint(&self) -> Option<(&Metadata, &Error)> {
        match self {
            Self::PortablePending { local, cause } => Some((local.as_ref(), cause.as_ref())),
            _ => None,
        }
    }
}
